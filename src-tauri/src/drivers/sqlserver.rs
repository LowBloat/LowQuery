//! SQL Server session over TDS, using `tiberius`.
//!
//! - Each call is sent as a SQL batch, so temporary tables, `USE` and `SET`
//!   options persist across calls, as in SSMS. The UI splits scripts on `GO`.
//! - Plain `SELECT` statements are limited by the server with
//!   `SET ROWCOUNT`; everything else stops keeping rows at the limit.
//! - Manual commit uses `SET IMPLICIT_TRANSACTIONS ON`; `@@TRANCOUNT` is read
//!   after every call, so the transaction flag is what the server reports.
//! - Cancel sends a TDS attention to the running batch.
//! - tiberius does not expose INFO tokens (PRINT, low-severity RAISERROR) nor
//!   per-statement DONE counts, so there are no notices from the server and
//!   `rows_affected` is `ROWCOUNT_BIG()` of the batch's last statement.

use std::fmt::Write as _;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use futures_util::TryStreamExt;
use parking_lot::Mutex;
use tiberius::numeric::Numeric;
use tiberius::time::{Date, DateTime, DateTime2, DateTimeOffset, SmallDateTime, Time};
use tiberius::{
    AuthMethod, Client, Column, ColumnData, ColumnType, Config, EncryptionLevel, QueryItem,
    SqlBrowser,
};
use tokio::net::TcpStream;
use tokio_util::compat::{Compat, TokioAsyncWriteCompatExt};

use super::{first_keywords, TxState};
use crate::error::{AppError, AppResult, ErrorKind};
use crate::model::{
    ColumnInfo, ConnectionProfile, DbColumn, DbObject, ExecRequest, ExecResult, SslMode, ValueKind,
};

type MsClient = Client<Compat<TcpStream>>;

const CONNECT_TIMEOUT: Duration = Duration::from_secs(15);
/// How long to wait for the server to acknowledge a cancel.
const CANCEL_TIMEOUT: Duration = Duration::from_secs(15);
const BINARY_PREVIEW_BYTES: usize = 32 * 1024;
/// Days from 0001-01-01 and from 1900-01-01 to 1970-01-01.
const DAYS_0001_TO_EPOCH: i64 = 719_162;
const DAYS_1900_TO_EPOCH: i64 = 25_567;
/// Login failed (bad user or password, or an account without access).
const LOGIN_FAILED: u32 = 18456;
/// Cannot open the database requested by the login.
const CANNOT_OPEN_DATABASE: u32 = 4060;

pub struct MsSession {
    client: tokio::sync::Mutex<MsClient>,
    cancel: tokio::sync::Notify,
    broken: AtomicBool,
    state: Mutex<TxState>,
    pub server_version: String,
    pub current_user: Option<String>,
    pub current_schema: Option<String>,
}

pub async fn connect(profile: &ConnectionProfile, password: Option<&str>) -> AppResult<MsSession> {
    let (config, browser) = build_config(profile, password, None)?;
    let client = match open_client(config, browser).await {
        // Azure gateways may redirect the login to another node.
        Err(tiberius::error::Error::Routing { host, port }) => {
            let (config, _) = build_config(profile, password, Some((host, port)))?;
            open_client(config, false).await
        }
        other => other,
    };
    let mut client = client.map_err(connection_error)?;

    let rows = fetch(
        &mut client,
        "SET TEXTSIZE 2147483647; SET IMPLICIT_TRANSACTIONS OFF;
         SELECT CAST(SERVERPROPERTY('ProductVersion') AS nvarchar(128)),
                CAST(SERVERPROPERTY('EngineEdition') AS int), SUSER_SNAME(), SCHEMA_NAME()",
    )
    .await
    .map_err(connection_error)?;
    let mut row = rows.into_iter().next().unwrap_or_default().into_iter();
    let version = row.next().flatten().unwrap_or_default();
    let edition = row
        .next()
        .flatten()
        .and_then(|e| e.parse().ok())
        .unwrap_or(0);

    Ok(MsSession {
        server_version: server_label(&version, edition),
        current_user: row.next().flatten(),
        current_schema: row.next().flatten(),
        client: tokio::sync::Mutex::new(client),
        cancel: tokio::sync::Notify::new(),
        broken: AtomicBool::new(false),
        state: Mutex::new(TxState {
            autocommit: true,
            in_transaction: false,
        }),
    })
}

/// Builds the tiberius configuration. Returns whether the port must be
/// resolved through the SQL Browser service (named instance without an
/// explicit port). `target` overrides host and port after a routing redirect.
fn build_config(
    profile: &ConnectionProfile,
    password: Option<&str>,
    target: Option<(String, u16)>,
) -> AppResult<(Config, bool)> {
    let mut config = Config::new();
    let instance = profile.instance.trim();
    let mut browser = false;
    match target {
        Some((host, port)) => {
            config.host(host);
            config.port(port);
        }
        None => {
            config.host(profile.host.trim());
            if !instance.is_empty() {
                config.instance_name(instance);
            }
            // With a named instance, the default port means "ask SQL Browser".
            if !instance.is_empty() && matches!(profile.port, 0 | 1433) {
                browser = true;
            } else {
                config.port(if profile.port == 0 {
                    1433
                } else {
                    profile.port
                });
            }
        }
    }
    if !profile.database.trim().is_empty() {
        config.database(profile.database.trim());
    }
    config.application_name("LowQuery");
    config.handshake_timeout(Some(CONNECT_TIMEOUT));
    // Long-running statements are interrupted by the user, not by a timer.
    config.command_timeout(None);
    config.lossy_utf16_decoding(true);
    match profile.ssl_mode {
        // Only the login packet is encrypted, as SSMS does with Encrypt=false.
        SslMode::Disable => {
            config.encryption(EncryptionLevel::Off);
            config.trust_cert();
        }
        SslMode::Prefer => {
            config.encryption(EncryptionLevel::On);
            config.trust_cert();
        }
        SslMode::Require => {
            config.encryption(EncryptionLevel::Required);
            config.trust_cert();
        }
        SslMode::VerifyFull => config.encryption(EncryptionLevel::Required),
    }
    config.authentication(if profile.windows_auth {
        windows_auth()?
    } else {
        AuthMethod::sql_server(profile.user.trim(), password.unwrap_or_default())
    });
    Ok((config, browser))
}

#[cfg(windows)]
fn windows_auth() -> AppResult<AuthMethod> {
    Ok(AuthMethod::Integrated)
}

#[cfg(not(windows))]
fn windows_auth() -> AppResult<AuthMethod> {
    Err(AppError::invalid(
        "Windows authentication is only available when LowQuery runs on Windows.",
    ))
}

