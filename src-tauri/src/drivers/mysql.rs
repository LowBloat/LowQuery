//! MySQL and MariaDB session over `mysql_async` (native protocol).
//!
//! - Statements run through the text protocol: the server formats every
//!   value itself, so DECIMAL, DATETIME(6), zero dates and BIGINT UNSIGNED
//!   arrive exactly as the server prints them.
//! - Statements that return rows run with `sql_select_limit` set to the row
//!   limit + 1, so the server stops sending rows at the limit; it is restored
//!   right after.
//! - The transaction state comes from the server status flags
//!   (`SERVER_STATUS_IN_TRANS`, `SERVER_STATUS_AUTOCOMMIT`).
//! - Warnings are read with `SHOW WARNINGS` when the statement reports any.
//! - Cancel sends `KILL QUERY` over a separate short-lived connection.

use std::fmt::Write as _;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use mysql_async::consts::{ColumnFlags, ColumnType, StatusFlags};
use mysql_async::prelude::Queryable;
use mysql_async::{Column, Conn, DriverError, IoError, Opts, OptsBuilder, SslOpts, Value};
use parking_lot::Mutex;

use super::{first_keywords, TxState};
use crate::error::{AppError, AppResult, ErrorKind};
use crate::model::{
    ColumnInfo, ConnectionProfile, DbColumn, DbObject, Engine, ExecRequest, ExecResult, SslMode,
    ValueKind,
};

const CONNECT_TIMEOUT: Duration = Duration::from_secs(15);
const BINARY_PREVIEW_BYTES: usize = 32 * 1024;
const MAX_WARNINGS: usize = 100;
/// Character set number of `binary` (BLOB, BINARY, VARBINARY).
const BINARY_CHARSET: u16 = 63;
/// `ER_QUERY_INTERRUPTED`.
const QUERY_INTERRUPTED: u16 = 1317;

pub struct MySession {
    conn: tokio::sync::Mutex<Conn>,
    /// Options that opened the session, reused by `cancel`.
    opts: Opts,
    connection_id: u32,
    busy: AtomicBool,
    cancel_requested: AtomicBool,
    state: Mutex<TxState>,
    pub engine: Engine,
    pub server_version: String,
    pub current_user: Option<String>,
    pub current_schema: Option<String>,
}

pub async fn connect(profile: &ConnectionProfile, password: Option<&str>) -> AppResult<MySession> {
    // rustls has more than one crypto provider compiled in; mysql_async
    // builds its TLS config with the process default.
    let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();

    let database = profile.database.trim();
    let base = OptsBuilder::default()
        .ip_or_hostname(profile.host.trim())
        .tcp_port(if profile.port == 0 {
            3306
        } else {
            profile.port
        })
        .user(Some(profile.user.trim()))
        .pass(password)
        .db_name((!database.is_empty()).then_some(database))
        .prefer_socket(false)
        .tcp_keepalive(Some(Duration::from_secs(60)))
        .connect_attribute("program_name", "LowQuery");

    let (mut conn, opts) = match profile.ssl_mode {
        SslMode::Disable => open(base.into()).await?,
        SslMode::Prefer => match open(base.clone().ssl_opts(lenient_tls()).into()).await {
            Err(Open::Failed(e)) if tls_unavailable(&e) => open(base.into()).await?,
            other => other?,
        },
        SslMode::Require => open(base.ssl_opts(lenient_tls()).into()).await?,
        SslMode::VerifyFull => open(base.ssl_opts(strict_tls()).into()).await?,
    };

    let row: Option<(Option<String>, Option<String>, Option<String>)> = conn
        .query_first("SELECT VERSION(), CURRENT_USER(), DATABASE()")
        .await
        .map_err(connect_error)?;
    let (version, current_user, current_schema) = row.unwrap_or_default();
    let flags = status(&conn).unwrap_or(StatusFlags::SERVER_STATUS_AUTOCOMMIT);

    Ok(MySession {
        connection_id: conn.id(),
        conn: tokio::sync::Mutex::new(conn),
        opts,
        busy: AtomicBool::new(false),
        cancel_requested: AtomicBool::new(false),
        state: Mutex::new(tx_state_from(flags)),
        engine: profile.engine,
        server_version: product_version(version.as_deref().unwrap_or_default()),
        current_user,
        current_schema,
    })
}

enum Open {
    Failed(mysql_async::Error),
    TimedOut,
}

impl From<Open> for AppError {
    fn from(e: Open) -> Self {
        match e {
            Open::Failed(e) => connect_error(e),
            Open::TimedOut => AppError::connection("Timed out connecting to the server."),
        }
    }
}

async fn open(opts: Opts) -> Result<(Conn, Opts), Open> {
    match tokio::time::timeout(CONNECT_TIMEOUT, Conn::new(opts.clone())).await {
        Ok(Ok(conn)) => Ok((conn, opts)),
        Ok(Err(e)) => Err(Open::Failed(e)),
        Err(_) => Err(Open::TimedOut),
    }
}

/// `prefer` falls back to plain TCP when the server has no TLS or the
/// handshake fails, like libpq.
fn tls_unavailable(e: &mysql_async::Error) -> bool {
    matches!(
        e,
        mysql_async::Error::Driver(DriverError::NoClientSslFlagFromServer)
            | mysql_async::Error::Io(IoError::Tls(_))
    )
}

/// Encrypts without validating the certificate (`prefer`/`require`).
fn lenient_tls() -> SslOpts {
    SslOpts::default()
        .with_danger_accept_invalid_certs(true)
        .with_danger_skip_domain_validation(true)
}

