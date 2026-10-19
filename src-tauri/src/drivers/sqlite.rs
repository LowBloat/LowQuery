//! SQLite session on an embedded database file, through `rusqlite` with the
//! bundled SQLite library.
//!
//! The connection is synchronous and not `Sync`: it lives behind a mutex and
//! every call runs in `spawn_blocking`. The row limit stops stepping the
//! statement at `max_rows + 1`. Cancel uses `sqlite3_interrupt`, which is safe
//! to call from another thread while a statement runs.

use std::fmt::Write as _;
use std::path::Path;
use std::sync::Arc;
use std::time::Instant;

use parking_lot::Mutex;
use rusqlite::types::{Type, ValueRef};
use rusqlite::{ffi, Connection, InterruptHandle, OpenFlags};

use super::{first_keywords, TxState};
use crate::error::{AppError, AppResult, ErrorKind};
use crate::model::{
    ColumnInfo, ConnectionProfile, DbColumn, DbObject, ExecRequest, ExecResult, ValueKind,
};

const BINARY_PREVIEW_BYTES: usize = 32 * 1024;
const BUSY_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

pub struct LiteSession {
    conn: Arc<Mutex<Connection>>,
    interrupt: InterruptHandle,
    state: Arc<Mutex<TxState>>,
    pub server_version: String,
    pub current_user: Option<String>,
    pub current_schema: Option<String>,
}

pub async fn connect(profile: &ConnectionProfile) -> AppResult<LiteSession> {
    let path = profile.database.trim().to_string();
    let create = profile.create_file;
    blocking(move || {
        if path.is_empty() {
            return Err(AppError::invalid("Choose the database file."));
        }
        let special = path == ":memory:" || path.starts_with("file:");
        if !special && !Path::new(&path).exists() && !create {
            return Err(AppError::connection(format!(
                "Database file not found: {path}"
            )));
        }
        let mut flags = OpenFlags::SQLITE_OPEN_READ_WRITE
            | OpenFlags::SQLITE_OPEN_NO_MUTEX
            | OpenFlags::SQLITE_OPEN_URI;
        if create {
            flags |= OpenFlags::SQLITE_OPEN_CREATE;
        }
        let conn = Connection::open_with_flags(&path, flags).map_err(connection_error)?;
        conn.busy_timeout(BUSY_TIMEOUT).map_err(connection_error)?;
        conn.execute_batch("PRAGMA foreign_keys = ON")
            .map_err(connection_error)?;
        // Opening is lazy: reading the schema catches files that are not
        // databases now instead of on the first statement.
        conn.query_row("SELECT count(*) FROM sqlite_schema", [], |r| {
            r.get::<_, i64>(0)
        })
        .map_err(connection_error)?;

        Ok(LiteSession {
            interrupt: conn.get_interrupt_handle(),
            conn: Arc::new(Mutex::new(conn)),
            state: Arc::new(Mutex::new(TxState {
                autocommit: true,
                in_transaction: false,
            })),
            server_version: format!("SQLite {}", rusqlite::version()),
            current_user: None,
            current_schema: Some("main".into()),
        })
    })
    .await
}

async fn blocking<T, F>(f: F) -> AppResult<T>
where
    T: Send + 'static,
    F: FnOnce() -> AppResult<T> + Send + 'static,
{
    tauri::async_runtime::spawn_blocking(f)
        .await
        .map_err(|e| AppError::internal(e.to_string()))?
}

impl LiteSession {
    pub fn tx_state(&self) -> TxState {
        *self.state.lock()
    }

    pub async fn execute(&self, req: &ExecRequest) -> AppResult<ExecResult> {
        let conn = self.conn.clone();
        let state = self.state.clone();
        let sql = req.sql.clone();
        let max_rows = req.max_rows;
        blocking(move || {
            let conn = conn.lock();
            let autocommit = state.lock().autocommit;
            let started = Instant::now();
            let outcome = run(&conn, &sql, max_rows, autocommit);
            // SQLite knows whether a transaction is open, including one
            // started or ended by the user's own BEGIN/COMMIT/ROLLBACK.
            let in_transaction = !conn.is_autocommit();
            state.lock().in_transaction = in_transaction;
            let mut result = outcome?;
            result.elapsed_ms = started.elapsed().as_millis() as u64;
            result.in_transaction = in_transaction;
            Ok(result)
        })
        .await
    }

    pub async fn cancel(&self) -> AppResult<()> {
        self.interrupt.interrupt();
        Ok(())
    }

    pub async fn set_autocommit(&self, on: bool) -> AppResult<TxState> {
        let conn = self.conn.clone();
        let state = self.state.clone();
        blocking(move || {
            let conn = conn.lock();
            // Switching back to autocommit commits pending work, as JDBC
            // clients do.
            let outcome = if on && !conn.is_autocommit() {
                conn.execute_batch("COMMIT").map_err(map_error)
            } else {
                Ok(())
            };
            let mut st = state.lock();
            st.in_transaction = !conn.is_autocommit();
            outcome?;
            st.autocommit = on;
            Ok(*st)
        })
        .await
    }

