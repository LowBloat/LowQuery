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
//! - Grid editing: the result column metadata names the schema, table and
//!   column behind each value (empty for expressions); the table type, column
//!   types and unique keys come from `information_schema` in one round trip
//!   and are cached per session. Changes run as prepared statements with text
//!   parameters, which the server converts to the column type.

use std::collections::HashMap;
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
    ApplyRequest, ApplyResult, CellValue, ColumnInfo, ConnectionProfile, DbColumn, DbObject,
    EditColumn, EditTarget, Engine, ExecRequest, ExecResult, RowChange, SslMode, ValueKind,
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
    /// Catalog facts per (schema, table) for grid editing; cleared by DDL.
    catalog: Mutex<HashMap<(String, String), Arc<TableInfo>>>,
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
        catalog: Mutex::new(HashMap::new()),
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
    /// Applies grid edits. With autocommit on they run in their own
    /// transaction; otherwise they join the session's transaction behind a
    /// savepoint, so a failure undoes only them and the transaction stays
    /// open.
    pub async fn apply_changes(&self, req: &ApplyRequest) -> AppResult<ApplyResult> {
        // Bad input is rejected before anything runs.
        let statements = req
            .changes
            .iter()
            .enumerate()
            .map(|(i, c)| build_change(&req.target, self.engine, c).map_err(|e| at_row(e, i)))
            .collect::<AppResult<Vec<_>>>()?;

        let mut conn = self.conn.lock().await;
        if conn.is_disconnected() {
            return Err(AppError::connection(
                "The connection to the server was closed.",
            ));
        }
        let state = *self.state.lock();
        let own = state.autocommit && !state.in_transaction;
        let started = Instant::now();
        self.cancel_requested.store(false, Ordering::SeqCst);
        self.busy.store(true, Ordering::SeqCst);
        let outcome = apply_in_transaction(&mut conn, req, self.engine, statements, own).await;
        self.busy.store(false, Ordering::SeqCst);
        self.refresh_state(&mut conn).await;
        drop(conn);

        let mut result = outcome?;
        result.elapsed_ms = started.elapsed().as_millis() as u64;
        result.in_transaction = self.state.lock().in_transaction;
        Ok(result)
    }

    /// Editable target of a row-returning result, or why it is read-only.
    /// Never fails the query: catalog errors end in a read-only reason.
    async fn detect_target(
        &self,
        conn: &mut Conn,
        sql: &str,
        columns: &[Column],
    ) -> Result<EditTarget, String> {
        if !may_be_editable(sql) {
            return Err(NOT_A_TABLE.into());
        }
        let origins: Vec<Option<Origin>> = columns.iter().map(origin).collect();
        let (schema, table) = single_table(&origins)?;
        let info = self
            .table_info(conn, &schema, &table)
            .await
            .map_err(|_| UNKNOWN_TABLE.to_string())?;
        edit_target(schema, table, &info, &origins)
    }

    /// Table type, columns and unique keys, cached per session.
    async fn table_info(
        &self,
        conn: &mut Conn,
        schema: &str,
        table: &str,
    ) -> Result<Arc<TableInfo>, mysql_async::Error> {
        let key = (schema.to_string(), table.to_string());
        if let Some(info) = self.catalog.lock().get(&key) {
            return Ok(info.clone());
        }
        let mut result = conn.query_iter(catalog_sql(schema, table)).await?;
        let tables: Vec<mysql_async::Row> = result.collect().await?;
        let columns: Vec<mysql_async::Row> = result.collect().await?;
        let keys: Vec<mysql_async::Row> = result.collect().await?;
        result.drop_result().await?;
        let info = Arc::new(TableInfo::from_rows(
            &texts(tables),
            &texts(columns),
            &texts(keys),
        ));
        self.catalog.lock().insert(key, info.clone());
        Ok(info)
    }

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
        if is_ddl(sql) {
            self.catalog.lock().clear();
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
        let (editable, read_only_reason) = match &collected.columns {
            Some(_) if self.cancel_requested.load(Ordering::SeqCst) => (None, None),
            Some(cols) => match self.detect_target(conn, sql, cols).await {
                Ok(target) => (Some(target), None),
                Err(reason) => (None, Some(reason)),
            },
            None => (None, None),
        };
        Ok(ExecResult {
            columns,
            rows: collected.rows,
            truncated: collected.truncated,
            rows_affected: collected.affected,
            elapsed_ms,
            notices,
            in_transaction: false,
            editable,
            read_only_reason,
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
                "SELECT COLUMN_NAME, COLUMN_TYPE, IS_NULLABLE, COLUMN_KEY,
                        COLUMN_DEFAULT IS NULL, COLUMN_DEFAULT, EXTRA
                 FROM information_schema.COLUMNS
                 WHERE TABLE_SCHEMA = ? AND TABLE_NAME = ?
                 ORDER BY ORDINAL_POSITION",
                vec![schema.to_string(), table.to_string()],
            )
            .await?;
        let mariadb = self.engine == Engine::Mariadb;
        Ok(rows
            .into_iter()
            .map(|r| DbColumn {
                name: r[0].clone(),
                type_name: r[1].clone(),
                nullable: r[2] == "YES",
                primary_key: r[3] == "PRI",
                default_value: (r[4] == "0")
                    .then(|| default_expr(&r[5], &r[6], mariadb))
                    .flatten(),
            })
            .collect())
    }

    /// `SHOW CREATE` output, with routines and triggers between `DELIMITER`
    /// lines so the script runs as written.
    pub async fn object_ddl(&self, schema: &str, name: &str, kind: &str) -> AppResult<String> {
        let target = format!("{}.{}", quote_ident(schema), quote_ident(name));
        let (what, column) = match kind {
            "table" => ("TABLE", 1),
            "view" => ("VIEW", 1),
            "sequence" => ("SEQUENCE", 1),
            "function" => ("FUNCTION", 2),
            "procedure" => ("PROCEDURE", 2),
            _ => {
                return Err(AppError::invalid(format!(
                    "LowQuery cannot script a {kind} on this server."
                )))
            }
        };
        let def = self
            .show_create(&format!("SHOW CREATE {what} {target}"), column)
            .await?;
        if def.trim().is_empty() {
            return Ok(def);
        }
        let mut out = if matches!(kind, "function" | "procedure") {
            format!("DELIMITER $$\n{def}$$\nDELIMITER ;\n")
        } else {
            format!("{def};\n")
        };
        if kind == "table" {
            let triggers = self
                .query_rows(
                    "SELECT TRIGGER_NAME FROM information_schema.TRIGGERS
                     WHERE EVENT_OBJECT_SCHEMA = ? AND EVENT_OBJECT_TABLE = ?
                     ORDER BY ACTION_ORDER, TRIGGER_NAME",
                    vec![schema.to_string(), name.to_string()],
                )
                .await?;
            if !triggers.is_empty() {
                out.push_str("\nDELIMITER $$\n");
                for t in triggers {
                    let sql = format!(
                        "SHOW CREATE TRIGGER {}.{}",
                        quote_ident(schema),
                        quote_ident(&t[0])
                    );
                    out.push_str(&format!("{}$$\n", self.show_create(&sql, 2).await?));
                }
                out.push_str("DELIMITER ;\n");
            }
        }
        Ok(out)
    }

    /// One column of a `SHOW CREATE ...` row (text protocol: not every
    /// server prepares SHOW statements).
    async fn show_create(&self, sql: &str, column: usize) -> AppResult<String> {
        let mut conn = self.conn.lock().await;
        let rows: Vec<mysql_async::Row> = conn.query(sql).await.map_err(map_error)?;
        Ok(texts(rows)
            .into_iter()
            .next()
            .and_then(|r| r.into_iter().nth(column).flatten())
            .unwrap_or_default())
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

/// `COLUMN_DEFAULT` as an SQL expression. MariaDB already writes literals
/// quoted (and a NULL default as `NULL`); MySQL writes the bare value and marks
/// expressions with `DEFAULT_GENERATED`.
fn default_expr(value: &str, extra: &str, mariadb: bool) -> Option<String> {
    if mariadb {
        return (!value.eq_ignore_ascii_case("NULL")).then(|| value.to_string());
    }
    if extra.to_ascii_uppercase().contains("DEFAULT_GENERATED") {
        let lower = value.to_ascii_lowercase();
        let bare = ["current_timestamp", "now(", "localtime", "current_date"]
            .iter()
            .any(|p| lower.starts_with(p));
        return Some(if bare {
            value.to_string()
        } else {
            format!("({value})")
        });
    }
    if value.starts_with("b'") {
        return Some(value.to_string());
    }
    Some(format!(
        "'{}'",
        value.replace('\\', "\\\\").replace('\'', "''")
    ))
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

const NOT_A_TABLE: &str = "Read-only: only rows from a table can be edited.";
const SEVERAL_TABLES: &str = "Read-only: the result combines columns from several tables.";
const UNKNOWN_TABLE: &str = "Read-only: the table behind this result could not be read.";
const PREVIEW: &str = "This value is only a preview and cannot be saved.";
/// Savepoint that bounds grid edits inside the user's transaction.
const SAVEPOINT: &str = "lq_apply";

/// Statements that change the structure the editing cache describes.
fn is_ddl(sql: &str) -> bool {
    matches!(
        first_keywords(sql, 1).first().map(String::as_str),
        Some("CREATE" | "ALTER" | "DROP" | "RENAME")
    )
}

/// Plain queries whose metadata can name a single source table.
fn may_be_editable(sql: &str) -> bool {
    is_select_like(sql)
        && first_keywords(sql, 1).first().map(String::as_str) != Some("VALUES")
        && !has_set_operation(sql)
}

/// Whether the statement combines queries (`UNION`, `INTERSECT`, `EXCEPT`):
/// the column metadata does not say which query each row came from.
fn has_set_operation(sql: &str) -> bool {
    let b = sql.as_bytes();
    let mut i = 0;
    while i < b.len() {
        let c = b[i];
        match c {
            b'\'' | b'"' | b'`' => {
                i += 1;
                while i < b.len() && b[i] != c {
                    if b[i] == b'\\' && c != b'`' {
                        i += 1;
                    }
                    i += 1;
                }
                i += 1;
            }
            b'#' => {
                while i < b.len() && b[i] != b'\n' {
                    i += 1;
                }
            }
            b'-' if b.get(i + 1) == Some(&b'-')
                && b.get(i + 2).is_none_or(|n| n.is_ascii_whitespace()) =>
            {
                while i < b.len() && b[i] != b'\n' {
                    i += 1;
                }
            }
            b'/' if b.get(i + 1) == Some(&b'*') => {
                i += 2;
                while i < b.len() && !(b[i] == b'*' && b.get(i + 1) == Some(&b'/')) {
                    i += 1;
                }
                i += 2;
            }
            _ if c.is_ascii_alphanumeric() || c == b'_' || c == b'$' => {
                let start = i;
                while i < b.len() && (b[i].is_ascii_alphanumeric() || b[i] == b'_' || b[i] == b'$')
                {
                    i += 1;
                }
                let word = &sql[start..i];
                if ["UNION", "INTERSECT", "EXCEPT"]
                    .iter()
                    .any(|k| word.eq_ignore_ascii_case(k))
                {
                    return true;
                }
            }
            _ => i += 1,
        }
    }
    false
}

/// Where a result column comes from, as the protocol reports it.
#[derive(Debug, Clone, PartialEq)]
struct Origin {
    schema: String,
    /// Table alias in the query (tells a self-join apart).
    alias: String,
    table: String,
    column: String,
}

/// `None` for expressions, aggregates and literals (no original table).
fn origin(c: &Column) -> Option<Origin> {
    if c.org_table_ref().is_empty() || c.org_name_ref().is_empty() {
        return None;
    }
    Some(Origin {
        schema: c.schema_str().into_owned(),
        alias: c.table_str().into_owned(),
        table: c.org_table_str().into_owned(),
        column: c.org_name_str().into_owned(),
    })
}

/// The one (schema, table) every base column of the result comes from.
fn single_table(origins: &[Option<Origin>]) -> Result<(String, String), String> {
    let mut found: Option<&Origin> = None;
    for o in origins.iter().flatten() {
        match found {
            None => found = Some(o),
            Some(f) if f.schema == o.schema && f.table == o.table && f.alias == o.alias => {}
            Some(_) => return Err(SEVERAL_TABLES.into()),
        }
    }
    match found {
        // Derived and temporary result tables have no schema.
        Some(f) if !f.schema.is_empty() => Ok((f.schema.clone(), f.table.clone())),
        _ => Err(NOT_A_TABLE.into()),
    }
}

/// Catalog facts about one table.
#[derive(Debug, Default)]
struct TableInfo {
    /// `BASE TABLE`, `VIEW`...; empty when the table was not found.
    table_type: String,
    columns: Vec<TableColumn>,
    /// Unique indexes (`PRIMARY` included) with their columns in key order;
    /// `None` is an expression key part.
    keys: Vec<(String, Vec<Option<String>>)>,
}

#[derive(Debug)]
struct TableColumn {
    name: String,
    /// `COLUMN_TYPE`, as in DDL: `int unsigned`, `varbinary(16)`, `bit(5)`.
    column_type: String,
    nullable: bool,
    generated: bool,
}

impl TableInfo {
    /// From the three result sets of [`catalog_sql`].
    fn from_rows(
        tables: &[Vec<Option<String>>],
        columns: &[Vec<Option<String>>],
        keys: &[Vec<Option<String>>],
    ) -> Self {
        let text = |r: &[Option<String>], i: usize| r.get(i).cloned().flatten().unwrap_or_default();
        let mut info = TableInfo {
            table_type: tables.first().map(|r| text(r, 0)).unwrap_or_default(),
            ..Default::default()
        };
        for r in columns {
            let extra = text(r, 3).to_ascii_uppercase();
            info.columns.push(TableColumn {
                name: text(r, 0),
                column_type: text(r, 1),
                nullable: text(r, 2) == "YES",
                generated: [
                    "VIRTUAL GENERATED",
                    "STORED GENERATED",
                    "PERSISTENT GENERATED",
                ]
                .iter()
                .any(|g| extra.contains(g)),
            });
        }
        for r in keys {
            let name = text(r, 0);
            let part = r.get(1).cloned().flatten();
            match info.keys.last_mut() {
                Some((last, parts)) if *last == name => parts.push(part),
                _ => info.keys.push((name, vec![part])),
            }
        }
        info
    }

    fn column(&self, name: &str) -> Option<&TableColumn> {
        self.columns.iter().find(|c| same_column(&c.name, name))
    }
}

/// Column names are case-insensitive in MySQL and MariaDB.
fn same_column(a: &str, b: &str) -> bool {
    a == b || a.to_lowercase() == b.to_lowercase()
}

/// Three statements in one round trip: table type, columns, unique keys.
fn catalog_sql(schema: &str, table: &str) -> String {
    let filter = format!(
        "TABLE_SCHEMA = {} AND TABLE_NAME = {}",
        string_literal(schema),
        string_literal(table)
    );
    format!(
        "SELECT TABLE_TYPE FROM information_schema.TABLES WHERE {filter};\n\
         SELECT COLUMN_NAME, COLUMN_TYPE, IS_NULLABLE, EXTRA FROM information_schema.COLUMNS \
         WHERE {filter} ORDER BY ORDINAL_POSITION;\n\
         SELECT INDEX_NAME, COLUMN_NAME FROM information_schema.STATISTICS \
         WHERE {filter} AND NON_UNIQUE = 0 ORDER BY INDEX_NAME, SEQ_IN_INDEX"
    )
}

/// String literal that reads the same whatever `sql_mode` says about
/// backslashes: hexadecimal with a character set introducer.
fn string_literal(s: &str) -> String {
    let mut out = String::with_capacity(12 + s.len() * 2);
    out.push_str("_utf8mb4 X'");
    for b in s.as_bytes() {
        let _ = write!(out, "{b:02X}");
    }
    out.push('\'');
    out
}

fn texts(rows: Vec<mysql_async::Row>) -> Vec<Vec<Option<String>>> {
    rows.into_iter()
        .map(|row| {
            row.unwrap()
                .into_iter()
                .map(|v| match v {
                    Value::NULL => None,
                    v => Some(plain_text(v)),
                })
                .collect()
        })
        .collect()
}

/// The edit target once the source table is known: its primary key (or,
/// without one, a unique key on NOT NULL columns) must be in the result.
fn edit_target(
    schema: String,
    table: String,
    info: &TableInfo,
    origins: &[Option<Origin>],
) -> Result<EditTarget, String> {
    // `SYSTEM VERSIONED`: MariaDB tables that keep row history.
    if !matches!(info.table_type.as_str(), "BASE TABLE" | "SYSTEM VERSIONED") {
        return Err(NOT_A_TABLE.into());
    }
    let primary = info.keys.iter().find(|(name, _)| name == "PRIMARY");
    let candidates: Vec<&Vec<Option<String>>> = match primary {
        Some((_, parts)) => vec![parts],
        None => info
            .keys
            .iter()
            .map(|(_, parts)| parts)
            .filter(|parts| {
                parts.iter().all(|p| {
                    p.as_deref()
                        .and_then(|name| info.column(name))
                        .is_some_and(|c| !c.nullable)
                })
            })
            .collect(),
    };
    let Some(first) = candidates.first() else {
        return Err(format!("Read-only: table {table} has no primary key."));
    };
    let position = |name: &str| {
        origins
            .iter()
            .position(|o| o.as_ref().is_some_and(|o| same_column(&o.column, name)))
    };
    let key = candidates.iter().find_map(|parts| {
        parts
            .iter()
            .map(|p| p.as_deref().and_then(position))
            .collect::<Option<Vec<usize>>>()
    });
    let Some(key) = key else {
        let names: Vec<&str> = first.iter().flatten().map(String::as_str).collect();
        let what = if primary.is_some() {
            "primary key"
        } else {
            "key"
        };
        return Err(format!(
            "Read-only: add the {what} columns ({}) to the query to edit rows.",
            names.join(", ")
        ));
    };
    let columns = origins
        .iter()
        .enumerate()
        .map(|(i, o)| {
            let c = info.column(&o.as_ref()?.column)?;
            // Generated columns cannot be written; key columns still need
            // their name for the WHERE clause.
            if c.generated && !key.contains(&i) {
                return None;
            }
            Some(EditColumn {
                name: c.name.clone(),
                type_name: c.column_type.clone(),
            })
        })
        .collect();
    Ok(EditTarget {
        schema: Some(schema),
        table,
        columns,
        key,
    })
}

/// How a grid value is sent for a column type (`COLUMN_TYPE`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Conversion {
    /// Text parameter; the server converts it to the column type.
    Text,
    /// TINYINT(1)/BOOLEAN: also accepts `true` and `false`.
    Bool,
    /// Binary strings and geometry, shown as `0x...`: sent as hex to UNHEX().
    Hex,
    /// BIT, shown as a number: sent as an integer.
    Bit,
    /// VECTOR, shown as `[1.5,-2]`.
    Vector,
}

fn conversion(column_type: &str) -> Conversion {
    let t = column_type.trim().to_ascii_lowercase();
    let base = t.split(['(', ' ']).next().unwrap_or_default();
    match base {
        "binary" | "varbinary" | "tinyblob" | "blob" | "mediumblob" | "longblob" | "geometry"
        | "point" | "linestring" | "polygon" | "multipoint" | "multilinestring"
        | "multipolygon" | "geometrycollection" | "geomcollection" => Conversion::Hex,
        "bit" => Conversion::Bit,
        "vector" => Conversion::Vector,
        "bool" | "boolean" => Conversion::Bool,
        "tinyint" if t.starts_with("tinyint(1)") => Conversion::Bool,
        _ => Conversion::Text,
    }
}

/// SQL for one bound value of the given conversion.
fn placeholder(conv: Conversion, engine: Engine) -> &'static str {
    match conv {
        Conversion::Hex => "UNHEX(?)",
        Conversion::Vector if engine == Engine::Mariadb => "VEC_FromText(?)",
        Conversion::Vector => "STRING_TO_VECTOR(?)",
        _ => "?",
    }
}

/// Parameter for a grid value shown by [`format_bytes`].
fn bind_value(conv: Conversion, text: &str) -> AppResult<Value> {
    match conv {
        Conversion::Text | Conversion::Vector => Ok(Value::Bytes(text.as_bytes().to_vec())),
        Conversion::Bool => {
            let t = text.trim();
            let v = if t.eq_ignore_ascii_case("true") {
                "1"
            } else if t.eq_ignore_ascii_case("false") {
                "0"
            } else {
                text
            };
            Ok(Value::Bytes(v.as_bytes().to_vec()))
        }
        Conversion::Hex => {
            if text.contains("… (") {
                return Err(AppError::invalid(PREVIEW));
            }
            let t = text.trim();
            let digits = t
                .strip_prefix("0x")
                .or_else(|| t.strip_prefix("0X"))
                .unwrap_or(t);
            if !digits.len().is_multiple_of(2) || !digits.bytes().all(|b| b.is_ascii_hexdigit()) {
                return Err(AppError::invalid(
                    "Binary values are written in hexadecimal, like 0x00FF.",
                ));
            }
            Ok(Value::Bytes(digits.as_bytes().to_vec()))
        }
        Conversion::Bit => parse_bit(text).map(Value::UInt).ok_or_else(|| {
            AppError::invalid("BIT values are written as a number, like 5, or as 0x05 or b'101'.")
        }),
    }
}

/// `21`, `0x15` or `b'10101'`.
fn parse_bit(text: &str) -> Option<u64> {
    let t = text.trim();
    if let Some(h) = t.strip_prefix("0x").or_else(|| t.strip_prefix("0X")) {
        return (!h.is_empty() && h.len() <= 16)
            .then(|| u64::from_str_radix(h, 16).ok())
            .flatten();
    }
    if let Some(bits) = t
        .strip_prefix("b'")
        .or_else(|| t.strip_prefix("B'"))
        .and_then(|r| r.strip_suffix('\''))
    {
        return (!bits.is_empty() && bits.len() <= 64)
            .then(|| u64::from_str_radix(bits, 2).ok())
            .flatten();
    }
    t.parse().ok()
}

/// Backtick-quoted identifier.
fn quote_ident(name: &str) -> String {
    format!("`{}`", name.replace('`', "``"))
}

fn table_ref(target: &EditTarget) -> String {
    match target.schema.as_deref().filter(|s| !s.is_empty()) {
        Some(schema) => format!("{}.{}", quote_ident(schema), quote_ident(&target.table)),
        None => quote_ident(&target.table),
    }
}

fn edit_column(target: &EditTarget, index: usize) -> AppResult<&EditColumn> {
    target
        .columns
        .get(index)
        .and_then(Option::as_ref)
        .ok_or_else(|| {
            AppError::invalid(format!(
                "Column {} of the result cannot be edited.",
                index + 1
            ))
        })
}

/// One prepared statement for a grid change.
#[derive(Debug, PartialEq)]
struct Statement {
    sql: String,
    params: Vec<Value>,
}

/// Values per table column; a column given twice keeps the last value.
fn assignments<'a>(
    target: &'a EditTarget,
    values: &'a [CellValue],
) -> AppResult<Vec<(&'a EditColumn, Option<&'a str>)>> {
    let mut out: Vec<(&EditColumn, Option<&str>)> = Vec::new();
    for v in values {
        let col = edit_column(target, v.column)?;
        let value = v.value.as_deref();
        match out
            .iter_mut()
            .find(|(c, _)| same_column(&c.name, &col.name))
        {
            Some(slot) => slot.1 = value,
            None => out.push((col, value)),
        }
    }
    Ok(out)
}

