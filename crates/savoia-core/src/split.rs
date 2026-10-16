//! Splits SQL text into statements without parsing it: quotes, comments,
//! Postgres dollar quotes and MySQL `DELIMITER` lines are skipped over, so a
//! `;` inside them doesn't end a statement. Used by the console to run the
//! statement under the caret, and by import (M4) to run files.

use std::ops::Range;

use crate::Engine;

/// The byte ranges of the statements in `sql`, without their delimiter and
/// without the comments and whitespace before them.
pub fn split(sql: &str, engine: Engine) -> Vec<Range<usize>> {
    Splitter::new(sql, engine).run().0
}

/// [`split`] for a script read in pieces: starts with `delimiter` in effect
/// (MySQL's `DELIMITER` may have changed it earlier in the script) and pairs
/// each statement with the delimiter in effect where it starts.
pub fn split_from(sql: &str, engine: Engine, delimiter: &str) -> Vec<(Range<usize>, String)> {
    let mut splitter = Splitter::new(sql, engine);
    splitter.delimiter = delimiter.to_owned();
    splitter.scan();
    splitter
        .statements
        .into_iter()
        .zip(splitter.delimiters)
        .collect()
}

/// The statement the caret at byte `offset` belongs to: the one around it,
/// else the one ending earlier on the caret's line, else the next one, else
/// the last one.
pub fn statement_at(sql: &str, engine: Engine, offset: usize) -> Option<Range<usize>> {
    let statements = split(sql, engine);
    if let Some(around) = statements
        .iter()
        .find(|s| s.start <= offset && offset <= s.end)
    {
        return Some(around.clone());
    }
    let same_line = statements
        .iter()
        .rev()
        .find(|s| s.end <= offset && !sql[s.end..offset].contains('\n'));
    let next = statements.iter().find(|s| s.start > offset);
    same_line.or(next).or(statements.last()).cloned()
}

/// `sql` as the server should receive it for a whole-script run. MySQL's
/// `DELIMITER` lines are client commands, so a script that has them is sent
/// as its statements joined by `;`. Anything else is sent as written.
pub fn script(sql: &str, engine: Engine) -> String {
    let (statements, directives) = Splitter::new(sql, engine).run();
    if !directives {
        return sql.to_owned();
    }
    statements
        .into_iter()
        .map(|r| &sql[r])
        .collect::<Vec<_>>()
        .join(";\n")
}

struct Splitter<'a> {
    sql: &'a str,
    bytes: &'a [u8],
    engine: Engine,
    delimiter: String,
    /// Start of the current statement's first significant byte.
    start: Option<usize>,
    /// End of the current statement's last significant byte.
    end: usize,
    statements: Vec<Range<usize>>,
    /// The delimiter in effect for each statement in `statements`.
    delimiters: Vec<String>,
    directives: bool,
}

impl<'a> Splitter<'a> {
    fn new(sql: &'a str, engine: Engine) -> Self {
        Self {
            sql,
            bytes: sql.as_bytes(),
            engine,
            delimiter: ";".into(),
            start: None,
            end: 0,
            statements: Vec::new(),
            delimiters: Vec::new(),
            directives: false,
        }
    }

    fn mysql(&self) -> bool {
        self.engine == Engine::Mysql
    }

    fn run(mut self) -> (Vec<Range<usize>>, bool) {
        self.scan();
        (self.statements, self.directives)
    }

    fn scan(&mut self) {
        let n = self.bytes.len();
        let mut i = 0;
        while i < n {
            if let Some(next) = self.directive(i) {
                i = next;
                continue;
            }
            let rest = &self.sql[i..];
            let c = self.bytes[i];
            if c.is_ascii_whitespace() {
                i += 1;
                continue;
            }
            if rest.starts_with("--") && (!self.mysql() || self.dash_comment(i)) {
                i = line_end(self.sql, i);
                continue;
            }
            if self.mysql() && c == b'#' {
                i = line_end(self.sql, i);
                continue;
            }
            if rest.starts_with("/*") {
                let after = self.block_comment(i);
                // `/*! … */` is MySQL code the server runs.
                if self.mysql() && rest.starts_with("/*!") {
                    self.start.get_or_insert(i);
                    self.end = after;
                }
                i = after;
                continue;
            }
            if rest.starts_with(self.delimiter.as_str()) {
                self.finish();
                i += self.delimiter.len();
                continue;
            }
            self.start.get_or_insert(i);
            i = match c {
                b'\'' => self.quoted(i, b'\'', self.mysql() || self.escape_string(i)),
                b'"' => self.quoted(i, b'"', self.mysql()),
                b'`' if self.mysql() => self.quoted(i, b'`', false),
                b'$' if !self.mysql() => self.dollar_quoted(i).unwrap_or(i + 1),
                _ => next_char(self.sql, i),
            };
            self.end = i;
        }
        self.finish();
    }

