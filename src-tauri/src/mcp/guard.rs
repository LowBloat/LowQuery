//! Statement checks for agents: one statement per call, read-only
//! classification and the Redis command lists.
//!
//! The lexer only needs to know where strings, quoted names and comments
//! start and end in each dialect, so a `;` or a keyword inside them is not
//! mistaken for SQL. Whenever the dialect leaves room for doubt (MySQL with
//! or without backslash escapes, an unterminated string) the text is refused
//! instead of guessed.

use crate::drivers::oracle::is_plsql;
use crate::drivers::redis_db::split_args;
use crate::model::Engine;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Dialect {
    Postgres,
    Oracle,
    Mysql,
    SqlServer,
    Sqlite,
    Cassandra,
}

impl Dialect {
    fn of(engine: Engine) -> Self {
        match engine {
            Engine::Postgres => Self::Postgres,
            Engine::Oracle => Self::Oracle,
            Engine::Mysql | Engine::Mariadb => Self::Mysql,
            Engine::Sqlserver => Self::SqlServer,
            // Redis never reaches the SQL lexer.
            Engine::Sqlite | Engine::Redis => Self::Sqlite,
            Engine::Cassandra => Self::Cassandra,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Tok {
    /// Keyword or unquoted name, uppercased.
    Word(String),
    /// String literal or quoted name.
    Quoted,
    Number,
    Punct(u8),
    Semi,
    /// A line holding only `/` (SQL*Plus), Oracle only.
    Slash,
}

#[derive(Debug, Clone)]
struct Token {
    tok: Tok,
    end: usize,
}

/// One statement, ready to send.
#[derive(Debug, Clone)]
pub struct Checked {
    /// The statement without the trailing `;` or `/` line (PL/SQL and
    /// routine bodies keep their final `;`).
    pub sql: String,
    /// Tokens of the statement, once per way the dialect may read it.
    pub views: Vec<Vec<Tok>>,
}

impl Checked {
    pub fn first_word(&self) -> Option<&str> {
        self.views.first().and_then(|t| first_word(t))
    }
}

fn first_word(toks: &[Tok]) -> Option<&str> {
    toks.iter()
        .find(|t| **t != Tok::Punct(b'('))
        .and_then(|t| match t {
            Tok::Word(w) => Some(w.as_str()),
            _ => None,
        })
}

fn words(toks: &[Tok]) -> impl Iterator<Item = &str> {
    toks.iter().filter_map(|t| match t {
        Tok::Word(w) => Some(w.as_str()),
        _ => None,
    })
}

// ---------------------------------------------------------------------------
// Lexer

fn is_word_start(c: u8) -> bool {
    c.is_ascii_alphabetic() || c == b'_' || c >= 0x80
}

fn is_word_char(c: u8, d: Dialect) -> bool {
    c.is_ascii_alphanumeric()
        || c == b'_'
        || c >= 0x80
        || (c == b'$' && !matches!(d, Dialect::Sqlite | Dialect::Cassandra))
        || (c == b'#' && matches!(d, Dialect::Oracle | Dialect::SqlServer))
}

const UNTERMINATED: &str = "The text has an unterminated string, quoted name or comment.";

/// Skips a quoted run opened by `b[i] == q`. The quote doubled stands for
/// itself; with `backslash`, `\` escapes the next character.
fn skip_quoted(b: &[u8], i: usize, q: u8, backslash: bool) -> Result<usize, String> {
    let mut j = i + 1;
    while j < b.len() {
        if backslash && b[j] == b'\\' {
            j += 2;
        } else if b[j] == q {
            if b.get(j + 1) == Some(&q) {
                j += 2;
            } else {
                return Ok(j + 1);
            }
        } else {
            j += 1;
        }
    }
    Err(UNTERMINATED.into())
}

/// Oracle `q'[...]'`: `i` is at the opening `'`.
fn skip_q_quote(b: &[u8], i: usize) -> Result<usize, String> {
    let open = *b.get(i + 1).ok_or(UNTERMINATED)?;
    let close = match open {
        b'[' => b']',
        b'{' => b'}',
        b'<' => b'>',
        b'(' => b')',
        c if c.is_ascii_whitespace() => return Err("Invalid q'' quote delimiter.".into()),
        c => c,
    };
    let mut j = i + 2;
    while j + 1 < b.len() {
        if b[j] == close && b[j + 1] == b'\'' {
            return Ok(j + 2);
        }
        j += 1;
    }
    Err(UNTERMINATED.into())
}

/// PostgreSQL `$tag$...$tag$`: `None` when the `$` does not open one.
fn skip_dollar_quote(b: &[u8], i: usize) -> Result<Option<usize>, String> {
    let mut j = i + 1;
    if b.get(j) != Some(&b'$') {
        if !b.get(j).is_some_and(|&c| is_word_start(c)) {
            return Ok(None);
        }
        while j < b.len() && (b[j].is_ascii_alphanumeric() || b[j] == b'_' || b[j] >= 0x80) {
            j += 1;
        }
        if b.get(j) != Some(&b'$') {
            return Ok(None);
        }
    }
    let tag = &b[i..=j];
    let body = j + 1;
    b[body..]
        .windows(tag.len())
        .position(|w| w == tag)
        .map(|p| Some(body + p + tag.len()))
        .ok_or_else(|| UNTERMINATED.to_string())
}

/// CQL `$$...$$`: `i` is at the first `$`, the next one is known to be `$`.
fn skip_double_dollar(b: &[u8], i: usize) -> Result<usize, String> {
    b[i + 2..]
        .windows(2)
        .position(|w| w == b"$$")
        .map(|p| i + 2 + p + 2)
        .ok_or_else(|| UNTERMINATED.to_string())
}

fn skip_block_comment(b: &[u8], i: usize, nested: bool) -> Result<usize, String> {
    let mut depth = 1;
    let mut j = i + 2;
    while j + 1 < b.len() {
        if b[j] == b'*' && b[j + 1] == b'/' {
            depth -= 1;
            j += 2;
            if depth == 0 {
                return Ok(j);
            }
        } else if nested && b[j] == b'/' && b[j + 1] == b'*' {
            depth += 1;
            j += 2;
        } else {
            j += 1;
        }
    }
    Err(UNTERMINATED.into())
}

/// Length of the non-ASCII whitespace character at `i`, if there is one.
fn unicode_space(sql: &str, i: usize) -> Option<usize> {
    if sql.as_bytes()[i] < 0x80 {
        return None;
    }
    sql.get(i..)
        .and_then(|s| s.chars().next())
        .filter(|c| c.is_whitespace())
        .map(char::len_utf8)
}

fn line_end(b: &[u8], i: usize) -> usize {
    b[i..]
        .iter()
        .position(|&c| c == b'\n')
        .map_or(b.len(), |p| i + p)
}

/// A `/` alone on its line (spaces aside).
fn slash_alone(b: &[u8], i: usize) -> bool {
    let start = b[..i]
        .iter()
        .rposition(|&c| c == b'\n')
        .map_or(0, |p| p + 1);
    let end = line_end(b, i);
    b[start..i].iter().all(u8::is_ascii_whitespace)
        && b[i + 1..end].iter().all(u8::is_ascii_whitespace)
}

fn lex(sql: &str, d: Dialect, backslash: bool) -> Result<Vec<Token>, String> {
    let b = sql.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    let push = |out: &mut Vec<Token>, tok, end| out.push(Token { tok, end });
    while i < b.len() {
        let c = b[i];
        let next = b.get(i + 1).copied();
        if c.is_ascii_whitespace() {
            i += 1;
        } else if let Some(n) = unicode_space(sql, i) {
            // T-SQL reads U+00A0 and friends as spaces.
            i += n;
        } else if c == b'-' && next == Some(b'-') {
            // MySQL needs a space (or control character) after `--`:
            // `1--1` is arithmetic there.
            let comment = d != Dialect::Mysql
                || b.get(i + 2)
                    .is_none_or(|n| n.is_ascii_whitespace() || n.is_ascii_control());
            if comment {
                i = line_end(b, i);
            } else {
                push(&mut out, Tok::Punct(c), i + 1);
                i += 1;
            }
        } else if (c == b'#' && d == Dialect::Mysql)
            || (c == b'/' && next == Some(b'/') && d == Dialect::Cassandra)
        {
            i = line_end(b, i);
        } else if c == b'/' && next == Some(b'*') {
            if d == Dialect::Mysql
                && (b.get(i + 2) == Some(&b'!')
                    || (b.get(i + 2) == Some(&b'M') && b.get(i + 3) == Some(&b'!')))
            {
                return Err(
                    "MySQL executable comments (/*! ... */) are not accepted; write the SQL plainly."
                        .into(),
                );
            }
            let nested = matches!(d, Dialect::Postgres | Dialect::SqlServer);
            i = skip_block_comment(b, i, nested)?;
        } else if c == b'/' && d == Dialect::Oracle && slash_alone(b, i) {
            push(&mut out, Tok::Slash, i + 1);
            i += 1;
        } else if c == b'\'' {
            i = skip_quoted(b, i, b'\'', backslash)?;
            push(&mut out, Tok::Quoted, i);
        } else if c == b'"' {
            i = skip_quoted(b, i, b'"', backslash && d == Dialect::Mysql)?;
            push(&mut out, Tok::Quoted, i);
        } else if c == b'`' && matches!(d, Dialect::Mysql | Dialect::Sqlite) {
            i = skip_quoted(b, i, b'`', false)?;
            push(&mut out, Tok::Quoted, i);
        } else if c == b'[' && matches!(d, Dialect::SqlServer | Dialect::Sqlite) {
            // `]]` stands for `]` inside the name.
            let mut j = i + 1;
            loop {
                match b.get(j) {
                    None => return Err(UNTERMINATED.into()),
                    Some(b']') if b.get(j + 1) == Some(&b']') => j += 2,
                    Some(b']') => break,
                    Some(_) => j += 1,
                }
            }
            i = j + 1;
            push(&mut out, Tok::Quoted, i);
        } else if c == b'$' && d == Dialect::Cassandra {
            if next == Some(b'$') {
                i = skip_double_dollar(b, i)?;
                push(&mut out, Tok::Quoted, i);
            } else {
                i += 1;
                push(&mut out, Tok::Punct(c), i);
            }
        } else if c == b'$' && d == Dialect::Postgres {
            match skip_dollar_quote(b, i)? {
                Some(end) => {
                    i = end;
                    push(&mut out, Tok::Quoted, i);
                }
                None => {
                    i += 1;
                    push(&mut out, Tok::Punct(c), i);
                }
            }
        } else if c == b';' {
            i += 1;
            push(&mut out, Tok::Semi, i);
        } else if c.is_ascii_digit() {
            // Letters right after a number start a new word, as PostgreSQL
            // before 15 reads `1into`.
            while i < b.len() && (b[i].is_ascii_digit() || b[i] == b'.') {
                i += 1;
            }
            push(&mut out, Tok::Number, i);
        } else if is_word_start(c) {
            let start = i;
            while i < b.len() {
                if b[i] >= 0x80 {
                    if unicode_space(sql, i).is_some() {
                        break;
                    }
                    i += sql[i..].chars().next().map_or(1, char::len_utf8);
                } else if is_word_char(b[i], d) {
                    i += 1;
                } else {
                    break;
                }
            }
            let word = &sql[start..i];
            if b.get(i) == Some(&b'\'') {
                let upper = word.to_ascii_uppercase();
                if d == Dialect::Postgres && upper == "E" {
                    i = skip_quoted(b, i, b'\'', true)?;
                    push(&mut out, Tok::Quoted, i);
                    continue;
                }
                if d == Dialect::Oracle && (upper == "Q" || upper == "NQ") {
                    i = skip_q_quote(b, i)?;
                    push(&mut out, Tok::Quoted, i);
                    continue;
                }
            }
            push(&mut out, Tok::Word(word.to_ascii_uppercase()), i);
        } else {
            i += 1;
            push(&mut out, Tok::Punct(c), i);
        }
    }
    Ok(out)
}

/// CREATE PROCEDURE, FUNCTION, TRIGGER... whose body may hold `;`.
fn is_routine(toks: &[Token]) -> bool {
    let mut head = toks
        .iter()
        .take_while(|t| t.tok != Tok::Punct(b'('))
        .take(16)
        .filter_map(|t| match &t.tok {
            Tok::Word(w) => Some(w.as_str()),
            _ => None,
        });
    matches!(head.next(), Some("CREATE" | "ALTER"))
        && head.any(|w| {
            matches!(
                w,
                "PROCEDURE" | "PROC" | "FUNCTION" | "TRIGGER" | "EVENT" | "PACKAGE"
            )
        })
}

/// A CQL `BEGIN [UNLOGGED|COUNTER] BATCH` whose `APPLY BATCH` has not come
/// yet: the `;` between its statements do not end it.
fn is_cql_batch_open(group: &[Token]) -> bool {
    let mut w = group.iter().filter_map(|t| match &t.tok {
        Tok::Word(w) => Some(w.as_str()),
        _ => None,
    });
    let opens = w.next() == Some("BEGIN")
        && match w.next() {
            Some("UNLOGGED" | "COUNTER") => w.next() == Some("BATCH"),
            Some("BATCH") => true,
            _ => false,
        };
    if !opens {
        return false;
    }
    let mut prev = "";
    for word in group.iter().filter_map(|t| match &t.tok {
        Tok::Word(w) => Some(w.as_str()),
        _ => None,
    }) {
        if prev == "APPLY" && word == "BATCH" {
            return false;
        }
        prev = word;
    }
    true
}

/// Splits `toks` and keeps the only statement, with the byte offset where
/// it ends.
fn split_one(toks: Vec<Token>, sql: &str, d: Dialect) -> Result<(Vec<Tok>, usize), String> {
    // CQL function bodies are `$$...$$` strings, so only batches span `;`.
    let block = match d {
        Dialect::Oracle => is_plsql(sql),
        Dialect::Cassandra => false,
        _ => is_routine(&toks),
    };
    let mut groups: Vec<Vec<Token>> = vec![Vec::new()];
    for t in toks {
        let in_batch = d == Dialect::Cassandra
            && t.tok == Tok::Semi
            && groups.last().is_some_and(|g| is_cql_batch_open(g));
        let separator = t.tok == Tok::Slash || (t.tok == Tok::Semi && !block && !in_batch);
        if separator {
            groups.push(Vec::new());
        } else if let Some(g) = groups.last_mut() {
            g.push(t);
        }
    }
    groups.retain(|g| !g.is_empty());
    match groups.len() {
        0 => Err("Nothing to run.".into()),
        1 => {
            let g = groups.pop().unwrap_or_default();
            let end = g.last().map_or(0, |t| t.end);
            Ok((g.into_iter().map(|t| t.tok).collect(), end))
        }
        n => Err(format!(
            "Send one statement per call; this text has {n} statements."
        )),
    }
}

/// Checks that `sql` is exactly one statement for `engine`.
pub fn one_statement(sql: &str, engine: Engine) -> Result<Checked, String> {
    let d = Dialect::of(engine);
    // MySQL reads `\'` as an escape unless NO_BACKSLASH_ESCAPES is set: the
    // text must be one statement both ways.
    let modes: &[bool] = if d == Dialect::Mysql {
        &[true, false]
    } else {
        &[false]
    };
    let mut views = Vec::new();
    let mut cut = 0;
    for &backslash in modes {
        let (toks, end) = split_one(lex(sql, d, backslash)?, sql, d)?;
        cut = cut.max(end);
        views.push(toks);
    }
    Ok(Checked {
        sql: sql[..cut].trim().to_string(),
        views,
    })
}

// ---------------------------------------------------------------------------
// Read-only classifier

/// Words that change data or run other statements, refused anywhere in a
/// read statement (data-changing CTEs, T-SQL batches without `;`, functions
/// with side effects).
const WRITE_WORDS: &[&str] = &[
    "INSERT",
    "UPDATE",
    "DELETE",
    "MERGE",
    "UPSERT",
    "TRUNCATE",
    "DROP",
    "ALTER",
    "CREATE",
    "GRANT",
    "REVOKE",
    "INTO",
    // PostgreSQL functions that act outside the transaction or change state.
    "SET_CONFIG",
    "NEXTVAL",
    "SETVAL",
    "PG_TERMINATE_BACKEND",
    "PG_CANCEL_BACKEND",
    "PG_RELOAD_CONF",
    "PG_ROTATE_LOGFILE",
    "PG_SWITCH_WAL",
    "PG_CREATE_RESTORE_POINT",
    "PG_PROMOTE",
    "PG_LOGICAL_EMIT_MESSAGE",
    "PG_FILE_WRITE",
    "PG_FILE_RENAME",
    "PG_FILE_UNLINK",
    "LO_IMPORT",
    "LO_EXPORT",
    "LO_UNLINK",
    "LO_CREATE",
    "LO_CREAT",
    "LO_FROM_BYTEA",
    "LO_PUT",
    "DBLINK",
    "DBLINK_EXEC",
    "DBLINK_CONNECT",
    // Session-level locks outlive the rolled-back transaction.
    "PG_ADVISORY_LOCK",
    "PG_ADVISORY_LOCK_SHARED",
    "PG_TRY_ADVISORY_LOCK",
    "PG_TRY_ADVISORY_LOCK_SHARED",
    "GET_LOCK",
    // SQLite
    "LOAD_EXTENSION",
];

/// T-SQL runs every statement of a batch, `;` or not.
const TSQL_WORDS: &[&str] = &[
    "EXEC",
    "EXECUTE",
    "BEGIN",
    "COMMIT",
    "ROLLBACK",
    "SAVE",
    "DECLARE",
    "SET",
    "USE",
    "DBCC",
    "KILL",
    "SHUTDOWN",
    "BACKUP",
    "RESTORE",
    "BULK",
    "DENY",
    "WAITFOR",
    "RECONFIGURE",
    "CHECKPOINT",
    "REVERT",
    "SETUSER",
    "OPENQUERY",
    "OPENROWSET",
    "OPENDATASOURCE",
];

/// PRAGMAs that only report.
const READ_PRAGMAS: &[&str] = &[
    "APPLICATION_ID",
    "AUTO_VACUUM",
    "AUTOMATIC_INDEX",
    "BUSY_TIMEOUT",
    "CACHE_SIZE",
    "CACHE_SPILL",
    "CELL_SIZE_CHECK",
    "COLLATION_LIST",
    "COMPILE_OPTIONS",
    "DATA_VERSION",
    "DATABASE_LIST",
    "DEFER_FOREIGN_KEYS",
    "ENCODING",
    "FOREIGN_KEYS",
    "FREELIST_COUNT",
    "FUNCTION_LIST",
    "JOURNAL_MODE",
    "JOURNAL_SIZE_LIMIT",
    "LEGACY_ALTER_TABLE",
    "LOCKING_MODE",
    "MAX_PAGE_COUNT",
    "MMAP_SIZE",
    "MODULE_LIST",
    "PAGE_COUNT",
    "PAGE_SIZE",
    "PRAGMA_LIST",
    "QUERY_ONLY",
    "RECURSIVE_TRIGGERS",
    "SCHEMA_VERSION",
    "SECURE_DELETE",
    "SYNCHRONOUS",
    "TEMP_STORE",
    "TRUSTED_SCHEMA",
    "USER_VERSION",
    "WAL_AUTOCHECKPOINT",
];

/// PRAGMAs that take a table or index name and only report.
const ARG_PRAGMAS: &[&str] = &[
    "TABLE_INFO",
    "TABLE_XINFO",
    "TABLE_LIST",
    "INDEX_LIST",
    "INDEX_INFO",
    "INDEX_XINFO",
    "FOREIGN_KEY_LIST",
    "FOREIGN_KEY_CHECK",
    "INTEGRITY_CHECK",
    "QUICK_CHECK",
];

fn check_pragma(toks: &[Tok]) -> Result<(), String> {
    if toks.contains(&Tok::Punct(b'=')) {
        return Err("PRAGMA with = changes a setting.".into());
    }
    // PRAGMA [schema.]name [(argument)]
    let mut rest = &toks[1..];
    if rest.get(1) == Some(&Tok::Punct(b'.')) {
        rest = &rest[2..];
    }
    let Some(Tok::Word(name)) = rest.first() else {
        return Err("Name the PRAGMA to read.".into());
    };
    let has_arg = rest.len() > 1;
    if ARG_PRAGMAS.contains(&name.as_str()) || (!has_arg && READ_PRAGMAS.contains(&name.as_str())) {
        Ok(())
    } else {
        Err(format!(
            "PRAGMA {name} is not one of the PRAGMAs that only read."
        ))
    }
}

/// CQL reads: SELECT and LIST only. LIST (ROLES, USERS, PERMISSIONS) only
/// reports, so its permission names (CREATE, ALTER, DROP...) are not scanned.
fn check_cql_view(first: &str, toks: &[Tok]) -> Result<(), String> {
    match first {
        "LIST" => Ok(()),
        "SELECT" => match words(toks).find(|w| WRITE_WORDS.contains(w)) {
            Some(w) => Err(format!("{w} is not allowed in a read statement.")),
            None => Ok(()),
        },
        "DESCRIBE" | "DESC" => Err(
            "DESCRIBE is a cqlsh command, not CQL; use the schema, table and search tools instead."
                .into(),
        ),
        "WITH" | "EXPLAIN" => Err(format!("{first} does not exist in CQL; send a SELECT.")),
        _ => Err(format!(
            "{first} is not a read statement; CQL reads are SELECT and LIST."
        )),
    }
}

fn check_view(toks: &[Tok], engine: Engine) -> Result<(), String> {
    let first =
        first_word(toks).ok_or("The statement must start with a keyword such as SELECT.")?;
    if engine == Engine::Cassandra {
        return check_cql_view(first, toks);
    }
    let allowed = matches!(
        first,
        "SELECT" | "WITH" | "SHOW" | "DESCRIBE" | "DESC" | "EXPLAIN" | "VALUES" | "TABLE"
    ) || (first == "PRAGMA" && engine == Engine::Sqlite);
    if !allowed {
        return Err(format!("{first} is not a read statement."));
    }
    if matches!(first, "EXPLAIN" | "DESCRIBE" | "DESC")
        && matches!(engine, Engine::Postgres | Engine::Mysql | Engine::Mariadb)
        && words(toks).any(|w| w == "ANALYZE" || w == "ANALYSE")
    {
        return Err(
            "EXPLAIN ANALYZE runs the statement; use the explain tool for the plan.".into(),
        );
    }
    if first == "PRAGMA" {
        return check_pragma(toks);
    }
    let mut prev = "";
    let mut prev2 = "";
    for w in words(toks) {
        // NEXT VALUE FOR (SQL Server, MariaDB) advances a sequence, which
        // no rollback undoes.
        if w == "FOR" && prev == "VALUE" && prev2 == "NEXT" {
            return Err("NEXT VALUE FOR is not allowed in a read statement.".into());
        }
        // SELECT ... FOR UPDATE / FOR NO KEY UPDATE only lock, and the lock
        // ends with the rolled-back transaction.
        let locking = w == "UPDATE" && matches!(prev, "FOR" | "KEY");
        let refused =
            WRITE_WORDS.contains(&w) || (engine == Engine::Sqlserver && TSQL_WORDS.contains(&w));
        if refused && !locking {
            return Err(format!("{w} is not allowed in a read statement."));
        }
        prev2 = prev;
        prev = w;
    }
    Ok(())
}

/// Accepts SELECT, WITH, SHOW, DESCRIBE, EXPLAIN (without ANALYZE), VALUES,
/// TABLE and reading PRAGMAs (CQL: SELECT and LIST); the error says why the
/// text is not a read.
pub fn check_read(stmt: &Checked, engine: Engine) -> Result<(), String> {
    stmt.views.iter().try_for_each(|v| check_view(v, engine))
}

/// `execute` statements: no transaction control, since each call commits on
/// its own.
pub fn check_write(stmt: &Checked, engine: Engine) -> Result<(), String> {
    const MESSAGE: &str =
        "Transaction control is not available to agents: each execute call commits on its own.";
    for toks in &stmt.views {
        let mut w = words(toks);
        let first = w.next().unwrap_or("");
        let second = w.next().unwrap_or("");
        let control = match first {
            // `BEGIN [UNLOGGED|COUNTER] BATCH` is one statement, not a
            // transaction; Accord (Cassandra 5.1+) uses BEGIN TRANSACTION.
            "BEGIN" if engine == Engine::Cassandra => {
                matches!(second, "TRANSACTION" | "TRAN")
            }
            // CQL has no savepoints; USE would change the session's keyspace
            // for later calls.
            "USE" if engine == Engine::Cassandra => {
                return Err(
                    "USE changes the session keyspace; qualify names as keyspace.table instead."
                        .into(),
                );
            }
            "BEGIN" => match engine {
                Engine::Oracle => false,
                Engine::Sqlserver => matches!(second, "TRAN" | "TRANSACTION" | "DISTRIBUTED"),
                _ => true,
            },
            "START" => second == "TRANSACTION",
            "COMMIT" | "ROLLBACK" | "SAVEPOINT" | "RELEASE" | "ABORT" | "XA" => true,
            "END" => matches!(engine, Engine::Postgres | Engine::Sqlite),
            "SAVE" => engine == Engine::Sqlserver,
            "LOCK" => matches!(engine, Engine::Mysql | Engine::Mariadb),
            "SET" => words(toks).any(|w| w == "AUTOCOMMIT" || w == "IMPLICIT_TRANSACTIONS"),
            _ => false,
        };
        if control {
            return Err(MESSAGE.into());
        }
    }
    Ok(())
}

/// `explain` takes the statement itself; the engine's EXPLAIN is added.
pub fn check_explain(stmt: &Checked) -> Result<(), String> {
    match stmt.first_word() {
        Some("EXPLAIN" | "DESCRIBE" | "DESC" | "ANALYZE" | "ANALYSE") => {
            Err("Pass the statement itself; explain adds EXPLAIN and never runs it.".into())
        }
        _ => Ok(()),
    }
}

// ---------------------------------------------------------------------------
// Redis

/// Commands agents never send: they change the session for later calls or
/// take over the connection.
const REDIS_BLOCKED: &[&str] = &[
    "MULTI",
    "EXEC",
    "DISCARD",
    "WATCH",
    "UNWATCH",
    "SELECT",
    "SWAPDB",
    "AUTH",
    "HELLO",
    "QUIT",
    "RESET",
    "CLIENT",
    "MONITOR",
    "SUBSCRIBE",
    "PSUBSCRIBE",
    "SSUBSCRIBE",
    "UNSUBSCRIBE",
    "PUNSUBSCRIBE",
    "SUNSUBSCRIBE",
    "SYNC",
    "PSYNC",
    "SHUTDOWN",
    "DEBUG",
    "REPLICAOF",
    "SLAVEOF",
    "FAILOVER",
];

/// Commands that only read.
const REDIS_READ: &[&str] = &[
    "GET",
    "MGET",
    "STRLEN",
    "GETRANGE",
    "SUBSTR",
    "LCS",
    "GETBIT",
    "BITCOUNT",
    "BITPOS",
    "BITFIELD_RO",
    "EXISTS",
    "TYPE",
    "TTL",
    "PTTL",
    "EXPIRETIME",
    "PEXPIRETIME",
    "RANDOMKEY",
    "SCAN",
    "HSCAN",
    "SSCAN",
    "ZSCAN",
    "HGET",
    "HMGET",
    "HGETALL",
    "HKEYS",
    "HVALS",
    "HLEN",
    "HEXISTS",
    "HSTRLEN",
    "HRANDFIELD",
    "LRANGE",
    "LLEN",
    "LINDEX",
    "LPOS",
    "SMEMBERS",
    "SISMEMBER",
    "SMISMEMBER",
    "SCARD",
    "SRANDMEMBER",
    "SINTER",
    "SINTERCARD",
    "SUNION",
    "SDIFF",
    "ZRANGE",
    "ZRANGEBYSCORE",
    "ZRANGEBYLEX",
    "ZREVRANGE",
    "ZREVRANGEBYSCORE",
    "ZREVRANGEBYLEX",
    "ZSCORE",
    "ZMSCORE",
    "ZCARD",
    "ZCOUNT",
    "ZLEXCOUNT",
    "ZRANK",
    "ZREVRANK",
    "ZRANDMEMBER",
    "ZINTER",
    "ZINTERCARD",
    "ZUNION",
    "ZDIFF",
    "XRANGE",
    "XREVRANGE",
    "XLEN",
    "XPENDING",
    "PFCOUNT",
    "GEOPOS",
    "GEODIST",
    "GEOHASH",
    "GEOSEARCH",
    "GEORADIUS_RO",
    "GEORADIUSBYMEMBER_RO",
    "SORT_RO",
    "INFO",
    "DBSIZE",
    "LASTSAVE",
    "PING",
    "ECHO",
    "TIME",
];

/// Read commands with a subcommand.
const REDIS_READ_SUB: &[(&str, &[&str])] = &[
    ("MEMORY", &["USAGE", "STATS", "DOCTOR"]),
    ("OBJECT", &["ENCODING", "FREQ", "IDLETIME", "REFCOUNT"]),
    ("XINFO", &["STREAM", "GROUPS", "CONSUMERS"]),
];

/// One redis-cli command; `write` lifts the read allowlist.
pub fn check_redis(text: &str, write: bool) -> Result<(), String> {
    let text = text.trim();
    if text.contains(['\n', '\r']) {
        return Err(
            "Send one Redis command per call, on one line (write \\n inside double quotes).".into(),
        );
    }
    let args = split_args(text).map_err(|e| e.message)?;
    let Some(name) = args.first() else {
        return Err("Enter a Redis command.".into());
    };
    let name = String::from_utf8_lossy(name).to_ascii_uppercase();
    if REDIS_BLOCKED.contains(&name.as_str()) {
        return Err(format!("{name} is not available to agents."));
    }
    if write {
        return Ok(());
    }
    if name == "KEYS" {
        return Err(
            "KEYS blocks the server while it walks every key; use SCAN 0 MATCH pattern COUNT 100."
                .into(),
        );
    }
    if REDIS_READ.contains(&name.as_str()) {
        return Ok(());
    }
    if let Some((_, subs)) = REDIS_READ_SUB.iter().find(|(n, _)| *n == name) {
        let sub = args
            .get(1)
            .map(|s| String::from_utf8_lossy(s).to_ascii_uppercase())
            .unwrap_or_default();
        if subs.contains(&sub.as_str()) {
            return Ok(());
        }
        return Err(format!("{name} {sub} is not a read command."));
    }
    Err(format!("{name} is not a read command."))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn one(sql: &str, engine: Engine) -> Result<String, String> {
        one_statement(sql, engine).map(|c| c.sql)
    }

    fn read(sql: &str, engine: Engine) -> Result<(), String> {
        check_read(&one_statement(sql, engine)?, engine)
    }

    #[test]
    fn single_statement_with_trailing_terminators() {
        assert_eq!(one("select 1;", Engine::Postgres).unwrap(), "select 1");
        assert_eq!(
            one("select 1 ; ;  -- done", Engine::Postgres).unwrap(),
            "select 1"
        );
        assert_eq!(one("select 1\n/\n", Engine::Oracle).unwrap(), "select 1");
        assert_eq!(one("select 1;\n/", Engine::Oracle).unwrap(), "select 1");
        assert!(one("  -- nothing\n ; ", Engine::Sqlite).is_err());
    }

    #[test]
    fn multiple_statements_are_refused() {
        for engine in [
            Engine::Postgres,
            Engine::Oracle,
            Engine::Mysql,
            Engine::Sqlserver,
            Engine::Sqlite,
        ] {
            assert!(one("select 1; select 2", engine).is_err(), "{engine:?}");
            assert!(one("select 1;delete from t", engine).is_err(), "{engine:?}");
        }
        assert!(one("select 1\n/\nselect 2", Engine::Oracle).is_err());
    }

    #[test]
    fn semicolons_inside_quotes_and_comments() {
        let cases = [
            (
                Engine::Postgres,
                "select 'a;b', \"c;d\" -- ;\n from t /* ; /* ; */ ; */",
            ),
            (Engine::Postgres, "select $$a;b$$, $fn$ x; $$ ; $fn$"),
            (Engine::Postgres, r"select E'it\'s; here'"),
            (Engine::Postgres, "select 'it''s; here'"),
            (Engine::Postgres, "select $1::int"),
            (Engine::Oracle, "select q'[a ; it's]' from dual"),
            (Engine::Oracle, "select nq'{;}' from dual"),
            (Engine::Mysql, "select `a;b`, 'x;y' # ;\n from t"),
            (Engine::Mysql, "select 'it''s;'"),
            (Engine::Sqlserver, "select [a;]]b] from t"),
            (Engine::Sqlserver, "select 1 /* /* ; */ ; */"),
            (Engine::Sqlite, "select [a;b], `c;d`, \"e;f\" from t"),
        ];
        for (engine, sql) in cases {
            assert!(one(sql, engine).is_ok(), "{engine:?}: {sql}");
        }
    }

    #[test]
    fn dialect_specific_traps() {
        // Non-nested comments end at the first */ in MySQL, Oracle and SQLite.
        assert!(one("select 1 /* /* */ ; delete from t */", Engine::Mysql).is_err());
        assert!(one("select 1 /* /* */ ; delete from t */", Engine::Sqlite).is_err());
        // MySQL `--` needs a space.
        assert!(one("select 1--1; delete from t", Engine::Mysql).is_err());
        // Backslash escapes: one statement only if it is one either way.
        assert!(one(r"select 'a\'; delete from t; -- '", Engine::Mysql).is_err());
        assert!(one("select 1 /*! ; delete from t */", Engine::Mysql).is_err());
        // Unterminated text is refused, not guessed.
        assert!(one("select 'abc", Engine::Postgres).is_err());
        assert!(one("select $x$ abc", Engine::Postgres).is_err());
        assert!(one("select 1 /* open", Engine::Sqlserver).is_err());
    }

    #[test]
    fn routine_bodies_are_one_statement() {
        let plsql = "begin\n  update t set a = 1;\n  commit;\nend;\n/";
        assert_eq!(
            one(plsql, Engine::Oracle).unwrap(),
            "begin\n  update t set a = 1;\n  commit;\nend;"
        );
        assert!(one("begin null; end;\n/\nselect 1 from dual", Engine::Oracle).is_err());
        assert!(one(
            "CREATE PROCEDURE p() BEGIN SELECT 1; SELECT 2; END",
            Engine::Mysql
        )
        .is_ok());
        assert!(one(
            "CREATE TRIGGER tr AFTER INSERT ON t BEGIN UPDATE u SET n = n + 1; END;",
            Engine::Sqlite
        )
        .is_ok());
    }

    #[test]
    fn classifier_accepts_reads() {
        assert!(read("SELECT * FROM t", Engine::Postgres).is_ok());
        assert!(read("(select 1) union (select 2)", Engine::Postgres).is_ok());
        assert!(read("with x as (select 1) select * from x", Engine::Sqlserver).is_ok());
        assert!(read("select * from t for update", Engine::Oracle).is_ok());
        assert!(read("select * from t for no key update", Engine::Postgres).is_ok());
        assert!(read("show tables", Engine::Mysql).is_ok());
        assert!(read("explain select 1", Engine::Mysql).is_ok());
        assert!(read("values (1), (2)", Engine::Sqlite).is_ok());
        assert!(read("select 'insert into t'", Engine::Postgres).is_ok());
        assert!(read("select last_update, \"delete\" from t", Engine::Postgres).is_ok());
        assert!(read("pragma table_info('t')", Engine::Sqlite).is_ok());
        assert!(read("PRAGMA main.user_version", Engine::Sqlite).is_ok());
    }

    #[test]
    fn classifier_refuses_writes() {
        let cases = [
            (Engine::Postgres, "delete from t"),
            (
                Engine::Postgres,
                "with d as (delete from t returning *) select * from d",
            ),
            (Engine::Postgres, "select * into t2 from t"),
            (Engine::Postgres, "select 1into t2"),
            (Engine::Postgres, "explain analyze delete from t"),
            (Engine::Postgres, "explain (analyze, costs off) select 1"),
            (
                Engine::Postgres,
                "select set_config('default_transaction_read_only', 'off', false)",
            ),
            (Engine::Postgres, "select nextval('s')"),
            (Engine::Postgres, "copy t to stdout"),
            (Engine::Postgres, "call p()"),
            (Engine::Mysql, "select * from t into outfile '/tmp/x'"),
            (Engine::Mysql, "explain analyze select 1"),
            (Engine::Mysql, "replace into t values (1)"),
            (Engine::Sqlserver, "select 1 delete from t"),
            (Engine::Sqlserver, "select 1 exec sp_who"),
            (
                Engine::Sqlserver,
                "with c as (select * from t) update c set a = 1",
            ),
            (
                Engine::Sqlserver,
                "select * from openquery(srv, 'delete x')",
            ),
            (Engine::Sqlserver, "select 1\u{a0}delete from t"),
            (Engine::Oracle, "begin null; end;"),
            (
                Engine::Oracle,
                "merge into t using u on (1 = 1) when matched then update set a = 1",
            ),
            (Engine::Sqlite, "pragma user_version = 5"),
            (Engine::Sqlite, "pragma query_only(0)"),
            (Engine::Sqlite, "pragma writable_schema"),
            (Engine::Sqlite, "attach 'x.db' as x"),
            (
                Engine::Sqlite,
                "with x as (select 1) insert into t select * from x",
            ),
        ];
        for (engine, sql) in cases {
            assert!(read(sql, engine).is_err(), "{engine:?}: {sql}");
        }
    }

    #[test]
    fn write_check_refuses_transaction_control() {
        let write = |sql: &str, engine| check_write(&one_statement(sql, engine).unwrap(), engine);
        assert!(write("update t set a = 1", Engine::Postgres).is_ok());
        assert!(write("begin update t set a = 1; end;", Engine::Oracle).is_ok());
        assert!(write("begin", Engine::Postgres).is_err());
        assert!(write("start transaction", Engine::Mysql).is_err());
        assert!(write("commit", Engine::Oracle).is_err());
        assert!(write("begin tran", Engine::Sqlserver).is_err());
        assert!(write("set autocommit = 0", Engine::Mysql).is_err());
        assert!(write("set implicit_transactions on", Engine::Sqlserver).is_err());
    }

    #[test]
    fn cql_statements_and_batches() {
        let cql = |sql: &str| one(sql, Engine::Cassandra);
        assert_eq!(cql("select * from ks.t;").unwrap(), "select * from ks.t");
        assert!(cql("select 1 from t; select 2 from t").is_err());
        assert!(cql("select 1 from t; delete from t where k = 1;").is_err());
        assert!(cql("  -- nothing\n // nothing\n ; ").is_err());
        let batch = "BEGIN BATCH\n INSERT INTO ks.t (k) VALUES (1);\n \
                     INSERT INTO ks.t (k) VALUES (2);\nAPPLY BATCH;";
        assert!(cql(batch).unwrap().ends_with("APPLY BATCH"));
        for head in ["begin unlogged batch", "BEGIN COUNTER BATCH"] {
            let sql = format!("{head} update t set c = c + 1 where k = 1; apply batch");
            assert!(cql(&sql).is_ok(), "{sql}");
        }
        // Whatever follows APPLY BATCH is a second statement.
        assert!(cql(&format!("{batch} select 1 from t")).is_err());
        assert!(cql(&format!("{batch} delete from t where k = 1;")).is_err());
        // Other BEGIN statements still split at `;`.
        assert!(cql("begin; select 1 from t").is_err());
    }

    #[test]
    fn cql_lexer_hides_semicolons_and_keywords() {
        let cases = [
            "select 'a;b', \"c;d\" from t -- ;\n where k = 1",
            "select 'it''s; here' from t // ; delete from t\n",
            "select \"we\"\"ird;\" from t /* ; /* */",
            "create function f(a int) returns null on null input returns int \
             language java as $$ return a; $$",
            "select $$a;b$$ from t",
        ];
        for sql in cases {
            assert!(one(sql, Engine::Cassandra).is_ok(), "{sql}");
        }
        // Block comments do not nest, `\` does not escape in strings.
        assert!(one("select 1 /* /* */ ; delete from t */", Engine::Cassandra).is_err());
        assert!(one(r"select 'a\'; delete from t; -- '", Engine::Cassandra).is_err());
        assert!(one("select 'abc", Engine::Cassandra).is_err());
        assert!(one("select $$ abc", Engine::Cassandra).is_err());
        assert!(one("select 1 /* open", Engine::Cassandra).is_err());
        // A comment hides a write word from the classifier too.
        assert!(read("select a from t -- delete\n", Engine::Cassandra).is_ok());
    }

    #[test]
    fn cql_classifier() {
        let cql = |sql: &str| read(sql, Engine::Cassandra);
        assert!(cql("SELECT * FROM ks.t WHERE k = 1 LIMIT 10").is_ok());
        assert!(cql("select * from ks.t where c > 1 allow filtering").is_ok());
        assert!(cql("select \"insert\", 'delete from t' from t").is_ok());
        assert!(cql("select writetime(v), ttl(v) from t").is_ok());
        assert!(cql("LIST ROLES").is_ok());
        assert!(cql("list all permissions of alice").is_ok());
        assert!(cql("LIST USERS").is_ok());
        let describe = cql("describe keyspaces").unwrap_err();
        assert!(describe.contains("cqlsh"), "{describe}");
        assert!(cql("desc table ks.t").is_err());
        for sql in [
            "insert into t (k) values (1)",
            "update t set v = 1 where k = 1",
            "delete from t where k = 1",
            "begin batch insert into t (k) values (1); apply batch",
            "create table t (k int primary key)",
            "alter table t add c int",
            "drop table t",
            "truncate t",
            "grant select on t to bob",
            "revoke select on t from bob",
            "use ks",
            "with x as (select 1) select * from x",
            "explain select * from t",
            "select * from t where k = 1 delete from t",
            "select * into t2 from t",
        ] {
            assert!(cql(sql).is_err(), "{sql}");
        }
    }

    #[test]
    fn cql_write_check() {
        let write = |sql: &str| {
            check_write(
                &one_statement(sql, Engine::Cassandra).unwrap(),
                Engine::Cassandra,
            )
        };
        assert!(write("insert into ks.t (k) values (1)").is_ok());
        assert!(write("create table ks.t (k int primary key)").is_ok());
        assert!(write("begin batch insert into t (k) values (1); apply batch").is_ok());
        assert!(write("BEGIN UNLOGGED BATCH delete from t where k = 1; APPLY BATCH").is_ok());
        assert!(write("BEGIN COUNTER BATCH update t set c = c + 1; APPLY BATCH").is_ok());
        assert!(write("begin transaction").is_err());
        assert!(write("commit transaction").is_err());
        assert!(write("start transaction").is_err());
        assert!(write("commit").is_err());
        assert!(write("rollback").is_err());
        assert!(write("use ks").is_err());
    }

    #[test]
    fn explain_takes_the_statement() {
        let c = one_statement("explain select 1", Engine::Postgres).unwrap();
        assert!(check_explain(&c).is_err());
        let c = one_statement("update t set a = 1", Engine::Postgres).unwrap();
        assert!(check_explain(&c).is_ok());
    }

    #[test]
    fn redis_allowlist() {
        assert!(check_redis("GET key", false).is_ok());
        assert!(check_redis("hgetall \"user:1\"", false).is_ok());
        assert!(check_redis("SCAN 0 MATCH user:* COUNT 100", false).is_ok());
        assert!(check_redis("MEMORY USAGE key", false).is_ok());
        assert!(check_redis("OBJECT ENCODING key", false).is_ok());
        assert!(check_redis("set key 1", false).is_err());
        assert!(check_redis("DEL key", false).is_err());
        assert!(check_redis("MEMORY PURGE", false).is_err());
        assert!(check_redis("CONFIG GET *", false).is_err());
        let keys = check_redis("KEYS *", false).unwrap_err();
        assert!(keys.contains("SCAN"));
        assert!(check_redis("GET a\nDEL a", false).is_err());
        assert!(check_redis("", false).is_err());
        // Write access lifts the allowlist but not the session commands.
        assert!(check_redis("SET key 1", true).is_ok());
        assert!(check_redis("MULTI", true).is_err());
        assert!(check_redis("select 2", true).is_err());
    }

    #[test]
    fn refuses_effects_that_outlive_a_rollback() {
        for (engine, sql) in [
            (Engine::Sqlserver, "SELECT NEXT VALUE FOR dbo.seq"),
            (Engine::Mariadb, "select next value for s"),
            (Engine::Mysql, "SELECT GET_LOCK('x', 1)"),
            (Engine::Postgres, "select pg_advisory_lock(1)"),
            (Engine::Postgres, "select lo_creat(-1)"),
        ] {
            let checked = one_statement(sql, engine).unwrap();
            assert!(check_read(&checked, engine).is_err(), "{sql}");
        }
        let checked = one_statement("select pg_advisory_xact_lock(1)", Engine::Postgres).unwrap();
        assert!(check_read(&checked, Engine::Postgres).is_ok());
    }
}