    pub async fn end_transaction(&self, commit: bool) -> AppResult<TxState> {
        let conn = self.conn.clone();
        let state = self.state.clone();
        blocking(move || {
            let conn = conn.lock();
            let outcome = if conn.is_autocommit() {
                Ok(())
            } else {
                conn.execute_batch(if commit { "COMMIT" } else { "ROLLBACK" })
                    .map_err(map_error)
            };
            let mut st = state.lock();
            st.in_transaction = !conn.is_autocommit();
            outcome?;
            Ok(*st)
        })
        .await
    }

    pub async fn list_schemas(&self) -> AppResult<Vec<String>> {
        let rows = self
            .query_rows(
                "SELECT name FROM pragma_database_list
                 ORDER BY CASE name WHEN 'main' THEN 0 WHEN 'temp' THEN 2 ELSE 1 END, name"
                    .into(),
                Vec::new(),
            )
            .await?;
        Ok(rows.into_iter().map(|mut r| r.swap_remove(0)).collect())
    }

    pub async fn list_objects(&self, schema: &str) -> AppResult<Vec<DbObject>> {
        let sql = format!(
            "SELECT name, type FROM {}.sqlite_schema
             WHERE type IN ('table', 'view') AND name NOT LIKE 'sqlite\\_%' ESCAPE '\\'
             ORDER BY type, name",
            quote_ident(schema)
        );
        let rows = self.query_rows(sql, Vec::new()).await?;
        Ok(rows
            .into_iter()
            .map(|mut r| DbObject {
                kind: r.pop().unwrap_or_default(),
                name: r.pop().unwrap_or_default(),
            })
            .collect())
    }

    pub async fn list_columns(&self, schema: &str, table: &str) -> AppResult<Vec<DbColumn>> {
        // table_xinfo also lists generated columns; hidden = 1 are the
        // hidden columns of virtual tables.
        let rows = self
            .query_rows(
                "SELECT name, type, \"notnull\", pk FROM pragma_table_xinfo(?1, ?2)
                 WHERE hidden <> 1 ORDER BY cid"
                    .into(),
                vec![table.to_string(), schema.to_string()],
            )
            .await?;
        Ok(rows
            .into_iter()
            .map(|r| DbColumn {
                name: r[0].clone(),
                type_name: r[1].clone(),
                nullable: r[2] == "0",
                primary_key: r[3] != "0",
            })
            .collect())
    }

    async fn query_rows(&self, sql: String, params: Vec<String>) -> AppResult<Vec<Vec<String>>> {
        let conn = self.conn.clone();
        blocking(move || {
            let conn = conn.lock();
            let mut stmt = conn.prepare(&sql).map_err(map_error)?;
            let n = stmt.column_count();
            let mut rows = stmt
                .query(rusqlite::params_from_iter(params.iter()))
                .map_err(map_error)?;
            let mut out = Vec::new();
            while let Some(row) = rows.next().map_err(map_error)? {
                let mut cells = Vec::with_capacity(n);
                for i in 0..n {
                    cells.push(cell(row.get_ref(i).map_err(map_error)?).unwrap_or_default());
                }
                out.push(cells);
            }
            Ok(out)
        })
        .await
    }
}

/// Runs one statement. In manual-commit mode a transaction is opened first
/// and stays open until the user ends it.
fn run(conn: &Connection, sql: &str, max_rows: usize, autocommit: bool) -> AppResult<ExecResult> {
    if is_blank(sql) {
        return Ok(ExecResult::default());
    }
    let kw = first_keywords(sql, 1);
    let first = kw.first().map(String::as_str).unwrap_or("");
    // Transaction control is the user's own; VACUUM, ATTACH and DETACH
    // cannot run inside a transaction, and several PRAGMAs are ignored or
    // fail there.
    let skip_begin = matches!(
        first,
        "BEGIN"
            | "COMMIT"
            | "END"
            | "ROLLBACK"
            | "SAVEPOINT"
            | "RELEASE"
            | "VACUUM"
            | "ATTACH"
            | "DETACH"
            | "PRAGMA"
    );
    let opened = !autocommit && conn.is_autocommit() && !skip_begin;
    if opened {
        conn.execute_batch("BEGIN").map_err(map_error)?;
    }
    let outcome = statement(conn, sql, max_rows, first);
    if outcome.is_err() && opened && !conn.is_autocommit() {
        // The failed statement was the only one in the transaction opened
        // for it: do not leave an empty transaction behind.
        let _ = conn.execute_batch("ROLLBACK");
    }
    outcome
}