    fn finish(&mut self) {
        if let Some(start) = self.start.take() {
            self.statements.push(start..self.end);
            // `DELIMITER` only applies between statements, so this is the
            // one the statement started with.
            self.delimiters.push(self.delimiter.clone());
        }
    }

    /// A MySQL `DELIMITER x` line at the start of a statement: sets the
    /// delimiter and returns where the line ends.
    fn directive(&mut self, i: usize) -> Option<usize> {
        if !self.mysql() || self.start.is_some() {
            return None;
        }
        let line_start = self.sql[..i].rfind('\n').map_or(0, |p| p + 1);
        if !self.sql[line_start..i].trim().is_empty() {
            return None;
        }
        let word = self.sql.get(i..i + 9)?;
        let space = self
            .bytes
            .get(i + 9)
            .is_some_and(|b| b.is_ascii_whitespace());
        if !word.eq_ignore_ascii_case("delimiter") || !space {
            return None;
        }
        let end = line_end(self.sql, i);
        let delimiter = self.sql[i + 9..end].trim();
        if !delimiter.is_empty() {
            self.delimiter = delimiter.to_owned();
            self.directives = true;
        }
        Some(end)
    }

    /// MySQL needs whitespace (or the end) after `--` for a comment.
    fn dash_comment(&self, i: usize) -> bool {
        self.bytes
            .get(i + 2)
            .is_none_or(|b| b.is_ascii_whitespace())
    }

    /// Where a `/* … */` comment starting at `i` ends. Postgres nests them.
    /// Walks bytes, not `str` slices: `j` steps into multibyte characters.
    fn block_comment(&self, i: usize) -> usize {
        let mut depth = 0;
        let mut j = i;
        while j < self.bytes.len() {
            if self.bytes[j..].starts_with(b"/*") && (depth == 0 || !self.mysql()) {
                depth += 1;
                j += 2;
            } else if self.bytes[j..].starts_with(b"*/") {
                depth -= 1;
                j += 2;
                if depth == 0 {
                    return j;
                }
            } else {
                j += 1;
            }
        }
        self.bytes.len()
    }

    /// A Postgres `E'…'` string, where backslashes escape.
    fn escape_string(&self, i: usize) -> bool {
        i > 0 && matches!(self.bytes[i - 1], b'E' | b'e') && (i < 2 || !is_ident(self.bytes[i - 2]))
    }

    /// Where a quoted run starting at `i` ends. A doubled quote is part of
    /// it, and so is a backslash-escaped one when `backslash` is set.
    fn quoted(&self, i: usize, quote: u8, backslash: bool) -> usize {
        let mut j = i + 1;
        while j < self.bytes.len() {
            let b = self.bytes[j];
            if backslash && b == b'\\' {
                j += 2;
            } else if b == quote {
                if self.bytes.get(j + 1) == Some(&quote) {
                    j += 2;
                } else {
                    return j + 1;
                }
            } else {
                j += 1;
            }
        }
        self.bytes.len()
    }

    /// Where a `$tag$ … $tag$` string starting at `i` ends, if `i` opens one.
    fn dollar_quoted(&self, i: usize) -> Option<usize> {
        if i > 0 && is_ident(self.bytes[i - 1]) {
            return None;
        }
        let close = self.sql[i + 1..].find('$')? + i + 1;
        let tag = &self.sql[i + 1..close];
        let valid = tag
            .bytes()
            .enumerate()
            .all(|(k, b)| b == b'_' || b.is_ascii_alphabetic() || (k > 0 && b.is_ascii_digit()));
        if !valid {
            return None;
        }
        let delimiter = &self.sql[i..=close];
        Some(
            self.sql[close + 1..]
                .find(delimiter)
                .map_or(self.bytes.len(), |p| close + 1 + p + delimiter.len()),
        )
    }
}

fn is_ident(b: u8) -> bool {
    b == b'_' || b.is_ascii_alphanumeric() || b >= 0x80
}

fn line_end(sql: &str, i: usize) -> usize {
    sql[i..].find('\n').map_or(sql.len(), |p| i + p)
}