async fn open_client(config: Config, browser: bool) -> tiberius::Result<MsClient> {
    let addr = config.get_addr();
    let tcp = tokio::time::timeout(CONNECT_TIMEOUT, async {
        if browser {
            TcpStream::connect_named(&config).await
        } else {
            Ok(TcpStream::connect(&addr).await?)
        }
    })
    .await
    .map_err(|_| tiberius::error::Error::Io {
        kind: std::io::ErrorKind::TimedOut,
        message: format!("Timed out connecting to {addr}."),
    })??;
    tcp.set_nodelay(true)?;
    Client::connect(config, tcp.compat_write()).await
}

fn server_label(version: &str, engine_edition: i64) -> String {
    match engine_edition {
        // 5: Azure SQL Database, 8: Azure SQL Managed Instance.
        5 | 8 => format!("Azure SQL {version}"),
        _ => format!("SQL Server {version}"),
    }
}

/// Rows read from the first result set of a batch.
#[derive(Default)]
struct Batch {
    columns: Option<Vec<ColumnInfo>>,
    rows: Vec<Vec<Option<String>>>,
    truncated: bool,
}

/// What the server reports after a batch.
struct After {
    rowcount: u64,
    trancount: i64,
    xact_state: i64,
}

impl MsSession {
    pub fn tx_state(&self) -> TxState {
        *self.state.lock()
    }

    pub async fn execute(&self, req: &ExecRequest) -> AppResult<ExecResult> {
        let mut client = self.lock().await?;
        let limited = req.max_rows > 0 && row_limit_applies(&req.sql);
        let batch = if limited {
            // Same line as the statement, so error line numbers do not shift.
            format!(
                "SET ROWCOUNT {}; {}",
                req.max_rows.saturating_add(1),
                req.sql
            )
        } else {
            req.sql.clone()
        };

        let started = Instant::now();
        let outcome = {
            let run = read_batch(&mut client, batch, req.max_rows);
            tokio::select! {
                r = run => Some(r),
                _ = self.cancel.notified() => None,
            }
        };
        let elapsed_ms = started.elapsed().as_millis() as u64;

        let outcome = match outcome {
            Some(Ok(batch)) => Ok(batch),
            Some(Err(e)) => Err(self.fail(e, Some(&req.sql))),
            None => {
                // The batch future was dropped; the attention tells the server
                // to stop and drains the rest of the response.
                let acked = matches!(
                    tokio::time::timeout(CANCEL_TIMEOUT, client.cancel_query()).await,
                    Ok(Ok(()))
                );
                if acked {
                    Err(AppError::new(
                        ErrorKind::Cancelled,
                        "The statement was cancelled.",
                    ))
                } else {
                    self.broken.store(true, Ordering::SeqCst);
                    Err(AppError::new(
                        ErrorKind::Cancelled,
                        "The statement was cancelled, but the connection could not be recovered. Reconnect to continue.",
                    ))
                }
            }
        };

        // Row count, transaction state, and the ROWCOUNT reset in one trip.
        let after = if self.broken.load(Ordering::SeqCst) {
            None
        } else {
            let sql = format!(
                "SELECT ROWCOUNT_BIG(), @@TRANCOUNT, XACT_STATE();{}",
                if limited { " SET ROWCOUNT 0;" } else { "" }
            );
            match fetch(&mut client, &sql).await {
                Ok(rows) => Some(parse_after(&rows)),
                Err(e) => {
                    let err = self.fail(e, None);
                    if outcome.is_ok() {
                        return Err(err);
                    }
                    None
                }
            }
        };
        drop(client);

        if let Some(after) = &after {
            self.state.lock().in_transaction = after.trancount > 0;
        }
        let batch = outcome?;
        let after = after.unwrap_or(After {
            rowcount: 0,
            trancount: 0,
            xact_state: 0,
        });

        let mut result = ExecResult {
            elapsed_ms,
            in_transaction: self.state.lock().in_transaction,
            ..Default::default()
        };
        let first = first_keywords(&req.sql, 1);
        let counts_rows = match first.first().map(String::as_str) {
            Some("INSERT" | "UPDATE" | "DELETE" | "MERGE" | "BULK") => true,
            // SELECT ... INTO and WITH ... <DML> return no result set.
            Some("SELECT" | "WITH") => batch.columns.is_none(),
            _ => false,
        };
        if counts_rows {
            result.rows_affected = Some(after.rowcount);
        }
        if after.xact_state == -1 {
            result
                .notices
                .push("The transaction can no longer be committed; roll it back.".into());
        }
        result.columns = batch.columns;
        result.rows = batch.rows;
        result.truncated = batch.truncated;
        Ok(result)
    }

    pub async fn cancel(&self) -> AppResult<()> {
        self.cancel.notify_waiters();
        Ok(())
    }

    pub async fn set_autocommit(&self, on: bool) -> AppResult<TxState> {
        let mut client = self.lock().await?;
        let sql = if on {
            // Turning autocommit back on commits pending work, like JDBC.
            "WHILE @@TRANCOUNT > 0 AND XACT_STATE() = 1 COMMIT TRAN;
             IF @@TRANCOUNT = 0 SET IMPLICIT_TRANSACTIONS OFF;
             SELECT @@TRANCOUNT"
        } else {
            "SET IMPLICIT_TRANSACTIONS ON; SELECT @@TRANCOUNT"
        };
        let rows = fetch(&mut client, sql)
            .await
            .map_err(|e| self.fail(e, None))?;
        drop(client);
        let trancount = first_int(&rows);
        let mut st = self.state.lock();
        st.in_transaction = trancount > 0;
        if on && st.in_transaction {
            return Err(doomed());
        }
        st.autocommit = on;
        Ok(*st)
    }

    pub async fn end_transaction(&self, commit: bool) -> AppResult<TxState> {
        let mut client = self.lock().await?;
        let sql = if commit {
            // Nested BEGIN TRAN needs one COMMIT per level. A doomed
            // transaction cannot commit, and COMMIT would fail in a loop.
            "WHILE @@TRANCOUNT > 0 AND XACT_STATE() = 1 COMMIT TRAN; SELECT @@TRANCOUNT"
        } else {
            "IF @@TRANCOUNT > 0 ROLLBACK TRAN; SELECT @@TRANCOUNT"
        };
        let rows = fetch(&mut client, sql)
            .await
            .map_err(|e| self.fail(e, None))?;
        drop(client);
        let trancount = first_int(&rows);
        let mut st = self.state.lock();
        st.in_transaction = trancount > 0;
        if st.in_transaction {
            return Err(doomed());
        }
        Ok(*st)
    }

    pub async fn list_schemas(&self) -> AppResult<Vec<String>> {
        // Fixed database-role schemas (db_owner...) only show up when they
        // hold objects; system schemas go last.
        let rows = self
            .metadata(
                "SELECT s.name FROM sys.schemas s
                 WHERE s.schema_id NOT BETWEEN 16384 AND 16399
                    OR EXISTS (SELECT 1 FROM sys.objects o WHERE o.schema_id = s.schema_id)
                 ORDER BY CASE WHEN s.name = SCHEMA_NAME() THEN 0
                               WHEN s.name IN ('sys', 'INFORMATION_SCHEMA', 'guest')
                                 OR s.schema_id BETWEEN 16384 AND 16399 THEN 2
                               ELSE 1 END, s.name"
                    .to_string(),
            )
            .await?;
        Ok(rows
            .into_iter()
            .filter_map(|mut r| r.drain(..).next().flatten())
            .collect())
    }