/// ` WHERE k1 = ? AND k2 IS NULL`, with the key values in `params`.
fn push_where(
    sql: &mut String,
    params: &mut Vec<Value>,
    target: &EditTarget,
    engine: Engine,
    key: &[Option<String>],
) -> AppResult<()> {
    if key.len() != target.key.len() {
        return Err(AppError::invalid(
            "The row key does not match the result's key columns.",
        ));
    }
    for (n, (&index, value)) in target.key.iter().zip(key).enumerate() {
        let col = edit_column(target, index)?;
        sql.push_str(if n == 0 { " WHERE " } else { " AND " });
        sql.push_str(&quote_ident(&col.name));
        match value {
            None => sql.push_str(" IS NULL"),
            Some(v) => {
                let conv = conversion(&col.type_name);
                let _ = write!(sql, " = {}", placeholder(conv, engine));
                params.push(bind_value(conv, v)?);
            }
        }
    }
    Ok(())
}

/// The statement for one change; `None` when an update changes nothing.
fn build_change(
    target: &EditTarget,
    engine: Engine,
    change: &RowChange,
) -> AppResult<Option<Statement>> {
    let table = table_ref(target);
    let mut params = Vec::new();
    let value = |col: &EditColumn, v: Option<&str>, params: &mut Vec<Value>| {
        let conv = conversion(&col.type_name);
        params.push(match v {
            None => Value::NULL,
            Some(v) => bind_value(conv, v)?,
        });
        AppResult::Ok(placeholder(conv, engine))
    };
    let sql = match change {
        RowChange::Update { key, values } => {
            let set = assignments(target, values)?;
            if set.is_empty() {
                return Ok(None);
            }
            let mut sql = format!("UPDATE {table} SET ");
            for (n, (col, v)) in set.into_iter().enumerate() {
                if n > 0 {
                    sql.push_str(", ");
                }
                let p = value(col, v, &mut params)?;
                let _ = write!(sql, "{} = {p}", quote_ident(&col.name));
            }
            push_where(&mut sql, &mut params, target, engine, key)?;
            sql
        }
        RowChange::Delete { key } => {
            let mut sql = format!("DELETE FROM {table}");
            push_where(&mut sql, &mut params, target, engine, key)?;
            sql
        }
        RowChange::Insert { values } => {
            let set = assignments(target, values)?;
            if set.is_empty() {
                format!("INSERT INTO {table} () VALUES ()")
            } else {
                let mut names = Vec::new();
                let mut holders = Vec::new();
                for (col, v) in set {
                    names.push(quote_ident(&col.name));
                    holders.push(value(col, v, &mut params)?);
                }
                format!(
                    "INSERT INTO {table} ({}) VALUES ({})",
                    names.join(", "),
                    holders.join(", ")
                )
            }
        }
    };
    Ok(Some(Statement { sql, params }))
}

