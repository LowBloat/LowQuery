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
//! - Grid editing: plain queries are described by the server
//!   (`sys.dm_exec_describe_first_result_set` in browse mode) together with
//!   the unique keys of the source table, in one extra call. Edits are
//!   applied with `sp_executesql` and explicit, style-fixed conversions.

use std::fmt::Write as _;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use futures_util::TryStreamExt;
use parking_lot::Mutex;
use tiberius::numeric::Numeric;
use tiberius::time::{Date, DateTime, DateTime2, DateTimeOffset, SmallDateTime, Time};
use tiberius::{
    AuthMethod, Client, Column, ColumnData, ColumnType, Config, EncryptionLevel, QueryItem, Row,
    SqlBrowser, ToSql,
};
use tokio::net::TcpStream;
use tokio_util::compat::{Compat, TokioAsyncWriteCompatExt};

use super::{first_keywords, TxState};
use crate::error::{AppError, AppResult, ErrorKind};
use crate::model::{
    ApplyRequest, ApplyResult, ColumnInfo, ConnectionProfile, DbColumn, DbObject, EditColumn,
    EditTarget, ExecRequest, ExecResult, RowChange, SslMode, ValueKind,
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
const ONLY_TABLE_ROWS: &str = "Read-only: only rows from a table can be edited.";
/// Marks a binary value cut by `hex`.
const PREVIEW_MARK: &str = "… (";

/// Describes the first result set of `@P1` in browse mode (base tables, hidden
/// key columns) and lists the unique keys of the tables it reads, in one
/// call. Describe errors come back as rows, not as raised errors. A
/// transaction opened only by this call (manual-commit mode) is ended here.
const DESCRIBE_SQL: &str = "DECLARE @lq_tc int = @@TRANCOUNT;
DECLARE @lq_d TABLE (ord int, hidden bit, other_db bit, sch nvarchar(128), tab nvarchar(128),
                     col nvarchar(128), type_name nvarchar(256), computed bit, err nvarchar(4000));
INSERT INTO @lq_d
SELECT column_ordinal, is_hidden,
       CASE WHEN source_server IS NOT NULL OR source_database <> DB_NAME() THEN 1 ELSE 0 END,
       source_schema, source_table, source_column,
       COALESCE(system_type_name, CASE WHEN user_type_schema = N'sys' THEN user_type_name END),
       is_computed_column, error_message
FROM sys.dm_exec_describe_first_result_set(@P1, NULL, 1);
SELECT hidden, other_db, sch, tab, col, type_name, computed, err FROM @lq_d ORDER BY ord;
SELECT o.sch, o.tab, i.index_id, i.is_primary_key, c.name, c.is_nullable
FROM (SELECT DISTINCT sch, tab FROM @lq_d WHERE tab IS NOT NULL AND other_db = 0) o
JOIN sys.indexes i ON i.object_id = OBJECT_ID(QUOTENAME(o.sch) + N'.' + QUOTENAME(o.tab))
JOIN sys.index_columns ic ON ic.object_id = i.object_id AND ic.index_id = i.index_id
 AND ic.key_ordinal > 0
JOIN sys.columns c ON c.object_id = ic.object_id AND c.column_id = ic.column_id
WHERE i.is_unique = 1 AND i.has_filter = 0 AND i.is_disabled = 0 AND i.is_hypothetical = 0
ORDER BY o.sch, o.tab, i.is_primary_key DESC, i.index_id, ic.key_ordinal;
IF @@TRANCOUNT > @lq_tc COMMIT TRAN;";

pub struct MsSession {
    client: tokio::sync::Mutex<MsClient>,
    /// Kept to reopen the connection when a cancel cannot be acknowledged.
    profile: ConnectionProfile,
    password: Option<String>,
    cancel: tokio::sync::Notify,
    broken: AtomicBool,
    state: Mutex<TxState>,
    pub server_version: String,
    pub current_user: Option<String>,
    pub current_schema: Option<String>,
}

pub async fn connect(profile: &ConnectionProfile, password: Option<&str>) -> AppResult<MsSession> {
    let mut client = dial(profile, password).await?;
    let rows = fetch(
        &mut client,
        "SELECT CAST(SERVERPROPERTY('ProductVersion') AS nvarchar(128)),
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
        profile: profile.clone(),
        password: password.map(str::to_string),
        cancel: tokio::sync::Notify::new(),
        broken: AtomicBool::new(false),
        state: Mutex::new(TxState {
            autocommit: true,
            in_transaction: false,
        }),
    })
}