    pub async fn list_objects(&self, schema: &str) -> AppResult<Vec<DbObject>> {
        let schema = nstr(schema);
        let rows = self
            .metadata(format!(
                "SELECT name, kind FROM (
                   SELECT o.name,
                          CASE o.type WHEN 'U' THEN 'table' WHEN 'V' THEN 'view'
                               WHEN 'SO' THEN 'sequence' WHEN 'SN' THEN 'synonym'
                               WHEN 'P' THEN 'procedure' WHEN 'PC' THEN 'procedure'
                               WHEN 'X' THEN 'procedure' ELSE 'function' END AS kind
                   FROM sys.all_objects o
                   WHERE o.schema_id = SCHEMA_ID({schema})
                     AND o.type IN ('U','V','SO','SN','P','PC','X','FN','IF','TF','FS','FT')
                     AND (o.is_ms_shipped = 0 OR {schema} IN (N'sys', N'INFORMATION_SCHEMA'))
                   UNION ALL
                   SELECT t.name, 'type' FROM sys.types t
                   WHERE t.is_user_defined = 1 AND t.schema_id = SCHEMA_ID({schema})
                 ) x
                 ORDER BY kind, name"
            ))
            .await?;
        Ok(rows
            .into_iter()
            .map(|mut r| {
                let kind = r.pop().flatten().unwrap_or_default();
                DbObject {
                    name: r.pop().flatten().unwrap_or_default(),
                    kind,
                }
            })
            .collect())
    }

    pub async fn list_columns(&self, schema: &str, table: &str) -> AppResult<Vec<DbColumn>> {
        let (schema, table) = (nstr(schema), nstr(table));
        let rows = self
            .metadata(format!(
                "SELECT c.name,
                        CASE WHEN t.is_user_defined = 1 THEN t.name
                             WHEN t.name IN ('varchar', 'char', 'varbinary', 'binary')
                               THEN t.name + '(' + CASE WHEN c.max_length = -1 THEN 'max'
                                    ELSE CAST(c.max_length AS varchar(10)) END + ')'
                             WHEN t.name IN ('nvarchar', 'nchar')
                               THEN t.name + '(' + CASE WHEN c.max_length = -1 THEN 'max'
                                    ELSE CAST(c.max_length / 2 AS varchar(10)) END + ')'
                             WHEN t.name IN ('decimal', 'numeric')
                               THEN t.name + '(' + CAST(c.precision AS varchar(10)) + ','
                                    + CAST(c.scale AS varchar(10)) + ')'
                             WHEN t.name IN ('datetime2', 'time', 'datetimeoffset')
                               THEN t.name + '(' + CAST(c.scale AS varchar(10)) + ')'
                             ELSE t.name END,
                        c.is_nullable,
                        CAST(CASE WHEN EXISTS (
                          SELECT 1 FROM sys.indexes i
                          JOIN sys.index_columns ic ON ic.object_id = i.object_id
                           AND ic.index_id = i.index_id
                          WHERE i.object_id = c.object_id AND i.is_primary_key = 1
                            AND ic.column_id = c.column_id) THEN 1 ELSE 0 END AS bit)
                 FROM sys.all_columns c
                 JOIN sys.types t ON t.user_type_id = c.user_type_id
                 WHERE c.object_id = OBJECT_ID(QUOTENAME({schema}) + N'.' + QUOTENAME({table}))
                 ORDER BY c.column_id"
            ))
            .await?;
        Ok(rows
            .into_iter()
            .map(|r| {
                let text = |i: usize| r.get(i).cloned().flatten().unwrap_or_default();
                DbColumn {
                    name: text(0),
                    type_name: text(1),
                    nullable: text(2) == "true",
                    primary_key: text(3) == "true",
                }
            })
            .collect())
    }

    /// Runs a catalog query. In manual-commit mode a SELECT opens an implicit
    /// transaction, so one opened only by this query is ended right away.
    async fn metadata(&self, sql: String) -> AppResult<Vec<Vec<Option<String>>>> {
        let mut client = self.lock().await?;
        let state = *self.state.lock();
        let sql = if !state.autocommit && !state.in_transaction {
            format!("{sql};\nIF @@TRANCOUNT > 0 COMMIT TRAN;")
        } else {
            sql
        };
        fetch(&mut client, &sql)
            .await
            .map_err(|e| self.fail(e, None))
    }

    async fn lock(&self) -> AppResult<tokio::sync::MutexGuard<'_, MsClient>> {
        self.ensure_open()?;
        let client = self.client.lock().await;
        self.ensure_open()?;
        Ok(client)
    }

    fn ensure_open(&self) -> AppResult<()> {
        if self.broken.load(Ordering::SeqCst) {
            return Err(AppError::connection(
                "The connection to the server was lost. Reconnect to continue.",
            ));
        }
        Ok(())
    }

    /// Maps the error and marks the session unusable when the stream broke.
    fn fail(&self, e: tiberius::error::Error, sql: Option<&str>) -> AppError {
        use tiberius::error::Error as E;
        if matches!(e, E::Io { .. } | E::Protocol(_) | E::Tls(_)) {
            self.broken.store(true, Ordering::SeqCst);
        }
        map_error(e, sql)
    }
}

fn doomed() -> AppError {
    AppError::new(
        ErrorKind::Database,
        "The transaction cannot be committed (an error doomed it). Roll it back.",
    )
}

/// Runs the batch and keeps the first result set, up to `max_rows` rows. The
/// rest of the response is read and discarded so the connection stays in
/// sync.
async fn read_batch(
    client: &mut MsClient,
    sql: String,
    max_rows: usize,
) -> tiberius::Result<Batch> {
    let mut stream = client.simple_query(sql).await?;
    let mut out = Batch::default();
    while let Some(item) = stream.try_next().await? {
        match item {
            QueryItem::Metadata(meta) if meta.result_index() == 0 => {
                out.columns = Some(meta.columns().iter().map(column_info).collect());
            }
            QueryItem::Row(row) if row.result_index() == 0 => {
                if max_rows > 0 && out.rows.len() >= max_rows {
                    out.truncated = true;
                } else {
                    out.rows
                        .push(row.cells().map(|(c, d)| cell(c.column_type(), d)).collect());
                }
            }
            _ => {}
        }
    }
    Ok(out)
}

/// Rows of the first result set of a batch, formatted as text.
async fn fetch(client: &mut MsClient, sql: &str) -> tiberius::Result<Vec<Vec<Option<String>>>> {
    let rows = client
        .simple_query(sql.to_string())
        .await?
        .into_first_result()
        .await?;
    Ok(rows
        .iter()
        .map(|r| r.cells().map(|(c, d)| cell(c.column_type(), d)).collect())
        .collect())
}

