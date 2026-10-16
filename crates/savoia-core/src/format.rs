//! Formats SQL for the console's Format command: keywords in capitals, and
//! each statement laid out by [`sqlformat`], one clause per line.
//!
//! Formatting never changes what a statement does. Two things could:
//!
//! - **Case.** Postgres folds unquoted names to lower case, but MySQL can
//!   treat table names and aliases as case-sensitive, so `logs` and `LOGS`
//!   may be two tables there. [`capitalize`] changes only words that can't
//!   be such a name where they stand. sqlformat's own option capitalizes
//!   names too, whenever they are keywords to it: `status`, `type`, `logs`.
//! - **Layout.** sqlformat doesn't read SQL the way either server does: it
//!   reformats inside `$$` bodies and splits `B'…'` and `U&'…'` strings. So a
//!   laid-out statement is read again with each engine's own rules, and kept
//!   only if it has the same tokens with only the whitespace between them
//!   changed. Otherwise the statement keeps its own layout. A last check
//!   over the whole text makes sure the statements still split as before.
//!
//! Statements are always formatted whole, as the splitter finds them in all
//! of the text: a part of one read on its own could start inside a comment
//! or a string, and read its rest as code.

use std::ops::Range;

use sqlformat::{Dialect, FormatOptions, Indent, QueryParams};

use crate::Engine;
use crate::split::{self, comment_end, is_ident, quoted_end};

/// `sql` formatted, statement by statement. Statements are one blank line
/// apart, unless comments or `DELIMITER` lines stand between them, which
/// are kept as written.
pub fn format(sql: &str, engine: Engine) -> String {
    format_range(sql, 0..sql.len(), engine).0
}

/// `sql` with the statements that `range` touches formatted, as [`format`]
/// formats them all, and where those statements now are. A range that
/// touches none, or a result that fails the last check, leaves `sql` as it
/// is, with `range`.
pub fn format_range(sql: &str, range: Range<usize>, engine: Engine) -> (String, Range<usize>) {
    let (out, span) = statements(sql, &range, engine);
    // A last check over the whole text: only whitespace and case changed,
    // and the splitter reads the same statements. Line breaks matter to it:
    // `DELIMITER` after a `;` is no directive, but on a line of its own it
    // is one.
    let delimiters = |text: &str| -> Vec<String> {
        split::split_from(text, engine, ";")
            .into_iter()
            .map(|(_, delimiter)| delimiter)
            .collect()
    };
    let kept = same_tokens(
        &upper_words(sql, engine),
        &upper_words(&out, engine),
        engine,
    ) && delimiters(sql) == delimiters(&out);
    match span {
        Some(span) if kept => (out, span),
        _ => (sql.to_owned(), range),
    }
}

/// What [`format_range`] does, but for its last check.
fn statements(sql: &str, range: &Range<usize>, engine: Engine) -> (String, Option<Range<usize>>) {
    let statements = split::split_from(sql, engine, ";");
    let touched = |r: &Range<usize>| r.start < range.end && range.start < r.end;
    let word = |b: u8| is_ident(b) || b == b'$';
    let mut out = String::with_capacity(sql.len() + sql.len() / 4);
    let mut span: Option<Range<usize>> = None;
    let mut last = 0;
    for (k, (r, _)) in statements.iter().enumerate() {
        let gap = &sql[last..r.start];
        // A MySQL `DELIMITER $$` can end a statement inside what reads as
        // one word, `END$$CREATE`, which a line break would split.
        let glued = gap.bytes().last().is_some_and(word) && word(sql.as_bytes()[r.start]);
        match k.checked_sub(1).map(|p| &statements[p]) {
            // Just the delimiter between two formatted statements: kept as
            // typed, then a blank line.
            Some((p, delimiter))
                if touched(p) && touched(r) && gap.trim() == delimiter && !glued =>
            {
                out.push_str(gap.trim_end());
                out.push_str("\n\n");
            }
            _ => out.push_str(gap),
        }
        if touched(r) {
            let next = statements.get(k + 1).map_or(sql.len(), |(n, _)| n.start);
            let start = out.len();
            out.push_str(&statement(&sql[r.clone()], &sql[r.end..next], engine));
            span = Some(span.map_or(start, |s| s.start)..out.len());
        } else {
            out.push_str(&sql[r.clone()]);
        }
        last = r.end;
    }
    out.push_str(&sql[last..]);
    (out, span)
}