/// Validates the certificate against the bundled roots and the OS store.
fn strict_tls() -> SslOpts {
    let mut store = rustls::RootCertStore::empty();
    let native: Vec<Vec<u8>> = rustls_native_certs::load_native_certs()
        .certs
        .into_iter()
        // Certificates rustls cannot parse would fail the whole connection.
        .filter(|c| store.add(c.clone()).is_ok())
        .map(|c| c.to_vec())
        .collect();
    SslOpts::default().with_root_certs(native.into_iter().map(Into::into).collect())
}

/// `MySQL 8.4.2` or `MariaDB 11.4.2`, from `VERSION()`.
pub fn product_version(version: &str) -> String {
    let v = version.trim();
    let mariadb = v.to_ascii_lowercase().contains("mariadb");
    // Old MariaDB servers prefix the version with `5.5.5-` for compatibility.
    let v = if mariadb {
        v.strip_prefix("5.5.5-").unwrap_or(v)
    } else {
        v
    };
    let number = v.split('-').next().unwrap_or(v);
    format!("{} {number}", if mariadb { "MariaDB" } else { "MySQL" })
}

fn status(conn: &Conn) -> Option<StatusFlags> {
    conn.last_ok_packet().map(|ok| ok.status_flags())
}

fn tx_state_from(flags: StatusFlags) -> TxState {
    TxState {
        autocommit: flags.contains(StatusFlags::SERVER_STATUS_AUTOCOMMIT),
        in_transaction: flags.contains(StatusFlags::SERVER_STATUS_IN_TRANS),
    }
}

/// Drops the trailing `;` (the UI may keep it) and whitespace.
fn prepare_sql(sql: &str) -> &str {
    let mut text = sql.trim_end();
    while let Some(stripped) = text.strip_suffix(';') {
        text = stripped.trim_end();
    }
    text
}

/// Statements whose top-level SELECT `sql_select_limit` applies to.
fn is_select_like(sql: &str) -> bool {
    match first_keywords(sql, 1).first().map(String::as_str) {
        Some(w) => matches!(w, "SELECT" | "WITH" | "TABLE" | "VALUES"),
        None => skip_comments(sql).starts_with('('),
    }
}

fn skip_comments(sql: &str) -> &str {
    let mut s = sql.trim_start();
    loop {
        if s.starts_with("--") || s.starts_with('#') {
            s = s.find('\n').map_or("", |i| &s[i + 1..]).trim_start();
        } else if let Some(rest) = s.strip_prefix("/*") {
            s = rest.find("*/").map_or("", |i| &rest[i + 2..]).trim_start();
        } else {
            return s;
        }
    }
}

#[derive(Default)]
struct Collected {
    columns: Option<Arc<[Column]>>,
    rows: Vec<Vec<Option<String>>>,
    truncated: bool,
    affected: Option<u64>,
    warnings: u16,
}

/// Reads every result set: the first one with columns is kept (up to
/// `max_rows`), the others are drained; affected rows are summed.
async fn read_results(
    conn: &mut Conn,
    sql: &str,
    max_rows: usize,
) -> Result<Collected, mysql_async::Error> {
    let mut out = Collected::default();
    let mut result = conn.query_iter(sql).await?;
    while let Some(columns) = result.columns() {
        if columns.is_empty() {
            *out.affected.get_or_insert(0) += result.affected_rows();
            out.warnings = out.warnings.max(result.warnings());
            result.next().await?;
            continue;
        }
        let keep = out.columns.is_none();
        if keep {
            out.columns = Some(columns.clone());
        }
        while let Some(row) = result.next().await? {
            if !keep {
                continue;
            }
            if max_rows > 0 && out.rows.len() >= max_rows {
                out.truncated = true;
                continue;
            }
            out.rows.push(
                columns
                    .iter()
                    .zip(row.unwrap())
                    .map(|(c, v)| cell(c, v))
                    .collect(),
            );
        }
        out.warnings = out.warnings.max(result.warnings());
    }
    // Surfaces an error from a later result set (inside a CALL, for example).
    result.drop_result().await?;
    Ok(out)
}

impl MySession {
    pub fn tx_state(&self) -> TxState {
        *self.state.lock()
    }

    pub async fn execute(&self, req: &ExecRequest) -> AppResult<ExecResult> {
        let mut conn = self.conn.lock().await;
        if conn.is_disconnected() {
            return Err(AppError::connection(
                "The connection to the server was closed.",
            ));
        }
        self.cancel_requested.store(false, Ordering::SeqCst);
        self.busy.store(true, Ordering::SeqCst);
        let outcome = self.run(&mut conn, req).await;
        self.busy.store(false, Ordering::SeqCst);
        self.refresh_state(&mut conn).await;
        drop(conn);

        let mut result = outcome?;
        if result.columns.is_some() && self.cancel_requested.swap(false, Ordering::SeqCst) {
            // `SELECT SLEEP(n)` interrupted by KILL QUERY returns a row
            // instead of an error.
            let mut err = AppError::new(ErrorKind::Cancelled, "Query execution was interrupted");
            err.code = Some(format!("{QUERY_INTERRUPTED} (70100)"));
            return Err(err);
        }
        result.in_transaction = self.state.lock().in_transaction;
        Ok(result)
    }