fn first_int(rows: &[Vec<Option<String>>]) -> i64 {
    rows.first()
        .and_then(|r| r.first())
        .cloned()
        .flatten()
        .and_then(|v| v.parse().ok())
        .unwrap_or(0)
}

fn parse_after(rows: &[Vec<Option<String>>]) -> After {
    let int = |i: usize| -> i64 {
        rows.first()
            .and_then(|r| r.get(i))
            .cloned()
            .flatten()
            .and_then(|v| v.parse().ok())
            .unwrap_or(0)
    };
    After {
        rowcount: int(0).max(0) as u64,
        trancount: int(1),
        xact_state: int(2),
    }
}

/// `SET ROWCOUNT` also limits INSERT/UPDATE/DELETE and `SELECT ... INTO`, so
/// it is only used when the batch is made of plain queries: it starts with
/// SELECT (or a CTE) and no reserved word that writes or controls flow
/// appears outside literals and comments.
fn row_limit_applies(sql: &str) -> bool {
    const BLOCKING: &[&str] = &[
        "INTO",
        "INSERT",
        "UPDATE",
        "DELETE",
        "MERGE",
        "EXEC",
        "EXECUTE",
        "SET",
        "DECLARE",
        "CREATE",
        "ALTER",
        "DROP",
        "TRUNCATE",
        "BEGIN",
        "COMMIT",
        "ROLLBACK",
        "SAVE",
        "USE",
        "IF",
        "WHILE",
        "BULK",
        "DBCC",
        "OPEN",
        "CLOSE",
        "DEALLOCATE",
        "GRANT",
        "REVOKE",
        "DENY",
        "BACKUP",
        "RESTORE",
    ];
    let first = first_keywords(sql, 1);
    if !matches!(first.first().map(String::as_str), Some("SELECT" | "WITH")) {
        return false;
    }
    !sql_words(sql)
        .iter()
        .any(|w| BLOCKING.contains(&w.as_str()))
}

/// Uppercased words of a T-SQL text, skipping string literals, quoted
/// identifiers (`"x"`, `[x]`) and comments (`--`, nested `/* */`).
fn sql_words(sql: &str) -> Vec<String> {
    let chars: Vec<char> = sql.chars().collect();
    let mut words = Vec::new();
    let mut i = 0;
    let closing = |chars: &[char], mut i: usize, end: char| {
        // Skips to after the closing char; a doubled closing char is an escape.
        while i < chars.len() {
            if chars[i] == end {
                if chars.get(i + 1) == Some(&end) {
                    i += 2;
                    continue;
                }
                return i + 1;
            }
            i += 1;
        }
        i
    };
    while i < chars.len() {
        let c = chars[i];
        if c == '\'' {
            i = closing(&chars, i + 1, '\'');
        } else if c == '"' {
            i = closing(&chars, i + 1, '"');
        } else if c == '[' {
            i = closing(&chars, i + 1, ']');
        } else if c == '-' && chars.get(i + 1) == Some(&'-') {
            while i < chars.len() && chars[i] != '\n' {
                i += 1;
            }
        } else if c == '/' && chars.get(i + 1) == Some(&'*') {
            let mut depth = 0;
            while i < chars.len() {
                if chars[i] == '/' && chars.get(i + 1) == Some(&'*') {
                    depth += 1;
                    i += 2;
                } else if chars[i] == '*' && chars.get(i + 1) == Some(&'/') {
                    depth -= 1;
                    i += 2;
                    if depth == 0 {
                        break;
                    }
                } else {
                    i += 1;
                }
            }
        } else if c.is_alphabetic() || c == '_' || c == '@' || c == '#' {
            let start = i;
            while i < chars.len()
                && (chars[i].is_alphanumeric() || matches!(chars[i], '_' | '@' | '#' | '$'))
            {
                i += 1;
            }
            let word: String = chars[start..i].iter().collect();
            // N'...' is a Unicode literal, not the word N.
            if !(word.eq_ignore_ascii_case("N") && chars.get(i) == Some(&'\'')) {
                words.push(word.to_uppercase());
            }
        } else {
            i += 1;
        }
    }
    words
}

/// T-SQL Unicode string literal.
fn nstr(s: &str) -> String {
    format!("N'{}'", s.replace('\'', "''"))
}

/// 0-based character offset where the given 1-based line starts.
fn line_start(sql: &str, line: u32) -> Option<usize> {
    if line == 0 {
        return None;
    }
    if line == 1 {
        return Some(0);
    }
    let mut current = 1;
    for (pos, c) in sql.chars().enumerate() {
        if c == '\n' {
            current += 1;
            if current == line {
                return Some(pos + 1);
            }
        }
    }
    None
}

fn column_info(c: &Column) -> ColumnInfo {
    let ty = c.column_type();
    ColumnInfo {
        name: c.name().to_string(),
        type_name: type_label(ty).to_string(),
        kind: value_kind(ty),
    }
}

fn type_label(ty: ColumnType) -> &'static str {
    use ColumnType as T;
    match ty {
        T::Null => "null",
        T::Bit | T::Bitn => "bit",
        T::Int1 => "tinyint",
        T::Int2 => "smallint",
        T::Int4 | T::Intn => "int",
        T::Int8 => "bigint",
        T::Float4 => "real",
        T::Float8 | T::Floatn => "float",
        T::Money => "money",
        T::Money4 => "smallmoney",
        T::Decimaln => "decimal",
        T::Numericn => "numeric",
        T::Datetime4 => "smalldatetime",
        T::Datetime | T::Datetimen => "datetime",
        T::Daten => "date",
        T::Timen => "time",
        T::Datetime2 => "datetime2",
        T::DatetimeOffsetn => "datetimeoffset",
        T::Guid => "uniqueidentifier",
        T::BigVarBin => "varbinary",
        T::BigBinary => "binary",
        T::Image => "image",
        T::BigVarChar => "varchar",
        T::BigChar => "char",
        T::NVarchar => "nvarchar",
        T::NChar => "nchar",
        T::Text => "text",
        T::NText => "ntext",
        T::Xml => "xml",
        T::Udt => "udt",
        T::SSVariant => "sql_variant",
    }
}

fn value_kind(ty: ColumnType) -> ValueKind {
    use ColumnType as T;
    match ty {
        T::Int1
        | T::Int2
        | T::Int4
        | T::Int8
        | T::Intn
        | T::Float4
        | T::Float8
        | T::Floatn
        | T::Money
        | T::Money4
        | T::Decimaln
        | T::Numericn => ValueKind::Number,
        T::Bit | T::Bitn => ValueKind::Bool,
        T::Datetime4
        | T::Datetime
        | T::Datetimen
        | T::Daten
        | T::Timen
        | T::Datetime2
        | T::DatetimeOffsetn => ValueKind::Temporal,
        T::BigVarBin | T::BigBinary | T::Image | T::Udt => ValueKind::Binary,
        T::BigVarChar
        | T::BigChar
        | T::NVarchar
        | T::NChar
        | T::Text
        | T::NText
        | T::Xml
        | T::Guid => ValueKind::Text,
        T::Null | T::SSVariant => ValueKind::Other,
    }
}