fn statement(conn: &Connection, sql: &str, max_rows: usize, first: &str) -> AppResult<ExecResult> {
    let mut stmt = conn.prepare(sql).map_err(map_error)?;
    if stmt.column_count() == 0 {
        stmt.raw_execute().map_err(map_error)?;
        // changes() keeps the count of the last DML, so it only means
        // something right after one. WITH without result columns is DML.
        let dml = matches!(first, "INSERT" | "UPDATE" | "DELETE" | "REPLACE" | "WITH");
        return Ok(ExecResult {
            rows_affected: dml.then(|| conn.changes()),
            ..Default::default()
        });
    }

    let declared: Vec<(String, Option<String>)> = stmt
        .columns()
        .iter()
        .map(|c| {
            (
                c.name().to_string(),
                c.decl_type()
                    .map(|t| t.trim().to_string())
                    .filter(|t| !t.is_empty()),
            )
        })
        .collect();
    let n = declared.len();
    // Storage class of the first non-null value, for columns without a
    // declared type (expressions).
    let mut classes: Vec<Option<Type>> = vec![None; n];
    let mut out = Vec::new();
    let mut truncated = false;
    let mut rows = stmt.raw_query();
    while let Some(row) = rows.next().map_err(map_error)? {
        if max_rows > 0 && out.len() == max_rows {
            truncated = true;
            break;
        }
        let mut cells = Vec::with_capacity(n);
        for (i, class) in classes.iter_mut().enumerate() {
            let value = row.get_ref(i).map_err(map_error)?;
            if class.is_none() && value.data_type() != Type::Null {
                *class = Some(value.data_type());
            }
            cells.push(cell(value));
        }
        out.push(cells);
    }

    let columns = declared
        .into_iter()
        .zip(classes)
        .map(|((name, decl), class)| match decl {
            Some(t) => ColumnInfo {
                name,
                kind: declared_kind(&t),
                type_name: t,
            },
            None => ColumnInfo {
                name,
                type_name: class_name(class).into(),
                kind: class_kind(class),
            },
        })
        .collect();
    Ok(ExecResult {
        columns: Some(columns),
        rows: out,
        truncated,
        ..Default::default()
    })
}

/// Only whitespace, comments and semicolons: SQLite prepares nothing.
fn is_blank(sql: &str) -> bool {
    let bytes = sql.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        let c = bytes[i];
        if c.is_ascii_whitespace() || c == b';' {
            i += 1;
        } else if c == b'-' && bytes.get(i + 1) == Some(&b'-') {
            while i < bytes.len() && bytes[i] != b'\n' {
                i += 1;
            }
        } else if c == b'/' && bytes.get(i + 1) == Some(&b'*') {
            i += 2;
            while i + 1 < bytes.len() && !(bytes[i] == b'*' && bytes[i + 1] == b'/') {
                i += 1;
            }
            i += 2;
        } else {
            return false;
        }
    }
    true
}

fn quote_ident(name: &str) -> String {
    format!("\"{}\"", name.replace('"', "\"\""))
}

/// Kind from the declared type, following SQLite's affinity rules plus the
/// common names for booleans, dates and JSON.
fn declared_kind(decl: &str) -> ValueKind {
    let t = decl.to_ascii_uppercase();
    let has = |s: &str| t.contains(s);
    if has("BOOL") {
        ValueKind::Bool
    } else if has("JSON") {
        ValueKind::Json
    } else if has("DATE") || has("TIME") {
        ValueKind::Temporal
    } else if has("INT") {
        ValueKind::Number
    } else if has("CHAR") || has("CLOB") || has("TEXT") || has("STRING") || has("UUID") {
        ValueKind::Text
    } else if has("BLOB") || has("BINARY") {
        ValueKind::Binary
    } else if has("REAL") || has("FLOA") || has("DOUB") || has("NUM") || has("DEC") || has("MONEY")
    {
        ValueKind::Number
    } else {
        ValueKind::Other
    }
}

fn class_name(class: Option<Type>) -> &'static str {
    match class {
        Some(Type::Integer) => "INTEGER",
        Some(Type::Real) => "REAL",
        Some(Type::Text) => "TEXT",
        Some(Type::Blob) => "BLOB",
        Some(Type::Null) | None => "NULL",
    }
}

fn class_kind(class: Option<Type>) -> ValueKind {
    match class {
        Some(Type::Integer | Type::Real) => ValueKind::Number,
        Some(Type::Text) => ValueKind::Text,
        Some(Type::Blob) => ValueKind::Binary,
        Some(Type::Null) | None => ValueKind::Other,
    }
}

fn cell(value: ValueRef<'_>) -> Option<String> {
    match value {
        ValueRef::Null => None,
        ValueRef::Integer(i) => Some(i.to_string()),
        ValueRef::Real(f) => Some(real(f)),
        ValueRef::Text(t) => Some(String::from_utf8_lossy(t).into_owned()),
        ValueRef::Blob(b) => Some(blob(b)),
    }
}

/// Shortest text that reads back as the same double. Always shows it is a
/// REAL (`3.0`, `1.0e+300`), like the sqlite3 shell.
fn real(v: f64) -> String {
    if v.is_nan() {
        return "NaN".into();
    }
    if v.is_infinite() {
        return if v > 0.0 { "Inf" } else { "-Inf" }.into();
    }
    let abs = v.abs();
    if abs != 0.0 && !(1e-5..1e16).contains(&abs) {
        let s = format!("{v:e}");
        let (mantissa, exp) = s.split_once('e').unwrap_or((&s, "0"));
        let mantissa = if mantissa.contains('.') {
            mantissa.to_string()
        } else {
            format!("{mantissa}.0")
        };
        let exp = if exp.starts_with('-') {
            exp.to_string()
        } else {
            format!("+{exp}")
        };
        return format!("{mantissa}e{exp}");
    }
    let s = v.to_string();
    if s.contains('.') {
        s
    } else {
        s + ".0"
    }
}

/// `X'00FF'` like a blob literal; large blobs show the first 32 KiB.
fn blob(bytes: &[u8]) -> String {
    let shown = &bytes[..bytes.len().min(BINARY_PREVIEW_BYTES)];
    let mut s = String::with_capacity(shown.len() * 2 + 3);
    s.push_str("X'");
    for b in shown {
        let _ = write!(s, "{b:02X}");
    }
    s.push('\'');
    if bytes.len() > shown.len() {
        let _ = write!(s, "… ({} bytes)", bytes.len());
    }
    s
}