    async fn run(&self, conn: &mut Conn, req: &ExecRequest) -> AppResult<ExecResult> {
        let sql = prepare_sql(&req.sql);
        let limit = req.max_rows > 0 && is_select_like(sql);
        if limit {
            conn.query_drop(format!(
                "SET SESSION sql_select_limit = {}",
                req.max_rows.saturating_add(1)
            ))
            .await
            .map_err(map_error)?;
        }

        let started = Instant::now();
        let outcome = read_results(conn, sql, req.max_rows).await;
        let elapsed_ms = started.elapsed().as_millis() as u64;

        let notices = match &outcome {
            Ok(c) if c.warnings > 0 => read_warnings(conn).await,
            _ => Vec::new(),
        };
        if limit {
            let _ = conn
                .query_drop("SET SESSION sql_select_limit = DEFAULT")
                .await;
        }

        let collected = outcome.map_err(|e| {
            let mut err = map_error(e);
            if err.kind == ErrorKind::Database {
                err.position = error_position(sql, &err.message);
            }
            err
        })?;
        let columns = collected.columns.as_ref().map(|cols| {
            cols.iter()
                .map(|c| ColumnInfo {
                    name: c.name_str().into_owned(),
                    type_name: type_name(c.column_type(), c.flags(), c.character_set()),
                    kind: value_kind(c.column_type(), c.character_set()),
                })
                .collect()
        });
        Ok(ExecResult {
            columns,
            rows: collected.rows,
            truncated: collected.truncated,
            rows_affected: collected.affected,
            elapsed_ms,
            notices,
            in_transaction: false,
        })
    }

    /// Reads the transaction state from the last OK packet. After an error
    /// there is none, so a no-op statement fetches fresh flags.
    async fn refresh_state(&self, conn: &mut Conn) {
        if status(conn).is_none() && !conn.is_disconnected() {
            let _ = conn.query_drop("DO 0").await;
        }
        if let Some(flags) = status(conn) {
            *self.state.lock() = tx_state_from(flags);
        }
    }

    pub async fn cancel(&self) -> AppResult<()> {
        if !self.busy.load(Ordering::SeqCst) {
            return Ok(());
        }
        self.cancel_requested.store(true, Ordering::SeqCst);
        let (mut conn, _) = open(self.opts.clone()).await?;
        let killed = conn
            .query_drop(format!("KILL QUERY {}", self.connection_id))
            .await
            .map_err(map_error);
        let _ = conn.disconnect().await;
        killed
    }

    pub async fn set_autocommit(&self, on: bool) -> AppResult<TxState> {
        // Turning autocommit on commits the open transaction (server rule).
        self.control(if on {
            "SET autocommit = 1"
        } else {
            "SET autocommit = 0"
        })
        .await
    }

    pub async fn end_transaction(&self, commit: bool) -> AppResult<TxState> {
        self.control(if commit { "COMMIT" } else { "ROLLBACK" })
            .await
    }

    async fn control(&self, sql: &str) -> AppResult<TxState> {
        let mut conn = self.conn.lock().await;
        let outcome = conn.query_drop(sql).await.map_err(map_error);
        self.refresh_state(&mut conn).await;
        outcome?;
        Ok(*self.state.lock())
    }

    pub async fn list_schemas(&self) -> AppResult<Vec<String>> {
        let current = self
            .query_rows("SELECT DATABASE()", Vec::new())
            .await?
            .into_iter()
            .next()
            .and_then(|r| r.into_iter().next())
            .unwrap_or_default();
        let mut names: Vec<String> = self
            .query_rows(
                "SELECT SCHEMA_NAME FROM information_schema.SCHEMATA",
                Vec::new(),
            )
            .await?
            .into_iter()
            .filter_map(|r| r.into_iter().next())
            .collect();
        names.sort_by_cached_key(|n| {
            let rank = if *n == current {
                0
            } else if is_system_schema(n) {
                2
            } else {
                1
            };
            (rank, n.to_lowercase())
        });
        Ok(names)
    }

    pub async fn list_objects(&self, schema: &str) -> AppResult<Vec<DbObject>> {
        let rows = self
            .query_rows(
                "SELECT TABLE_NAME,
                        CASE TABLE_TYPE WHEN 'VIEW' THEN 'view' WHEN 'SYSTEM VIEW' THEN 'view'
                             WHEN 'SEQUENCE' THEN 'sequence' ELSE 'table' END
                 FROM information_schema.TABLES WHERE TABLE_SCHEMA = ?
                 UNION ALL
                 SELECT ROUTINE_NAME, CASE ROUTINE_TYPE WHEN 'FUNCTION' THEN 'function'
                                      ELSE 'procedure' END
                 FROM information_schema.ROUTINES
                 WHERE ROUTINE_SCHEMA = ? AND ROUTINE_TYPE IN ('FUNCTION', 'PROCEDURE')",
                vec![schema.to_string(), schema.to_string()],
            )
            .await?;
        let mut objects: Vec<DbObject> = rows
            .into_iter()
            .map(|mut r| DbObject {
                kind: r.pop().unwrap_or_default(),
                name: r.pop().unwrap_or_default(),
            })
            .collect();
        objects.sort_by(|a, b| (&a.kind, &a.name).cmp(&(&b.kind, &b.name)));
        Ok(objects)
    }

    pub async fn list_columns(&self, schema: &str, table: &str) -> AppResult<Vec<DbColumn>> {
        let rows = self
            .query_rows(
                "SELECT COLUMN_NAME, COLUMN_TYPE, IS_NULLABLE, COLUMN_KEY
                 FROM information_schema.COLUMNS
                 WHERE TABLE_SCHEMA = ? AND TABLE_NAME = ?
                 ORDER BY ORDINAL_POSITION",
                vec![schema.to_string(), table.to_string()],
            )
            .await?;
        Ok(rows
            .into_iter()
            .map(|r| DbColumn {
                name: r[0].clone(),
                type_name: r[1].clone(),
                nullable: r[2] == "YES",
                primary_key: r[3] == "PRI",
            })
            .collect())
    }

    /// Metadata query through a prepared statement, every value as text.
    async fn query_rows(&self, sql: &str, params: Vec<String>) -> AppResult<Vec<Vec<String>>> {
        let mut conn = self.conn.lock().await;
        let rows: Vec<mysql_async::Row> = conn.exec(sql, params).await.map_err(map_error)?;
        Ok(rows
            .into_iter()
            .map(|row| row.unwrap().into_iter().map(plain_text).collect())
            .collect())
    }
}