/// Adds the 1-based row number to an error about one change.
fn at_row(mut e: AppError, index: usize) -> AppError {
    if e.detail.is_none() {
        e.detail = Some(format!("Row {} was not saved.", index + 1));
    }
    e
}

/// Rows an UPDATE matched, from the OK packet's info text
/// (`Rows matched: 1  Changed: 0  Warnings: 0`; translated servers keep the
/// order). Affected rows count only changed rows.
fn matched_rows(info: &str) -> Option<u64> {
    let start = info.find(|c: char| c.is_ascii_digit())?;
    let digits: String = info[start..]
        .chars()
        .take_while(char::is_ascii_digit)
        .collect();
    digits.parse().ok()
}

fn check_matched(index: usize, matched: u64) -> AppResult<()> {
    let row = index + 1;
    let message = match matched {
        1 => return Ok(()),
        0 => format!(
            "Row {row} was not saved: 0 rows matched its key (changed or removed by someone else)."
        ),
        n => format!("Row {row} was not saved: {n} rows matched its key, which must match one."),
    };
    Err(AppError::new(ErrorKind::Database, message))
}

/// Runs the changes between `START TRANSACTION` and `COMMIT` (`own`) or
/// inside a savepoint of the open transaction; any error undoes them all.
async fn apply_in_transaction(
    conn: &mut Conn,
    req: &ApplyRequest,
    engine: Engine,
    statements: Vec<Option<Statement>>,
    own: bool,
) -> AppResult<ApplyResult> {
    let begin = if own {
        "START TRANSACTION".to_string()
    } else {
        format!("SAVEPOINT {SAVEPOINT}")
    };
    conn.query_drop(begin).await.map_err(map_error)?;
    let outcome = run_changes(conn, req, engine, statements).await;
    let end = match (&outcome, own) {
        (Ok(_), true) => "COMMIT".to_string(),
        (Ok(_), false) => format!("RELEASE SAVEPOINT {SAVEPOINT}"),
        (Err(_), true) => "ROLLBACK".to_string(),
        (Err(_), false) => format!("ROLLBACK TO SAVEPOINT {SAVEPOINT}"),
    };
    let ended = conn.query_drop(end).await.map_err(map_error);
    let result = match outcome {
        Ok(result) => result,
        Err(e) => {
            if !own {
                let _ = conn
                    .query_drop(format!("RELEASE SAVEPOINT {SAVEPOINT}"))
                    .await;
            }
            return Err(e);
        }
    };
    if let Err(e) = ended {
        if own {
            let _ = conn.query_drop("ROLLBACK").await;
        }
        return Err(e);
    }
    Ok(result)
}