/// Formats a value the way SSMS shows it, without losing digits.
fn cell(ty: ColumnType, data: &ColumnData<'_>) -> Option<String> {
    match data {
        ColumnData::U8(v) => v.map(|v| v.to_string()),
        ColumnData::I16(v) => v.map(|v| v.to_string()),
        ColumnData::I32(v) => v.map(|v| v.to_string()),
        ColumnData::I64(v) => v.map(|v| v.to_string()),
        ColumnData::F32(v) => v.map(|v| float(v.to_string(), f64::from(v))),
        // money and smallmoney arrive as f64 holding value / 10^4.
        ColumnData::F64(v) if matches!(ty, ColumnType::Money | ColumnType::Money4) => v.map(money),
        ColumnData::F64(v) => v.map(|v| float(v.to_string(), v)),
        ColumnData::Bit(v) => v.map(|b| if b { "true" } else { "false" }.to_string()),
        ColumnData::String(v) => v.as_ref().map(|s| s.to_string()),
        ColumnData::Guid(v) => v.map(|g| g.to_string().to_ascii_uppercase()),
        ColumnData::Binary(v) => v.as_ref().map(|b| hex(b)),
        ColumnData::Numeric(v) => v.map(numeric),
        ColumnData::Xml(v) => v.as_ref().map(|x| x.to_string()),
        ColumnData::DateTime(v) => v.map(datetime),
        ColumnData::SmallDateTime(v) => v.map(smalldatetime),
        ColumnData::Time(v) => v.map(time),
        ColumnData::Date(v) => v.map(date),
        ColumnData::DateTime2(v) => v.map(datetime2),
        ColumnData::DateTimeOffset(v) => v.map(datetimeoffset),
    }
}

/// Shortest round-trip text; exponent form for very large or small values.
fn float(text: String, v: f64) -> String {
    let abs = v.abs();
    if abs != 0.0 && !(1e-5..1e16).contains(&abs) {
        let exp = format!("{v:e}");
        match exp.split_once('e') {
            Some((m, e)) if e.starts_with('-') => format!("{m}E{e}"),
            Some((m, e)) => format!("{m}E+{e}"),
            None => exp,
        }
    } else {
        text
    }
}

fn numeric(n: Numeric) -> String {
    decimal(n.value(), u32::from(n.scale()))
}

fn money(v: f64) -> String {
    // Exact for |value| below 2^53 / 10^4 (about 900 billion).
    decimal((v * 10_000.0).round() as i128, 4)
}

/// Integer `value` scaled by 10^-`scale`, with exactly `scale` decimals.
fn decimal(value: i128, scale: u32) -> String {
    let digits = value.unsigned_abs().to_string();
    let scale = scale as usize;
    let mut s = String::with_capacity(digits.len() + 3);
    if value < 0 {
        s.push('-');
    }
    if scale == 0 {
        s.push_str(&digits);
    } else if digits.len() > scale {
        let (int, frac) = digits.split_at(digits.len() - scale);
        let _ = write!(s, "{int}.{frac}");
    } else {
        let _ = write!(s, "0.{digits:0>scale$}");
    }
    s
}

/// Uppercase hexadecimal with the 0x prefix, as SSMS shows binary values.
fn hex(bytes: &[u8]) -> String {
    let shown = &bytes[..bytes.len().min(BINARY_PREVIEW_BYTES)];
    let mut s = String::with_capacity(shown.len() * 2 + 2);
    s.push_str("0x");
    for b in shown {
        let _ = write!(s, "{b:02X}");
    }
    if bytes.len() > shown.len() {
        let _ = write!(s, "… ({} bytes)", bytes.len());
    }
    s
}

/// Proleptic Gregorian date for a day count relative to 1970-01-01.
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let month = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    let year = yoe + era * 400 + i64::from(month <= 2);
    (year, month, day)
}

fn ymd(days_since_epoch: i64) -> String {
    let (y, m, d) = civil_from_days(days_since_epoch);
    format!("{y:04}-{m:02}-{d:02}")
}

/// `hh:mm:ss[.fffffff]` from 10^-scale second increments since midnight.
fn clock(increments: u64, scale: u8) -> String {
    let pow = 10u64.pow(u32::from(scale));
    let secs = increments / pow;
    let mut s = format!("{:02}:{:02}:{:02}", secs / 3600, secs / 60 % 60, secs % 60);
    if scale > 0 {
        let frac = increments % pow;
        let _ = write!(s, ".{frac:0width$}", width = scale as usize);
    }
    s
}

fn date(d: Date) -> String {
    ymd(i64::from(d.days()) - DAYS_0001_TO_EPOCH)
}

fn time(t: Time) -> String {
    clock(t.increments(), t.scale())
}

fn datetime2(dt: DateTime2) -> String {
    format!("{} {}", date(dt.date()), time(dt.time()))
}

/// The wire value is UTC; SSMS shows the local time with its offset.
fn datetimeoffset(dto: DateTimeOffset) -> String {
    let dt = dto.datetime2();
    let scale = dt.time().scale();
    let pow = i128::from(10u64.pow(u32::from(scale)));
    let day = 86_400 * pow;
    let offset = i128::from(dto.offset());
    let total =
        i128::from(dt.date().days()) * day + i128::from(dt.time().increments()) + offset * 60 * pow;
    let days = total.div_euclid(day) as i64;
    let increments = total.rem_euclid(day) as u64;
    let sign = if offset < 0 { '-' } else { '+' };
    let abs = offset.unsigned_abs();
    format!(
        "{} {} {sign}{:02}:{:02}",
        ymd(days - DAYS_0001_TO_EPOCH),
        clock(increments, scale),
        abs / 60,
        abs % 60
    )
}

/// `datetime` counts 1/300 s; SSMS rounds it to milliseconds (.000/.003/.007).
fn datetime(dt: DateTime) -> String {
    let fragments = dt.seconds_fragments();
    let secs = u64::from(fragments / 300);
    let millis = (u64::from(fragments % 300) * 10 + 1) / 3;
    format!(
        "{} {}.{millis:03}",
        ymd(i64::from(dt.days()) - DAYS_1900_TO_EPOCH),
        clock(secs, 0)
    )
}

/// `smalldatetime` counts whole minutes.
fn smalldatetime(dt: SmallDateTime) -> String {
    format!(
        "{} {}",
        ymd(i64::from(dt.days()) - DAYS_1900_TO_EPOCH),
        clock(u64::from(dt.seconds_fragments()) * 60, 0)
    )
}