fn is_system_schema(name: &str) -> bool {
    matches!(
        name.to_ascii_lowercase().as_str(),
        "mysql" | "information_schema" | "performance_schema" | "sys"
    )
}

fn plain_text(v: Value) -> String {
    match v {
        Value::NULL => String::new(),
        Value::Bytes(b) => String::from_utf8_lossy(&b).into_owned(),
        other => other.as_sql(true),
    }
}

/// `Level Code: Message` lines from `SHOW WARNINGS`.
async fn read_warnings(conn: &mut Conn) -> Vec<String> {
    let rows: Vec<(String, u32, String)> = conn
        .query(format!("SHOW WARNINGS LIMIT {MAX_WARNINGS}"))
        .await
        .unwrap_or_default();
    rows.into_iter()
        .map(|(level, code, message)| format!("{level} {code}: {message}"))
        .collect()
}

fn cell(col: &Column, value: Value) -> Option<String> {
    match value {
        Value::NULL => None,
        Value::Bytes(b) => Some(format_bytes(col.column_type(), col.character_set(), &b)),
        // The text protocol only sends bytes; kept for completeness.
        other => Some(other.as_sql(true)),
    }
}

/// Formats a text-protocol value. The server already printed numbers and
/// dates (zero dates included); binary strings become `0x...` and BIT a
/// number.
pub fn format_bytes(t: ColumnType, charset: u16, bytes: &[u8]) -> String {
    use ColumnType::*;
    match t {
        MYSQL_TYPE_BIT if bytes.len() <= 8 => bytes
            .iter()
            .fold(0u64, |acc, b| (acc << 8) | u64::from(*b))
            .to_string(),
        MYSQL_TYPE_JSON => String::from_utf8_lossy(bytes).into_owned(),
        MYSQL_TYPE_VECTOR if bytes.len().is_multiple_of(4) => {
            let items: Vec<String> = bytes
                .as_chunks::<4>()
                .0
                .iter()
                .map(|c| f32::from_le_bytes(*c).to_string())
                .collect();
            format!("[{}]", items.join(","))
        }
        MYSQL_TYPE_GEOMETRY | MYSQL_TYPE_BIT | MYSQL_TYPE_VECTOR => hex(bytes),
        _ if is_string_type(t) && charset == BINARY_CHARSET => hex(bytes),
        _ => String::from_utf8_lossy(bytes).into_owned(),
    }
}

fn is_string_type(t: ColumnType) -> bool {
    use ColumnType::*;
    matches!(
        t,
        MYSQL_TYPE_VARCHAR
            | MYSQL_TYPE_VAR_STRING
            | MYSQL_TYPE_STRING
            | MYSQL_TYPE_TINY_BLOB
            | MYSQL_TYPE_MEDIUM_BLOB
            | MYSQL_TYPE_LONG_BLOB
            | MYSQL_TYPE_BLOB
    )
}

/// `0x` + uppercase hexadecimal, the way the mysql client shows binary data.
fn hex(bytes: &[u8]) -> String {
    let shown = &bytes[..bytes.len().min(BINARY_PREVIEW_BYTES)];
    let mut s = String::with_capacity(2 + shown.len() * 2);
    s.push_str("0x");
    for b in shown {
        let _ = write!(s, "{b:02X}");
    }
    if bytes.len() > shown.len() {
        let _ = write!(s, "… ({} bytes)", bytes.len());
    }
    s
}

pub fn value_kind(t: ColumnType, charset: u16) -> ValueKind {
    use ColumnType::*;
    match t {
        MYSQL_TYPE_TINY
        | MYSQL_TYPE_SHORT
        | MYSQL_TYPE_INT24
        | MYSQL_TYPE_LONG
        | MYSQL_TYPE_LONGLONG
        | MYSQL_TYPE_DECIMAL
        | MYSQL_TYPE_NEWDECIMAL
        | MYSQL_TYPE_FLOAT
        | MYSQL_TYPE_DOUBLE
        | MYSQL_TYPE_BIT => ValueKind::Number,
        MYSQL_TYPE_DATE
        | MYSQL_TYPE_NEWDATE
        | MYSQL_TYPE_TIME
        | MYSQL_TYPE_TIME2
        | MYSQL_TYPE_DATETIME
        | MYSQL_TYPE_DATETIME2
        | MYSQL_TYPE_TIMESTAMP
        | MYSQL_TYPE_TIMESTAMP2
        | MYSQL_TYPE_YEAR => ValueKind::Temporal,
        MYSQL_TYPE_JSON => ValueKind::Json,
        MYSQL_TYPE_GEOMETRY => ValueKind::Binary,
        _ if is_string_type(t) && charset == BINARY_CHARSET => ValueKind::Binary,
        _ if is_string_type(t) || matches!(t, MYSQL_TYPE_ENUM | MYSQL_TYPE_SET) => ValueKind::Text,
        _ => ValueKind::Other,
    }
}