fn next_char(sql: &str, i: usize) -> usize {
    let mut j = i + 1;
    while !sql.is_char_boundary(j) {
        j += 1;
    }
    j
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn split_from_resumes_with_a_delimiter_and_reports_each() {
        let sql = "CREATE PROCEDURE p() BEGIN SELECT 1; END $$\nDELIMITER ;\nSELECT 2;";
        let parts: Vec<(&str, String)> = split_from(sql, Engine::Mysql, "$$")
            .into_iter()
            .map(|(range, delimiter)| (&sql[range], delimiter))
            .collect();
        assert_eq!(
            parts,
            [
                ("CREATE PROCEDURE p() BEGIN SELECT 1; END", "$$".to_owned()),
                ("SELECT 2", ";".to_owned()),
            ]
        );
    }

    fn texts(sql: &str, engine: Engine) -> Vec<&str> {
        split(sql, engine).into_iter().map(|r| &sql[r]).collect()
    }

    #[test]
    fn splits_on_semicolons_and_drops_empty_statements() {
        let sql = "SELECT 1;\n  SELECT 2 ;;\n\nSELECT 3";
        assert_eq!(
            texts(sql, Engine::Postgres),
            ["SELECT 1", "SELECT 2", "SELECT 3"]
        );
    }

    #[test]
    fn ignores_semicolons_in_quotes_and_comments() {
        let sql = "SELECT 'a;''b', \"x;y\" -- c;\n/* d; /* e; */ f; */ FROM t; SELECT 2";
        assert_eq!(
            texts(sql, Engine::Postgres),
            [
                "SELECT 'a;''b', \"x;y\" -- c;\n/* d; /* e; */ f; */ FROM t",
                "SELECT 2"
            ]
        );
    }

    #[test]
    fn leading_comments_are_not_part_of_a_statement() {
        let sql = "-- first\n/* note */\nSELECT 1;";
        assert_eq!(texts(sql, Engine::Postgres), ["SELECT 1"]);
    }

    #[test]
    fn postgres_dollar_quotes_and_escape_strings() {
        let sql = "CREATE FUNCTION f() RETURNS int AS $body$ BEGIN; RETURN 1; END $body$ LANGUAGE plpgsql;\n\
                   SELECT $$a;b$$, E'it\\'s;', $1;";
        assert_eq!(
            texts(sql, Engine::Postgres),
            [
                "CREATE FUNCTION f() RETURNS int AS $body$ BEGIN; RETURN 1; END $body$ LANGUAGE plpgsql",
                "SELECT $$a;b$$, E'it\\'s;', $1"
            ]
        );
    }

    #[test]
    fn mysql_quotes_comments_and_backslashes() {
        let sql = "SELECT 'it\\'s;', `a;b`, \"c\\\";\" # x;\nFROM t; SELECT 1--2;\nSELECT 3";
        assert_eq!(
            texts(sql, Engine::Mysql),
            [
                "SELECT 'it\\'s;', `a;b`, \"c\\\";\" # x;\nFROM t",
                "SELECT 1--2",
                "SELECT 3"
            ]
        );
    }

    #[test]
    fn mysql_delimiter_lines() {
        let sql = "DELIMITER //\nCREATE PROCEDURE p() BEGIN SELECT 1; SELECT 2; END //\ndelimiter ;\nCALL p();";
        assert_eq!(
            texts(sql, Engine::Mysql),
            [
                "CREATE PROCEDURE p() BEGIN SELECT 1; SELECT 2; END",
                "CALL p()"
            ]
        );
        assert_eq!(
            script(sql, Engine::Mysql),
            "CREATE PROCEDURE p() BEGIN SELECT 1; SELECT 2; END;\nCALL p()"
        );
        assert_eq!(
            script("SELECT 1; SELECT 2", Engine::Mysql),
            "SELECT 1; SELECT 2"
        );
    }

    #[test]
    fn mysql_executable_comments_are_statements() {
        let sql = "/*!40101 SET NAMES utf8 */;\nSELECT 1;";
        assert_eq!(
            texts(sql, Engine::Mysql),
            ["/*!40101 SET NAMES utf8 */", "SELECT 1"]
        );
    }

    #[test]
    fn unterminated_quotes_run_to_the_end() {
        assert_eq!(
            texts("SELECT 'a; SELECT 2", Engine::Postgres),
            ["SELECT 'a; SELECT 2"]
        );
    }

    #[test]
    fn keeps_multibyte_text_whole() {
        assert_eq!(
            texts("SELECT 'città'; SELECT ñ", Engine::Postgres),
            ["SELECT 'città'", "SELECT ñ"]
        );
        for engine in Engine::ALL {
            assert_eq!(
                texts("/* città */ SELECT 1; /* ñ é */ SELECT 2", engine),
                ["SELECT 1", "SELECT 2"],
                "{engine:?}"
            );
        }
        assert_eq!(
            texts("/* ñ /* é */ ü */ SELECT 1", Engine::Postgres),
            ["SELECT 1"]
        );
    }

    #[test]
    fn statement_at_the_caret() {
        let sql = "SELECT 1;\n\nSELECT 2; -- two\nSELECT 3";
        let at = |offset| statement_at(sql, Engine::Postgres, offset).map(|r| &sql[r]);
        assert_eq!(at(0), Some("SELECT 1"));
        assert_eq!(at(4), Some("SELECT 1"));
        assert_eq!(at(9), Some("SELECT 1"), "right after the semicolon");
        assert_eq!(at(10), Some("SELECT 2"), "blank line: the next one");
        assert_eq!(at(sql.find("two").unwrap()), Some("SELECT 2"), "same line");
        assert_eq!(at(sql.len()), Some("SELECT 3"));
        assert_eq!(statement_at("  -- nothing", Engine::Postgres, 0), None);
    }
}