/// Where byte `offset` of `before` lands in `after`, its formatted text.
/// Formatting changes only whitespace and letter case, so it lands among
/// the same characters: after the one it followed, or before the one it
/// preceded when whitespace came first.
pub fn map_offset(before: &str, after: &str, offset: usize) -> usize {
    let head = &before[..offset];
    let kept = head.chars().filter(|c| !c.is_whitespace()).count();
    let before_next = head.chars().next_back().is_none_or(char::is_whitespace);
    let mut seen = 0;
    for (i, c) in after.char_indices().filter(|(_, c)| !c.is_whitespace()) {
        if seen == kept && before_next {
            return i;
        }
        seen += 1;
        if seen == kept && !before_next {
            return i + c.len_utf8();
        }
    }
    after.len()
}

/// Statements longer than this are capitalized but not laid out. On a long
/// statement full of keywords sqlformat slows down with the square of its
/// length; this keeps a Format well under a second.
const MAX_LAYOUT: usize = 256 * 1024;

/// One statement, without its delimiter. `then` is what follows it, up to
/// the next statement: its end reads differently without it (in MySQL,
/// `--` is a comment before whitespace or at the end, not before `;`).
fn statement(sql: &str, then: &str, engine: Engine) -> String {
    let capitalized = capitalize(sql, engine);
    if capitalized.len() > MAX_LAYOUT {
        return capitalized;
    }
    let laid_out = sqlformat::format(&capitalized, &QueryParams::None, &options(engine));
    let alike = same_tokens(
        &format!("{capitalized}{then}"),
        &format!("{laid_out}{then}"),
        engine,
    );
    if alike { laid_out } else { capitalized }
}

fn options(engine: Engine) -> FormatOptions<'static> {
    FormatOptions {
        indent: Indent::Spaces(2),
        // Case is `capitalize`'s job: see the module docs.
        uppercase: None,
        dialect: match engine {
            Engine::Postgres => Dialect::PostgreSql,
            Engine::Mysql => Dialect::Generic,
        },
        ..FormatOptions::default()
    }
}

/// Keywords wherever they stand, except as part of a qualified name. MySQL
/// 8.4 and MariaDB 11.4 reserve every one of them, so none can be an
/// unquoted name there.
const KEYWORDS: &str = "\
    ADD ALL ALTER ANALYZE AND AS ASC BETWEEN BY CALL CASCADE CASE CHECK COLLATE COLUMN \
    CONSTRAINT CREATE CROSS CURRENT_DATE CURRENT_TIME CURRENT_TIMESTAMP CURRENT_USER \
    DEFAULT DELETE DESC DESCRIBE DISTINCT DIV DROP DUAL ELSE EXCEPT EXISTS EXPLAIN \
    FALSE FETCH FOR FOREIGN FROM GRANT GROUP HAVING IF IN INDEX INNER INSERT INTERSECT \
    INTERVAL INTO IS JOIN LEFT LIKE LIMIT LOCALTIME LOCALTIMESTAMP NATURAL NOT NULL ON \
    OR ORDER OUTER OVER PARTITION PRIMARY PROCEDURE RANGE RECURSIVE REFERENCES REGEXP \
    RENAME REPLACE RESTRICT REVOKE RIGHT RLIKE ROWS SELECT SEPARATOR SET SHOW TABLE THEN \
    TO TRIGGER TRUE UNION UNIQUE UPDATE USE USING VALUES WHEN WHERE WITH XOR";

/// Keywords wherever they stand in Postgres, which reserves them or only
/// uses them in syntax of its own. MySQL or MariaDB doesn't reserve them.
const POSTGRES_KEYWORDS: &str = "\
    ANY ARRAY CAST CONCURRENTLY DO END FILTER FULL ILIKE ISNULL LATERAL NOTNULL NULLS OF \
    OFFSET ONLY RETURNING ROW SIMILAR SOME WINDOW WITHIN";

/// Keywords as the first word of a statement, where no name can stand.
const STATEMENTS: &str = "\
    BEGIN CHECKPOINT COMMENT COMMIT COPY DEALLOCATE DISCARD DO END EXECUTE FLUSH KILL \
    LISTEN LOAD LOCK MERGE NOTIFY OPTIMIZE PREPARE REFRESH REINDEX RELEASE RESET \
    ROLLBACK SAVEPOINT START TRUNCATE UNLISTEN UNLOCK VACUUM";