/// Type name from the result metadata (`INT UNSIGNED`, `VARBINARY`...).
pub fn type_name(t: ColumnType, flags: ColumnFlags, charset: u16) -> String {
    use ColumnType::*;
    let binary = charset == BINARY_CHARSET;
    let name = match t {
        MYSQL_TYPE_DECIMAL | MYSQL_TYPE_NEWDECIMAL => "DECIMAL",
        MYSQL_TYPE_TINY => "TINYINT",
        MYSQL_TYPE_SHORT => "SMALLINT",
        MYSQL_TYPE_INT24 => "MEDIUMINT",
        MYSQL_TYPE_LONG => "INT",
        MYSQL_TYPE_LONGLONG => "BIGINT",
        MYSQL_TYPE_FLOAT => "FLOAT",
        MYSQL_TYPE_DOUBLE => "DOUBLE",
        MYSQL_TYPE_NULL => "NULL",
        MYSQL_TYPE_TIMESTAMP | MYSQL_TYPE_TIMESTAMP2 => "TIMESTAMP",
        MYSQL_TYPE_DATE | MYSQL_TYPE_NEWDATE => "DATE",
        MYSQL_TYPE_TIME | MYSQL_TYPE_TIME2 => "TIME",
        MYSQL_TYPE_DATETIME | MYSQL_TYPE_DATETIME2 => "DATETIME",
        MYSQL_TYPE_YEAR => "YEAR",
        MYSQL_TYPE_BIT => "BIT",
        MYSQL_TYPE_JSON => "JSON",
        MYSQL_TYPE_VECTOR => "VECTOR",
        MYSQL_TYPE_GEOMETRY => "GEOMETRY",
        MYSQL_TYPE_ENUM => "ENUM",
        MYSQL_TYPE_SET => "SET",
        MYSQL_TYPE_STRING if flags.contains(ColumnFlags::ENUM_FLAG) => "ENUM",
        MYSQL_TYPE_STRING if flags.contains(ColumnFlags::SET_FLAG) => "SET",
        MYSQL_TYPE_STRING if binary => "BINARY",
        MYSQL_TYPE_STRING => "CHAR",
        MYSQL_TYPE_VARCHAR | MYSQL_TYPE_VAR_STRING if binary => "VARBINARY",
        MYSQL_TYPE_VARCHAR | MYSQL_TYPE_VAR_STRING => "VARCHAR",
        MYSQL_TYPE_TINY_BLOB | MYSQL_TYPE_MEDIUM_BLOB | MYSQL_TYPE_LONG_BLOB | MYSQL_TYPE_BLOB
            if binary =>
        {
            "BLOB"
        }
        MYSQL_TYPE_TINY_BLOB | MYSQL_TYPE_MEDIUM_BLOB | MYSQL_TYPE_LONG_BLOB | MYSQL_TYPE_BLOB => {
            "TEXT"
        }
        _ => "UNKNOWN",
    };
    let numeric = matches!(value_kind(t, charset), ValueKind::Number) && t != MYSQL_TYPE_BIT;
    if numeric && flags.contains(ColumnFlags::UNSIGNED_FLAG) {
        format!("{name} UNSIGNED")
    } else {
        name.to_string()
    }
}

/// Position (0-based, in characters) of a syntax error, from MySQL's
/// `... near '<rest of the statement>' at line N`. The quoted text is the
/// statement from the error on, cut at 80 characters.
pub fn error_position(sql: &str, message: &str) -> Option<usize> {
    let start = message.find(" near '")? + " near '".len();
    let end = message.rfind("' at line ")?;
    if end < start {
        return None;
    }
    let snippet = &message[start..end];
    let line: usize = message[end + "' at line ".len()..]
        .chars()
        .take_while(char::is_ascii_digit)
        .collect::<String>()
        .parse()
        .ok()?;
    let byte = if snippet.is_empty() {
        // The statement ended too early.
        sql.trim_end().len()
    } else if sql.ends_with(snippet) {
        sql.len() - snippet.len()
    } else {
        let mut line_start = 0;
        for _ in 1..line {
            line_start += sql[line_start..].find('\n')? + 1;
        }
        line_start + sql[line_start..].find(snippet)?
    };
    Some(sql[..byte].chars().count())
}

/// Errors while opening a session are connection errors (access denied is
/// 1045, or 1044 for the database; both keep their code).
fn connect_error(e: mysql_async::Error) -> AppError {
    let mut err = map_error(e);
    err.kind = ErrorKind::Connection;
    err
}