macro_rules! code_names {
    ($code:expr; $($name:ident),* $(,)?) => {
        match $code {
            $(ffi::$name => Some(stringify!($name)),)*
            _ => None,
        }
    };
}

/// Name of an (extended) result code, as in the SQLite documentation.
fn code_name(code: i32) -> String {
    fn lookup(code: i32) -> Option<&'static str> {
        code_names!(code;
            SQLITE_ERROR, SQLITE_INTERNAL, SQLITE_PERM, SQLITE_ABORT, SQLITE_BUSY,
            SQLITE_LOCKED, SQLITE_NOMEM, SQLITE_READONLY, SQLITE_INTERRUPT, SQLITE_IOERR,
            SQLITE_CORRUPT, SQLITE_NOTFOUND, SQLITE_FULL, SQLITE_CANTOPEN, SQLITE_PROTOCOL,
            SQLITE_EMPTY, SQLITE_SCHEMA, SQLITE_TOOBIG, SQLITE_CONSTRAINT, SQLITE_MISMATCH,
            SQLITE_MISUSE, SQLITE_NOLFS, SQLITE_AUTH, SQLITE_FORMAT, SQLITE_RANGE,
            SQLITE_NOTADB,
            SQLITE_ERROR_MISSING_COLLSEQ, SQLITE_ERROR_RETRY, SQLITE_ERROR_SNAPSHOT,
            SQLITE_IOERR_READ, SQLITE_IOERR_SHORT_READ, SQLITE_IOERR_WRITE, SQLITE_IOERR_FSYNC,
            SQLITE_IOERR_DIR_FSYNC, SQLITE_IOERR_TRUNCATE, SQLITE_IOERR_FSTAT,
            SQLITE_IOERR_UNLOCK, SQLITE_IOERR_RDLOCK, SQLITE_IOERR_DELETE, SQLITE_IOERR_BLOCKED,
            SQLITE_IOERR_NOMEM, SQLITE_IOERR_ACCESS, SQLITE_IOERR_CHECKRESERVEDLOCK,
            SQLITE_IOERR_LOCK, SQLITE_IOERR_CLOSE, SQLITE_IOERR_DIR_CLOSE, SQLITE_IOERR_SHMOPEN,
            SQLITE_IOERR_SHMSIZE, SQLITE_IOERR_SHMLOCK, SQLITE_IOERR_SHMMAP, SQLITE_IOERR_SEEK,
            SQLITE_IOERR_DELETE_NOENT, SQLITE_IOERR_MMAP, SQLITE_IOERR_GETTEMPPATH,
            SQLITE_IOERR_CONVPATH, SQLITE_IOERR_VNODE, SQLITE_IOERR_AUTH,
            SQLITE_IOERR_BEGIN_ATOMIC, SQLITE_IOERR_COMMIT_ATOMIC, SQLITE_IOERR_ROLLBACK_ATOMIC,
            SQLITE_IOERR_DATA, SQLITE_IOERR_CORRUPTFS,
            SQLITE_LOCKED_SHAREDCACHE, SQLITE_LOCKED_VTAB,
            SQLITE_BUSY_RECOVERY, SQLITE_BUSY_SNAPSHOT, SQLITE_BUSY_TIMEOUT,
            SQLITE_CANTOPEN_NOTEMPDIR, SQLITE_CANTOPEN_ISDIR, SQLITE_CANTOPEN_FULLPATH,
            SQLITE_CANTOPEN_CONVPATH, SQLITE_CANTOPEN_DIRTYWAL, SQLITE_CANTOPEN_SYMLINK,
            SQLITE_CORRUPT_VTAB, SQLITE_CORRUPT_SEQUENCE, SQLITE_CORRUPT_INDEX,
            SQLITE_READONLY_RECOVERY, SQLITE_READONLY_CANTLOCK, SQLITE_READONLY_ROLLBACK,
            SQLITE_READONLY_DBMOVED, SQLITE_READONLY_CANTINIT, SQLITE_READONLY_DIRECTORY,
            SQLITE_ABORT_ROLLBACK,
            SQLITE_CONSTRAINT_CHECK, SQLITE_CONSTRAINT_COMMITHOOK, SQLITE_CONSTRAINT_FOREIGNKEY,
            SQLITE_CONSTRAINT_FUNCTION, SQLITE_CONSTRAINT_NOTNULL, SQLITE_CONSTRAINT_PRIMARYKEY,
            SQLITE_CONSTRAINT_TRIGGER, SQLITE_CONSTRAINT_UNIQUE, SQLITE_CONSTRAINT_VTAB,
            SQLITE_CONSTRAINT_ROWID, SQLITE_CONSTRAINT_PINNED, SQLITE_CONSTRAINT_DATATYPE,
            SQLITE_AUTH_USER,
        )
    }
    lookup(code)
        .or_else(|| lookup(code & 0xff))
        .map(str::to_string)
        .unwrap_or_else(|| format!("SQLITE_{code}"))
}

