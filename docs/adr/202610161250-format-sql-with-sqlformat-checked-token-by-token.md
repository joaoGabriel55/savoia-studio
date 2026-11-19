# Format SQL with sqlformat, checked token by token

## Status

Proposed

## Context

[Issue #16](https://github.com/joaoGabriel55/savoia-studio/issues/16) asked for the console to format SQL, with keywords in capitals, on Ctrl+Shift+F: the shortcut DBeaver and Eclipse use, and the one people moving from DBeaver will try first. Savoia had no formatter. gpui-kit's editor highlights SQL with tree-sitter but has no formatting.

A formatter that changes what a statement does is worse than none: the console runs what is in the editor. Three ways a formatter can do that turned up while trying the options (October 2026):

- **Case.** Postgres folds unquoted names to lower case, so capitalizing any of them is safe there. MySQL and MariaDB, by default on Linux, keep table names and aliases case-sensitive: `SELECT * FROM LOGS` fails when the table is `logs`.
- **Layout.** Whitespace inside quoted text is data, a MySQL `--` is a comment only before whitespace, MySQL runs the code inside `/*! … */` comments, and Postgres joins `'a'` and `'b'` into one string only across a line break.
- **Context.** A selection read on its own can start inside a comment or a string, and then its rest reads as code.

The options:

- **[sqlformat](https://crates.io/crates/sqlformat) 0.5.0** (MIT or Apache-2.0, a Rust port of sql-formatter-plus). It lays statements out one clause per line, with each clause's items indented below it. But it reads every dialect with one generic lexer. Its own keyword option capitalizes names that are keywords to it (`status`, `type`, `logs`, `events`), so `select status from logs` becomes `SELECT STATUS FROM LOGS`. It reformats inside `$$` function bodies, splits `B'1010'` and `U&'…'` into two tokens, and puts `'a'` and `'b'` from two lines on one.
- **A formatter of our own.** Full control over dialects, but a layout engine (clauses, nesting, `CASE`, joins, lists, comments) is a project of its own to write and maintain.
- **Parse and print** (sqlparser-rs). It drops comments and the user's own spelling, and refuses SQL it can't parse, which a console is full of while it's being written.

## Decision

Format with **sqlformat**, but only for layout, and never trust its output blindly. The code is `savoia_core::format`:

- **Keyword case is Savoia's**, not sqlformat's (its `uppercase` option stays off). A word changes case only where it can't be a name: a list of keywords that both MySQL 8.4 and MariaDB 11.4 reserve (checked on 2026-10-16 against MySQL 8.4.11's `INFORMATION_SCHEMA.KEYWORDS`, and by creating a table of each name on MariaDB 11.4.13, which took `function`, `window`, `database` and a few more), Postgres-only keywords in Postgres, a statement's first word, words right after keywords that no name can follow (`PRIMARY KEY`, `CREATE VIEW`), `ON DUPLICATE KEY`, `END` closing a `CASE` and `OFFSET` after a number. Never a word next to a `.`, or inside quotes or comments. Function names and types keep the case they were typed in.
- **Statements are formatted whole**, as the splitter finds them in the whole editor. With a selection, Format takes the statements it touches, so `DELIMITER` lines and the comments between statements stay as typed, and a selection that starts inside a comment or a string can't turn its rest into code.
- **Each laid-out statement is checked**: it is read again with the engine's own rules for quotes and comments (the splitter's), and kept only if it has the same tokens in the same order, with only whitespace between them changed. MySQL's `/*! … */`, `/*M! … */` and `/*+ … */` comments must match byte for byte; other comments word for word. Otherwise that statement keeps its layout, with its keywords capitalized. So does a statement longer than 256 KB, where sqlformat gets slow.
- **The whole result is checked last**: same tokens apart from case, and the same statements to the splitter. If not, the text stays as it was.

The check is what lets Savoia use a formatter that doesn't know the dialects. A property test feeds thousands of random token strings, and random ranges of them, through both engines and asserts that nothing but whitespace and case changes, and that the text splits into the same statements.

## Consequences

- Formatting can't change what a statement does, as far as the splitter's reading of each engine is right. A statement sqlformat can't lay out safely, such as a Postgres function with a `$$` body, keeps its layout.
- In MySQL, some keywords stay as typed wherever a table could have their name: `LATERAL`, `WINDOW` and `OF`, or `FUNCTION` and `ROW` outside `CREATE FUNCTION`, `FOR EACH ROW` and the like. MySQL 5.7, out of support since October 2023, doesn't reserve five of the listed words (`EXCEPT`, `INTERSECT`, `OVER`, `RECURSIVE`, `ROWS`; checked on 5.7.44), so a 5.7 table with one of those names would change case.
- A selection is widened to whole statements, so Format can change text just outside it; the statements it formatted end up selected.
- Two crates join the build: `sqlformat` and `unicode_categories`. `winnow`, its parser, was already in the tree. Measured in isolation with the release profile, this adds about 0.25 MB to the binary, about 75 KB once compressed, so the packages stay under the 15 MB budget.
- The layout is sqlformat's (two-space indent, one clause per line) and has no settings yet. Style options, and capitals for function names, would be a follow-up.
- Upgrading sqlformat can change layouts, but not meaning: the checks stay in force. Upgrade it deliberately, with the formatter's tests.
- The splitter's reading of each engine now matters to formatting as well as to running statements. Sharing that code with the formatter turned up two splitter bugs, fixed along with it: it panicked on a block comment with non-ASCII text, and it opened a dollar quote at a `$` inside a Postgres name (`a$$b`).