pub fn map_error(e: mysql_async::Error) -> AppError {
    use mysql_async::Error as E;
    match e {
        E::Server(s) => {
            let kind = if s.code == QUERY_INTERRUPTED {
                ErrorKind::Cancelled
            } else {
                ErrorKind::Database
            };
            let mut err = AppError::new(kind, s.message.trim_end());
            err.code = Some(if s.state.is_empty() {
                s.code.to_string()
            } else {
                format!("{} ({})", s.code, s.state)
            });
            err
        }
        E::Driver(DriverError::ConnectionClosed) => {
            AppError::connection("The connection to the server was closed.")
        }
        E::Url(u) => AppError::invalid(u.to_string()),
        E::Other(o) => AppError::new(ErrorKind::Database, o.to_string()),
        other => {
            let mut message = other.to_string();
            let mut source = std::error::Error::source(&other);
            while let Some(s) = source {
                let text = s.to_string();
                if !message.contains(&text) {
                    message.push_str(": ");
                    message.push_str(&text);
                }
                source = s.source();
            }
            AppError::connection(message)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_syntax_error_position() {
        let msg = |near: &str, line: usize| {
            format!(
                "You have an error in your SQL syntax; check the manual that corresponds to your \
                 MySQL server version for the right syntax to use near '{near}' at line {line}"
            )
        };
        let sql = "SELECT *\nFROMM lq_x";
        assert_eq!(error_position(sql, &msg("FROMM lq_x", 2)), Some(9));
        // Cut snippet: located from the reported line on.
        let sql = "select 1 x,\n  2 y fromm t where a = 'é' and b = 1";
        let near = "fromm t where";
        assert_eq!(error_position(sql, &msg(near, 2)), Some(18));
        // Earlier text on the same line equal to a short suffix.
        assert_eq!(error_position("select 1 +", &msg("", 1)), Some(10));
        let sql = "select 'é', 1 1";
        assert_eq!(error_position(sql, &msg("1", 1)), Some(14));
        assert_eq!(
            error_position(sql, "Unknown column 'x' in 'field list'"),
            None
        );
        // Quotes inside the snippet.
        let sql = "selec 'a' from t";
        assert_eq!(error_position(sql, &msg("selec 'a' from t", 1)), Some(0));
    }

    #[test]
    fn formats_values() {
        use ColumnType::*;
        assert_eq!(
            format_bytes(MYSQL_TYPE_BLOB, 63, &[0, 0xff, 0x10]),
            "0x00FF10"
        );
        assert_eq!(
            format_bytes(MYSQL_TYPE_VAR_STRING, 63, b"\xca\xfe"),
            "0xCAFE"
        );
        assert_eq!(
            format_bytes(MYSQL_TYPE_VAR_STRING, 255, "ação".as_bytes()),
            "ação"
        );
        assert_eq!(format_bytes(MYSQL_TYPE_BLOB, 255, b"text"), "text");
        assert_eq!(format_bytes(MYSQL_TYPE_BIT, 63, &[0x15]), "21");
        assert_eq!(format_bytes(MYSQL_TYPE_BIT, 63, &[1, 0]), "256");
        assert_eq!(
            format_bytes(MYSQL_TYPE_JSON, 63, br#"{"a": 1}"#),
            r#"{"a": 1}"#
        );
        assert_eq!(
            format_bytes(MYSQL_TYPE_NEWDECIMAL, 63, b"12345678901234567890.123456789"),
            "12345678901234567890.123456789"
        );
        assert_eq!(
            format_bytes(MYSQL_TYPE_DATETIME, 63, b"0000-00-00 00:00:00"),
            "0000-00-00 00:00:00"
        );
        let mut v = 1.5f32.to_le_bytes().to_vec();
        v.extend_from_slice(&(-2f32).to_le_bytes());
        assert_eq!(format_bytes(MYSQL_TYPE_VECTOR, 63, &v), "[1.5,-2]");
        let big = vec![0u8; BINARY_PREVIEW_BYTES + 1];
        assert!(format_bytes(MYSQL_TYPE_BLOB, 63, &big)
            .ends_with(&format!("… ({} bytes)", BINARY_PREVIEW_BYTES + 1)));
    }

    #[test]
    fn names_and_kinds() {
        use ColumnType::*;
        let none = ColumnFlags::empty();
        assert_eq!(
            type_name(MYSQL_TYPE_LONGLONG, ColumnFlags::UNSIGNED_FLAG, 63),
            "BIGINT UNSIGNED"
        );
        assert_eq!(type_name(MYSQL_TYPE_VAR_STRING, none, 63), "VARBINARY");
        assert_eq!(type_name(MYSQL_TYPE_VAR_STRING, none, 255), "VARCHAR");
        assert_eq!(type_name(MYSQL_TYPE_BLOB, none, 63), "BLOB");
        assert_eq!(type_name(MYSQL_TYPE_BLOB, none, 45), "TEXT");
        assert_eq!(
            type_name(MYSQL_TYPE_STRING, ColumnFlags::ENUM_FLAG, 45),
            "ENUM"
        );
        assert_eq!(value_kind(MYSQL_TYPE_NEWDECIMAL, 63), ValueKind::Number);
        assert_eq!(value_kind(MYSQL_TYPE_DATETIME, 63), ValueKind::Temporal);
        assert_eq!(value_kind(MYSQL_TYPE_JSON, 63), ValueKind::Json);
        assert_eq!(value_kind(MYSQL_TYPE_BLOB, 63), ValueKind::Binary);
        assert_eq!(value_kind(MYSQL_TYPE_BLOB, 255), ValueKind::Text);
    }

    #[test]
    fn versions_and_statement_kinds() {
        assert_eq!(product_version("8.4.2"), "MySQL 8.4.2");
        assert_eq!(product_version("8.0.36-0ubuntu0.22.04.1"), "MySQL 8.0.36");
        assert_eq!(product_version("11.4.2-MariaDB-ubu2404"), "MariaDB 11.4.2");
        assert_eq!(product_version("5.5.5-10.6.12-MariaDB"), "MariaDB 10.6.12");
        assert!(is_select_like("  select 1"));
        assert!(is_select_like("WITH x AS (SELECT 1) SELECT * FROM x"));
        assert!(is_select_like(
            "-- c\n# d\n/* e */ (SELECT 1) UNION (SELECT 2)"
        ));
        assert!(!is_select_like("UPDATE t SET a = 1"));
        assert!(!is_select_like("CALL p()"));
        assert_eq!(prepare_sql("select 1 ;\n "), "select 1");
    }

    // Integration tests: need a server. Variables: LQ_MYSQL_HOST,
    // LQ_MYSQL_PORT, LQ_MYSQL_USER, LQ_MYSQL_PASSWORD, LQ_MYSQL_DB and
    // LQ_MYSQL_ENGINE ("mysql" or "mariadb").

    fn env(name: &str) -> Option<String> {
        std::env::var(name).ok().filter(|v| !v.is_empty())
    }

    fn profile() -> Option<(ConnectionProfile, Option<String>)> {
        let host = env("LQ_MYSQL_HOST")?;
        let engine = if env("LQ_MYSQL_ENGINE").as_deref() == Some("mariadb") {
            Engine::Mariadb
        } else {
            Engine::Mysql
        };
        Some((
            ConnectionProfile {
                name: "test".into(),
                engine,
                host,
                port: env("LQ_MYSQL_PORT")
                    .and_then(|p| p.parse().ok())
                    .unwrap_or(3306),
                user: env("LQ_MYSQL_USER").unwrap_or_else(|| "root".into()),
                database: env("LQ_MYSQL_DB").unwrap_or_else(|| "lowquery".into()),
                ssl_mode: SslMode::Prefer,
                ..Default::default()
            },
            env("LQ_MYSQL_PASSWORD"),
        ))
    }

    async fn session() -> Option<MySession> {
        let (profile, password) = profile()?;
        Some(
            connect(&profile, password.as_deref())
                .await
                .expect("connect"),
        )
    }

    fn req(sql: &str, max_rows: usize) -> ExecRequest {
        ExecRequest {
            sql: sql.into(),
            max_rows,
        }
    }

    async fn run(s: &MySession, sql: &str) -> ExecResult {
        s.execute(&req(sql, 100))
            .await
            .unwrap_or_else(|e| panic!("{sql}: {e}"))
    }

    #[tokio::test]
    #[ignore]
    async fn connects_and_reports_version() {
        let Some(s) = session().await else { return };
        let product = if s.engine == Engine::Mariadb {
            "MariaDB "
        } else {
            "MySQL "
        };
        assert!(
            s.server_version.starts_with(product),
            "{}",
            s.server_version
        );
        assert!(s.current_user.is_some());
        let (mut profile, _) = profile().unwrap();
        assert_eq!(s.current_schema.as_deref(), Some(profile.database.as_str()));
        assert!(s.tx_state().autocommit);
        assert!(!s.tx_state().in_transaction);

        profile.ssl_mode = SslMode::Require;
        let e = match connect(&profile, Some("wrong password")).await {
            Ok(_) => panic!("connected with a wrong password"),
            Err(e) => e,
        };
        assert_eq!(e.kind, ErrorKind::Connection);
        assert_eq!(e.code.as_deref(), Some("1045 (28000)"));
    }

    #[tokio::test]
    #[ignore]
    async fn formats_values_exactly() {
        let Some(s) = session().await else { return };
        run(&s, "SET SESSION sql_mode = ''").await;
        run(
            &s,
            "CREATE TEMPORARY TABLE lq_types (d DECIMAL(30,9), u BIGINT UNSIGNED, dt DATETIME(6),
             j JSON, b BLOB, vb VARBINARY(4), n INT, bt BIT(5), e ENUM('x','y'), f DOUBLE,
             z DATETIME, t VARCHAR(10))",
        )
        .await;
        let r = run(
            &s,
            "INSERT INTO lq_types VALUES (12345678901234567890.123456789, 18446744073709551615,
             '2024-05-01 13:45:00.123456', '{\"a\": 1}', x'00FF10', x'CAFE', NULL, b'10101', 'y',
             1.5, '0000-00-00 00:00:00', 'ação')",
        )
        .await;
        assert_eq!(r.rows_affected, Some(1));
        let r = run(&s, "SELECT * FROM lq_types").await;
        let row: Vec<Option<&str>> = r.rows[0].iter().map(|c| c.as_deref()).collect();
        assert_eq!(
            row,
            [
                Some("12345678901234567890.123456789"),
                Some("18446744073709551615"),
                Some("2024-05-01 13:45:00.123456"),
                Some("{\"a\": 1}"),
                Some("0x00FF10"),
                Some("0xCAFE"),
                None,
                Some("21"),
                Some("y"),
                Some("1.5"),
                Some("0000-00-00 00:00:00"),
                Some("ação"),
            ]
        );
        let cols = r.columns.unwrap();
        assert_eq!(cols[0].kind, ValueKind::Number);
        assert_eq!(cols[1].type_name, "BIGINT UNSIGNED");
        assert_eq!(cols[2].kind, ValueKind::Temporal);
        if s.engine == Engine::Mysql {
            assert_eq!(cols[3].kind, ValueKind::Json);
        }
        assert_eq!(cols[4].kind, ValueKind::Binary);
        assert_eq!(cols[11].kind, ValueKind::Text);
    }

    #[tokio::test]
    #[ignore]
    async fn limits_rows_on_the_server() {
        let Some(s) = session().await else { return };
        run(&s, "CREATE TEMPORARY TABLE lq_limit (n INT)").await;
        let values: Vec<String> = (1..=50).map(|i| format!("({i})")).collect();
        run(
            &s,
            &format!("INSERT INTO lq_limit VALUES {}", values.join(",")),
        )
        .await;

        let r = s
            .execute(&req("SELECT n FROM lq_limit ORDER BY n", 10))
            .await
            .unwrap();
        assert_eq!(r.rows.len(), 10);
        assert!(r.truncated);
        let r = s.execute(&req("SELECT n FROM lq_limit", 50)).await.unwrap();
        assert_eq!(r.rows.len(), 50);
        assert!(!r.truncated);

        // The limit does not leak into the next statements.
        let r = s
            .execute(&req("UPDATE lq_limit SET n = n + 1", 10))
            .await
            .unwrap();
        assert_eq!(r.rows_affected, Some(50));
        let r = s.execute(&req("SELECT n FROM lq_limit", 0)).await.unwrap();
        assert_eq!(r.rows.len(), 50);
        let r = run(&s, "SHOW VARIABLES LIKE 'sql_select_limit'").await;
        assert_eq!(r.rows[0][1].as_deref(), Some("18446744073709551615"));
    }

    #[tokio::test]
    #[ignore]
    async fn reports_errors_with_position() {
        let Some(s) = session().await else { return };
        let e = s
            .execute(&req("SELECT *\nFROMM lq_x", 10))
            .await
            .unwrap_err();
        assert_eq!(e.kind, ErrorKind::Database);
        assert_eq!(e.code.as_deref(), Some("1064 (42000)"));
        assert_eq!(e.position, Some(9));
        let e = s
            .execute(&req("SELECT * FROM lq_no_such_table", 10))
            .await
            .unwrap_err();
        assert_eq!(e.code.as_deref(), Some("1146 (42S02)"));
        // The session still works.
        assert_eq!(run(&s, "SELECT 1").await.rows[0][0].as_deref(), Some("1"));
    }

    #[tokio::test]
    #[ignore]
    async fn collects_warnings() {
        let Some(s) = session().await else { return };
        let r = run(&s, "SELECT CAST('abc' AS SIGNED)").await;
        assert!(
            r.notices.iter().any(|n| n.starts_with("Warning 1292:")),
            "{:?}",
            r.notices
        );
        let r = run(&s, "SELECT 1").await;
        assert!(r.notices.is_empty());
    }

    #[tokio::test]
    #[ignore]
    async fn manual_transactions() {
        let Some(s) = session().await else { return };
        run(&s, "DROP TABLE IF EXISTS lq_tx").await;
        run(&s, "CREATE TABLE lq_tx (n INT) ENGINE = InnoDB").await;

        let st = s.set_autocommit(false).await.unwrap();
        assert!(!st.autocommit);
        let r = run(&s, "INSERT INTO lq_tx VALUES (1)").await;
        assert!(r.in_transaction);
        let st = s.end_transaction(false).await.unwrap();
        assert!(!st.in_transaction);
        assert_eq!(
            run(&s, "SELECT COUNT(*) FROM lq_tx").await.rows[0][0].as_deref(),
            Some("0")
        );
        run(&s, "INSERT INTO lq_tx VALUES (2)").await;
        let st = s.end_transaction(true).await.unwrap();
        assert!(!st.in_transaction);
        run(&s, "INSERT INTO lq_tx VALUES (3)").await;
        // Turning autocommit back on commits pending work.
        let st = s.set_autocommit(true).await.unwrap();
        assert!(st.autocommit && !st.in_transaction);
        assert_eq!(
            run(&s, "SELECT COUNT(*) FROM lq_tx").await.rows[0][0].as_deref(),
            Some("2")
        );

        // Explicit transaction in autocommit mode.
        assert!(run(&s, "START TRANSACTION").await.in_transaction);
        assert!(run(&s, "DELETE FROM lq_tx").await.in_transaction);
        assert!(!run(&s, "ROLLBACK").await.in_transaction);
        assert_eq!(
            run(&s, "SELECT COUNT(*) FROM lq_tx").await.rows[0][0].as_deref(),
            Some("2")
        );
        run(&s, "DROP TABLE lq_tx").await;
    }

    #[tokio::test]
    #[ignore]
    async fn cancels_a_running_query() {
        let Some(s) = session().await else { return };
        let s = Arc::new(s);
        let started = Instant::now();
        let running = {
            let s = s.clone();
            tokio::spawn(async move { s.execute(&req("SELECT SLEEP(30)", 10)).await })
        };
        tokio::time::sleep(Duration::from_secs(1)).await;
        s.cancel().await.unwrap();
        let e = running.await.unwrap().unwrap_err();
        assert_eq!(e.kind, ErrorKind::Cancelled);
        assert!(started.elapsed() < Duration::from_secs(15));
        assert_eq!(run(&s, "SELECT 2").await.rows[0][0].as_deref(), Some("2"));
    }

    #[tokio::test]
    #[ignore]
    async fn lists_metadata() {
        let Some(s) = session().await else { return };
        let db = s.current_schema.clone().unwrap();
        for sql in [
            "DROP VIEW IF EXISTS lq_meta_v",
            "DROP TABLE IF EXISTS lq_meta_t",
            "DROP PROCEDURE IF EXISTS lq_meta_p",
            "DROP FUNCTION IF EXISTS lq_meta_f",
            "CREATE TABLE lq_meta_t (id INT NOT NULL PRIMARY KEY, name VARCHAR(20) NULL)",
            "CREATE VIEW lq_meta_v AS SELECT id FROM lq_meta_t",
            "CREATE PROCEDURE lq_meta_p() SELECT 1",
            "CREATE FUNCTION lq_meta_f() RETURNS INT DETERMINISTIC RETURN 1",
        ] {
            run(&s, sql).await;
        }
        if s.engine == Engine::Mariadb {
            run(&s, "CREATE OR REPLACE SEQUENCE lq_meta_s").await;
        }

        let schemas = s.list_schemas().await.unwrap();
        assert_eq!(schemas[0], db);
        let first_system = schemas.iter().position(|n| is_system_schema(n)).unwrap();
        assert!(schemas[first_system..].iter().all(|n| is_system_schema(n)));

        let objects = s.list_objects(&db).await.unwrap();
        let kind = |name: &str| {
            objects
                .iter()
                .find(|o| o.name == name)
                .map(|o| o.kind.as_str())
        };
        assert_eq!(kind("lq_meta_t"), Some("table"));
        assert_eq!(kind("lq_meta_v"), Some("view"));
        assert_eq!(kind("lq_meta_p"), Some("procedure"));
        assert_eq!(kind("lq_meta_f"), Some("function"));
        if s.engine == Engine::Mariadb {
            assert_eq!(kind("lq_meta_s"), Some("sequence"));
        }

        let cols = s.list_columns(&db, "lq_meta_t").await.unwrap();
        assert_eq!(cols.len(), 2);
        assert_eq!(cols[0].name, "id");
        // MariaDB keeps the display width: int(11).
        assert!(
            cols[0].type_name.starts_with("int"),
            "{}",
            cols[0].type_name
        );
        assert!(cols[0].primary_key && !cols[0].nullable);
        assert_eq!(cols[1].type_name, "varchar(20)");
        assert!(!cols[1].primary_key && cols[1].nullable);

        // CALL: rows of the first result set plus the affected rows.
        let r = run(&s, "CALL lq_meta_p()").await;
        assert_eq!(r.rows[0][0].as_deref(), Some("1"));
        assert!(r.rows_affected.is_some());
    }
}