fn sqlite_error(error: ffi::Error, msg: Option<String>) -> AppError {
    let message = msg.unwrap_or_else(|| ffi::code_to_str(error.extended_code).to_string());
    let mut err = AppError::new(ErrorKind::Database, message);
    if error.code == ffi::ErrorCode::OperationInterrupted {
        err.kind = ErrorKind::Cancelled;
        err.message = "The statement was cancelled.".into();
    }
    err.code = Some(code_name(error.extended_code));
    err
}

fn connection_error(e: rusqlite::Error) -> AppError {
    let mut err = map_error(e);
    err.kind = ErrorKind::Connection;
    err
}

pub fn map_error(e: rusqlite::Error) -> AppError {
    match e {
        rusqlite::Error::SqliteFailure(error, msg) => sqlite_error(error, msg),
        rusqlite::Error::SqlInputError {
            error,
            msg,
            sql,
            offset,
        } => {
            let mut err = sqlite_error(error, Some(msg));
            // SQLite reports a byte offset; the UI wants characters.
            err.position = usize::try_from(offset)
                .ok()
                .and_then(|o| sql.get(..o))
                .map(|s| s.chars().count());
            err
        }
        rusqlite::Error::MultipleStatement => {
            AppError::invalid("Only one statement can run at a time.")
        }
        other => AppError::new(ErrorKind::Database, other.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::Engine;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicUsize, Ordering};

    static COUNTER: AtomicUsize = AtomicUsize::new(0);

    fn temp_path(name: &str) -> PathBuf {
        let n = COUNTER.fetch_add(1, Ordering::SeqCst);
        let path = std::env::temp_dir().join(format!(
            "lowquery-test-{name}-{}-{n}.db",
            std::process::id()
        ));
        let _ = std::fs::remove_file(&path);
        path
    }

    fn profile(path: &Path, create: bool) -> ConnectionProfile {
        ConnectionProfile {
            engine: Engine::Sqlite,
            database: path.display().to_string(),
            create_file: create,
            ..Default::default()
        }
    }

    struct TempDb {
        path: PathBuf,
        session: Option<LiteSession>,
    }

    impl TempDb {
        async fn new(name: &str) -> Self {
            let path = temp_path(name);
            let session = connect(&profile(&path, true)).await.unwrap();
            Self {
                path,
                session: Some(session),
            }
        }

        fn s(&self) -> &LiteSession {
            self.session.as_ref().unwrap()
        }

        async fn run(&self, sql: &str) -> ExecResult {
            self.run_limit(sql, 0).await
        }

        async fn run_limit(&self, sql: &str, max_rows: usize) -> ExecResult {
            self.try_run(sql, max_rows)
                .await
                .unwrap_or_else(|e| panic!("{sql}: {e}"))
        }

        async fn try_run(&self, sql: &str, max_rows: usize) -> AppResult<ExecResult> {
            self.s()
                .execute(&ExecRequest {
                    sql: sql.into(),
                    max_rows,
                })
                .await
        }

        async fn scalar(&self, sql: &str) -> Option<String> {
            self.run(sql).await.rows[0][0].clone()
        }
    }

    impl Drop for TempDb {
        fn drop(&mut self) {
            self.session.take();
            for suffix in ["", "-journal", "-wal", "-shm"] {
                let _ = std::fs::remove_file(format!("{}{suffix}", self.path.display()));
            }
        }
    }

    #[test]
    fn formats_reals_shortest() {
        assert_eq!(real(3.0), "3.0");
        assert_eq!(real(-0.5), "-0.5");
        assert_eq!(real(0.1 + 0.2), "0.30000000000000004");
        assert_eq!(real(1e300), "1.0e+300");
        assert_eq!(real(-1.5e-7), "-1.5e-7");
        assert_eq!(real(123456789.125), "123456789.125");
        assert_eq!(real(f64::INFINITY), "Inf");
        assert_eq!(real(0.0), "0.0");
    }

    #[test]
    fn kinds_follow_affinity() {
        assert_eq!(declared_kind("INTEGER"), ValueKind::Number);
        assert_eq!(declared_kind("bigint"), ValueKind::Number);
        assert_eq!(declared_kind("NUMERIC(10,2)"), ValueKind::Number);
        assert_eq!(declared_kind("DOUBLE PRECISION"), ValueKind::Number);
        assert_eq!(declared_kind("VARCHAR(20)"), ValueKind::Text);
        assert_eq!(declared_kind("DATETIME"), ValueKind::Temporal);
        assert_eq!(declared_kind("BOOLEAN"), ValueKind::Bool);
        assert_eq!(declared_kind("JSON"), ValueKind::Json);
        assert_eq!(declared_kind("BLOB"), ValueKind::Binary);
        assert_eq!(declared_kind("GEOMETRY"), ValueKind::Other);
    }

    #[test]
    fn names_result_codes() {
        assert_eq!(code_name(ffi::SQLITE_ERROR), "SQLITE_ERROR");
        assert_eq!(
            code_name(ffi::SQLITE_CONSTRAINT_UNIQUE),
            "SQLITE_CONSTRAINT_UNIQUE"
        );
        assert_eq!(code_name(ffi::SQLITE_IOERR | (99 << 8)), "SQLITE_IOERR");
    }

    #[test]
    fn blank_statements() {
        assert!(is_blank(" ; -- x\n /* y */ ;"));
        assert!(!is_blank("-- x\nselect 1"));
    }

    #[tokio::test]
    async fn formats_every_storage_class() {
        let db = TempDb::new("values").await;
        assert!(db.s().server_version.starts_with("SQLite 3."));
        assert_eq!(db.s().current_schema.as_deref(), Some("main"));
        db.run("CREATE TABLE v (i INTEGER, r REAL, t TEXT, b BLOB, n NUMERIC, d DATETIME, j JSON, f BOOLEAN)")
            .await;
        db.run(
            "INSERT INTO v VALUES (-42, 2.5, 'olá', x'00ff10', 12.50, '2024-05-01 13:45:00', '{\"a\":1}', 1)",
        )
        .await;
        db.run("INSERT INTO v VALUES (NULL, 3, NULL, NULL, NULL, NULL, NULL, NULL)")
            .await;
        let r = db.run("SELECT * FROM v").await;
        let cols = r.columns.unwrap();
        let types: Vec<_> = cols.iter().map(|c| c.type_name.as_str()).collect();
        assert_eq!(
            types,
            ["INTEGER", "REAL", "TEXT", "BLOB", "NUMERIC", "DATETIME", "JSON", "BOOLEAN"]
        );
        let kinds: Vec<_> = cols.iter().map(|c| c.kind).collect();
        assert_eq!(
            kinds,
            [
                ValueKind::Number,
                ValueKind::Number,
                ValueKind::Text,
                ValueKind::Binary,
                ValueKind::Number,
                ValueKind::Temporal,
                ValueKind::Json,
                ValueKind::Bool
            ]
        );
        let row =
            |i: usize| -> Vec<Option<&str>> { r.rows[i].iter().map(|c| c.as_deref()).collect() };
        assert_eq!(
            row(0),
            [
                Some("-42"),
                Some("2.5"),
                Some("olá"),
                Some("X'00FF10'"),
                Some("12.5"),
                Some("2024-05-01 13:45:00"),
                Some("{\"a\":1}"),
                Some("1")
            ]
        );
        // REAL affinity stores 3 as 3.0.
        assert_eq!(row(1)[1], Some("3.0"));
        assert_eq!(row(1)[0], None);

        let r = db
            .run("SELECT 1 + 1 AS a, 0.1 + 0.2, 'x', x'0A', NULL, 1e300, typeof(1)")
            .await;
        let cols = r.columns.unwrap();
        let types: Vec<_> = cols.iter().map(|c| c.type_name.as_str()).collect();
        assert_eq!(
            types,
            ["INTEGER", "REAL", "TEXT", "BLOB", "NULL", "REAL", "TEXT"]
        );
        assert_eq!(cols[0].name, "a");
        assert_eq!(cols[4].kind, ValueKind::Other);
        let cells: Vec<_> = r.rows[0].iter().map(|c| c.as_deref()).collect();
        assert_eq!(
            cells,
            [
                Some("2"),
                Some("0.30000000000000004"),
                Some("x"),
                Some("X'0A'"),
                None,
                Some("1.0e+300"),
                Some("integer")
            ]
        );

        let big = db.scalar("SELECT zeroblob(40000)").await.unwrap();
        assert!(big.starts_with("X'0000"));
        assert!(big.ends_with("'… (40000 bytes)"));
        assert_eq!(
            big.len(),
            2 + BINARY_PREVIEW_BYTES * 2 + "'… (40000 bytes)".len()
        );
    }

    #[tokio::test]
    async fn applies_row_limit() {
        let db = TempDb::new("limit").await;
        let sql = "WITH RECURSIVE c(x) AS (SELECT 1 UNION ALL SELECT x + 1 FROM c WHERE x < 10) SELECT x FROM c";
        let r = db.run_limit(sql, 5).await;
        assert_eq!(r.rows.len(), 5);
        assert!(r.truncated);
        assert_eq!(r.rows[4][0].as_deref(), Some("5"));
        let r = db.run_limit(sql, 10).await;
        assert_eq!(r.rows.len(), 10);
        assert!(!r.truncated);
        let r = db.run_limit(sql, 0).await;
        assert_eq!(r.rows.len(), 10);
        assert!(!r.truncated);
    }

    #[tokio::test]
    async fn reports_rows_affected() {
        let db = TempDb::new("affected").await;
        let r = db
            .run("CREATE TABLE t (id INTEGER PRIMARY KEY, v TEXT)")
            .await;
        assert_eq!(r.rows_affected, None);
        assert!(r.columns.is_none());
        let r = db
            .run("INSERT INTO t (v) VALUES ('a'), ('b'), ('c');")
            .await;
        assert_eq!(r.rows_affected, Some(3));
        let r = db.run("UPDATE t SET v = 'z' WHERE id > 1").await;
        assert_eq!(r.rows_affected, Some(2));
        // A DDL right after keeps no stale count.
        let r = db.run("CREATE INDEX t_v ON t (v)").await;
        assert_eq!(r.rows_affected, None);
        let r = db.run("DELETE FROM t").await;
        assert_eq!(r.rows_affected, Some(3));
        // INSERT ... RETURNING returns rows.
        let r = db
            .run("INSERT INTO t (v) VALUES ('r') RETURNING id, v")
            .await;
        assert_eq!(r.rows.len(), 1);
        // Comments only: nothing to do.
        let r = db.run("-- nothing\n;").await;
        assert!(r.columns.is_none());
        // Trigger bodies arrive as one statement.
        db.run("CREATE TABLE log (msg TEXT)").await;
        db.run(
            "CREATE TRIGGER t_ins AFTER INSERT ON t BEGIN\n  INSERT INTO log VALUES (new.v);\n  INSERT INTO log VALUES ('again');\nEND;",
        )
        .await;
        db.run("INSERT INTO t (v) VALUES ('x')").await;
        assert_eq!(
            db.scalar("SELECT count(*) FROM log").await.as_deref(),
            Some("2")
        );
    }

    #[tokio::test]
    async fn maps_errors() {
        let db = TempDb::new("errors").await;
        let e = db.try_run("SELEC 1", 0).await.unwrap_err();
        assert_eq!(e.kind, ErrorKind::Database);
        assert_eq!(e.code.as_deref(), Some("SQLITE_ERROR"));
        assert_eq!(e.position, Some(0));
        assert!(e.message.contains("syntax error"), "{}", e.message);

        // Character offset, not bytes: 'ção' has multibyte characters.
        let e = db.try_run("SELECT 'ção', FROM t", 0).await.unwrap_err();
        assert_eq!(e.position, Some(14));

        let e = db.try_run("SELECT * FROM missing", 0).await.unwrap_err();
        assert_eq!(e.code.as_deref(), Some("SQLITE_ERROR"));
        assert!(e.message.contains("no such table"), "{}", e.message);

        db.run("CREATE TABLE u (k TEXT UNIQUE)").await;
        db.run("INSERT INTO u VALUES ('a')").await;
        let e = db
            .try_run("INSERT INTO u VALUES ('a')", 0)
            .await
            .unwrap_err();
        assert_eq!(e.code.as_deref(), Some("SQLITE_CONSTRAINT_UNIQUE"));
        assert_eq!(e.kind, ErrorKind::Database);

        // Foreign keys are enforced.
        db.run("CREATE TABLE p (id INTEGER PRIMARY KEY)").await;
        db.run("CREATE TABLE c (p INTEGER REFERENCES p (id))").await;
        let e = db.try_run("INSERT INTO c VALUES (7)", 0).await.unwrap_err();
        assert_eq!(e.code.as_deref(), Some("SQLITE_CONSTRAINT_FOREIGNKEY"));

        let e = db.try_run("SELECT 1; SELECT 2", 0).await.unwrap_err();
        assert_eq!(e.kind, ErrorKind::Invalid);
    }

    #[tokio::test]
    async fn manual_transactions() {
        let db = TempDb::new("manual").await;
        db.run("CREATE TABLE t (v INTEGER)").await;
        let st = db.s().set_autocommit(false).await.unwrap();
        assert!(!st.autocommit && !st.in_transaction);

        let r = db.run("INSERT INTO t VALUES (1)").await;
        assert!(r.in_transaction);
        assert!(db.s().tx_state().in_transaction);
        let st = db.s().end_transaction(false).await.unwrap();
        assert!(!st.in_transaction);
        let r = db.run("SELECT count(*) FROM t").await;
        assert_eq!(r.rows[0][0].as_deref(), Some("0"));
        // Reads also open the transaction in manual mode.
        assert!(r.in_transaction);

        db.run("INSERT INTO t VALUES (2)").await;
        let st = db.s().end_transaction(true).await.unwrap();
        assert!(!st.in_transaction);
        assert_eq!(
            db.scalar("SELECT count(*) FROM t").await.as_deref(),
            Some("1")
        );
        db.s().end_transaction(true).await.unwrap();

        // A failing first statement does not leave an empty transaction.
        let e = db.try_run("SELEC 1", 0).await.unwrap_err();
        assert_eq!(e.code.as_deref(), Some("SQLITE_ERROR"));
        assert!(!db.s().tx_state().in_transaction);

        // A failing statement inside a transaction keeps earlier work.
        db.run("INSERT INTO t VALUES (3)").await;
        db.try_run("INSERT INTO nope VALUES (1)", 0)
            .await
            .unwrap_err();
        assert!(db.s().tx_state().in_transaction);

        // COMMIT typed by the user ends it.
        let r = db.run("COMMIT").await;
        assert!(!r.in_transaction);
        assert!(!db.s().tx_state().in_transaction);
        assert_eq!(
            db.scalar("SELECT count(*) FROM t").await.as_deref(),
            Some("2")
        );

        // Back to autocommit commits pending work.
        db.run("INSERT INTO t VALUES (4)").await;
        let st = db.s().set_autocommit(true).await.unwrap();
        assert!(st.autocommit && !st.in_transaction);
        assert_eq!(
            db.scalar("SELECT count(*) FROM t").await.as_deref(),
            Some("3")
        );
        assert!(!db.s().tx_state().in_transaction);
    }

    #[tokio::test]
    async fn user_typed_transaction_control() {
        let db = TempDb::new("typed").await;
        db.run("CREATE TABLE t (v INTEGER)").await;
        let r = db.run("BEGIN").await;
        assert!(r.in_transaction);
        db.run("INSERT INTO t VALUES (1)").await;
        db.run("SAVEPOINT sp").await;
        db.run("INSERT INTO t VALUES (2)").await;
        let r = db.run("ROLLBACK TO sp").await;
        assert!(r.in_transaction);
        let r = db.run("COMMIT").await;
        assert!(!r.in_transaction);
        assert_eq!(
            db.scalar("SELECT count(*) FROM t").await.as_deref(),
            Some("1")
        );

        db.run("BEGIN").await;
        db.run("DELETE FROM t").await;
        let r = db.run("ROLLBACK").await;
        assert!(!r.in_transaction);
        assert_eq!(
            db.scalar("SELECT count(*) FROM t").await.as_deref(),
            Some("1")
        );
        assert!(db.s().tx_state().autocommit);
    }

    #[tokio::test]
    async fn interrupts_long_query() {
        let mut db = TempDb::new("cancel").await;
        let session = Arc::new(db.session.take().unwrap());
        let runner = session.clone();
        let task = tokio::spawn(async move {
            runner
                .execute(&ExecRequest {
                    sql: "WITH RECURSIVE c(x) AS (SELECT 1 UNION ALL SELECT x + 1 FROM c WHERE x < 1000000000) SELECT count(*) FROM c".into(),
                    max_rows: 0,
                })
                .await
        });
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        let started = Instant::now();
        session.cancel().await.unwrap();
        let e = task.await.unwrap().unwrap_err();
        assert_eq!(e.kind, ErrorKind::Cancelled);
        assert_eq!(e.code.as_deref(), Some("SQLITE_INTERRUPT"));
        assert!(started.elapsed() < std::time::Duration::from_secs(5));
        // The session is usable afterwards.
        let r = session
            .execute(&ExecRequest {
                sql: "SELECT 1".into(),
                max_rows: 0,
            })
            .await
            .unwrap();
        assert_eq!(r.rows[0][0].as_deref(), Some("1"));
        drop(session);
    }

    #[tokio::test]
    async fn creates_file_only_when_asked() {
        let path = temp_path("create");
        let e = connect(&profile(&path, false)).await.err().unwrap();
        assert_eq!(e.kind, ErrorKind::Connection);
        assert_eq!(
            e.message,
            format!("Database file not found: {}", path.display())
        );
        assert!(!path.exists());

        let session = connect(&profile(&path, true)).await.unwrap();
        assert!(path.exists());
        drop(session);
        // Existing file opens without create_file.
        let session = connect(&profile(&path, false)).await.unwrap();
        drop(session);
        let _ = std::fs::remove_file(&path);

        // A file that is not a database fails at connect time.
        let junk = temp_path("junk");
        std::fs::write(&junk, vec![b'x'; 4096]).unwrap();
        let e = connect(&profile(&junk, false)).await.err().unwrap();
        assert_eq!(e.kind, ErrorKind::Connection);
        assert_eq!(e.code.as_deref(), Some("SQLITE_NOTADB"));
        let _ = std::fs::remove_file(&junk);
    }

    #[tokio::test]
    async fn lists_metadata() {
        let db = TempDb::new("meta").await;
        let other = temp_path("meta-other");
        db.run(
            "CREATE TABLE person (id INTEGER PRIMARY KEY AUTOINCREMENT, name TEXT NOT NULL, \
             score NUMERIC(10,2), full TEXT GENERATED ALWAYS AS (name || '!') VIRTUAL)",
        )
        .await;
        db.run("INSERT INTO person (name) VALUES ('a')").await;
        db.run("CREATE VIEW people AS SELECT name FROM person")
            .await;
        db.run("CREATE TABLE another (x)").await;
        db.run(&format!(
            "ATTACH DATABASE '{}' AS extra",
            other.display().to_string().replace('\'', "''")
        ))
        .await;
        db.run("CREATE TABLE extra.ext (k TEXT, v BLOB, PRIMARY KEY (k, v))")
            .await;
        db.run("CREATE TEMP TABLE scratch (z)").await;

        let schemas = db.s().list_schemas().await.unwrap();
        assert_eq!(schemas, ["main", "extra", "temp"]);

        let objects = db.s().list_objects("main").await.unwrap();
        let names: Vec<_> = objects
            .iter()
            .map(|o| (o.kind.as_str(), o.name.as_str()))
            .collect();
        // sqlite_sequence is internal and hidden.
        assert_eq!(
            names,
            [
                ("table", "another"),
                ("table", "person"),
                ("view", "people")
            ]
        );
        let ext = db.s().list_objects("extra").await.unwrap();
        assert_eq!(ext.len(), 1);
        assert_eq!(ext[0].name, "ext");
        let tmp = db.s().list_objects("temp").await.unwrap();
        assert_eq!(tmp[0].name, "scratch");

        let cols = db.s().list_columns("main", "person").await.unwrap();
        let shape: Vec<_> = cols
            .iter()
            .map(|c| {
                (
                    c.name.as_str(),
                    c.type_name.as_str(),
                    c.nullable,
                    c.primary_key,
                )
            })
            .collect();
        assert_eq!(
            shape,
            [
                ("id", "INTEGER", true, true),
                ("name", "TEXT", false, false),
                ("score", "NUMERIC(10,2)", true, false),
                ("full", "TEXT", true, false),
            ]
        );
        let cols = db.s().list_columns("extra", "ext").await.unwrap();
        assert!(cols.iter().all(|c| c.primary_key));
        let cols = db.s().list_columns("main", "ext").await.unwrap();
        assert!(cols.is_empty());

        db.run("DETACH DATABASE extra").await;
        let _ = std::fs::remove_file(&other);
    }
}