fn connection_error(e: tiberius::error::Error) -> AppError {
    let mut err = map_error(e, None);
    err.kind = ErrorKind::Connection;
    err
}

pub fn map_error(e: tiberius::error::Error, sql: Option<&str>) -> AppError {
    use tiberius::error::Error as E;
    match e {
        E::Server(t) => server_error(
            t.code(),
            t.class(),
            t.state(),
            t.message(),
            t.procedure(),
            t.line(),
            sql,
        ),
        E::Io { kind, message } => {
            if matches!(
                kind,
                std::io::ErrorKind::UnexpectedEof
                    | std::io::ErrorKind::ConnectionReset
                    | std::io::ErrorKind::ConnectionAborted
                    | std::io::ErrorKind::BrokenPipe
            ) {
                AppError::connection("The connection to the server was closed.")
            } else {
                AppError::connection(message)
            }
        }
        E::Tls(message) => AppError::connection(format!("TLS: {message}")),
        E::Routing { host, port } => AppError::connection(format!(
            "The server redirected the connection to {host}:{port}."
        )),
        E::Protocol(m) => AppError::connection(format!("Protocol error: {m}")),
        other => AppError::new(ErrorKind::Database, other.to_string()),
    }
}

/// Server error with SSMS-style code (`Msg 208, Level 16, State 1`) and the
/// start of the reported line as position.
fn server_error(
    number: u32,
    class: u8,
    state: u8,
    message: &str,
    procedure: &str,
    line: u32,
    sql: Option<&str>,
) -> AppError {
    let mut err = AppError::new(ErrorKind::Database, message.trim_end());
    err.code = Some(format!("Msg {number}, Level {class}, State {state}"));
    if procedure.is_empty() {
        if line > 0 {
            err.detail = Some(format!("Line {line}"));
        }
        err.position = sql.and_then(|s| line_start(s, line));
    } else {
        err.detail = Some(format!("Procedure {procedure}, Line {line}"));
    }
    if matches!(number, LOGIN_FAILED | CANNOT_OPEN_DATABASE) || class >= 20 {
        err.kind = ErrorKind::Connection;
    }
    err
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    #[test]
    fn row_limit_only_for_plain_queries() {
        assert!(row_limit_applies("select * from t"));
        assert!(row_limit_applies(
            "-- x\nWITH c AS (SELECT 1 a) SELECT * FROM c"
        ));
        assert!(row_limit_applies(
            "SELECT 'insert into' AS x, [update] FROM t"
        ));
        assert!(row_limit_applies("SELECT N'set' FROM t /* delete */"));
        assert!(row_limit_applies(
            "SELECT * FROM t ORDER BY 1 OFFSET 0 ROWS FETCH NEXT 5 ROWS ONLY"
        ));
        assert!(!row_limit_applies("SELECT * INTO #t FROM s"));
        assert!(!row_limit_applies("SELECT 1 UPDATE t SET a = 1"));
        assert!(!row_limit_applies(
            "WITH c AS (SELECT 1 a) DELETE FROM t WHERE a IN (SELECT a FROM c)"
        ));
        assert!(!row_limit_applies("UPDATE t SET a = 1"));
        assert!(!row_limit_applies("EXEC sp_who"));
        assert!(!row_limit_applies("SELECT 1; EXEC p"));
    }

    #[test]
    fn words_skip_literals_and_comments() {
        assert_eq!(
            sql_words("select 'it''s' /* a /* b */ c */ [x]] y] \"q\" -- z\n from @v"),
            ["SELECT", "FROM", "@V"]
        );
    }

    #[test]
    fn line_offsets_are_in_chars() {
        let sql = "SELECT 'ã';\nSELECT x\nFROM t";
        assert_eq!(line_start(sql, 1), Some(0));
        assert_eq!(line_start(sql, 2), Some(12));
        assert_eq!(line_start(sql, 3), Some(21));
        assert_eq!(line_start(sql, 4), None);
        assert_eq!(line_start(sql, 0), None);
    }

    #[test]
    fn server_errors_keep_code_and_line() {
        let e = server_error(
            208,
            16,
            1,
            "Invalid object name 'x'.",
            "",
            2,
            Some("SELECT 1;\nSELECT * FROM x"),
        );
        assert_eq!(e.code.as_deref(), Some("Msg 208, Level 16, State 1"));
        assert_eq!(e.position, Some(10));
        assert_eq!(e.kind, ErrorKind::Database);
        let e = server_error(50000, 16, 1, "boom", "dbo.p", 7, Some("EXEC dbo.p"));
        assert_eq!(e.position, None);
        assert_eq!(e.detail.as_deref(), Some("Procedure dbo.p, Line 7"));
        let e = server_error(18456, 14, 1, "Login failed for user 'x'.", "", 1, None);
        assert_eq!(e.kind, ErrorKind::Connection);
        assert_eq!(e.code.as_deref(), Some("Msg 18456, Level 14, State 1"));
    }

    #[test]
    fn decimals_keep_every_digit() {
        assert_eq!(decimal(123_456_789, 4), "12345.6789");
        assert_eq!(decimal(-50, 2), "-0.50");
        assert_eq!(decimal(5, 3), "0.005");
        assert_eq!(decimal(42, 0), "42");
        assert_eq!(decimal(0, 2), "0.00");
        assert_eq!(
            numeric(Numeric::new_with_scale(
                12_345_678_901_234_567_890_123_456_789_012_345_678,
                10
            )),
            "1234567890123456789012345678.9012345678"
        );
        assert_eq!(money(-12.5), "-12.5000");
        assert_eq!(money(123_456_789.123_4), "123456789.1234");
        assert_eq!(money(0.0001), "0.0001");
    }

    #[test]
    fn floats_use_exponent_only_at_extremes() {
        assert_eq!(float(1.5f64.to_string(), 1.5), "1.5");
        assert_eq!(float(1e20f64.to_string(), 1e20), "1E+20");
        assert_eq!(float(1.5e-7f64.to_string(), 1.5e-7), "1.5E-7");
        assert_eq!(float(0f64.to_string(), 0.0), "0");
    }

    #[test]
    fn dates_and_times() {
        assert_eq!(ymd(0), "1970-01-01");
        assert_eq!(date(Date::new(0)), "0001-01-01");
        assert_eq!(date(Date::new(3_652_058)), "9999-12-31");
        assert_eq!(date(Date::new(738_624)), "2023-04-15");
        assert_eq!(time(Time::new(495_301_234_567, 7)), "13:45:30.1234567");
        assert_eq!(time(Time::new(4_953_012, 2)), "13:45:30.12");
        assert_eq!(time(Time::new(49_530, 0)), "13:45:30");
        assert_eq!(
            datetime2(DateTime2::new(Date::new(738_624), Time::new(0, 3))),
            "2023-04-15 00:00:00.000"
        );
        // 1900-01-01 + 45000 days, 13:45:30 and 299/300 s.
        assert_eq!(
            datetime(DateTime::new(
                45_000,
                (13 * 3600 + 45 * 60 + 30) * 300 + 299
            )),
            "2023-03-17 13:45:30.997"
        );
        assert_eq!(datetime(DateTime::new(0, 2)), "1900-01-01 00:00:00.007");
        assert_eq!(
            datetime(DateTime::new(-53_690, 0)),
            "1753-01-01 00:00:00.000"
        );
        assert_eq!(
            smalldatetime(SmallDateTime::new(45_000, 13 * 60 + 45)),
            "2023-03-17 13:45:00"
        );
    }

    #[test]
    fn datetimeoffset_shows_local_time() {
        // 2023-04-15 22:30:00.123 UTC at -03:00 and +03:00.
        let utc = DateTime2::new(Date::new(738_624), Time::new(81_000_123, 3));
        assert_eq!(
            datetimeoffset(DateTimeOffset::new(utc, -180)),
            "2023-04-15 19:30:00.123 -03:00"
        );
        assert_eq!(
            datetimeoffset(DateTimeOffset::new(utc, 180)),
            "2023-04-16 01:30:00.123 +03:00"
        );
        assert_eq!(
            datetimeoffset(DateTimeOffset::new(utc, 330)),
            "2023-04-16 04:00:00.123 +05:30"
        );
    }

    #[test]
    fn cells_by_type() {
        assert_eq!(
            cell(ColumnType::Bitn, &ColumnData::Bit(Some(true))),
            Some("true".into())
        );
        assert_eq!(
            cell(ColumnType::Money, &ColumnData::F64(Some(-12.5))),
            Some("-12.5000".into())
        );
        assert_eq!(
            cell(ColumnType::Float8, &ColumnData::F64(Some(-12.5))),
            Some("-12.5".into())
        );
        assert_eq!(
            cell(
                ColumnType::BigVarBin,
                &ColumnData::Binary(Some(vec![0x0a, 0xff].into()))
            ),
            Some("0x0AFF".into())
        );
        let g = tiberius::Uuid::parse_str("6f9619ff-8b86-d011-b42d-00c04fc964ff").unwrap();
        assert_eq!(
            cell(ColumnType::Guid, &ColumnData::Guid(Some(g))),
            Some("6F9619FF-8B86-D011-B42D-00C04FC964FF".into())
        );
        assert_eq!(cell(ColumnType::Int4, &ColumnData::I32(None)), None);
    }

    #[test]
    fn literals_and_labels() {
        assert_eq!(nstr("o'neil"), "N'o''neil'");
        assert_eq!(server_label("16.0.4135.4", 3), "SQL Server 16.0.4135.4");
        assert_eq!(server_label("12.0.2000.8", 5), "Azure SQL 12.0.2000.8");
    }

    // ---- Integration tests against a real server (see CONTRIBUTING). ----

    fn test_profile() -> Option<(ConnectionProfile, String)> {
        let host = std::env::var("LQ_MSSQL_HOST").ok()?;
        let profile = ConnectionProfile {
            engine: crate::model::Engine::Sqlserver,
            host,
            port: std::env::var("LQ_MSSQL_PORT")
                .ok()
                .and_then(|p| p.parse().ok())
                .unwrap_or(1433),
            user: std::env::var("LQ_MSSQL_USER").unwrap_or_else(|_| "sa".into()),
            database: std::env::var("LQ_MSSQL_DB").unwrap_or_else(|_| "master".into()),
            ssl_mode: SslMode::Prefer,
            ..Default::default()
        };
        let password = std::env::var("LQ_MSSQL_PASSWORD").unwrap_or_default();
        Some((profile, password))
    }

    async fn open() -> Option<MsSession> {
        let (profile, password) = test_profile()?;
        Some(connect(&profile, Some(&password)).await.expect("connect"))
    }

    async fn run(s: &MsSession, sql: &str, max_rows: usize) -> AppResult<ExecResult> {
        s.execute(&ExecRequest {
            sql: sql.into(),
            max_rows,
        })
        .await
    }

    #[tokio::test]
    #[ignore]
    async fn mssql_session_info() {
        let Some(s) = open().await else { return };
        assert!(
            s.server_version.starts_with("SQL Server "),
            "{}",
            s.server_version
        );
        assert!(s.current_user.is_some());
        assert_eq!(s.current_schema.as_deref(), Some("dbo"));
        let (profile, _) = test_profile().unwrap();
        let err = match connect(&profile, Some("wrong-password")).await {
            Err(e) => e,
            Ok(_) => panic!("login with a wrong password succeeded"),
        };
        assert_eq!(err.kind, ErrorKind::Connection);
        assert_eq!(err.code.as_deref(), Some("Msg 18456, Level 14, State 1"));
    }

    #[tokio::test]
    #[ignore]
    async fn mssql_formats_values() {
        let Some(s) = open().await else { return };
        let r = run(
            &s,
            "SELECT CAST(12345.6789 AS decimal(18,4)), CAST(-0.5 AS numeric(5,2)),
                    CAST(123456789.1234 AS money), CAST(-12.5 AS smallmoney),
                    CAST(1 AS bit), CAST(0 AS bit),
                    CAST('2024-05-01 13:45:30.1234567' AS datetime2(7)),
                    CAST('2024-05-01 13:45:30.12' AS datetime2(2)),
                    CAST('2024-05-01T13:45:30.123+03:00' AS datetimeoffset(3)),
                    CAST('2024-05-01 13:45:30.997' AS datetime),
                    CAST('2024-05-01 13:45' AS smalldatetime),
                    CAST('2024-05-01' AS date), CAST('13:45:30.5' AS time(1)),
                    CAST('6f9619ff-8b86-d011-b42d-00c04fc964ff' AS uniqueidentifier),
                    0x0A0BFF, CAST('<a>1</a>' AS xml), N'ação', CAST(NULL AS int),
                    CAST(1.5 AS float), CAST(9223372036854775807 AS bigint),
                    CAST(255 AS tinyint), CAST(-32768 AS smallint), CAST('x ' AS char(3))",
            0,
        )
        .await
        .expect("select");
        let expected = [
            "12345.6789",
            "-0.50",
            "123456789.1234",
            "-12.5000",
            "true",
            "false",
            "2024-05-01 13:45:30.1234567",
            "2024-05-01 13:45:30.12",
            "2024-05-01 13:45:30.123 +03:00",
            "2024-05-01 13:45:30.997",
            "2024-05-01 13:45:00",
            "2024-05-01",
            "13:45:30.5",
            "6F9619FF-8B86-D011-B42D-00C04FC964FF",
            "0x0A0BFF",
            "<a>1</a>",
            "ação",
        ];
        let row = &r.rows[0];
        for (i, want) in expected.iter().enumerate() {
            assert_eq!(row[i].as_deref(), Some(*want), "column {}", i + 1);
        }
        assert_eq!(row[17], None);
        assert_eq!(row[18].as_deref(), Some("1.5"));
        assert_eq!(row[19].as_deref(), Some("9223372036854775807"));
        assert_eq!(row[20].as_deref(), Some("255"));
        assert_eq!(row[21].as_deref(), Some("-32768"));
        assert_eq!(row[22].as_deref(), Some("x  "));
        let cols = r.columns.unwrap();
        assert_eq!(cols[0].kind, ValueKind::Number);
        assert_eq!(cols[6].kind, ValueKind::Temporal);
        assert_eq!(cols[14].kind, ValueKind::Binary);
    }

    #[tokio::test]
    #[ignore]
    async fn mssql_row_limit() {
        let Some(s) = open().await else { return };
        run(&s, "CREATE TABLE #lq_limit (id int)", 0).await.unwrap();
        let r = run(
            &s,
            "INSERT INTO #lq_limit SELECT TOP (30) 1 FROM sys.all_objects",
            0,
        )
        .await
        .unwrap();
        assert_eq!(r.rows_affected, Some(30));

        let r = run(&s, "SELECT * FROM #lq_limit", 10).await.unwrap();
        assert_eq!(r.rows.len(), 10);
        assert!(r.truncated);
        let r = run(&s, "SELECT TOP (10) * FROM #lq_limit", 10)
            .await
            .unwrap();
        assert_eq!(r.rows.len(), 10);
        assert!(!r.truncated);

        // ROWCOUNT was reset: the UPDATE touches every row.
        let r = run(&s, "UPDATE #lq_limit SET id = 2", 10).await.unwrap();
        assert_eq!(r.rows_affected, Some(30));
        // Not a plain SELECT: limited on the client, rows still all inserted.
        let r = run(&s, "SELECT * INTO #lq_copy FROM #lq_limit", 10)
            .await
            .unwrap();
        assert_eq!(r.rows_affected, Some(30));
        let r = run(&s, "EXEC('SELECT * FROM #lq_copy')", 5).await.unwrap();
        assert_eq!(r.rows.len(), 5);
        assert!(r.truncated);
    }

    #[tokio::test]
    #[ignore]
    async fn mssql_error_code_and_position() {
        let Some(s) = open().await else { return };
        let err = run(&s, "SELECT 1;\nSELECT * FROM dbo.lq_missing_table", 0)
            .await
            .unwrap_err();
        assert_eq!(err.kind, ErrorKind::Database);
        assert_eq!(err.code.as_deref(), Some("Msg 208, Level 16, State 1"));
        assert_eq!(err.position, Some(10));
        // The session keeps working.
        let r = run(&s, "SELECT 1", 0).await.unwrap();
        assert_eq!(r.rows[0][0].as_deref(), Some("1"));
        // PRINT runs; tiberius does not surface its text.
        run(&s, "PRINT 'hello'", 0).await.unwrap();
    }

    #[tokio::test]
    #[ignore]
    async fn mssql_manual_transactions() {
        let Some(s) = open().await else { return };
        run(&s, "CREATE TABLE #lq_tx (id int)", 0).await.unwrap();

        let st = s.set_autocommit(false).await.unwrap();
        assert!(!st.autocommit && !st.in_transaction);
        let r = run(&s, "INSERT INTO #lq_tx VALUES (1)", 0).await.unwrap();
        assert!(r.in_transaction);
        let st = s.end_transaction(false).await.unwrap();
        assert!(!st.in_transaction);
        let r = run(&s, "SELECT COUNT(*) FROM #lq_tx", 0).await.unwrap();
        assert_eq!(r.rows[0][0].as_deref(), Some("0"));

        run(&s, "INSERT INTO #lq_tx VALUES (2)", 0).await.unwrap();
        let st = s.end_transaction(true).await.unwrap();
        assert!(!st.in_transaction);

        run(&s, "INSERT INTO #lq_tx VALUES (3)", 0).await.unwrap();
        assert!(s.tx_state().in_transaction);
        // Back to autocommit commits pending work.
        let st = s.set_autocommit(true).await.unwrap();
        assert!(st.autocommit && !st.in_transaction);
        let r = run(&s, "SELECT COUNT(*) FROM #lq_tx", 0).await.unwrap();
        assert_eq!(r.rows[0][0].as_deref(), Some("2"));
        assert!(!r.in_transaction);

        // Explicit BEGIN TRAN in autocommit mode is tracked too.
        let r = run(&s, "BEGIN TRAN", 0).await.unwrap();
        assert!(r.in_transaction);
        let r = run(&s, "ROLLBACK", 0).await.unwrap();
        assert!(!r.in_transaction);
    }

    #[tokio::test]
    #[ignore]
    async fn mssql_metadata() {
        let Some(s) = open().await else { return };
        run(
            &s,
            "DROP TABLE IF EXISTS dbo.lq_meta;
             CREATE TABLE dbo.lq_meta (id int NOT NULL PRIMARY KEY, name nvarchar(40) NULL,
                                       amount decimal(10,2), data varbinary(max),
                                       at datetime2(3))",
            0,
        )
        .await
        .unwrap();
        let schemas = s.list_schemas().await.unwrap();
        assert_eq!(schemas.first().map(String::as_str), Some("dbo"));
        assert!(!schemas.iter().any(|n| n == "db_owner"));
        let objects = s.list_objects("dbo").await.unwrap();
        assert!(objects
            .iter()
            .any(|o| o.name == "lq_meta" && o.kind == "table"));
        let sys = s.list_objects("sys").await.unwrap();
        assert!(sys.iter().any(|o| o.name == "objects" && o.kind == "view"));
        let cols = s.list_columns("dbo", "lq_meta").await.unwrap();
        let types: Vec<&str> = cols.iter().map(|c| c.type_name.as_str()).collect();
        assert_eq!(
            types,
            [
                "int",
                "nvarchar(40)",
                "decimal(10,2)",
                "varbinary(max)",
                "datetime2(3)"
            ]
        );
        assert!(cols[0].primary_key && !cols[0].nullable);
        assert!(!cols[1].primary_key && cols[1].nullable);
        run(&s, "DROP TABLE dbo.lq_meta", 0).await.unwrap();
    }

    #[tokio::test]
    #[ignore]
    async fn mssql_cancel() {
        let Some(s) = open().await else { return };
        let s = Arc::new(s);
        let running = {
            let s = s.clone();
            tokio::spawn(async move { run(&s, "WAITFOR DELAY '00:00:30'", 0).await })
        };
        tokio::time::sleep(Duration::from_secs(1)).await;
        s.cancel().await.unwrap();
        let err = running.await.unwrap().unwrap_err();
        assert_eq!(err.kind, ErrorKind::Cancelled);
        let r = run(&s, "SELECT 2", 0).await.unwrap();
        assert_eq!(r.rows[0][0].as_deref(), Some("2"));
    }
}