/// Keywords right after one of the given words, where no name can stand in
/// either engine, and often names elsewhere (`key`, `first`, `view`).
const AFTER: &[(&str, &str)] = &[
    ("COLUMNS", "FULL SHOW"),
    ("DATABASE", "ALTER CREATE DROP"),
    ("FIRST", "FETCH NULLS"),
    ("FULL", "SHOW"),
    ("FUNCTION", "ALTER CREATE DROP SHOW"),
    ("KEY", "DUPLICATE FOREIGN PRIMARY UNIQUE"),
    ("LAST", "NULLS"),
    ("MATCHED", "NOT WHEN"),
    ("MATERIALIZED", "ALTER CREATE DROP REFRESH"),
    ("NEXT", "FETCH"),
    ("NOTHING", "DO"),
    ("ROW", "CURRENT EACH"),
    ("SAVEPOINT", "RELEASE"),
    ("SCHEMA", "ALTER CREATE DROP"),
    ("SEQUENCE", "ALTER CREATE DROP"),
    ("TABLES", "FULL LOCK SHOW UNLOCK"),
    ("TEMP", "CREATE DROP"),
    ("TEMPORARY", "CREATE DROP"),
    ("TRANSACTION", "BEGIN SET START"),
    ("VIEW", "ALTER CREATE DROP MATERIALIZED TEMP TEMPORARY"),
    ("WORK", "BEGIN COMMIT ROLLBACK"),
];

/// The same in Postgres only: in MySQL, a table can stand there too, as in
/// `RENAME TABLE a TO savepoint` or `REPLACE view VALUES (1)`.
const POSTGRES_AFTER: &[(&str, &str)] = &[
    ("CONFLICT", "ON"),
    ("FUNCTION", "REPLACE"),
    ("SAVEPOINT", "TO"),
    ("VIEW", "REPLACE"),
];

/// Where a word stands, for [`is_keyword`].
struct Place<'a> {
    /// First in its statement, or after a `;` inside a MySQL routine.
    first: bool,
    /// The word right before it.
    follows: Option<&'a str>,
    /// The token right after it.
    next: Option<&'a str>,
    /// Right after a number or a `?` placeholder.
    after_number: bool,
    /// Inside a `CASE` that hasn't ended.
    in_case: bool,
}

/// Whether `word`, in capitals, is a keyword where it stands. In MySQL, a
/// word it doesn't reserve counts only where no name can stand: first in a
/// statement, right after certain keywords, `DUPLICATE` in `ON DUPLICATE
/// KEY`, `END` closing a `CASE`, and `OFFSET` after a number.
fn is_keyword(word: &str, engine: Engine, at: &Place) -> bool {
    let postgres = engine == Engine::Postgres;
    let listed = |list: &str| list.split_whitespace().any(|k| k == word);
    let one_of = |token: Option<&str>, list: &str| {
        token.is_some_and(|t| list.split_whitespace().any(|k| k.eq_ignore_ascii_case(t)))
    };
    let after = |pairs: &[(&str, &str)]| {
        pairs
            .iter()
            .any(|&(keyword, words)| keyword == word && one_of(at.follows, words))
    };
    listed(KEYWORDS)
        || (postgres && listed(POSTGRES_KEYWORDS))
        || (at.first && listed(STATEMENTS))
        || after(AFTER)
        || (postgres && after(POSTGRES_AFTER))
        || (word == "DUPLICATE" && one_of(at.follows, "ON") && one_of(at.next, "KEY"))
        || (word == "END" && at.in_case)
        || (word == "OFFSET" && at.after_number)
}