async fn run_changes(
    conn: &mut Conn,
    req: &ApplyRequest,
    engine: Engine,
    statements: Vec<Option<Statement>>,
) -> AppResult<ApplyResult> {
    let mut out = ApplyResult::default();
    for (i, (change, statement)) in req.changes.iter().zip(statements).enumerate() {
        let Some(Statement { sql, params }) = statement else {
            continue;
        };
        conn.exec_drop(sql, params)
            .await
            .map_err(|e| at_row(map_error(e), i))?;
        let affected = conn.affected_rows();
        match change {
            RowChange::Insert { .. } => out.inserted += 1,
            RowChange::Delete { .. } => {
                check_matched(i, affected)?;
                out.deleted += 1;
            }
            RowChange::Update { key, .. } => {
                let matched = match affected {
                    1 => 1,
                    // Unchanged values: ask how many rows the key matched.
                    _ => match matched_rows(&conn.info()) {
                        Some(n) => n,
                        None => count_rows(conn, &req.target, engine, key)
                            .await
                            .map_err(|e| at_row(e, i))?,
                    },
                };
                check_matched(i, matched)?;
                out.updated += 1;
            }
        }
    }
    Ok(out)
}

/// Rows with the given key (an UPDATE that changed nothing left it as is).
async fn count_rows(
    conn: &mut Conn,
    target: &EditTarget,
    engine: Engine,
    key: &[Option<String>],
) -> AppResult<u64> {
    let mut sql = format!("SELECT COUNT(*) FROM {}", table_ref(target));
    let mut params = Vec::new();
    push_where(&mut sql, &mut params, target, engine, key)?;
    let n: Option<u64> = conn.exec_first(sql, params).await.map_err(map_error)?;
    Ok(n.unwrap_or(0))
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

    #[test]
    fn spots_set_operations_and_ddl() {
        assert!(has_set_operation("select a from t union select a from u"));
        assert!(has_set_operation("(SELECT 1)\nINTERSECT\n(SELECT 2)"));
        assert!(!has_set_operation(
            "select 'union', \"except\", `intersect` from t"
        ));
        assert!(!has_set_operation("select 'it\\'s union' from t"));
        assert!(!has_set_operation(
            "select a -- union\n, b # except\n /* intersect */ from reunion"
        ));
        // `1--1` is arithmetic, not a comment.
        assert!(has_set_operation("select 1--1\nunion select 2"));
        assert!(may_be_editable("select * from t"));
        assert!(!may_be_editable("values row(1)"));
        assert!(!may_be_editable("show tables"));
        assert!(!may_be_editable("select 1 union select 2"));
        assert!(is_ddl("alter table t add primary key (id)"));
        assert!(!is_ddl("select 1"));
    }

    fn o(alias: &str, table: &str, column: &str) -> Option<Origin> {
        Some(Origin {
            schema: "db".into(),
            alias: alias.into(),
            table: table.into(),
            column: column.into(),
        })
    }

    fn rows(items: &[&[Option<&str>]]) -> Vec<Vec<Option<String>>> {
        items
            .iter()
            .map(|r| r.iter().map(|v| v.map(str::to_string)).collect())
            .collect()
    }

    fn info(keys: &[&[Option<&str>]]) -> TableInfo {
        TableInfo::from_rows(
            &rows(&[&[Some("BASE TABLE")]]),
            &rows(&[
                &[Some("id"), Some("int"), Some("NO"), Some("auto_increment")],
                &[Some("code"), Some("varchar(10)"), Some("NO"), Some("")],
                &[Some("Name"), Some("varchar(20)"), Some("YES"), Some("")],
                &[
                    Some("up"),
                    Some("varchar(20)"),
                    Some("YES"),
                    Some("VIRTUAL GENERATED"),
                ],
            ]),
            &rows(keys),
        )
    }

    #[test]
    fn finds_the_single_source_table() {
        assert_eq!(
            single_table(&[o("t", "t", "id"), None, o("t", "t", "name")]),
            Ok(("db".into(), "t".into()))
        );
        assert_eq!(
            single_table(&[o("a", "t", "id"), o("b", "t", "id")]).unwrap_err(),
            SEVERAL_TABLES
        );
        assert_eq!(
            single_table(&[o("t", "t", "id"), o("u", "u", "id")]).unwrap_err(),
            SEVERAL_TABLES
        );
        assert_eq!(single_table(&[None, None]).unwrap_err(), NOT_A_TABLE);
        let mut derived = o("x", "x", "id");
        derived.as_mut().unwrap().schema.clear();
        assert_eq!(single_table(&[derived]).unwrap_err(), NOT_A_TABLE);
    }

    #[test]
    fn builds_edit_targets() {
        let pk = info(&[&[Some("PRIMARY"), Some("id")]]);
        let origins = [
            o("t", "t", "name"),
            None,
            o("t", "t", "ID"),
            o("t", "t", "up"),
        ];
        let t = edit_target("db".into(), "t".into(), &pk, &origins).unwrap();
        assert_eq!(t.key, [2]);
        assert_eq!(t.schema.as_deref(), Some("db"));
        let c = t.columns[0].as_ref().unwrap();
        assert_eq!(
            (c.name.as_str(), c.type_name.as_str()),
            ("Name", "varchar(20)")
        );
        assert!(t.columns[1].is_none());
        assert_eq!(t.columns[2].as_ref().unwrap().name, "id");
        // Generated columns stay read-only.
        assert!(t.columns[3].is_none());

        let e = edit_target("db".into(), "t".into(), &pk, &[o("t", "t", "code")]).unwrap_err();
        assert_eq!(
            e,
            "Read-only: add the primary key columns (id) to the query to edit rows."
        );

        let none = info(&[]);
        let e = edit_target("db".into(), "t".into(), &none, &origins).unwrap_err();
        assert_eq!(e, "Read-only: table t has no primary key.");

        // Without a primary key: unique keys on NOT NULL columns only.
        let unique = info(&[
            &[Some("a_name"), Some("Name")],
            &[Some("b_code"), Some("code")],
            &[Some("b_code"), Some("id")],
        ]);
        let t = edit_target(
            "db".into(),
            "t".into(),
            &unique,
            &[o("t", "t", "id"), o("t", "t", "Name"), o("t", "t", "code")],
        )
        .unwrap();
        assert_eq!(t.key, [2, 0]);
        let e = edit_target("db".into(), "t".into(), &unique, &[o("t", "t", "id")]).unwrap_err();
        assert_eq!(
            e,
            "Read-only: add the key columns (code, id) to the query to edit rows."
        );
        let nullable = info(&[&[Some("a_name"), Some("Name")]]);
        let e = edit_target("db".into(), "t".into(), &nullable, &origins).unwrap_err();
        assert_eq!(e, "Read-only: table t has no primary key.");
        // Expression key parts are unusable.
        let functional = info(&[&[Some("f"), None]]);
        assert!(edit_target("db".into(), "t".into(), &functional, &origins).is_err());

        let mut view = info(&[&[Some("PRIMARY"), Some("id")]]);
        view.table_type = "VIEW".into();
        let e = edit_target("db".into(), "t".into(), &view, &origins).unwrap_err();
        assert_eq!(e, NOT_A_TABLE);
        let missing = TableInfo::default();
        assert_eq!(
            edit_target("db".into(), "t".into(), &missing, &origins).unwrap_err(),
            NOT_A_TABLE
        );
    }

    #[test]
    fn catalog_query_uses_hex_literals() {
        assert_eq!(string_literal("a'b\\"), "_utf8mb4 X'6127625C'");
        assert_eq!(string_literal(""), "_utf8mb4 X''");
        let sql = catalog_sql("db", "t");
        assert_eq!(sql.matches("TABLE_SCHEMA = _utf8mb4 X'6462'").count(), 3);
        assert_eq!(sql.matches(';').count(), 2);
    }

    #[test]
    fn converts_values() {
        use Conversion::*;
        assert_eq!(conversion("varbinary(16)"), Hex);
        assert_eq!(conversion("LONGBLOB"), Hex);
        assert_eq!(conversion("point"), Hex);
        assert_eq!(conversion("bit(5)"), Bit);
        assert_eq!(conversion("tinyint(1)"), Bool);
        assert_eq!(conversion("tinyint(1) unsigned"), Bool);
        assert_eq!(conversion("tinyint(4)"), Text);
        assert_eq!(conversion("bigint unsigned"), Text);
        assert_eq!(conversion("json"), Text);
        assert_eq!(conversion("vector(3)"), Vector);
        assert_eq!(conversion("blob_like_name"), Text);

        let bytes = |v: AppResult<Value>| match v.unwrap() {
            Value::Bytes(b) => String::from_utf8(b).unwrap(),
            other => panic!("{other:?}"),
        };
        assert_eq!(bytes(bind_value(Hex, "0x00FF10")), "00FF10");
        assert_eq!(bytes(bind_value(Hex, "cafe")), "cafe");
        assert_eq!(bytes(bind_value(Hex, "0x")), "");
        assert_eq!(bind_value(Hex, "0x0").unwrap_err().kind, ErrorKind::Invalid);
        assert_eq!(
            bind_value(Hex, "0xZZ").unwrap_err().kind,
            ErrorKind::Invalid
        );
        let preview = format_bytes(
            ColumnType::MYSQL_TYPE_BLOB,
            BINARY_CHARSET,
            &vec![0u8; BINARY_PREVIEW_BYTES + 1],
        );
        let e = bind_value(Hex, &preview).unwrap_err();
        assert_eq!((e.kind, e.message.as_str()), (ErrorKind::Invalid, PREVIEW));
        assert_eq!(bind_value(Bit, "21").unwrap(), Value::UInt(21));
        assert_eq!(bind_value(Bit, "0x15").unwrap(), Value::UInt(21));
        assert_eq!(bind_value(Bit, "b'10101'").unwrap(), Value::UInt(21));
        assert_eq!(
            bind_value(Bit, "18446744073709551615").unwrap(),
            Value::UInt(u64::MAX)
        );
        assert!(bind_value(Bit, "x").is_err());
        assert_eq!(bytes(bind_value(Bool, "TRUE")), "1");
        assert_eq!(bytes(bind_value(Bool, "false")), "0");
        assert_eq!(bytes(bind_value(Bool, "1")), "1");
        let text = "it's \"x\"\n\\ … (3 bytes)";
        assert_eq!(bytes(bind_value(Text, text)), text);
        assert_eq!(placeholder(Hex, Engine::Mysql), "UNHEX(?)");
        assert_eq!(placeholder(Vector, Engine::Mysql), "STRING_TO_VECTOR(?)");
        assert_eq!(placeholder(Vector, Engine::Mariadb), "VEC_FromText(?)");
        assert_eq!(placeholder(Text, Engine::Mysql), "?");
    }

    fn target() -> EditTarget {
        let col = |name: &str, t: &str| {
            Some(EditColumn {
                name: name.into(),
                type_name: t.into(),
            })
        };
        EditTarget {
            schema: Some("my db".into()),
            table: "Order`s".into(),
            columns: vec![
                col("Order Id", "int"),
                col("data", "blob"),
                None,
                col("flags", "bit(3)"),
                col("k2", "varchar(5)"),
            ],
            key: vec![0, 4],
        }
    }

    fn cv(column: usize, value: Option<&str>) -> CellValue {
        CellValue {
            column,
            value: value.map(str::to_string),
        }
    }

    fn text(v: &str) -> Value {
        Value::Bytes(v.as_bytes().to_vec())
    }

    #[test]
    fn builds_change_statements() {
        let t = target();
        let key = vec![Some("7".to_string()), None];
        let s = build_change(
            &t,
            Engine::Mysql,
            &RowChange::Update {
                key: key.clone(),
                values: vec![cv(1, Some("0xCAFE")), cv(3, Some("5")), cv(1, None)],
            },
        )
        .unwrap()
        .unwrap();
        assert_eq!(
            s.sql,
            "UPDATE `my db`.`Order``s` SET `data` = UNHEX(?), `flags` = ? \
             WHERE `Order Id` = ? AND `k2` IS NULL"
        );
        assert_eq!(s.params, [Value::NULL, Value::UInt(5), text("7")]);

        let s = build_change(&t, Engine::Mysql, &RowChange::Delete { key: key.clone() })
            .unwrap()
            .unwrap();
        assert_eq!(
            s.sql,
            "DELETE FROM `my db`.`Order``s` WHERE `Order Id` = ? AND `k2` IS NULL"
        );
        assert_eq!(s.params, [text("7")]);

        let s = build_change(
            &t,
            Engine::Mysql,
            &RowChange::Insert {
                values: vec![cv(4, Some("a'b")), cv(1, Some("0x00"))],
            },
        )
        .unwrap()
        .unwrap();
        assert_eq!(
            s.sql,
            "INSERT INTO `my db`.`Order``s` (`k2`, `data`) VALUES (?, UNHEX(?))"
        );
        assert_eq!(s.params, [text("a'b"), text("00")]);

        let s = build_change(&t, Engine::Mysql, &RowChange::Insert { values: vec![] })
            .unwrap()
            .unwrap();
        assert_eq!(s.sql, "INSERT INTO `my db`.`Order``s` () VALUES ()");
        assert!(s.params.is_empty());

        let empty = RowChange::Update {
            key: key.clone(),
            values: vec![],
        };
        assert!(build_change(&t, Engine::Mysql, &empty).unwrap().is_none());

        // Expression columns and malformed keys are rejected.
        let e = build_change(
            &t,
            Engine::Mysql,
            &RowChange::Update {
                key: key.clone(),
                values: vec![cv(2, Some("x"))],
            },
        )
        .unwrap_err();
        assert_eq!(e.kind, ErrorKind::Invalid);
        let e = build_change(
            &t,
            Engine::Mysql,
            &RowChange::Delete {
                key: vec![Some("7".into())],
            },
        )
        .unwrap_err();
        assert_eq!(e.kind, ErrorKind::Invalid);
        let e = build_change(
            &t,
            Engine::Mysql,
            &RowChange::Insert {
                values: vec![cv(9, Some("x"))],
            },
        )
        .unwrap_err();
        assert_eq!(e.kind, ErrorKind::Invalid);
    }

    #[test]
    fn reads_matched_rows() {
        assert_eq!(
            matched_rows("Rows matched: 1  Changed: 0  Warnings: 0"),
            Some(1)
        );
        assert_eq!(
            matched_rows("Rows matched: 0  Changed: 0  Warnings: 0"),
            Some(0)
        );
        assert_eq!(matched_rows(""), None);
        assert!(check_matched(0, 1).is_ok());
        let e = check_matched(2, 0).unwrap_err();
        assert_eq!(e.kind, ErrorKind::Database);
        assert_eq!(
            e.message,
            "Row 3 was not saved: 0 rows matched its key (changed or removed by someone else)."
        );
        assert!(check_matched(0, 2).is_err());
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

    async fn editable(s: &MySession, sql: &str) -> EditTarget {
        let r = run(s, sql).await;
        assert!(r.read_only_reason.is_none(), "{sql}");
        r.editable
            .unwrap_or_else(|| panic!("{sql}: not editable, no reason"))
    }

    async fn read_only(s: &MySession, sql: &str) -> String {
        let r = run(s, sql).await;
        assert!(r.editable.is_none(), "{sql}: editable");
        r.read_only_reason
            .unwrap_or_else(|| panic!("{sql}: no reason"))
    }

    fn cells(items: &[(usize, Option<&str>)]) -> Vec<CellValue> {
        items.iter().map(|(c, v)| cv(*c, *v)).collect()
    }

    fn key(values: &[&str]) -> Vec<Option<String>> {
        values.iter().map(|v| Some(v.to_string())).collect()
    }

    async fn apply(
        s: &MySession,
        target: &EditTarget,
        changes: Vec<RowChange>,
    ) -> AppResult<ApplyResult> {
        s.apply_changes(&ApplyRequest {
            target: target.clone(),
            changes,
        })
        .await
    }

    async fn texts_of(s: &MySession, sql: &str) -> Vec<Vec<Option<String>>> {
        run(s, sql).await.rows
    }

    #[tokio::test]
    #[ignore]
    async fn detects_editable_results() {
        let Some(s) = session().await else { return };
        let db = s.current_schema.clone().unwrap();
        for sql in [
            "DROP VIEW IF EXISTS lq_edit_v",
            "DROP TABLE IF EXISTS lq_edit_a, lq_edit_b, lq_edit_nokey, lq_edit_uk",
            "CREATE TABLE lq_edit_a (id INT NOT NULL PRIMARY KEY, name VARCHAR(20), b_id INT)",
            "CREATE TABLE lq_edit_b (id INT NOT NULL PRIMARY KEY, label VARCHAR(20))",
            "CREATE TABLE lq_edit_nokey (n INT, m INT)",
            "CREATE TABLE lq_edit_uk (a INT NOT NULL, b INT NOT NULL, c INT, UNIQUE KEY uk (b, a))",
            "CREATE VIEW lq_edit_v AS SELECT id, name FROM lq_edit_a",
        ] {
            run(&s, sql).await;
        }

        let t = editable(&s, "SELECT id, name, id * 2 AS dbl FROM lq_edit_a").await;
        assert_eq!(t.schema.as_deref(), Some(db.as_str()));
        assert_eq!(t.table, "lq_edit_a");
        assert_eq!(t.key, [0]);
        let name = t.columns[1].as_ref().unwrap();
        assert_eq!(
            (name.name.as_str(), name.type_name.as_str()),
            ("name", "varchar(20)")
        );
        assert!(t.columns[2].is_none());

        let t = editable(&s, "SELECT name AS n, x.id FROM lq_edit_a x WHERE 1 = 0").await;
        assert_eq!(t.key, [1]);
        assert_eq!(t.columns[0].as_ref().unwrap().name, "name");

        let t = editable(&s, "SELECT c, a, b FROM lq_edit_uk").await;
        assert_eq!(t.key, [2, 1]);

        assert_eq!(
            read_only(
                &s,
                "SELECT a.id, b.label FROM lq_edit_a a JOIN lq_edit_b b ON b.id = a.b_id"
            )
            .await,
            SEVERAL_TABLES
        );
        assert_eq!(
            read_only(
                &s,
                "SELECT a.id, b.id FROM lq_edit_a a JOIN lq_edit_a b ON a.id = b.id"
            )
            .await,
            SEVERAL_TABLES
        );
        assert_eq!(
            read_only(&s, "SELECT * FROM lq_edit_nokey").await,
            "Read-only: table lq_edit_nokey has no primary key."
        );
        assert_eq!(
            read_only(&s, "SELECT name FROM lq_edit_a").await,
            "Read-only: add the primary key columns (id) to the query to edit rows."
        );
        assert_eq!(
            read_only(&s, "SELECT COUNT(*) FROM lq_edit_a").await,
            NOT_A_TABLE
        );
        assert_eq!(
            read_only(
                &s,
                "SELECT id FROM lq_edit_a UNION SELECT id FROM lq_edit_b"
            )
            .await,
            NOT_A_TABLE
        );
        read_only(&s, "SHOW TABLES").await;
        // A view is read-only, unless the server names its base table.
        let r = run(&s, "SELECT * FROM lq_edit_v").await;
        match &r.editable {
            Some(t) => assert_eq!(t.table, "lq_edit_a"),
            None => assert_eq!(r.read_only_reason.as_deref(), Some(NOT_A_TABLE)),
        }
        let r = run(&s, "UPDATE lq_edit_a SET name = name").await;
        assert!(r.editable.is_none() && r.read_only_reason.is_none());

        // DDL clears the cached catalog.
        run(&s, "ALTER TABLE lq_edit_nokey ADD PRIMARY KEY (n)").await;
        assert_eq!(
            editable(&s, "SELECT m, n FROM lq_edit_nokey").await.key,
            [1]
        );

        run(&s, "DROP VIEW lq_edit_v").await;
        run(
            &s,
            "DROP TABLE lq_edit_a, lq_edit_b, lq_edit_nokey, lq_edit_uk",
        )
        .await;
    }

    #[tokio::test]
    #[ignore]
    async fn round_trips_every_type() {
        let Some(s) = session().await else { return };
        run(&s, "DROP TABLE IF EXISTS lq_edit_types").await;
        run(
            &s,
            "CREATE TABLE lq_edit_types (id INT AUTO_INCREMENT PRIMARY KEY, d DECIMAL(30,9),
             i BIGINT, u BIGINT UNSIGNED, f DOUBLE, fl FLOAT, b BOOLEAN, dt DATE,
             ts DATETIME(6), tz TIMESTAMP(3) NULL, tm TIME(6), y YEAR, txt TEXT,
             vc VARCHAR(100), bin VARBINARY(16), bl BLOB, bt BIT(5), j JSON,
             e ENUM('x','y'), n VARCHAR(5) NULL) DEFAULT CHARSET = utf8mb4",
        )
        .await;
        let target = editable(&s, "SELECT * FROM lq_edit_types").await;
        assert_eq!(
            target.columns[14].as_ref().unwrap().type_name,
            "varbinary(16)"
        );

        let expected: Vec<Option<&str>> = vec![
            Some("12345678901234567890.123456789"),
            Some("-9223372036854775808"),
            Some("18446744073709551615"),
            Some("-1234.5678"),
            Some("1.5"),
            Some("1"),
            Some("2024-02-29"),
            Some("2024-05-01 13:45:00.123456"),
            Some("2024-05-01 13:45:00.120"),
            Some("-12:34:56.789000"),
            Some("2024"),
            Some("it's \"quoted\"\nline 2 \\ back"),
            Some("ação 😀"),
            Some("0x00FF10CA"),
            Some("0xDEADBEEF"),
            Some("21"),
            Some("{\"a\": 1, \"b\": [true, null]}"),
            Some("y"),
            None,
        ];
        let values: Vec<(usize, Option<&str>)> = expected
            .iter()
            .enumerate()
            .map(|(i, v)| (i + 1, *v))
            .collect();
        let r = apply(
            &s,
            &target,
            vec![RowChange::Insert {
                values: cells(&values),
            }],
        )
        .await
        .unwrap();
        assert_eq!((r.inserted, r.in_transaction), (1, false));

        let rows = texts_of(&s, "SELECT * FROM lq_edit_types").await;
        let shown: Vec<Option<&str>> = rows[0][1..].iter().map(|v| v.as_deref()).collect();
        assert_eq!(shown, expected);

        // Every column saved with its own displayed text stays identical
        // (nothing changes, so the server reports 0 affected rows).
        let id = rows[0][0].clone().unwrap();
        let same: Vec<(usize, Option<&str>)> = rows[0]
            .iter()
            .enumerate()
            .skip(1)
            .map(|(i, v)| (i, v.as_deref()))
            .collect();
        let r = apply(
            &s,
            &target,
            vec![RowChange::Update {
                key: key(&[&id]),
                values: cells(&same),
            }],
        )
        .await
        .unwrap();
        assert_eq!(r.updated, 1);
        assert_eq!(texts_of(&s, "SELECT * FROM lq_edit_types").await, rows);

        // Changed values, committed for other sessions too.
        let r = apply(
            &s,
            &target,
            vec![RowChange::Update {
                key: key(&[&id]),
                values: cells(&[
                    (15, Some("0x")),
                    (16, Some("2")),
                    (19, Some("ok")),
                    (6, None),
                ]),
            }],
        )
        .await
        .unwrap();
        assert_eq!((r.updated, r.in_transaction), (1, false));
        let other = session().await.unwrap();
        let rows = texts_of(&other, "SELECT bl, bt, n, b FROM lq_edit_types").await;
        assert_eq!(
            rows[0],
            [
                Some("0x".to_string()),
                Some("2".into()),
                Some("ok".into()),
                None
            ]
        );

        let r = apply(
            &s,
            &target,
            vec![
                RowChange::Insert { values: vec![] },
                RowChange::Delete { key: key(&[&id]) },
            ],
        )
        .await
        .unwrap();
        assert_eq!((r.inserted, r.deleted), (1, 1));
        let rows = texts_of(&other, "SELECT id, e, n FROM lq_edit_types").await;
        assert_eq!(rows.len(), 1);
        assert_ne!(rows[0][0].as_deref(), Some(id.as_str()));
        run(&s, "DROP TABLE lq_edit_types").await;
    }

    #[tokio::test]
    #[ignore]
    async fn failed_changes_roll_back() {
        let Some(s) = session().await else { return };
        run(&s, "DROP TABLE IF EXISTS lq_edit_rb").await;
        run(
            &s,
            "CREATE TABLE lq_edit_rb (id INT PRIMARY KEY, v VARCHAR(10), bl BLOB) ENGINE = InnoDB",
        )
        .await;
        run(
            &s,
            "INSERT INTO lq_edit_rb VALUES (1, 'a', NULL), (2, 'b', NULL)",
        )
        .await;
        let target = editable(&s, "SELECT id, v, bl FROM lq_edit_rb").await;
        let before = texts_of(&s, "SELECT * FROM lq_edit_rb ORDER BY id").await;

        let e = apply(
            &s,
            &target,
            vec![
                RowChange::Update {
                    key: key(&["1"]),
                    values: cells(&[(1, Some("changed"))]),
                },
                RowChange::Delete { key: key(&["2"]) },
                RowChange::Update {
                    key: key(&["999"]),
                    values: cells(&[(1, Some("x"))]),
                },
            ],
        )
        .await
        .unwrap_err();
        assert_eq!(e.kind, ErrorKind::Database);
        assert_eq!(
            e.message,
            "Row 3 was not saved: 0 rows matched its key (changed or removed by someone else)."
        );
        assert_eq!(
            texts_of(&s, "SELECT * FROM lq_edit_rb ORDER BY id").await,
            before
        );
        assert!(!s.tx_state().in_transaction);

        // Server errors keep their code.
        let e = apply(
            &s,
            &target,
            vec![
                RowChange::Delete { key: key(&["1"]) },
                RowChange::Insert {
                    values: cells(&[(0, Some("2"))]),
                },
            ],
        )
        .await
        .unwrap_err();
        assert_eq!(e.code.as_deref(), Some("1062 (23000)"));
        assert_eq!(e.detail.as_deref(), Some("Row 2 was not saved."));
        assert_eq!(
            texts_of(&s, "SELECT * FROM lq_edit_rb ORDER BY id").await,
            before
        );

        // Previews never reach the server.
        let e = apply(
            &s,
            &target,
            vec![RowChange::Update {
                key: key(&["1"]),
                values: cells(&[(2, Some("0x0000… (40000 bytes)"))]),
            }],
        )
        .await
        .unwrap_err();
        assert_eq!((e.kind, e.message.as_str()), (ErrorKind::Invalid, PREVIEW));
        assert_eq!(run(&s, "SELECT 1").await.rows[0][0].as_deref(), Some("1"));
        run(&s, "DROP TABLE lq_edit_rb").await;
    }

    #[tokio::test]
    #[ignore]
    async fn joins_the_manual_transaction() {
        let Some(s) = session().await else { return };
        run(&s, "DROP TABLE IF EXISTS lq_edit_tx").await;
        run(
            &s,
            "CREATE TABLE lq_edit_tx (id INT PRIMARY KEY) ENGINE = InnoDB",
        )
        .await;
        let target = editable(&s, "SELECT id FROM lq_edit_tx").await;
        let insert = |id: &str| RowChange::Insert {
            values: cells(&[(0, Some(id))]),
        };
        let ids = |rows: Vec<Vec<Option<String>>>| -> Vec<String> {
            rows.into_iter().map(|r| r[0].clone().unwrap()).collect()
        };

        s.set_autocommit(false).await.unwrap();
        run(&s, "INSERT INTO lq_edit_tx VALUES (10)").await;
        let r = apply(&s, &target, vec![insert("11")]).await.unwrap();
        assert!(r.in_transaction);
        // A failure undoes only this batch; the transaction stays open.
        let e = apply(
            &s,
            &target,
            vec![insert("12"), RowChange::Delete { key: key(&["999"]) }],
        )
        .await
        .unwrap_err();
        assert_eq!(e.kind, ErrorKind::Database);
        assert!(s.tx_state().in_transaction);
        assert_eq!(
            ids(texts_of(&s, "SELECT id FROM lq_edit_tx ORDER BY id").await),
            ["10", "11"]
        );
        s.end_transaction(false).await.unwrap();
        assert!(texts_of(&s, "SELECT id FROM lq_edit_tx").await.is_empty());

        // First statement of a manual transaction.
        s.end_transaction(false).await.unwrap();
        let r = apply(&s, &target, vec![insert("13")]).await.unwrap();
        assert!(r.in_transaction);
        s.end_transaction(true).await.unwrap();
        s.set_autocommit(true).await.unwrap();

        // An explicit transaction in autocommit mode is joined, not committed.
        run(&s, "START TRANSACTION").await;
        let r = apply(&s, &target, vec![insert("14")]).await.unwrap();
        assert!(r.in_transaction);
        run(&s, "ROLLBACK").await;
        assert_eq!(ids(texts_of(&s, "SELECT id FROM lq_edit_tx").await), ["13"]);
        run(&s, "DROP TABLE lq_edit_tx").await;
    }

    #[tokio::test]
    #[ignore]
    async fn edits_names_that_need_quoting() {
        let Some(s) = session().await else { return };
        run(&s, "DROP TABLE IF EXISTS `lq_edit_Mixed Case`").await;
        run(
            &s,
            "CREATE TABLE `lq_edit_Mixed Case` (`Order Id` INT NOT NULL PRIMARY KEY,
             `Some Name` VARCHAR(20))",
        )
        .await;
        run(&s, "INSERT INTO `lq_edit_Mixed Case` VALUES (1, 'one')").await;
        let target = editable(
            &s,
            "SELECT `Some Name` AS label, `Order Id` FROM `lq_edit_Mixed Case`",
        )
        .await;
        assert!(target.table.eq_ignore_ascii_case("lq_edit_Mixed Case"));
        assert_eq!(target.key, [1]);
        assert_eq!(target.columns[0].as_ref().unwrap().name, "Some Name");

        let r = apply(
            &s,
            &target,
            vec![
                RowChange::Update {
                    key: key(&["1"]),
                    values: cells(&[(0, Some("uno")), (1, Some("5"))]),
                },
                RowChange::Insert {
                    values: cells(&[(1, Some("2")), (0, Some("two"))]),
                },
            ],
        )
        .await
        .unwrap();
        assert_eq!((r.updated, r.inserted), (1, 1));
        let rows = texts_of(&s, "SELECT * FROM `lq_edit_Mixed Case` ORDER BY 1").await;
        assert_eq!(
            rows,
            [
                [Some("2".to_string()), Some("two".into())],
                [Some("5".into()), Some("uno".into())]
            ]
        );
        let r = apply(&s, &target, vec![RowChange::Delete { key: key(&["5"]) }])
            .await
            .unwrap();
        assert_eq!(r.deleted, 1);
        run(&s, "DROP TABLE `lq_edit_Mixed Case`").await;
    }

    #[tokio::test]
    #[ignore]
    async fn result_view_sql() {
        let Some((profile, password)) = profile() else {
            return;
        };
        let s = crate::drivers::Session::open(&profile, password.as_deref())
            .await
            .expect("connect");
        crate::drivers::view_fixtures::check(&s, "mysql").await;
    }

    #[tokio::test]
    #[ignore]
    async fn ddl_sql() {
        let Some((profile, password)) = profile() else {
            return;
        };
        let s = crate::drivers::Session::open(&profile, password.as_deref())
            .await
            .expect("connect");
        crate::drivers::ddl_fixtures::check(&s, "mysql", &profile.database).await;
    }
}