/// Opens a connection with the session defaults applied.
async fn dial(profile: &ConnectionProfile, password: Option<&str>) -> AppResult<MsClient> {
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
    fetch(
        &mut client,
        "SET TEXTSIZE 2147483647; SET IMPLICIT_TRANSACTIONS OFF; SELECT 1",
    )
    .await
    .map_err(connection_error)?;
    Ok(client)
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
    /// Applies grid edits as one unit. With no transaction open, they run in
    /// one that commits (autocommit) or stays open (manual commit). Inside an
    /// open transaction they run after a savepoint, so a failure undoes only
    /// them.
    pub async fn apply_changes(&self, req: &ApplyRequest) -> AppResult<ApplyResult> {
        let target = &req.target;
        let table = qualified(target.schema.as_deref(), &target.table);
        let mut client = self.lock().await?;
        let autocommit = self.state.lock().autocommit;
        let started = Instant::now();

        // State, identity columns, and the transaction or savepoint in one
        // trip. Sent as a plain batch: BEGIN TRAN inside sp_executesql would
        // change the transaction count of the call.
        let start = format!(
            "DECLARE @lq_tc int = @@TRANCOUNT, @lq_xs int = XACT_STATE();
             SELECT @lq_tc, @lq_xs;
             SELECT name FROM sys.identity_columns WHERE object_id = OBJECT_ID({});
             IF @lq_xs = 1 SAVE TRAN lq_apply;
             ELSE IF @lq_xs = 0 AND @@TRANCOUNT = 0 BEGIN TRAN;",
            nstr(&table)
        );
        let sets = fetch_all(&mut client, &start)
            .await
            .map_err(|e| self.fail(e, None))?;
        let state_rows = sets.first().map(Vec::as_slice).unwrap_or_default();
        let (trancount, xact_state) = (int_at(state_rows, 0), int_at(state_rows, 1));
        if xact_state == -1 {
            return Err(doomed());
        }
        let identity: Vec<String> = sets
            .get(1)
            .map(|rows| rows.iter().filter_map(|r| r.first().cloned().flatten()))
            .into_iter()
            .flatten()
            .collect();
        // Whether this call opened the transaction (or the implicit one).
        let fresh = trancount == 0;

        let outcome = self.run_changes(&mut client, req, &identity).await;
        let end = match &outcome {
            // A commit that cannot complete is rolled back and reported.
            Ok(_) if fresh && autocommit => {
                "DECLARE @lq_ok int = 1;
                 WHILE @@TRANCOUNT > 0 AND XACT_STATE() = 1 COMMIT TRAN;
                 IF @@TRANCOUNT > 0 BEGIN SET @lq_ok = 0; ROLLBACK TRAN; END;
                 SELECT @@TRANCOUNT, XACT_STATE(), @lq_ok"
            }
            Ok(_) => "SELECT @@TRANCOUNT, XACT_STATE(), 1",
            // Everything since the start belongs to this call.
            Err(_) if fresh => {
                "IF @@TRANCOUNT > 0 ROLLBACK TRAN; SELECT @@TRANCOUNT, XACT_STATE(), 0"
            }
            Err(_) => {
                "IF XACT_STATE() = 1 ROLLBACK TRAN lq_apply;
                 SELECT @@TRANCOUNT, XACT_STATE(), 0"
            }
        };
        // Transaction count, XACT_STATE() and whether the commit completed.
        let after = if self.broken.load(Ordering::SeqCst) {
            None
        } else {
            match fetch(&mut client, end).await {
                Ok(rows) => Some((int_at(&rows, 0), int_at(&rows, 1), int_at(&rows, 2))),
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
        if let Some((trancount, _, _)) = after {
            self.state.lock().in_transaction = trancount > 0;
        }

        let (updated, deleted, inserted) = match outcome {
            Ok(counts) => counts,
            Err(mut err) => {
                if after.is_some_and(|(_, xact_state, _)| xact_state == -1) {
                    err.hint =
                        Some("The transaction can no longer be committed; roll it back.".into());
                }
                return Err(err);
            }
        };
        if after.is_some_and(|(_, _, committed)| committed == 0) {
            return Err(AppError::new(
                ErrorKind::Database,
                "The changes could not be committed and were rolled back.",
            ));
        }
        Ok(ApplyResult {
            updated,
            deleted,
            inserted,
            elapsed_ms: started.elapsed().as_millis() as u64,
            in_transaction: self.state.lock().in_transaction,
        })
    }

    /// Runs every change; stops at the first failure. Returns the updated,
    /// deleted and inserted counts.
    async fn run_changes(
        &self,
        client: &mut MsClient,
        req: &ApplyRequest,
        identity: &[String],
    ) -> AppResult<(u64, u64, u64)> {
        let statements = req
            .changes
            .iter()
            .map(|c| change_sql(&req.target, c, identity))
            .collect::<AppResult<Vec<_>>>()?;
        let (mut updated, mut deleted, mut inserted) = (0, 0, 0);
        for (i, statement) in statements.iter().enumerate() {
            let Some(statement) = statement else { continue };
            let row = i + 1;
            let params: Vec<&dyn ToSql> =
                statement.params.iter().map(|p| p as &dyn ToSql).collect();
            let sql = format!("{};\nSELECT ROWCOUNT_BIG();", statement.sql);
            let results = match client.query(sql, &params).await {
                Ok(stream) => stream.into_results().await,
                Err(e) => Err(e),
            };
            let results = results.map_err(|e| {
                let mut err = self.fail(e, None);
                err.position = None;
                err.detail = Some(format!("Row {row} was not saved."));
                err
            })?;
            let count = results
                .last()
                .and_then(|rows| rows.first())
                .and_then(|r| r.try_get::<i64, _>(0).ok().flatten())
                .unwrap_or(0);
            match statement.op {
                ChangeOp::Insert => inserted += 1,
                op => {
                    if count != 1 {
                        let why = if count == 0 {
                            "0 rows matched its key (changed or removed by someone else)".into()
                        } else {
                            format!("{count} rows matched its key")
                        };
                        return Err(AppError::new(
                            ErrorKind::Database,
                            format!("Row {row} was not saved: {why}."),
                        ));
                    }
                    if op == ChangeOp::Update {
                        updated += 1;
                    } else {
                        deleted += 1;
                    }
                }
            }
        }
        Ok((updated, deleted, inserted))
    }

    /// Decides whether the rows of a result can be edited: the server
    /// describes the query and reports the source table's unique keys.
    /// Any failure only makes the result read-only.
    async fn detect_edit(
        &self,
        client: &mut MsClient,
        sql: &str,
        columns: usize,
        after: &After,
    ) -> Result<EditTarget, String> {
        if !row_limit_applies(sql) {
            return Err(ONLY_TABLE_ROWS.into());
        }
        if sql_words(sql)
            .iter()
            .any(|w| matches!(w.as_str(), "UNION" | "EXCEPT" | "INTERSECT"))
        {
            return Err("Read-only: the result combines several queries.".into());
        }
        if after.xact_state == -1 {
            return Err("Read-only: the transaction must be rolled back first.".into());
        }
        let results = match client.query(DESCRIBE_SQL, &[&sql]).await {
            Ok(stream) => stream.into_results().await,
            Err(e) => Err(e),
        };
        let sets = match results {
            Ok(sets) => texts(&sets),
            Err(e) => {
                // Only marks the session broken when the stream failed.
                self.fail(e, None);
                return Err(ONLY_TABLE_ROWS.into());
            }
        };
        let text = |r: &Vec<Option<String>>, i: usize| r.get(i).cloned().flatten();
        let flag =
            |r: &Vec<Option<String>>, i: usize| matches!(text(r, i).as_deref(), Some("true" | "1"));
        let described: Vec<Described> = sets
            .first()
            .map(Vec::as_slice)
            .unwrap_or_default()
            .iter()
            .map(|r| Described {
                hidden: flag(r, 0),
                other_db: flag(r, 1),
                schema: text(r, 2),
                table: text(r, 3),
                column: text(r, 4),
                type_name: text(r, 5),
                computed: flag(r, 6),
                error: text(r, 7),
            })
            .collect();
        let keys: Vec<KeyPart> = sets
            .get(1)
            .map(Vec::as_slice)
            .unwrap_or_default()
            .iter()
            .map(|r| KeyPart {
                schema: text(r, 0).unwrap_or_default(),
                table: text(r, 1).unwrap_or_default(),
                index: text(r, 2).and_then(|v| v.parse().ok()).unwrap_or(0),
                primary: flag(r, 3),
                column: text(r, 4).unwrap_or_default(),
                nullable: flag(r, 5),
            })
            .collect();
        edit_target(&described, &keys, columns)
    }

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
                let acked = tokio::time::timeout(CANCEL_TIMEOUT, client.cancel_query()).await;
                if matches!(acked, Ok(Ok(()))) {
                    Err(AppError::new(
                        ErrorKind::Cancelled,
                        "The statement was cancelled.",
                    ))
                } else {
                    // The server may send the attention acknowledgement in a
                    // separate message that the driver does not read, which
                    // would leave every later response one message behind.
                    // A fresh connection is the only safe state.
                    match self.reopen(&mut client).await {
                        Ok(()) => {
                            let mut err =
                                AppError::new(ErrorKind::Cancelled, "The statement was cancelled.");
                            err.detail = Some(
                                "LowQuery reopened the connection to finish the cancel: temporary tables, session settings and uncommitted work of this tab were discarded."
                                    .into(),
                            );
                            Err(err)
                        }
                        Err(e) => {
                            self.broken.store(true, Ordering::SeqCst);
                            let mut err = AppError::new(
                                ErrorKind::Cancelled,
                                "The statement was cancelled, but the connection could not be reopened. Reconnect to continue.",
                            );
                            err.detail = Some(e.message);
                            Err(err)
                        }
                    }
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
                    // Keep the original error, but say why the session broke.
                    if let Err(original) = &outcome {
                        if self.broken.load(Ordering::SeqCst) && original.detail.is_none() {
                            return Err(AppError {
                                detail: Some(err.message),
                                ..original.clone()
                            });
                        }
                    }
                    None
                }
            }
        };
        // Whether the grid may edit the rows, while the connection is held.
        let edit = match (&outcome, &after) {
            (
                Ok(Batch {
                    columns: Some(cols),
                    ..
                }),
                Some(after),
            ) => Some(
                self.detect_edit(&mut client, &req.sql, cols.len(), after)
                    .await,
            ),
            _ => None,
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
        match edit {
            Some(Ok(target)) => result.editable = Some(target),
            Some(Err(reason)) => result.read_only_reason = Some(reason),
            None => {}
        }
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

    /// Replaces the connection with a new one, keeping the autocommit mode.
    async fn reopen(&self, client: &mut MsClient) -> AppResult<()> {
        let mut fresh = dial(&self.profile, self.password.as_deref()).await?;
        if !self.state.lock().autocommit {
            fetch(&mut fresh, "SET IMPLICIT_TRANSACTIONS ON; SELECT 1")
                .await
                .map_err(connection_error)?;
        }
        *client = fresh;
        self.state.lock().in_transaction = false;
        Ok(())
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

/// Every result set of a plain batch, formatted as text.
async fn fetch_all(
    client: &mut MsClient,
    sql: &str,
) -> tiberius::Result<Vec<Vec<Vec<Option<String>>>>> {
    let sets = client
        .simple_query(sql.to_string())
        .await?
        .into_results()
        .await?;
    Ok(texts(&sets))
}

fn texts(sets: &[Vec<Row>]) -> Vec<Vec<Vec<Option<String>>>> {
    sets.iter()
        .map(|rows| {
            rows.iter()
                .map(|r| r.cells().map(|(c, d)| cell(c.column_type(), d)).collect())
                .collect()
        })
        .collect()
}

/// Integer in column `i` of the first row, 0 when missing.
fn int_at(rows: &[Vec<Option<String>>], i: usize) -> i64 {
    rows.first()
        .and_then(|r| r.get(i))
        .cloned()
        .flatten()
        .and_then(|v| v.parse().ok())
        .unwrap_or(0)
}

fn first_int(rows: &[Vec<Option<String>>]) -> i64 {
    int_at(rows, 0)
}

fn parse_after(rows: &[Vec<Option<String>>]) -> After {
    After {
        rowcount: int_at(rows, 0).max(0) as u64,
        trancount: int_at(rows, 1),
        xact_state: int_at(rows, 2),
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

/// Bracket-quoted identifier.
fn quote_ident(name: &str) -> String {
    format!("[{}]", name.replace(']', "]]"))
}

fn qualified(schema: Option<&str>, table: &str) -> String {
    match schema {
        Some(s) if !s.is_empty() => format!("{}.{}", quote_ident(s), quote_ident(table)),
        _ => quote_ident(table),
    }
}

/// One result column as the server describes it.
#[derive(Debug, Clone, Default)]
struct Described {
    /// Key column added by browse mode, not part of the result.
    hidden: bool,
    /// Comes from another database or a linked server.
    other_db: bool,
    schema: Option<String>,
    table: Option<String>,
    column: Option<String>,
    type_name: Option<String>,
    computed: bool,
    error: Option<String>,
}

/// One column of a unique index of a source table.
#[derive(Debug, Clone, Default)]
struct KeyPart {
    schema: String,
    table: String,
    index: i64,
    primary: bool,
    column: String,
    nullable: bool,
}

/// The table behind a result, or why its rows cannot be edited.
fn edit_target(
    described: &[Described],
    keys: &[KeyPart],
    result_columns: usize,
) -> Result<EditTarget, String> {
    let generic = || ONLY_TABLE_ROWS.to_string();
    if described.iter().any(|c| c.error.is_some()) {
        return Err(generic());
    }
    let visible: Vec<&Described> = described.iter().filter(|c| !c.hidden).collect();
    if visible.is_empty() || visible.len() != result_columns {
        return Err(generic());
    }
    let mut source: Option<(&str, &str)> = None;
    for c in &visible {
        let (Some(schema), Some(table), Some(_)) =
            (c.schema.as_deref(), c.table.as_deref(), c.column.as_deref())
        else {
            continue;
        };
        if c.other_db {
            return Err("Read-only: the table is in another database.".into());
        }
        match source {
            None => source = Some((schema, table)),
            Some(s) if s != (schema, table) => {
                return Err("Read-only: the result combines columns from several tables.".into())
            }
            _ => {}
        }
    }
    let (schema, table) = source.ok_or_else(generic)?;
    let of_table =
        |c: &Described| c.schema.as_deref() == Some(schema) && c.table.as_deref() == Some(table);

    // Plain, writable columns of the table; a repeated column is edited
    // through its first occurrence only.
    let mut columns: Vec<Option<EditColumn>> = Vec::with_capacity(visible.len());
    let mut seen: Vec<&str> = Vec::new();
    for c in &visible {
        let editable = match (c.column.as_deref(), c.type_name.as_deref()) {
            (Some(name), Some(ty))
                if of_table(c)
                    && !c.computed
                    && !seen.contains(&name)
                    && value_expr(ty, 1).is_ok() =>
            {
                seen.push(name);
                Some(EditColumn {
                    name: name.into(),
                    type_name: ty.into(),
                })
            }
            _ => None,
        };
        columns.push(editable);
    }

    // Unique indexes of the table, in the order the server listed them
    // (primary key first).
    let mut candidates: Vec<(bool, Vec<&KeyPart>)> = Vec::new();
    for k in keys
        .iter()
        .filter(|k| k.schema == schema && k.table == table)
    {
        match candidates.last_mut() {
            Some((_, parts)) if parts[0].index == k.index => parts.push(k),
            _ => candidates.push((k.primary, vec![k])),
        }
    }
    let primary = candidates.iter().any(|(p, _)| *p);
    candidates.retain(|(p, parts)| {
        if primary {
            *p
        } else {
            parts.iter().all(|k| !k.nullable)
        }
    });
    let Some((_, first)) = candidates.first() else {
        return Err(format!("Read-only: table {table} has no primary key."));
    };
    for (_, parts) in &candidates {
        let positions: Option<Vec<usize>> = parts
            .iter()
            .map(|k| {
                columns
                    .iter()
                    .position(|c| c.as_ref().is_some_and(|c| c.name == k.column))
            })
            .collect();
        let Some(key) = positions else { continue };
        // A key shown twice, or one browse mode had to add, means the table
        // appears more than once (a self join): rows would be ambiguous.
        let shown_twice = parts.iter().any(|k| {
            visible
                .iter()
                .filter(|c| of_table(c) && c.column.as_deref() == Some(k.column.as_str()))
                .count()
                > 1
        });
        let added = described.iter().any(|c| {
            c.hidden
                && of_table(c)
                && parts
                    .iter()
                    .any(|k| c.column.as_deref() == Some(k.column.as_str()))
        });
        if shown_twice || added {
            return Err(format!(
                "Read-only: the result shows table {table} more than once."
            ));
        }
        return Ok(EditTarget {
            schema: Some(schema.into()),
            table: table.into(),
            columns,
            key,
        });
    }
    let names: Vec<&str> = first.iter().map(|k| k.column.as_str()).collect();
    Err(format!(
        "Read-only: add the {} columns ({}) to the query to edit rows.",
        if primary { "primary key" } else { "key" },
        names.join(", ")
    ))
}

/// Base name (lowercase) and arguments of a type such as `decimal(10,2)`;
/// `None` when the text is not a plain type name.
fn parse_type(type_name: &str) -> Option<(String, Vec<String>)> {
    let t = type_name.trim();
    let (base, args) = match t.split_once('(') {
        Some((base, rest)) => (base.trim(), Some(rest.strip_suffix(')')?)),
        None => (t, None),
    };
    if base.is_empty() || !base.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
        return None;
    }
    let args = match args {
        Some(a) => a
            .split(',')
            .map(|p| {
                let p = p.trim().to_ascii_lowercase();
                (p == "max" || (!p.is_empty() && p.chars().all(|c| c.is_ascii_digit())))
                    .then_some(p)
            })
            .collect::<Option<Vec<_>>>()?,
        None => Vec::new(),
    };
    Some((base.to_ascii_lowercase(), args))
}

fn is_binary_type(base: &str) -> bool {
    matches!(
        base,
        "binary" | "varbinary" | "image" | "hierarchyid" | "geometry" | "geography"
    )
}

/// SQL that converts parameter `@P{n}` (text in the driver's display format)
/// to the column type. Styles are explicit, so DATEFORMAT and LANGUAGE never
/// change the result.
fn value_expr(type_name: &str, n: usize) -> AppResult<String> {
    let unsupported =
        || AppError::invalid(format!("Columns of type {type_name} cannot be edited."));
    let (base, args) = parse_type(type_name).ok_or_else(unsupported)?;
    let ty = if args.is_empty() {
        base.clone()
    } else {
        format!("{base}({})", args.join(","))
    };
    let p = format!("@P{n}");
    Ok(match base.as_str() {
        // Plain-text forms of these types do not depend on session settings;
        // bit takes true/false or 1/0.
        "bit" | "tinyint" | "smallint" | "int" | "bigint" | "decimal" | "numeric" | "money"
        | "smallmoney" | "float" | "real" | "uniqueidentifier" | "time" => {
            format!("CAST({p} AS {ty})")
        }
        // ODBC canonical (yyyy-mm-dd hh:mi:ss.fffffff [+hh:mm]).
        "date" | "datetime" | "datetime2" | "smalldatetime" | "datetimeoffset" => {
            format!("CONVERT({ty}, {p}, 121)")
        }
        // The nvarchar parameter converts to the column on assignment.
        "char" | "varchar" | "text" | "nchar" | "nvarchar" | "ntext" | "sysname" | "xml" => p,
        // Style 1: 0x-prefixed hexadecimal.
        "binary" | "varbinary" => format!("CONVERT({ty}, {p}, 1)"),
        "image" => format!("CONVERT(varbinary(max), {p}, 1)"),
        "hierarchyid" | "geometry" | "geography" => {
            format!("CAST(CONVERT(varbinary(max), {p}, 1) AS {ty})")
        }
        // timestamp/rowversion, sql_variant, user CLR types.
        _ => return Err(unsupported()),
    })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ChangeOp {
    Update,
    Delete,
    Insert,
}

/// One statement of an apply with its parameters (`@P1`...).
#[derive(Debug)]
struct ChangeSql {
    op: ChangeOp,
    sql: String,
    params: Vec<Option<String>>,
}

fn target_column(target: &EditTarget, index: usize) -> AppResult<&EditColumn> {
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

/// Binds a value and returns the expression that converts it.
fn bind(
    params: &mut Vec<Option<String>>,
    column: &EditColumn,
    value: &Option<String>,
) -> AppResult<String> {
    if let Some(v) = value {
        let binary = parse_type(&column.type_name).is_some_and(|(b, _)| is_binary_type(&b));
        if binary && v.contains(PREVIEW_MARK) {
            return Err(AppError::invalid(
                "This value is only a preview and cannot be saved.",
            ));
        }
    }
    params.push(value.clone());
    value_expr(&column.type_name, params.len())
}

fn where_key(
    target: &EditTarget,
    key: &[Option<String>],
    params: &mut Vec<Option<String>>,
) -> AppResult<String> {
    if key.len() != target.key.len() {
        return Err(AppError::invalid("The row key does not match the result."));
    }
    let mut parts = Vec::with_capacity(key.len());
    for (&index, value) in target.key.iter().zip(key) {
        let column = target_column(target, index)?;
        let name = quote_ident(&column.name);
        parts.push(match value {
            None => format!("{name} IS NULL"),
            Some(_) => format!("{name} = {}", bind(params, column, value)?),
        });
    }
    Ok(parts.join(" AND "))
}

/// Edited values by table column, the last one winning.
fn assignments<'a>(
    target: &'a EditTarget,
    values: &'a [crate::model::CellValue],
) -> AppResult<Vec<(&'a EditColumn, &'a Option<String>)>> {
    let mut out: Vec<(&EditColumn, &Option<String>)> = Vec::new();
    for v in values {
        let column = target_column(target, v.column)?;
        match out.iter_mut().find(|(c, _)| c.name == column.name) {
            Some(slot) => slot.1 = &v.value,
            None => out.push((column, &v.value)),
        }
    }
    Ok(out)
}

/// Statement for one grid change; `None` for an update with nothing to set.
fn change_sql(
    target: &EditTarget,
    change: &RowChange,
    identity: &[String],
) -> AppResult<Option<ChangeSql>> {
    let table = qualified(target.schema.as_deref(), &target.table);
    let mut params = Vec::new();
    let (op, sql) = match change {
        RowChange::Update { key, values } => {
            let values = assignments(target, values)?;
            if values.is_empty() {
                return Ok(None);
            }
            let mut sets = Vec::with_capacity(values.len());
            for (column, value) in values {
                if identity.contains(&column.name) {
                    return Err(AppError::invalid(format!(
                        "Column {} is an identity column and cannot be changed.",
                        column.name
                    )));
                }
                let expr = bind(&mut params, column, value)?;
                sets.push(format!("{} = {expr}", quote_ident(&column.name)));
            }
            let filter = where_key(target, key, &mut params)?;
            (
                ChangeOp::Update,
                format!("UPDATE {table} SET {} WHERE {filter}", sets.join(", ")),
            )
        }
        RowChange::Delete { key } => {
            let filter = where_key(target, key, &mut params)?;
            (
                ChangeOp::Delete,
                format!("DELETE FROM {table} WHERE {filter}"),
            )
        }
        RowChange::Insert { values } => {
            let mut names = Vec::new();
            let mut exprs = Vec::new();
            for (column, value) in assignments(target, values)? {
                if identity.contains(&column.name) {
                    if value.is_none() {
                        continue;
                    }
                    return Err(AppError::invalid(format!(
                        "Column {} is an identity column: leave it empty so the server numbers the row (an explicit value needs IDENTITY_INSERT).",
                        column.name
                    )));
                }
                exprs.push(bind(&mut params, column, value)?);
                names.push(quote_ident(&column.name));
            }
            let sql = if names.is_empty() {
                format!("INSERT INTO {table} DEFAULT VALUES")
            } else {
                format!(
                    "INSERT INTO {table} ({}) VALUES ({})",
                    names.join(", "),
                    exprs.join(", ")
                )
            };
            (ChangeOp::Insert, sql)
        }
    };
    Ok(Some(ChangeSql { op, sql, params }))
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

    fn col(table: &str, column: &str, ty: &str) -> Described {
        Described {
            schema: Some("dbo".into()),
            table: Some(table.into()),
            column: Some(column.into()),
            type_name: Some(ty.into()),
            ..Default::default()
        }
    }

    fn expr(ty: &str) -> Described {
        Described {
            type_name: Some(ty.into()),
            ..Default::default()
        }
    }

    fn key(table: &str, index: i64, primary: bool, column: &str, nullable: bool) -> KeyPart {
        KeyPart {
            schema: "dbo".into(),
            table: table.into(),
            index,
            primary,
            column: column.into(),
            nullable,
        }
    }

    #[test]
    fn detection_finds_table_and_key() {
        let described = [
            col("t", "name", "nvarchar(40)"),
            expr("int"),
            col("t", "b", "int"),
            col("t", "a", "int"),
            Described {
                computed: true,
                ..col("t", "total", "int")
            },
            col("t", "ver", "timestamp"),
        ];
        let keys = [key("t", 1, true, "a", false), key("t", 1, true, "b", false)];
        let t = edit_target(&described, &keys, 6).unwrap();
        assert_eq!(t.schema.as_deref(), Some("dbo"));
        assert_eq!(t.table, "t");
        assert_eq!(t.key, [3, 2]);
        let c = t.columns[0].as_ref().unwrap();
        assert_eq!(
            (c.name.as_str(), c.type_name.as_str()),
            ("name", "nvarchar(40)")
        );
        assert!(t.columns[1].is_none());
        assert!(t.columns[4].is_none(), "computed");
        assert!(t.columns[5].is_none(), "rowversion");
    }

    #[test]
    fn detection_read_only_reasons() {
        let pk = [key("t", 1, true, "id", false)];
        // Two tables.
        let err = edit_target(&[col("t", "id", "int"), col("u", "id", "int")], &pk, 2).unwrap_err();
        assert!(err.contains("several tables"), "{err}");
        // No table at all.
        let err = edit_target(&[expr("int")], &pk, 1).unwrap_err();
        assert_eq!(err, ONLY_TABLE_ROWS);
        // Describe error.
        let failed = Described {
            error: Some("uses a temp table".into()),
            ..Default::default()
        };
        assert_eq!(edit_target(&[failed], &pk, 1).unwrap_err(), ONLY_TABLE_ROWS);
        // Column count does not match the result.
        assert!(edit_target(&[col("t", "id", "int")], &pk, 2).is_err());
        // Key missing; browse mode added it as a hidden column.
        let hidden = Described {
            hidden: true,
            ..col("t", "id", "int")
        };
        let err = edit_target(&[col("t", "name", "nvarchar(5)"), hidden], &pk, 1).unwrap_err();
        assert_eq!(
            err,
            "Read-only: add the primary key columns (id) to the query to edit rows."
        );
        // No key at all; a unique index on a nullable column does not count.
        let err = edit_target(
            &[col("t", "id", "int")],
            &[key("t", 2, false, "id", true)],
            1,
        )
        .unwrap_err();
        assert_eq!(err, "Read-only: table t has no primary key.");
        // A NOT NULL unique key is used when there is no primary key.
        let t = edit_target(
            &[col("t", "x", "int"), col("t", "code", "varchar(5)")],
            &[key("t", 2, false, "code", false)],
            2,
        )
        .unwrap();
        assert_eq!(t.key, [1]);
        // Another database.
        let other = Described {
            other_db: true,
            ..col("t", "id", "int")
        };
        assert!(edit_target(&[other], &pk, 1)
            .unwrap_err()
            .contains("another database"));
        // Self join: the key shows twice.
        let err = edit_target(&[col("t", "id", "int"), col("t", "id", "int")], &pk, 2).unwrap_err();
        assert!(err.contains("more than once"), "{err}");
        // Self join: browse mode added the other instance's key.
        let err = edit_target(
            &[
                col("t", "id", "int"),
                col("t", "name", "int"),
                Described {
                    hidden: true,
                    ..col("t", "id", "int")
                },
            ],
            &pk,
            2,
        )
        .unwrap_err();
        assert!(err.contains("more than once"), "{err}");
    }

    #[test]
    fn identifiers_are_bracket_quoted() {
        assert_eq!(quote_ident("a]b"), "[a]]b]");
        assert_eq!(
            qualified(Some("Sales Dept"), "Order Items"),
            "[Sales Dept].[Order Items]"
        );
        assert_eq!(qualified(None, "t"), "[t]");
    }

    #[test]
    fn values_convert_with_fixed_styles() {
        let v = |t: &str| value_expr(t, 2).unwrap();
        assert_eq!(v("decimal(10, 2)"), "CAST(@P2 AS decimal(10,2))");
        assert_eq!(v("BIT"), "CAST(@P2 AS bit)");
        assert_eq!(v("datetime2(7)"), "CONVERT(datetime2(7), @P2, 121)");
        assert_eq!(
            v("datetimeoffset(3)"),
            "CONVERT(datetimeoffset(3), @P2, 121)"
        );
        assert_eq!(v("datetime"), "CONVERT(datetime, @P2, 121)");
        assert_eq!(v("time(3)"), "CAST(@P2 AS time(3))");
        assert_eq!(v("varbinary(max)"), "CONVERT(varbinary(max), @P2, 1)");
        assert_eq!(v("binary(4)"), "CONVERT(binary(4), @P2, 1)");
        assert_eq!(v("image"), "CONVERT(varbinary(max), @P2, 1)");
        assert_eq!(
            v("hierarchyid"),
            "CAST(CONVERT(varbinary(max), @P2, 1) AS hierarchyid)"
        );
        assert_eq!(v("nvarchar(40)"), "@P2");
        assert_eq!(v("uniqueidentifier"), "CAST(@P2 AS uniqueidentifier)");
        for bad in [
            "timestamp",
            "sql_variant",
            "int); DROP TABLE x; --",
            "decimal(10,2",
            "varchar(-1)",
            "",
        ] {
            assert!(value_expr(bad, 1).is_err(), "{bad}");
        }
    }

    fn target() -> EditTarget {
        let c = |name: &str, ty: &str| {
            Some(EditColumn {
                name: name.into(),
                type_name: ty.into(),
            })
        };
        EditTarget {
            schema: Some("dbo".into()),
            table: "Order Items".into(),
            columns: vec![
                c("Item Id", "int"),
                c("name", "nvarchar(40)"),
                None,
                c("data", "varbinary(max)"),
                c("code", "char(3)"),
            ],
            key: vec![0, 4],
        }
    }

    fn cv(column: usize, value: Option<&str>) -> crate::model::CellValue {
        crate::model::CellValue {
            column,
            value: value.map(str::to_string),
        }
    }

    #[test]
    fn change_statements() {
        let t = target();
        let s = change_sql(
            &t,
            &RowChange::Update {
                key: vec![Some("7".into()), None],
                values: vec![cv(1, Some("it's")), cv(3, None)],
            },
            &[],
        )
        .unwrap()
        .unwrap();
        assert_eq!(s.op, ChangeOp::Update);
        assert_eq!(
            s.sql,
            "UPDATE [dbo].[Order Items] SET [name] = @P1, [data] = CONVERT(varbinary(max), @P2, 1) \
             WHERE [Item Id] = CAST(@P3 AS int) AND [code] IS NULL"
        );
        assert_eq!(s.params, [Some("it's".into()), None, Some("7".into())]);

        let s = change_sql(
            &t,
            &RowChange::Delete {
                key: vec![Some("7".into()), Some("ab ".into())],
            },
            &[],
        )
        .unwrap()
        .unwrap();
        assert_eq!(
            s.sql,
            "DELETE FROM [dbo].[Order Items] WHERE [Item Id] = CAST(@P1 AS int) AND [code] = @P2"
        );

        let s = change_sql(
            &t,
            &RowChange::Insert {
                values: vec![cv(1, Some("a")), cv(1, Some("b")), cv(0, None)],
            },
            &["Item Id".into()],
        )
        .unwrap()
        .unwrap();
        assert_eq!(s.op, ChangeOp::Insert);
        assert_eq!(
            s.sql,
            "INSERT INTO [dbo].[Order Items] ([name]) VALUES (@P1)"
        );
        assert_eq!(s.params, [Some("b".into())]);

        let s = change_sql(&t, &RowChange::Insert { values: vec![] }, &[])
            .unwrap()
            .unwrap();
        assert_eq!(s.sql, "INSERT INTO [dbo].[Order Items] DEFAULT VALUES");

        // Nothing to set.
        let none = change_sql(
            &t,
            &RowChange::Update {
                key: vec![Some("1".into()), Some("x".into())],
                values: vec![],
            },
            &[],
        )
        .unwrap();
        assert!(none.is_none());
    }

    #[test]
    fn invalid_changes_are_rejected() {
        let t = target();
        let key = || vec![Some("1".into()), Some("x".into())];
        let invalid = |change: RowChange, identity: &[String]| {
            let err = change_sql(&t, &change, identity).unwrap_err();
            assert_eq!(err.kind, ErrorKind::Invalid);
            err.message
        };
        // Expression column.
        let m = invalid(
            RowChange::Update {
                key: key(),
                values: vec![cv(2, Some("1"))],
            },
            &[],
        );
        assert!(m.contains("Column 3"), "{m}");
        // Truncated binary preview.
        let m = invalid(
            RowChange::Update {
                key: key(),
                values: vec![cv(3, Some("0x00FF… (40000 bytes)"))],
            },
            &[],
        );
        assert_eq!(m, "This value is only a preview and cannot be saved.");
        // Identity value on insert and identity change on update.
        let id = ["Item Id".to_string()];
        let m = invalid(
            RowChange::Insert {
                values: vec![cv(0, Some("5"))],
            },
            &id,
        );
        assert!(m.contains("IDENTITY_INSERT"), "{m}");
        let m = invalid(
            RowChange::Update {
                key: key(),
                values: vec![cv(0, Some("5"))],
            },
            &id,
        );
        assert!(m.contains("identity"), "{m}");
        // Key of the wrong length.
        invalid(
            RowChange::Delete {
                key: vec![Some("1".into())],
            },
            &[],
        );
        // Text that only looks like a preview is fine.
        assert!(change_sql(
            &t,
            &RowChange::Update {
                key: key(),
                values: vec![cv(1, Some("see… (below)"))],
            },
            &[],
        )
        .is_ok());
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

    // ---- Grid editing ----

    fn reason(r: &ExecResult) -> &str {
        assert!(r.editable.is_none(), "unexpectedly editable");
        r.read_only_reason.as_deref().expect("a read-only reason")
    }

    async fn apply(
        s: &MsSession,
        target: &EditTarget,
        changes: Vec<RowChange>,
    ) -> AppResult<ApplyResult> {
        s.apply_changes(&ApplyRequest {
            target: target.clone(),
            changes,
        })
        .await
    }

    async fn scalar(s: &MsSession, sql: &str) -> Option<String> {
        run(s, sql, 0).await.unwrap().rows[0][0].clone()
    }

    #[tokio::test]
    #[ignore]
    async fn mssql_edit_detection() {
        let Some(s) = open().await else { return };
        run(
            &s,
            "DROP TABLE IF EXISTS dbo.lq_edit_items, dbo.lq_edit_owner, dbo.lq_edit_nokey;
             CREATE TABLE dbo.lq_edit_items (id int NOT NULL PRIMARY KEY, name nvarchar(40),
                                             qty int, total AS qty * 2, ver rowversion);
             CREATE TABLE dbo.lq_edit_owner (id int NOT NULL PRIMARY KEY, item_id int);
             CREATE TABLE dbo.lq_edit_nokey (a int, b int);
             INSERT INTO dbo.lq_edit_items (id, name, qty) VALUES (1, N'a', 1);
             INSERT INTO dbo.lq_edit_owner VALUES (1, 1);
             INSERT INTO dbo.lq_edit_nokey VALUES (1, 2);",
            0,
        )
        .await
        .unwrap();

        let r = run(
            &s,
            "SELECT name, id, qty + 1 AS next, total, ver FROM dbo.lq_edit_items",
            0,
        )
        .await
        .unwrap();
        let t = r.editable.expect("editable");
        assert!(r.read_only_reason.is_none());
        assert_eq!(t.schema.as_deref(), Some("dbo"));
        assert_eq!(t.table, "lq_edit_items");
        assert_eq!(t.key, [1]);
        let name = t.columns[0].as_ref().unwrap();
        assert_eq!(
            (name.name.as_str(), name.type_name.as_str()),
            ("name", "nvarchar(40)")
        );
        assert_eq!(t.columns[1].as_ref().unwrap().type_name, "int");
        assert!(t.columns[2].is_none(), "expression");
        assert!(t.columns[3].is_none(), "computed");
        assert!(t.columns[4].is_none(), "rowversion");

        let r = run(
            &s,
            "SELECT i.id, i.name, o.id AS owner FROM dbo.lq_edit_items i
             JOIN dbo.lq_edit_owner o ON o.item_id = i.id",
            0,
        )
        .await
        .unwrap();
        assert!(reason(&r).contains("several tables"), "{}", reason(&r));

        // Only columns of one table from a join: editable.
        let r = run(
            &s,
            "SELECT i.id, i.name FROM dbo.lq_edit_items i
             JOIN dbo.lq_edit_owner o ON o.item_id = i.id",
            0,
        )
        .await
        .unwrap();
        assert!(r.editable.is_some(), "{:?}", r.read_only_reason);

        let r = run(&s, "SELECT a, b FROM dbo.lq_edit_nokey", 0)
            .await
            .unwrap();
        reason(&r);
        let r = run(&s, "SELECT name, qty FROM dbo.lq_edit_items", 0)
            .await
            .unwrap();
        assert!(reason(&r).contains("(id)"), "{}", reason(&r));
        let r = run(&s, "SELECT 1 AS x", 0).await.unwrap();
        reason(&r);
        let r = run(&s, "EXEC('SELECT id FROM dbo.lq_edit_items')", 0)
            .await
            .unwrap();
        assert_eq!(reason(&r), ONLY_TABLE_ROWS);
        // A limited query is still described.
        let r = run(&s, "SELECT id, name FROM dbo.lq_edit_items", 1)
            .await
            .unwrap();
        assert!(r.editable.is_some());

        // Manual commit: describing never leaves a transaction behind.
        s.set_autocommit(false).await.unwrap();
        let r = run(&s, "SELECT 1 AS x", 0).await.unwrap();
        assert!(!r.in_transaction);
        assert_eq!(scalar(&s, "SELECT @@TRANCOUNT").await.as_deref(), Some("0"));
        let r = run(&s, "SELECT id, name FROM dbo.lq_edit_items", 0)
            .await
            .unwrap();
        assert!(r.editable.is_some());
        s.end_transaction(false).await.unwrap();
        s.set_autocommit(true).await.unwrap();

        run(
            &s,
            "DROP TABLE dbo.lq_edit_items, dbo.lq_edit_owner, dbo.lq_edit_nokey",
            0,
        )
        .await
        .unwrap();
    }

    #[tokio::test]
    #[ignore]
    async fn mssql_edit_apply() {
        let Some(s) = open().await else { return };
        run(
            &s,
            "DROP TABLE IF EXISTS dbo.lq_edit_rows;
             CREATE TABLE dbo.lq_edit_rows (id int IDENTITY PRIMARY KEY,
                                            name nvarchar(40) NOT NULL, qty int NULL);
             INSERT INTO dbo.lq_edit_rows (name, qty) VALUES (N'a', 1), (N'b', 2), (N'c', 3);",
            0,
        )
        .await
        .unwrap();
        let r = run(
            &s,
            "SELECT id, name, qty FROM dbo.lq_edit_rows ORDER BY id",
            0,
        )
        .await
        .unwrap();
        let t = r.editable.expect("editable");
        let res = apply(
            &s,
            &t,
            vec![
                RowChange::Update {
                    key: vec![Some("1".into())],
                    values: vec![cv(1, Some("a2")), cv(2, None)],
                },
                RowChange::Delete {
                    key: vec![Some("2".into())],
                },
                RowChange::Insert {
                    values: vec![cv(1, Some("d")), cv(0, None)],
                },
            ],
        )
        .await
        .unwrap();
        assert_eq!((res.updated, res.deleted, res.inserted), (1, 1, 1));
        assert!(!res.in_transaction);
        let all = "SELECT STRING_AGG(name + ':' + ISNULL(CAST(qty AS varchar(10)), 'null'), ',')
                   WITHIN GROUP (ORDER BY id) FROM dbo.lq_edit_rows";
        assert_eq!(scalar(&s, all).await.as_deref(), Some("a2:null,c:3,d:null"));

        // An identity value on insert is refused before anything runs.
        let err = apply(
            &s,
            &t,
            vec![RowChange::Insert {
                values: vec![cv(0, Some("50")), cv(1, Some("x"))],
            }],
        )
        .await
        .unwrap_err();
        assert_eq!(err.kind, ErrorKind::Invalid);
        assert!(!s.tx_state().in_transaction);

        // A key that matches nothing rolls back the earlier changes.
        let err = apply(
            &s,
            &t,
            vec![
                RowChange::Update {
                    key: vec![Some("1".into())],
                    values: vec![cv(1, Some("changed"))],
                },
                RowChange::Update {
                    key: vec![Some("999".into())],
                    values: vec![cv(1, Some("ghost"))],
                },
            ],
        )
        .await
        .unwrap_err();
        assert_eq!(err.kind, ErrorKind::Database);
        assert_eq!(
            err.message,
            "Row 2 was not saved: 0 rows matched its key (changed or removed by someone else)."
        );
        assert!(!s.tx_state().in_transaction);
        assert_eq!(scalar(&s, all).await.as_deref(), Some("a2:null,c:3,d:null"));

        // A server error (NOT NULL) maps like execute and rolls back.
        let err = apply(
            &s,
            &t,
            vec![
                RowChange::Delete {
                    key: vec![Some("3".into())],
                },
                RowChange::Update {
                    key: vec![Some("1".into())],
                    values: vec![cv(1, None)],
                },
            ],
        )
        .await
        .unwrap_err();
        assert_eq!(err.code.as_deref(), Some("Msg 515, Level 16, State 2"));
        assert_eq!(err.detail.as_deref(), Some("Row 2 was not saved."));
        assert_eq!(scalar(&s, all).await.as_deref(), Some("a2:null,c:3,d:null"));

        // Manual commit: the changes stay in the open transaction.
        s.set_autocommit(false).await.unwrap();
        let change = |name: &str| {
            vec![RowChange::Update {
                key: vec![Some("1".into())],
                values: vec![cv(1, Some(name))],
            }]
        };
        let res = apply(&s, &t, change("m1")).await.unwrap();
        assert!(res.in_transaction && s.tx_state().in_transaction);
        s.end_transaction(false).await.unwrap();
        assert_eq!(scalar(&s, all).await.as_deref(), Some("a2:null,c:3,d:null"));
        s.end_transaction(true).await.unwrap();
        apply(&s, &t, change("m2")).await.unwrap();
        // A failure inside the transaction undoes only this call.
        let mut failing = change("m3");
        failing.push(RowChange::Delete {
            key: vec![Some("999".into())],
        });
        apply(&s, &t, failing).await.unwrap_err();
        assert!(s.tx_state().in_transaction);
        s.end_transaction(true).await.unwrap();
        s.set_autocommit(true).await.unwrap();
        assert_eq!(scalar(&s, all).await.as_deref(), Some("m2:null,c:3,d:null"));

        // An explicit transaction in autocommit mode is joined, not committed.
        run(&s, "BEGIN TRAN", 0).await.unwrap();
        let res = apply(&s, &t, change("x1")).await.unwrap();
        assert!(res.in_transaction);
        run(&s, "ROLLBACK", 0).await.unwrap();
        assert_eq!(scalar(&s, all).await.as_deref(), Some("m2:null,c:3,d:null"));

        run(&s, "DROP TABLE dbo.lq_edit_rows", 0).await.unwrap();
    }

    #[tokio::test]
    #[ignore]
    async fn mssql_edit_round_trip() {
        let Some(s) = open().await else { return };
        run(
            &s,
            "DROP TABLE IF EXISTS dbo.lq_edit_types;
             CREATE TABLE dbo.lq_edit_types (
               id int NOT NULL PRIMARY KEY, dec decimal(18,4), num numeric(5,2), mon money,
               smon smallmoney, ti tinyint, si smallint, i int, bi bigint, fl float, re real,
               b bit, d date, dt datetime, sdt smalldatetime, dt2 datetime2(7),
               dto datetimeoffset(3), tm time(3), g uniqueidentifier, s nvarchar(100),
               vs varchar(100), ch char(5), bin varbinary(20), fbin binary(4), x xml,
               n int NULL);
             INSERT INTO dbo.lq_edit_types VALUES (
               1, 12345.6789, -0.5, 123456789.1234, -12.5, 255, -32768, -7,
               9223372036854775807, 0.1, 1.5, 1, '2024-05-21', '2024-05-21T13:45:30.997',
               '2024-05-21T13:45:00', '2024-05-21T13:45:30.1234567',
               '2024-05-21T13:45:30.123+05:30', '13:45:30.5',
               '6f9619ff-8b86-d011-b42d-00c04fc964ff', N'it''s \"q\"' + CHAR(10) + N'ação',
               'plain', 'ab', 0x0A0BFF, 0x00000001, N'<a>1</a>', NULL)",
            0,
        )
        .await
        .unwrap();
        // Settings that would change implicit conversions must not matter.
        run(&s, "SET DATEFORMAT ydm; SET LANGUAGE Deutsch", 0)
            .await
            .unwrap();
        let select = "SELECT * FROM dbo.lq_edit_types";
        let before = run(&s, select, 0).await.unwrap();
        let t = before.editable.clone().expect("editable");
        let row = &before.rows[0];
        let values = (1..row.len()).map(|i| crate::model::CellValue {
            column: i,
            value: row[i].clone(),
        });
        let res = apply(
            &s,
            &t,
            vec![RowChange::Update {
                key: vec![row[0].clone()],
                values: values.collect(),
            }],
        )
        .await
        .unwrap();
        assert_eq!(res.updated, 1);
        let after = run(&s, select, 0).await.unwrap();
        let names: Vec<&str> = before
            .columns
            .as_ref()
            .unwrap()
            .iter()
            .map(|c| c.name.as_str())
            .collect();
        for (i, name) in names.iter().enumerate() {
            assert_eq!(after.rows[0][i], before.rows[0][i], "column {name}");
        }

        // New values typed in the displayed format.
        let typed = [
            ("dt2", "2023-12-31 23:59:59.0000001"),
            ("dto", "2023-01-02 03:04:05.678 -03:00"),
            ("dt", "2023-01-13 00:00:00.003"),
            ("d", "2023-01-13"),
            ("b", "false"),
            ("bin", "0x"),
            ("dec", "-0.0001"),
        ];
        let values = typed
            .iter()
            .map(|(name, v)| cv(names.iter().position(|n| n == name).unwrap(), Some(v)))
            .collect();
        apply(
            &s,
            &t,
            vec![RowChange::Update {
                key: vec![Some("1".into())],
                values,
            }],
        )
        .await
        .unwrap();
        let after = run(&s, select, 0).await.unwrap();
        for (name, v) in typed {
            let i = names.iter().position(|n| *n == name).unwrap();
            assert_eq!(after.rows[0][i].as_deref(), Some(v), "column {name}");
        }
        run(
            &s,
            "SET DATEFORMAT mdy; SET LANGUAGE us_english; DROP TABLE dbo.lq_edit_types",
            0,
        )
        .await
        .unwrap();
    }

    #[tokio::test]
    #[ignore]
    async fn mssql_edit_quoted_names() {
        let Some(s) = open().await else { return };
        run(
            &s,
            "DROP TABLE IF EXISTS dbo.[lq_edit_Mixed Case];
             CREATE TABLE dbo.[lq_edit_Mixed Case] ([Item Id] int NOT NULL PRIMARY KEY,
                                                   [Some]]Name] nvarchar(20));
             INSERT INTO dbo.[lq_edit_Mixed Case] VALUES (1, N'one');",
            0,
        )
        .await
        .unwrap();
        let r = run(
            &s,
            "SELECT [Some]]Name], [Item Id] FROM dbo.[lq_edit_Mixed Case]",
            0,
        )
        .await
        .unwrap();
        let t = r.editable.expect("editable");
        assert_eq!(t.table, "lq_edit_Mixed Case");
        assert_eq!(t.key, [1]);
        assert_eq!(t.columns[0].as_ref().unwrap().name, "Some]Name");
        let res = apply(
            &s,
            &t,
            vec![
                RowChange::Update {
                    key: vec![Some("1".into())],
                    values: vec![cv(0, Some("uno"))],
                },
                RowChange::Insert {
                    values: vec![cv(1, Some("2")), cv(0, Some("dos"))],
                },
                RowChange::Insert {
                    values: vec![cv(1, Some("3"))],
                },
                RowChange::Delete {
                    key: vec![Some("3".into())],
                },
            ],
        )
        .await
        .unwrap();
        assert_eq!((res.updated, res.deleted, res.inserted), (1, 1, 2));
        let r = run(
            &s,
            "SELECT STRING_AGG([Some]]Name], ',') WITHIN GROUP (ORDER BY [Item Id])
             FROM dbo.[lq_edit_Mixed Case]",
            0,
        )
        .await
        .unwrap();
        assert_eq!(r.rows[0][0].as_deref(), Some("uno,dos"));
        run(&s, "DROP TABLE dbo.[lq_edit_Mixed Case]", 0)
            .await
            .unwrap();
    }
}