/// `sql` with its keywords in capitals: see [`is_keyword`]. A word next to a
/// `.` is part of a qualified name and never changes.
fn capitalize(sql: &str, engine: Engine) -> String {
    let tokens: Vec<Token> = tokens(sql, engine)
        .into_iter()
        .filter(|t| t.kind != Kind::Comment)
        .collect();
    let text = |t: &Token| &sql[t.range.clone()];
    let mut out = String::with_capacity(sql.len());
    let mut copied = 0;
    let mut cases = 0;
    for (k, token) in tokens.iter().enumerate() {
        if !matches!(token.kind, Kind::Word | Kind::Call) {
            continue;
        }
        let word = text(token).to_ascii_uppercase();
        let before = k.checked_sub(1).map(|p| &tokens[p]);
        let place = Place {
            first: before.is_none_or(|t| text(t) == ";"),
            follows: before
                .filter(|t| matches!(t.kind, Kind::Word | Kind::Call))
                .map(text),
            next: tokens.get(k + 1).map(text),
            after_number: before.is_some_and(|t| t.kind == Kind::Number || text(t) == "?"),
            in_case: cases > 0,
        };
        let qualified = before.is_some_and(|t| text(t) == ".") || place.next == Some(".");
        let after_end = place.follows.is_some_and(|w| w.eq_ignore_ascii_case("END"));
        match word.as_str() {
            // The `CASE` of a MySQL routine's `END CASE` closes, not opens.
            "CASE" if !after_end => cases += 1,
            "END" if cases > 0 => cases -= 1,
            _ => {}
        }
        if !qualified && word != text(token) && is_keyword(&word, engine, &place) {
            out.push_str(&sql[copied..token.range.start]);
            out.push_str(&word);
            copied = token.range.end;
        }
    }
    out.push_str(&sql[copied..]);
    out
}

/// `sql` with every word in capitals, keywords or not.
fn upper_words(sql: &str, engine: Engine) -> String {
    let mut out = sql.to_owned();
    for t in tokens(sql, engine) {
        if matches!(t.kind, Kind::Word | Kind::Call) {
            // Same length: only ASCII letters change.
            out.replace_range(t.range.clone(), &sql[t.range].to_ascii_uppercase());
        }
    }
    out
}

/// Whether `after` is `before` with only the whitespace between tokens
/// changed: the same tokens in the same order, comments word for word, and
/// strings in a row still on one line or not, since Postgres joins `'a'`
/// and `'b'` into one string only across a line break.
fn same_tokens(before: &str, after: &str, engine: Engine) -> bool {
    let (a, b) = (tokens(before, engine), tokens(after, engine));
    a.len() == b.len()
        && a.iter().zip(&b).enumerate().all(|(i, (x, y))| {
            let (s, t) = (&before[x.range.clone()], &after[y.range.clone()]);
            let same = if x.kind == Kind::Comment && !(engine == Engine::Mysql && read_by_mysql(s))
            {
                s.split_whitespace().eq(t.split_whitespace())
            } else {
                s == t
            };
            let joined = i > 0 && x.kind == Kind::Quoted && a[i - 1].kind == Kind::Quoted;
            x.kind == y.kind && same && (!joined || x.line_break == y.line_break)
        })
}

/// A MySQL comment the server reads, so every byte of it counts:
/// `/*! … */` and MariaDB's `/*M! … */` run as code, `/*+ … */` holds hints.
fn read_by_mysql(comment: &str) -> bool {
    ["/*!", "/*M!", "/*+"]
        .iter()
        .any(|p| comment.starts_with(p))
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Kind {
    /// A name or keyword.
    Word,
    /// A word right before `(`. MySQL reads `COUNT (*)`, with the space,
    /// as something else than a call.
    Call,
    Number,
    /// Quoted text, with its prefix (`E'…'`, `N'…'`, `U&"…"`), or a MySQL
    /// `@variable`: whitespace inside would change what it is.
    Quoted,
    Comment,
    /// A run of operator characters, or one other character.
    Symbol,
}

struct Token {
    kind: Kind,
    range: Range<usize>,
    /// The whitespace before it has a line break, `\n` or `\r` as Postgres
    /// counts them.
    line_break: bool,
}

/// Characters of Postgres operators, plus `:` for `::` and `:=`.
const OPERATOR: &[u8] = b"+-*/<>=~!@#%^&|`?:";

/// The tokens of `sql`, read the way `engine` reads them where formatting
/// could change meaning: what's quoted or a comment (by the splitter's own
/// rules), and what is one token. Errs toward longer tokens, so a space put
/// inside one shows.
fn tokens(sql: &str, engine: Engine) -> Vec<Token> {
    let bytes = sql.as_bytes();
    let mysql = engine == Engine::Mysql;
    let mut out = Vec::new();
    let (mut i, mut line_break) = (0, false);
    while i < bytes.len() {
        let b = bytes[i];
        if b.is_ascii_whitespace() {
            line_break |= matches!(b, b'\n' | b'\r');
            i += 1;
            continue;
        }
        let start = i;
        let kind = if let Some(end) = comment_end(sql, engine, i) {
            i = end;
            Kind::Comment
        } else if let Some(end) = quoted_end(sql, engine, i) {
            i = end;
            Kind::Quoted
        } else if mysql && b == b'@' {
            // `@name`, `@@session.name` or `@'name'`.
            i += if bytes.get(i + 1) == Some(&b'@') {
                2
            } else {
                1
            };
            match bytes.get(i).and_then(|_| quoted_end(sql, engine, i)) {
                Some(end) => i = end,
                None => i = scan(bytes, i, |c| is_ident(c) || c == b'$' || c == b'.'),
            }
            Kind::Quoted
        } else if b == b'$' && bytes.get(i + 1).is_some_and(u8::is_ascii_digit) {
            // A Postgres parameter, `$1`.
            i = scan(bytes, i + 1, |c| c.is_ascii_digit());
            Kind::Word
        } else if is_ident(b) && !b.is_ascii_digit() {
            // Names take `$`, as the splitter reads them.
            i = scan(bytes, i + 1, |c| is_ident(c) || c == b'$');
            let word = &sql[start..i];
            let prefix = matches!(word, "B" | "b" | "E" | "e" | "N" | "n" | "X" | "x")
                || (mysql && word.starts_with('_'));
            let unicode = !mysql && word.eq_ignore_ascii_case("u") && bytes.get(i) == Some(&b'&');
            match bytes.get(i) {
                Some(b'\'') if prefix => {
                    i = quoted_end(sql, engine, i).unwrap_or(bytes.len());
                    Kind::Quoted
                }
                Some(b'&') if unicode && matches!(bytes.get(i + 1), Some(b'\'' | b'"')) => {
                    i = quoted_end(sql, engine, i + 1).unwrap_or(bytes.len());
                    Kind::Quoted
                }
                Some(b'(') => Kind::Call,
                _ => Kind::Word,
            }
        } else if b.is_ascii_digit()
            || (b == b'.' && bytes.get(i + 1).is_some_and(u8::is_ascii_digit))
        {
            // Digits and what sticks to them: `1.5e-3`, `0x1F`. A letter
            // starts what the splitter reads as a name, which takes `$`.
            let mut name = false;
            i += 1;
            while i < bytes.len() {
                let c = bytes[i];
                let sign = matches!(c, b'+' | b'-')
                    && matches!(bytes[i - 1], b'e' | b'E')
                    && comment_end(sql, engine, i).is_none();
                if is_ident(c) && !c.is_ascii_digit() {
                    name = true;
                } else if sign {
                    name = false;
                } else if !(c.is_ascii_digit() || (c == b'.' && !name) || (c == b'$' && name)) {
                    break;
                }
                i += 1;
            }
            Kind::Number
        } else if mysql && b == b'\\' && matches!(bytes.get(i + 1), Some(b'N' | b'n')) {
            // MySQL's old spelling of NULL, still MariaDB's. Read the same in
            // either case, so capitals can't make one: `\null` isn't `\NULL`.
            i += 2;
            Kind::Quoted
        } else if OPERATOR.contains(&b) {
            // Up to a comment or quote, which the splitter reads at any
            // byte, or a MySQL `@variable`.
            i += 1;
            while i < bytes.len()
                && OPERATOR.contains(&bytes[i])
                && !(mysql && bytes[i] == b'@')
                && comment_end(sql, engine, i).is_none()
                && quoted_end(sql, engine, i).is_none()
            {
                i += 1;
            }
            Kind::Symbol
        } else {
            // Bytes from 0x80 up are word bytes, so this one is ASCII.
            i += 1;
            Kind::Symbol
        };
        out.push(Token {
            kind,
            range: start..i,
            line_break,
        });
        line_break = false;
    }
    out
}

/// The end of the run of bytes from `i` that `keep` accepts.
fn scan(bytes: &[u8], mut i: usize, keep: impl Fn(u8) -> bool) -> usize {
    while i < bytes.len() && keep(bytes[i]) {
        i += 1;
    }
    i
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pg(sql: &str) -> String {
        format(sql, Engine::Postgres)
    }

    fn my(sql: &str) -> String {
        format(sql, Engine::Mysql)
    }

    #[test]
    fn lays_out_clauses_with_keywords_in_capitals() {
        let sql = "select id, name from users where id=1 and active order by name desc limit 5";
        let formatted = "SELECT\n  id,\n  name\nFROM\n  users\nWHERE\n  id = 1\n  AND active\n\
                         ORDER BY\n  name DESC\nLIMIT\n  5";
        assert_eq!(pg(sql), formatted);
        assert_eq!(my(sql), formatted);
    }

    #[test]
    fn leaves_names_that_only_look_like_keywords() {
        // sqlformat's own capitals give `STATUS`, `TYPE`, `LOGS`, `EVENTS`;
        // in MySQL, `LOGS` may not be the `logs` table.
        let sql = "select status, type from logs join events e on e.log_id = logs.id";
        let formatted =
            "SELECT\n  status,\n  type\nFROM\n  logs\n  JOIN events e ON e.log_id = logs.id";
        assert_eq!(pg(sql), formatted);
        assert_eq!(my(sql), formatted);
    }

    #[test]
    fn mysql_capitalizes_words_it_does_not_reserve_only_where_no_name_can_stand() {
        let sql =
            "begin; select case when paid then 1 else 0 end from end limit 10 offset 5; commit";
        assert_eq!(
            capitalize(
                "select case when paid then 1 else 0 end from end limit 10 offset 5",
                Engine::Mysql
            ),
            "SELECT CASE WHEN paid THEN 1 ELSE 0 END FROM end LIMIT 10 OFFSET 5"
        );
        assert!(my(sql).starts_with("BEGIN;\n\nSELECT"), "{}", my(sql));
        assert!(my(sql).ends_with(";\n\nCOMMIT"), "{}", my(sql));
        // `view` and `key` as names; keywords after `CREATE` and `PRIMARY`.
        assert_eq!(
            capitalize("create view v as select key, view from t", Engine::Mysql),
            "CREATE VIEW v AS SELECT key, view FROM t"
        );
        assert_eq!(
            capitalize("create table t (id int primary key)", Engine::Mysql),
            "CREATE TABLE t (id int PRIMARY KEY)"
        );
        for (sql, capitalized) in [
            // MariaDB takes these as table names.
            (
                "select * from function, window, database, schema, row, of, lateral",
                "SELECT * FROM function, window, database, schema, row, of, lateral",
            ),
            (
                "create database d character set utf8mb4",
                "CREATE DATABASE d character SET utf8mb4",
            ),
            ("drop function if exists f", "DROP FUNCTION IF EXISTS f"),
            (
                "insert into t values (1) on duplicate key update a = 1",
                "INSERT INTO t VALUES (1) ON DUPLICATE KEY UPDATE a = 1",
            ),
            // Tables where Postgres has keywords.
            (
                "create index i on duplicate (a)",
                "CREATE INDEX i ON duplicate (a)",
            ),
            ("rename table a to savepoint", "RENAME TABLE a TO savepoint"),
            ("replace view values (1)", "REPLACE view VALUES (1)"),
            // `END CASE` opens no `CASE`, so the table `end` stays a name.
            (
                "begin case x when 1 then select 1; end case; select * from end; end",
                "BEGIN CASE x WHEN 1 THEN SELECT 1; END CASE; SELECT * FROM end; END",
            ),
        ] {
            assert_eq!(capitalize(sql, Engine::Mysql), capitalized, "{sql}");
        }
    }

    #[test]
    fn postgres_capitalizes_its_own_keywords() {
        assert_eq!(
            capitalize(
                "select * from only t where name ilike 'a%' order by 1 nulls last offset 2",
                Engine::Postgres
            ),
            "SELECT * FROM ONLY t WHERE name ILIKE 'a%' ORDER BY 1 NULLS LAST OFFSET 2"
        );
        assert_eq!(
            capitalize(
                "insert into t values (1) on conflict do nothing returning id",
                Engine::Postgres
            ),
            "INSERT INTO t VALUES (1) ON CONFLICT DO NOTHING RETURNING id"
        );
        assert_eq!(
            capitalize(
                "create or replace function f() returns int as $$ select 1 $$ language sql",
                Engine::Postgres
            ),
            "CREATE OR REPLACE FUNCTION f() returns int AS $$ select 1 $$ language sql"
        );
        assert_eq!(
            capitalize(
                "select * from t, lateral (select 1) x window w as (order by a); rollback to savepoint s",
                Engine::Postgres
            ),
            "SELECT * FROM t, LATERAL (SELECT 1) x WINDOW w AS (ORDER BY a); ROLLBACK TO SAVEPOINT s"
        );
        // Not keywords in MySQL, where they could be names.
        assert_eq!(
            capitalize("select * from only where a ilike b", Engine::Mysql),
            "SELECT * FROM only WHERE a ilike b"
        );
    }

    #[test]
    fn qualified_names_keep_their_case() {
        assert_eq!(
            capitalize("select t.select, s.order from s.table t", Engine::Mysql),
            "SELECT t.select, s.order FROM s.table t"
        );
    }

    #[test]
    fn keeps_quoted_text_and_comments() {
        assert_eq!(
            capitalize(
                "select 'from', \"where\", `group` -- order by\nfrom t /* limit */",
                Engine::Mysql
            ),
            "SELECT 'from', \"where\", `group` -- order by\nFROM t /* limit */"
        );
        assert_eq!(
            capitalize("select $$select 1$$, e'from' # where", Engine::Postgres),
            "SELECT $$select 1$$, e'from' # WHERE"
        );
    }

    #[test]
    fn statements_sqlformat_would_change_keep_their_layout() {
        for sql in [
            "select $$it's  a   body$$, 1",
            "select U&'d\\0061t\\+000061', 1",
            "select B'1010', 1",
            // Postgres joins strings across a line break, not on one line.
            "select 'a'\n'b', 1",
        ] {
            assert_eq!(pg(sql), sql.replacen("select", "SELECT", 1), "{sql}");
        }
        // What it reads right is laid out.
        assert_eq!(
            pg("select E'it\\'s  here', 1"),
            "SELECT\n  E'it\\'s  here',\n  1"
        );
        // `\r` breaks a line too, for joining strings.
        assert_eq!(pg("select 'a'\r'b', 1"), "SELECT 'a'\r'b', 1");
        // MySQL runs what is inside `/*! … */`, so it keeps every byte.
        let executable = "select /*!50000 'a\n    b' */ 1 from t";
        assert_eq!(
            my(executable),
            executable
                .replace("select", "SELECT")
                .replace("from", "FROM")
        );
        // MariaDB's `\N` is NULL; `\ N` would be nothing.
        let null = my("select \\N, 1");
        assert!(null.contains("\\N") && !null.contains("\\ N"), "{null}");
    }

    #[test]
    fn long_statements_are_only_capitalized() {
        let sql = format!("select 1{}", " or 1".repeat(MAX_LAYOUT / 5));
        let formatted = pg(&sql);
        assert!(formatted.starts_with("SELECT 1 OR 1 OR 1"));
        assert!(!formatted.contains('\n'));
    }

    #[test]
    fn a_range_formats_the_statements_it_touches_whole() {
        // From inside a comment: it stays a comment.
        let sql = "-- TODO: select from users later\nselect id, name from accounts where id = 1;";
        let start = sql.find("select from").unwrap();
        let (out, span) = format_range(sql, start..sql.len(), Engine::Postgres);
        let statement = "SELECT\n  id,\n  name\nFROM\n  accounts\nWHERE\n  id = 1";
        assert_eq!(
            out,
            format!("-- TODO: select from users later\n{statement};")
        );
        assert_eq!(&out[span], statement);
        // From inside a string: it stays as typed.
        let string = "'Dear customer,\nwe will select the best option from our catalog.'";
        let sql = format!("insert into notes (body) values ({string})");
        let start = sql.find("we will").unwrap();
        let (out, _) = format_range(&sql, start..sql.len(), Engine::Postgres);
        assert!(
            out.starts_with("INSERT INTO") && out.contains(string),
            "{out}"
        );
        // Only the statements touched.
        let (out, span) = format_range("select 1; select 2; select 3", 10..18, Engine::Postgres);
        assert_eq!(out, "select 1; SELECT\n  2; select 3");
        assert_eq!(&out[span], "SELECT\n  2");
        // None touched.
        let sql = "select 1; -- note\nselect 2";
        let note = sql.find("note").unwrap()..sql.find("note").unwrap() + 4;
        assert_eq!(
            format_range(sql, note.clone(), Engine::Postgres),
            (sql.to_owned(), note)
        );
    }

    #[test]
    fn statements_are_a_blank_line_apart_and_what_stands_between_is_kept() {
        assert_eq!(
            pg("-- top\nselect 1; select 2;\n\n\nselect 3; -- three\nselect 4;\n-- tail"),
            "-- top\nSELECT\n  1;\n\nSELECT\n  2;\n\nSELECT\n  3; -- three\nSELECT\n  4;\n-- tail"
        );
        let script =
            "DELIMITER //\ncreate procedure p() begin select 1; end //\nDELIMITER ;\ncall p();";
        let formatted = my(script);
        assert!(formatted.starts_with("DELIMITER //\n"), "{formatted}");
        assert!(
            formatted.contains(" //\nDELIMITER ;\nCALL p();"),
            "{formatted}"
        );
        assert_eq!(split::split(&formatted, Engine::Mysql).len(), 2);
        // `END$$CREATE` stays one word to anything reading it without the
        // delimiter, so nothing comes between.
        let glued =
            "DELIMITER $$\ncreate view a as select 1$$create view b as select 2$$\nDELIMITER ;";
        let formatted = my(glued);
        assert!(formatted.contains("1$$CREATE VIEW b"), "{formatted}");
        assert_eq!(split::split(&formatted, Engine::Mysql).len(), 2);
        // After a `;` on its line, `DELIMITER` is no directive; a blank line
        // would make it one, so the text stays as it was.
        let odd = "select 1;DELIMITER //\nselect 2//";
        assert_eq!(my(odd), odd);
    }

    #[test]
    fn formatting_again_changes_nothing() {
        for sql in [
            "select u.id, count(*) from users u left join orders o on o.user_id = u.id group by u.id",
            "insert into t (a, b) values (1, 'x'), (2, 'y')",
            "update t set a = 1 where b in (select b from u)",
            "with r as (select 1) select * from r",
        ] {
            for engine in Engine::ALL {
                let once = format(sql, engine);
                assert_eq!(format(&once, engine), once, "{sql}");
            }
        }
    }

    #[test]
    fn never_changes_more_than_whitespace_and_case() {
        let pieces = [
            "select ",
            "from ",
            "where ",
            "and ",
            "case ",
            "when ",
            "then ",
            "end ",
            "group by ",
            "order by ",
            "limit ",
            "offset ",
            "join ",
            "on ",
            "insert into ",
            "values ",
            "update ",
            "set ",
            "begin",
            "commit",
            "$$",
            "$1",
            "$a$",
            "'",
            "\"",
            "`",
            "(",
            ")",
            "[",
            "]",
            ",",
            ";",
            ".",
            "::",
            "->>",
            "#>>",
            "#",
            "-- c\n",
            "--",
            "/*",
            "*/",
            "\n",
            " ",
            "=",
            "<>",
            "-",
            "*",
            "?",
            "@v",
            "@@",
            "E'",
            "U&'",
            "B'",
            "N'",
            "x'1f'",
            "1",
            "1.5e-3",
            "a",
            "t.c",
            "città",
            "\\",
            "'it''s'",
            "status",
            "logs",
            "key",
            "view",
            "first",
            "nulls",
            "/*!",
            "\r",
            "\\N",
            "end case ",
            "function",
            "database",
        ];
        // xorshift: deterministic, so a failure reproduces.
        let mut seed = 0x9E37_79B9_7F4A_7C15_u64;
        let mut next = move || {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            seed as usize
        };
        for _ in 0..3000 {
            let sql: String = (0..next() % 30)
                .map(|_| pieces[next() % pieces.len()])
                .collect();
            for engine in Engine::ALL {
                // Past the last check in `format`, which would hide a slip.
                let (formatted, _) = statements(&sql, &(0..sql.len()), engine);
                assert!(
                    same_tokens(
                        &upper_words(&sql, engine),
                        &upper_words(&formatted, engine),
                        engine
                    ),
                    "{engine:?}: {sql:?} became {formatted:?}"
                );
                assert_eq!(
                    split::split(&sql, engine).len(),
                    split::split(&formatted, engine).len(),
                    "{engine:?}: {sql:?} became {formatted:?}"
                );
            }
        }
    }

    #[test]
    fn maps_an_offset_to_the_same_text() {
        let (before, after) = ("select 1 from t", "SELECT\n  1\nFROM\n  t");
        assert_eq!(map_offset(before, after, 0), 0);
        assert_eq!(map_offset(before, after, 8), after.find("1").unwrap() + 1);
        assert_eq!(map_offset(before, after, 9), after.find("FROM").unwrap());
        assert_eq!(map_offset(before, after, before.len()), after.len());
        assert_eq!(map_offset("a  ", "a", 3), 1);
    }
}
