//! PostgreSQL session over the tokio-postgres extended protocol.
//!
//! - Queries that return rows run in a portal with a row limit
//!   (`Execute` with `max_rows`), so a `SELECT` on a huge table does not
//!   pull everything over the network.
//! - Values are decoded from the binary format by [`super::pg_value`].
//! - Server NOTICE/WARNING messages (`RAISE NOTICE`) are collected and
//!   returned along with the result.
//! - Cancellation uses the server's cancel protocol, on a separate
//!   connection, without waiting for the running query.
//! - A SELECT is editable in the grid when the row description (table OID
//!   and attribute number of each column) points to one table whose key is
//!   in the result. Edits are sent as text-format parameters, so the server
//!   parses them with each column's own input function.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use futures_util::StreamExt;
use parking_lot::Mutex;
use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::crypto::CryptoProvider;
use rustls::pki_types::{CertificateDer, ServerName, UnixTime};
use rustls::{DigitallySignedStruct, SignatureScheme};
use tokio_postgres::error::ErrorPosition;
use tokio_postgres::types::{private::BytesMut, Format, IsNull, ToSql, Type};
use tokio_postgres::{AsyncMessage, CancelToken, Client, SimpleQueryMessage, Statement};
use tokio_postgres_rustls::MakeRustlsConnect;

use super::pg_value::{type_label, value_kind, PgCell};
use super::{first_keywords, TxState};
use crate::error::{AppError, AppResult, ErrorKind};
use crate::model::{
    ApplyRequest, ApplyResult, ColumnInfo, ConnectionProfile, DbColumn, DbObject, EditColumn,
    EditTarget, ExecRequest, ExecResult, RowChange, SslMode,
};

pub struct PgSession {
    client: tokio::sync::Mutex<Client>,
    cancel: CancelToken,
    tls: MakeRustlsConnect,
    notices: Arc<Mutex<Vec<String>>>,
    closed: Arc<AtomicBool>,
    state: Mutex<TxState>,
    /// Catalog data of tables seen in results, by table OID.
    tables: Mutex<HashMap<u32, Arc<TableMeta>>>,
    pub server_version: String,
    pub current_user: Option<String>,
    pub current_schema: Option<String>,
}

pub async fn connect(profile: &ConnectionProfile, password: Option<&str>) -> AppResult<PgSession> {
    let mut cfg = tokio_postgres::Config::new();
    cfg.host(profile.host.trim())
        .port(if profile.port == 0 {
            5432
        } else {
            profile.port
        })
        .user(profile.user.trim())
        .application_name("LowQuery")
        .connect_timeout(Duration::from_secs(15))
        .keepalives_idle(Duration::from_secs(60));
    if !profile.database.trim().is_empty() {
        cfg.dbname(profile.database.trim());
    }
    if let Some(pw) = password {
        cfg.password(pw);
    }
    cfg.ssl_mode(match profile.ssl_mode {
        SslMode::Disable => tokio_postgres::config::SslMode::Disable,
        SslMode::Prefer => tokio_postgres::config::SslMode::Prefer,
        SslMode::Require | SslMode::VerifyFull => tokio_postgres::config::SslMode::Require,
    });

    let tls = MakeRustlsConnect::new(tls_config(profile.ssl_mode)?);
    let (client, mut connection) = cfg.connect(tls.clone()).await.map_err(map_error)?;

    let notices = Arc::new(Mutex::new(Vec::new()));
    let closed = Arc::new(AtomicBool::new(false));
    {
        let notices = notices.clone();
        let closed = closed.clone();
        tauri::async_runtime::spawn(async move {
            let mut messages = futures_util::stream::poll_fn(move |cx| connection.poll_message(cx));
            while let Some(message) = messages.next().await {
                match message {
                    Ok(AsyncMessage::Notice(n)) => {
                        // 25001/25P01: redundant BEGIN/ROLLBACK warnings,
                        // produced by the internal transaction handling.
                        if !matches!(n.code().code(), "25001" | "25P01") {
                            notices
                                .lock()
                                .push(format!("{}: {}", n.severity(), n.message()));
                        }
                    }
                    Ok(_) => {}
                    Err(_) => break,
                }
            }
            closed.store(true, Ordering::SeqCst);
        });
    }

    let row = client
        .query_one(
            "SELECT current_setting('server_version'), current_user::text, current_schema()::text",
            &[],
        )
        .await
        .map_err(map_error)?;

    Ok(PgSession {
        cancel: client.cancel_token(),
        client: tokio::sync::Mutex::new(client),
        tls,
        notices,
        closed,
        state: Mutex::new(TxState {
            autocommit: true,
            in_transaction: false,
        }),
        tables: Mutex::new(HashMap::new()),
        server_version: format!("PostgreSQL {}", row.get::<_, String>(0)),
        current_user: row.get(1),
        current_schema: row.get(2),
    })
}

/// Statements the server rejects inside a transaction block.
fn must_run_outside_transaction(sql: &str) -> bool {
    let kw = first_keywords(sql, 2);
    let first = kw.first().map(String::as_str).unwrap_or("");
    let second = kw.get(1).map(String::as_str).unwrap_or("");
    match (first, second) {
        ("VACUUM", _) => true,
        ("CREATE" | "DROP", "DATABASE" | "TABLESPACE") => true,
        ("ALTER", "SYSTEM") => true,
        ("CREATE" | "DROP" | "REINDEX", _) => sql.to_ascii_uppercase().contains("CONCURRENTLY"),
        _ => false,
    }
}

impl PgSession {
    /// Runs the grid edits as one unit. With autocommit they commit together
    /// or not at all. Inside an open transaction they run under a savepoint,
    /// so a failure undoes only this batch and the transaction stays usable.
    /// In manual mode without a transaction, one is opened and left open.
    pub async fn apply_changes(&self, req: &ApplyRequest) -> AppResult<ApplyResult> {
        if self.is_closed() {
            return Err(AppError::connection(
                "The connection to the server was closed.",
            ));
        }
        let changes = req
            .changes
            .iter()
            .map(|c| build_change(&req.target, c))
            .collect::<AppResult<Vec<_>>>()?;

        let client = self.client.lock().await;
        let started = Instant::now();
        let state = *self.state.lock();
        let manual = !state.autocommit || state.in_transaction;
        let opened = !state.in_transaction;

        client
            .batch_execute(if opened {
                "BEGIN"
            } else {
                "SAVEPOINT lq_apply"
            })
            .await
            .map_err(map_error)?;
        if manual && opened {
            self.state.lock().in_transaction = true;
        }

        let mut outcome = run_changes(&client, &changes).await;
        if outcome.is_ok() {
            let end = if !opened {
                Some("RELEASE SAVEPOINT lq_apply")
            } else if !manual {
                Some("COMMIT")
            } else {
                None
            };
            if let Some(end) = end {
                if let Err(e) = client.batch_execute(end).await {
                    if end == "COMMIT" {
                        // A failed COMMIT ends the transaction anyway.
                        return Err(map_error(e));
                    }
                    outcome = Err(map_error(e));
                }
            }
        }
        let mut result = match outcome {
            Ok(result) => result,
            Err(e) => {
                // Undo only this batch inside an existing transaction; if
                // that fails too, end the transaction.
                let kept = !opened
                    && client
                        .batch_execute("ROLLBACK TO SAVEPOINT lq_apply; RELEASE SAVEPOINT lq_apply")
                        .await
                        .is_ok();
                if !kept {
                    let _ = client.batch_execute("ROLLBACK").await;
                    self.state.lock().in_transaction = false;
                    self.tables.lock().clear();
                }
                return Err(e);
            }
        };
        drop(client);
        result.elapsed_ms = started.elapsed().as_millis() as u64;
        result.in_transaction = self.state.lock().in_transaction;
        Ok(result)
    }

    /// Whether the rows of a result can be edited, from the table OID and
    /// attribute number the server reports for each column.
    async fn edit_target(
        &self,
        client: &Client,
        sql: &str,
        origins: &[(Option<u32>, Option<i16>)],
    ) -> Result<EditTarget, String> {
        let first = first_keywords(sql, 1);
        if !matches!(
            first.first().map(String::as_str),
            Some("SELECT" | "WITH" | "TABLE")
        ) {
            return Err(NOT_A_TABLE.into());
        }
        let oid = single_table(origins)?;
        let tokens = sql_tokens(sql);
        // The row description of a UNION names the first branch's table,
        // but the rows may come from any branch.
        if has_union(&tokens) {
            return Err("Read-only: the result combines rows from several queries.".into());
        }
        if joins_tables(&tokens) {
            return Err(JOINS_TABLES.into());
        }
        let meta = self
            .table_meta(client, oid)
            .await
            .ok_or_else(|| "Read-only: the table's key could not be read.".to_string())?;
        target_for(&meta, oid, origins)
    }

    /// Catalog data of one table, cached. One round trip on a miss; inside a
    /// transaction it runs under a savepoint so a failure cannot abort it.
    async fn table_meta(&self, client: &Client, oid: u32) -> Option<Arc<TableMeta>> {
        if let Some(meta) = self.tables.lock().get(&oid) {
            return Some(meta.clone());
        }
        let query = table_meta_sql(oid);
        let in_tx = self.state.lock().in_transaction;
        let sql = if in_tx {
            format!("SAVEPOINT lq_edit_meta; {query}; RELEASE SAVEPOINT lq_edit_meta")
        } else {
            query
        };
        let messages = match client.simple_query(&sql).await {
            Ok(m) => m,
            Err(_) => {
                if in_tx {
                    let _ = client
                        .batch_execute(
                            "ROLLBACK TO SAVEPOINT lq_edit_meta; RELEASE SAVEPOINT lq_edit_meta",
                        )
                        .await;
                }
                return None;
            }
        };
        let rows: Vec<Vec<Option<String>>> = messages
            .iter()
            .filter_map(|m| match m {
                SimpleQueryMessage::Row(r) => {
                    Some((0..r.len()).map(|i| r.get(i).map(str::to_string)).collect())
                }
                _ => None,
            })
            .collect();
        let meta = Arc::new(parse_table_meta(&rows)?);
        let mut tables = self.tables.lock();
        if tables.len() >= 256 {
            tables.clear();
        }
        tables.insert(oid, meta.clone());
        Some(meta)
    }

    pub fn is_closed(&self) -> bool {
        self.closed.load(Ordering::SeqCst)
    }

    pub fn tx_state(&self) -> TxState {
        *self.state.lock()
    }

    pub async fn execute(&self, req: &ExecRequest) -> AppResult<ExecResult> {
        if self.is_closed() {
            return Err(AppError::connection(
                "The connection to the server was closed.",
            ));
        }
        let mut client = self.client.lock().await;
        self.notices.lock().clear();
        let started = Instant::now();
        let state = *self.state.lock();
        let keywords = first_keywords(&req.sql, 2);
        let first = keywords.first().map(String::as_str).unwrap_or("");

        let outcome = self.run(&mut client, req, state).await;

        // Updates the transaction state based on what was executed.
        let in_transaction = self.state.lock().in_transaction;
        match (&outcome, first) {
            (Ok(_), "BEGIN" | "START") => self.state.lock().in_transaction = true,
            (Ok(_), "COMMIT" | "ROLLBACK" | "END" | "ABORT") => {
                // ROLLBACK TO SAVEPOINT keeps the transaction open.
                if keywords.get(1).map(String::as_str) != Some("TO") {
                    self.state.lock().in_transaction = false;
                }
            }
            (Err(_), _) if in_transaction => {
                // An error inside the block invalidates the whole transaction: the
                // only option is to roll it back. Doing it here keeps the next
                // statement from failing with "current transaction is aborted".
                let _ = client.batch_execute("ROLLBACK").await;
                self.state.lock().in_transaction = false;
                self.notices
                    .lock()
                    .push("The transaction was rolled back because of the error.".into());
            }
            _ => {}
        }
        // DDL (or undoing it) may change keys and column types.
        if matches!(
            first,
            "CREATE" | "ALTER" | "DROP" | "ROLLBACK" | "ABORT" | "DO" | "CALL" | "IMPORT"
        ) || (outcome.is_err() && in_transaction)
        {
            self.tables.lock().clear();
        }
        drop(client);

        let mut result = outcome?;
        result.elapsed_ms = started.elapsed().as_millis() as u64;
        result.notices = std::mem::take(&mut *self.notices.lock());
        result.in_transaction = self.state.lock().in_transaction;
        Ok(result)
    }

    async fn run(
        &self,
        client: &mut Client,
        req: &ExecRequest,
        state: TxState,
    ) -> AppResult<ExecResult> {
        let stmt = client.prepare(&req.sql).await.map_err(map_error)?;
        let manual = !state.autocommit || state.in_transaction;

        if stmt.columns().is_empty() {
            if !state.autocommit && !state.in_transaction && !must_run_outside_transaction(&req.sql)
            {
                let kw = first_keywords(&req.sql, 1);
                let is_tx_control = matches!(
                    kw.first().map(String::as_str),
                    Some(
                        "BEGIN"
                            | "START"
                            | "COMMIT"
                            | "ROLLBACK"
                            | "END"
                            | "ABORT"
                            | "SAVEPOINT"
                            | "RELEASE"
                    )
                );
                if !is_tx_control {
                    client.batch_execute("BEGIN").await.map_err(map_error)?;
                    self.state.lock().in_transaction = true;
                }
            }
            let affected = client.execute(&stmt, &[]).await.map_err(map_error)?;
            return Ok(ExecResult {
                rows_affected: Some(affected),
                ..Default::default()
            });
        }

        let columns = stmt
            .columns()
            .iter()
            .map(|c| ColumnInfo {
                name: c.name().to_string(),
                type_name: type_label(c.type_()),
                kind: value_kind(c.type_()),
            })
            .collect();

        if manual && !state.in_transaction {
            client.batch_execute("BEGIN").await.map_err(map_error)?;
            self.state.lock().in_transaction = true;
        }

        // Inside an already open transaction, the BEGIN from `transaction()`
        // becomes a (filtered) warning and the object is forgotten so it does
        // not end the user's transaction. In autocommit, it is a short
        // transaction just for the portal, committed right after.
        let tx = client.transaction().await.map_err(map_error)?;
        let rows = fetch_rows(&tx, &stmt, req.max_rows).await;
        let rows = match rows {
            Ok(rows) => rows,
            Err(e) => return Err(e), // drop → ROLLBACK
        };
        if manual {
            std::mem::forget(tx);
        } else {
            tx.commit().await.map_err(map_error)?;
        }

        let mut out = Vec::with_capacity(rows.len());
        for row in &rows {
            let mut cells = Vec::with_capacity(row.len());
            for i in 0..row.len() {
                let cell: PgCell = row
                    .try_get(i)
                    .map_err(|e| AppError::internal(format!("Column {}: {e}", i + 1)))?;
                cells.push(cell.0);
            }
            out.push(cells);
        }
        let truncated = req.max_rows > 0 && out.len() > req.max_rows;
        if truncated {
            out.truncate(req.max_rows);
        }
        let origins: Vec<_> = stmt
            .columns()
            .iter()
            .map(|c| (c.table_oid(), c.column_id()))
            .collect();
        let (editable, read_only_reason) = match self.edit_target(client, &req.sql, &origins).await
        {
            Ok(target) => (Some(target), None),
            Err(reason) => (None, Some(reason)),
        };
        Ok(ExecResult {
            columns: Some(columns),
            rows: out,
            truncated,
            editable,
            read_only_reason,
            ..Default::default()
        })
    }

    pub async fn cancel(&self) -> AppResult<()> {
        self.cancel
            .cancel_query(self.tls.clone())
            .await
            .map_err(map_error)
    }

    pub async fn set_autocommit(&self, on: bool) -> AppResult<TxState> {
        if on && self.state.lock().in_transaction {
            // Switching back to autocommit commits pending work,
            // as JDBC clients do.
            self.end_transaction(true).await?;
        }
        let mut st = self.state.lock();
        st.autocommit = on;
        Ok(*st)
    }

    pub async fn end_transaction(&self, commit: bool) -> AppResult<TxState> {
        let client = self.client.lock().await;
        client
            .batch_execute(if commit { "COMMIT" } else { "ROLLBACK" })
            .await
            .map_err(map_error)?;
        if !commit {
            self.tables.lock().clear();
        }
        let mut st = self.state.lock();
        st.in_transaction = false;
        Ok(*st)
    }

    pub async fn list_schemas(&self) -> AppResult<Vec<String>> {
        let client = self.client.lock().await;
        let rows = client
            .query(
                "SELECT nspname::text FROM pg_namespace
                 WHERE nspname NOT LIKE 'pg\\_toast%' AND nspname NOT LIKE 'pg\\_temp\\_%'
                 ORDER BY nspname IN ('pg_catalog', 'information_schema'), nspname",
                &[],
            )
            .await
            .map_err(map_error)?;
        Ok(rows.iter().map(|r| r.get(0)).collect())
    }

    pub async fn list_objects(&self, schema: &str) -> AppResult<Vec<DbObject>> {
        let client = self.client.lock().await;
        let rows = client
            .query(
                "SELECT c.relname::text,
                        CASE c.relkind WHEN 'r' THEN 'table' WHEN 'p' THEN 'table'
                             WHEN 'v' THEN 'view' WHEN 'm' THEN 'matview'
                             WHEN 'f' THEN 'foreign' ELSE 'sequence' END
                 FROM pg_class c JOIN pg_namespace n ON n.oid = c.relnamespace
                 WHERE n.nspname = $1 AND c.relkind IN ('r','p','v','m','f','S')
                   AND NOT c.relispartition
                 UNION ALL
                 SELECT p.proname || '(' || pg_get_function_identity_arguments(p.oid) || ')',
                        CASE p.prokind WHEN 'p' THEN 'procedure' ELSE 'function' END
                 FROM pg_proc p JOIN pg_namespace n ON n.oid = p.pronamespace
                 WHERE n.nspname = $1 AND p.prokind IN ('f','p')
                 ORDER BY 2, 1",
                &[&schema],
            )
            .await
            .map_err(map_error)?;
        Ok(rows
            .iter()
            .map(|r| DbObject {
                name: r.get(0),
                kind: r.get(1),
            })
            .collect())
    }

    pub async fn list_columns(&self, schema: &str, table: &str) -> AppResult<Vec<DbColumn>> {
        let client = self.client.lock().await;
        let rows = client
            .query(
                "SELECT a.attname::text, format_type(a.atttypid, a.atttypmod),
                        NOT a.attnotnull,
                        COALESCE(a.attnum = ANY(i.indkey::int2[]), false)
                 FROM pg_attribute a
                 JOIN pg_class c ON c.oid = a.attrelid
                 JOIN pg_namespace n ON n.oid = c.relnamespace
                 LEFT JOIN pg_index i ON i.indrelid = c.oid AND i.indisprimary
                 WHERE n.nspname = $1 AND c.relname = $2 AND a.attnum > 0 AND NOT a.attisdropped
                 ORDER BY a.attnum",
                &[&schema, &table],
            )
            .await
            .map_err(map_error)?;
        Ok(rows
            .iter()
            .map(|r| DbColumn {
                name: r.get(0),
                type_name: r.get(1),
                nullable: r.get(2),
                primary_key: r.get(3),
            })
            .collect())
    }
}

async fn fetch_rows(
    tx: &tokio_postgres::Transaction<'_>,
    stmt: &Statement,
    max_rows: usize,
) -> AppResult<Vec<tokio_postgres::Row>> {
    if max_rows == 0 {
        return tx.query(stmt, &[]).await.map_err(map_error);
    }
    let portal = tx.bind(stmt, &[]).await.map_err(map_error)?;
    let limit = i32::try_from(max_rows.saturating_add(1)).unwrap_or(i32::MAX);
    tx.query_portal(&portal, limit).await.map_err(map_error)
}

const NOT_A_TABLE: &str = "Read-only: only rows from a table can be edited.";
const JOINS_TABLES: &str =
    "Read-only: the query joins tables; select from one table to edit its rows.";

/// Catalog data of a table behind a result.
#[derive(Debug)]
struct TableMeta {
    schema: String,
    name: String,
    /// `pg_class.relkind`: `r` table, `p` partitioned table, `v` view...
    kind: String,
    /// attnum, name, type as in DDL.
    columns: Vec<(i16, String, String)>,
    /// Attribute numbers of the primary key, or of a unique key whose
    /// columns are all NOT NULL, in key order.
    key: Option<Vec<i16>>,
    primary: bool,
}

/// One row per column, all values as text (simple protocol). The chosen key
/// is the primary key, else the narrowest valid unique index without
/// expressions or predicate whose key columns are all NOT NULL.
fn table_meta_sql(oid: u32) -> String {
    format!(
        "SELECT n.nspname, c.relname, c.relkind, a.attnum, a.attname,
                pg_catalog.format_type(a.atttypid, a.atttypmod), k.cols, k.indisprimary
         FROM pg_catalog.pg_class c
         JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace
         LEFT JOIN pg_catalog.pg_attribute a
                ON a.attrelid = c.oid AND a.attnum > 0 AND NOT a.attisdropped
         LEFT JOIN LATERAL (
             SELECT (SELECT string_agg(u.k::text, ',' ORDER BY u.o)
                     FROM unnest(i.indkey::int2[]) WITH ORDINALITY u(k, o)
                     WHERE u.o <= i.indnkeyatts) AS cols,
                    i.indisprimary
             FROM pg_catalog.pg_index i
             WHERE i.indrelid = c.oid AND i.indisunique AND i.indisvalid
               AND (i.indisprimary OR (
                    i.indpred IS NULL AND i.indexprs IS NULL
                    AND NOT EXISTS (
                        SELECT 1
                        FROM unnest(i.indkey::int2[]) WITH ORDINALITY u(k, o)
                        JOIN pg_catalog.pg_attribute x
                          ON x.attrelid = c.oid AND x.attnum = u.k
                        WHERE u.o <= i.indnkeyatts AND NOT x.attnotnull)))
             ORDER BY i.indisprimary DESC, i.indnkeyatts, i.indexrelid
             LIMIT 1) k ON true
         WHERE c.oid = {oid}
         ORDER BY a.attnum"
    )
}

fn parse_table_meta(rows: &[Vec<Option<String>>]) -> Option<TableMeta> {
    let first = rows.first()?;
    let text = |row: &Vec<Option<String>>, i: usize| row.get(i).cloned().flatten();
    let key = text(first, 6).map(|cols| {
        cols.split(',')
            .filter_map(|k| k.trim().parse::<i16>().ok())
            .collect::<Vec<_>>()
    });
    let mut columns = Vec::with_capacity(rows.len());
    for row in rows {
        if let (Some(attnum), Some(name), Some(ty)) = (text(row, 3), text(row, 4), text(row, 5)) {
            columns.push((attnum.parse().ok()?, name, ty));
        }
    }
    Some(TableMeta {
        schema: text(first, 0)?,
        name: text(first, 1)?,
        kind: text(first, 2)?,
        columns,
        key: key.filter(|k| !k.is_empty()),
        primary: text(first, 7).as_deref() == Some("t"),
    })
}

/// The one table all base columns come from.
fn single_table(origins: &[(Option<u32>, Option<i16>)]) -> Result<u32, String> {
    let mut table = None;
    for &(oid, _) in origins {
        match (table, oid) {
            (_, None) => {}
            (None, Some(oid)) => table = Some(oid),
            (Some(t), Some(oid)) if t != oid => {
                return Err("Read-only: the result combines columns from several tables.".into())
            }
            _ => {}
        }
    }
    table.ok_or_else(|| NOT_A_TABLE.to_string())
}

fn target_for(
    meta: &TableMeta,
    oid: u32,
    origins: &[(Option<u32>, Option<i16>)],
) -> Result<EditTarget, String> {
    if !matches!(meta.kind.as_str(), "r" | "p") {
        return Err(NOT_A_TABLE.into());
    }
    let Some(key) = &meta.key else {
        return Err(format!(
            "Read-only: table {} has no primary key.",
            meta.name
        ));
    };
    let column = |attnum: i16| meta.columns.iter().find(|c| c.0 == attnum);
    let columns: Vec<Option<EditColumn>> = origins
        .iter()
        .map(|&(t, attnum)| match (t, attnum) {
            (Some(t), Some(attnum)) if t == oid => column(attnum).map(|c| EditColumn {
                name: c.1.clone(),
                type_name: c.2.clone(),
            }),
            _ => None,
        })
        .collect();
    let positions: Vec<Option<usize>> = key
        .iter()
        .map(|&k| {
            origins
                .iter()
                .position(|&(t, attnum)| t == Some(oid) && attnum == Some(k))
                .filter(|&i| columns[i].is_some())
        })
        .collect();
    if positions.iter().any(Option::is_none) {
        let names: Vec<&str> = key
            .iter()
            .map(|&k| column(k).map(|c| c.1.as_str()).unwrap_or("?"))
            .collect();
        return Err(format!(
            "Read-only: add the {} columns ({}) to the query to edit rows.",
            if meta.primary { "primary key" } else { "key" },
            names.join(", ")
        ));
    }
    Ok(EditTarget {
        schema: Some(meta.schema.clone()),
        table: meta.name.clone(),
        columns,
        key: positions.into_iter().flatten().collect(),
    })
}

/// Keywords and the punctuation that shapes clauses, outside strings,
/// quoted identifiers, comments and dollar quotes.
#[derive(Debug, PartialEq, Eq)]
enum SqlToken {
    /// Uppercased.
    Word(String),
    Open,
    Close,
    Comma,
}

fn sql_tokens(sql: &str) -> Vec<SqlToken> {
    let mut tokens = Vec::new();
    let b = sql.as_bytes();
    let is_word = |c: u8| c.is_ascii_alphanumeric() || c == b'_' || c == b'$' || c >= 0x80;
    let mut i = 0;
    while i < b.len() {
        let c = b[i];
        let prev_word = i > 0 && is_word(b[i - 1]);
        if c == b'-' && b.get(i + 1) == Some(&b'-') {
            while i < b.len() && b[i] != b'\n' {
                i += 1;
            }
        } else if c == b'/' && b.get(i + 1) == Some(&b'*') {
            // Block comments nest.
            let mut depth = 0;
            while i < b.len() {
                if b[i] == b'/' && b.get(i + 1) == Some(&b'*') {
                    depth += 1;
                    i += 2;
                } else if b[i] == b'*' && b.get(i + 1) == Some(&b'/') {
                    depth -= 1;
                    i += 2;
                    if depth == 0 {
                        break;
                    }
                } else {
                    i += 1;
                }
            }
        } else if c == b'\'' || c == b'"' {
            // E'...' strings accept backslash escapes.
            let escapes = c == b'\''
                && i > 0
                && matches!(b[i - 1], b'e' | b'E')
                && !(i > 1 && is_word(b[i - 2]));
            i += 1;
            while i < b.len() {
                if escapes && b[i] == b'\\' {
                    i += 2;
                } else if b[i] == c {
                    if b.get(i + 1) == Some(&c) {
                        i += 2;
                    } else {
                        break;
                    }
                } else {
                    i += 1;
                }
            }
            i += 1;
        } else if c == b'$' && !prev_word {
            // Dollar quote: $$...$$ or $tag$...$tag$.
            let mut j = i + 1;
            while j < b.len() && (b[j].is_ascii_alphanumeric() || b[j] == b'_' || b[j] >= 0x80) {
                j += 1;
            }
            let tag_ok =
                j < b.len() && b[j] == b'$' && !b.get(i + 1).is_some_and(|d| d.is_ascii_digit());
            if tag_ok {
                let tag = &sql[i..=j];
                i = match sql[j + 1..].find(tag) {
                    Some(end) => j + 1 + end + tag.len(),
                    None => b.len(),
                };
            } else {
                i += 1;
            }
        } else if is_word(c) && !prev_word {
            let start = i;
            while i < b.len() && is_word(b[i]) {
                i += 1;
            }
            tokens.push(SqlToken::Word(sql[start..i].to_ascii_uppercase()));
        } else {
            match c {
                b'(' => tokens.push(SqlToken::Open),
                b')' => tokens.push(SqlToken::Close),
                b',' => tokens.push(SqlToken::Comma),
                _ => {}
            }
            i += 1;
        }
    }
    tokens
}

/// Whether `UNION` appears as a keyword.
fn has_union(tokens: &[SqlToken]) -> bool {
    tokens
        .iter()
        .any(|t| matches!(t, SqlToken::Word(w) if w == "UNION"))
}

/// Whether the query joins tables at any depth: a `JOIN` keyword, or a
/// comma in a `FROM` list. Both sides of a self-join report the same table
/// OID, so the row description alone cannot tell them apart.
fn joins_tables(tokens: &[SqlToken]) -> bool {
    // One entry per paren depth: inside a FROM list at that depth.
    let mut in_from = vec![false];
    for t in tokens {
        match t {
            SqlToken::Open => in_from.push(false),
            SqlToken::Close => {
                if in_from.len() > 1 {
                    in_from.pop();
                }
            }
            SqlToken::Comma => {
                if in_from.last() == Some(&true) {
                    return true;
                }
            }
            SqlToken::Word(w) => {
                let state = in_from.last_mut().expect("depth 0 is never popped");
                match w.as_str() {
                    "JOIN" => return true,
                    "FROM" => *state = true,
                    "WHERE" | "GROUP" | "HAVING" | "WINDOW" | "ORDER" | "LIMIT" | "OFFSET"
                    | "FETCH" | "FOR" | "UNION" | "INTERSECT" | "EXCEPT" => *state = false,
                    _ => {}
                }
            }
        }
    }
    false
}

/// A value sent in text format: the server parses it with the input
/// function of the type it infers for the parameter (the column's type).
#[derive(Debug)]
struct TextParam<'a>(Option<&'a str>);

impl ToSql for TextParam<'_> {
    fn to_sql(
        &self,
        _ty: &Type,
        out: &mut BytesMut,
    ) -> Result<IsNull, Box<dyn std::error::Error + Sync + Send>> {
        match self.0 {
            Some(s) => {
                out.extend_from_slice(s.as_bytes());
                Ok(IsNull::No)
            }
            None => Ok(IsNull::Yes),
        }
    }

    fn accepts(_: &Type) -> bool {
        true
    }

    fn encode_format(&self, _ty: &Type) -> Format {
        Format::Text
    }

    tokio_postgres::types::to_sql_checked!();
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ChangeKind {
    Update,
    Delete,
    Insert,
}

/// One statement ready to run, with its parameters in order.
#[derive(Debug)]
struct BuiltChange {
    kind: ChangeKind,
    sql: String,
    params: Vec<Option<String>>,
}

fn quote_ident(name: &str) -> String {
    format!("\"{}\"", name.replace('"', "\"\""))
}

fn qualified_table(target: &EditTarget) -> String {
    match &target.schema {
        Some(schema) => format!("{}.{}", quote_ident(schema), quote_ident(&target.table)),
        None => quote_ident(&target.table),
    }
}

fn edit_column(target: &EditTarget, index: usize) -> AppResult<&EditColumn> {
    target
        .columns
        .get(index)
        .and_then(Option::as_ref)
        .ok_or_else(|| AppError::invalid(format!("Column {} cannot be edited.", index + 1)))
}

/// `$n`, routed through numeric for `money`, whose own input depends on
/// `lc_monetary`.
fn placeholder(n: usize, column: &EditColumn) -> String {
    if column.type_name == "money" {
        format!("${n}::numeric::money")
    } else {
        format!("${n}")
    }
}

fn check_value(value: &Option<String>) -> AppResult<Option<String>> {
    match value {
        Some(v) if v.contains("… (") => Err(AppError::invalid(
            "This value is only a preview and cannot be saved.",
        )),
        _ => Ok(value.clone()),
    }
}

/// Table columns and values of a change; a column given twice keeps the
/// last value.
fn assignments<'a>(
    target: &'a EditTarget,
    values: &[crate::model::CellValue],
) -> AppResult<Vec<(&'a EditColumn, Option<String>)>> {
    let mut out: Vec<(&EditColumn, Option<String>)> = Vec::with_capacity(values.len());
    for v in values {
        let column = edit_column(target, v.column)?;
        let value = check_value(&v.value)?;
        match out.iter_mut().find(|(c, _)| c.name == column.name) {
            Some(slot) => slot.1 = value,
            None => out.push((column, value)),
        }
    }
    Ok(out)
}

fn where_key(
    target: &EditTarget,
    key: &[Option<String>],
    params: &mut Vec<Option<String>>,
) -> AppResult<String> {
    if key.len() != target.key.len() {
        return Err(AppError::invalid(
            "The row key does not match the result's key columns.",
        ));
    }
    let mut parts = Vec::with_capacity(key.len());
    for (&index, value) in target.key.iter().zip(key) {
        let column = edit_column(target, index)?;
        match check_value(value)? {
            None => parts.push(format!("{} IS NULL", quote_ident(&column.name))),
            Some(v) => {
                params.push(Some(v));
                parts.push(format!(
                    "{} = {}",
                    quote_ident(&column.name),
                    placeholder(params.len(), column)
                ));
            }
        }
    }
    Ok(parts.join(" AND "))
}

/// The statement for one change; `None` for an update without values.
fn build_change(target: &EditTarget, change: &RowChange) -> AppResult<Option<BuiltChange>> {
    let table = qualified_table(target);
    let mut params = Vec::new();
    let built = match change {
        RowChange::Update { key, values } => {
            let sets = assignments(target, values)?;
            if sets.is_empty() {
                return Ok(None);
            }
            let mut set_sql = Vec::with_capacity(sets.len());
            for (column, value) in sets {
                params.push(value);
                set_sql.push(format!(
                    "{} = {}",
                    quote_ident(&column.name),
                    placeholder(params.len(), column)
                ));
            }
            let filter = where_key(target, key, &mut params)?;
            BuiltChange {
                kind: ChangeKind::Update,
                sql: format!("UPDATE {table} SET {} WHERE {filter}", set_sql.join(", ")),
                params,
            }
        }
        RowChange::Delete { key } => {
            let filter = where_key(target, key, &mut params)?;
            BuiltChange {
                kind: ChangeKind::Delete,
                sql: format!("DELETE FROM {table} WHERE {filter}"),
                params,
            }
        }
        RowChange::Insert { values } => {
            let sets = assignments(target, values)?;
            if sets.is_empty() {
                BuiltChange {
                    kind: ChangeKind::Insert,
                    sql: format!("INSERT INTO {table} DEFAULT VALUES"),
                    params,
                }
            } else {
                let mut names = Vec::with_capacity(sets.len());
                let mut slots = Vec::with_capacity(sets.len());
                for (column, value) in sets {
                    params.push(value);
                    names.push(quote_ident(&column.name));
                    slots.push(placeholder(params.len(), column));
                }
                BuiltChange {
                    kind: ChangeKind::Insert,
                    sql: format!(
                        "INSERT INTO {table} ({}) VALUES ({})",
                        names.join(", "),
                        slots.join(", ")
                    ),
                    params,
                }
            }
        }
    };
    Ok(Some(built))
}

/// Runs the statements in order, inside the caller's transaction.
async fn run_changes(client: &Client, changes: &[Option<BuiltChange>]) -> AppResult<ApplyResult> {
    let mut prepared: HashMap<&str, Statement> = HashMap::new();
    let mut result = ApplyResult::default();
    for (i, change) in changes.iter().enumerate() {
        let Some(change) = change else { continue };
        let row = i + 1;
        let row_error = |e: tokio_postgres::Error| {
            let mut err = map_error(e);
            if err.kind == ErrorKind::Database {
                err.message = format!("Row {row} was not saved: {}", err.message);
                err.position = None;
            }
            err
        };
        let stmt = match prepared.get(change.sql.as_str()) {
            Some(stmt) => stmt.clone(),
            None => {
                let stmt = client.prepare(&change.sql).await.map_err(row_error)?;
                prepared.insert(&change.sql, stmt.clone());
                stmt
            }
        };
        let params: Vec<TextParam> = change
            .params
            .iter()
            .map(|p| TextParam(p.as_deref()))
            .collect();
        let refs: Vec<&(dyn ToSql + Sync)> =
            params.iter().map(|p| p as &(dyn ToSql + Sync)).collect();
        let affected = client.execute(&stmt, &refs).await.map_err(row_error)?;
        match change.kind {
            ChangeKind::Insert => result.inserted += affected,
            kind => {
                if affected != 1 {
                    let mut msg =
                        format!("Row {row} was not saved: {affected} rows matched its key");
                    msg.push_str(if affected == 0 {
                        " (changed or removed by someone else)."
                    } else {
                        "."
                    });
                    return Err(AppError::new(ErrorKind::Database, msg));
                }
                if kind == ChangeKind::Update {
                    result.updated += 1;
                } else {
                    result.deleted += 1;
                }
            }
        }
    }
    Ok(result)
}

pub fn map_error(e: tokio_postgres::Error) -> AppError {
    if let Some(db) = e.as_db_error() {
        let code = db.code().code().to_string();
        let kind = if code == "57014" {
            ErrorKind::Cancelled
        } else {
            ErrorKind::Database
        };
        let mut err = AppError::new(kind, db.message());
        err.position = match db.position() {
            Some(ErrorPosition::Original(p)) => Some((*p as usize).saturating_sub(1)),
            _ => None,
        };
        err.detail = db.detail().map(str::to_string);
        err.hint = db.hint().map(str::to_string);
        // Authentication (class 28) and connection (08) errors are not SQL errors.
        if code.starts_with("28") || code.starts_with("08") {
            err.kind = ErrorKind::Connection;
        }
        err.code = Some(code);
        return err;
    }
    let mut message = e.to_string();
    let mut source = std::error::Error::source(&e);
    while let Some(s) = source {
        let text = s.to_string();
        if !message.contains(&text) {
            message.push_str(": ");
            message.push_str(&text);
        }
        source = s.source();
    }
    if e.is_closed() {
        return AppError::connection("The connection to the server was closed.");
    }
    AppError::connection(message)
}

fn tls_config(mode: SslMode) -> AppResult<rustls::ClientConfig> {
    let provider = Arc::new(rustls::crypto::aws_lc_rs::default_provider());
    let builder = rustls::ClientConfig::builder_with_provider(provider.clone())
        .with_safe_default_protocol_versions()
        .map_err(|e| AppError::internal(e.to_string()))?;
    let config = if mode == SslMode::VerifyFull {
        let mut roots = rustls::RootCertStore::empty();
        roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
        for cert in rustls_native_certs::load_native_certs().certs {
            let _ = roots.add(cert);
        }
        builder.with_root_certificates(roots).with_no_client_auth()
    } else {
        // libpq's `prefer`/`require` encrypt without validating the certificate.
        builder
            .dangerous()
            .with_custom_certificate_verifier(Arc::new(AcceptAnyCert(provider)))
            .with_no_client_auth()
    };
    Ok(config)
}

#[derive(Debug)]
struct AcceptAnyCert(Arc<CryptoProvider>);

impl ServerCertVerifier for AcceptAnyCert {
    fn verify_server_cert(
        &self,
        _end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        _now: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        Ok(ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls12_signature(
            message,
            cert,
            dss,
            &self.0.signature_verification_algorithms,
        )
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature(
            message,
            cert,
            dss,
            &self.0.signature_verification_algorithms,
        )
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.0.signature_verification_algorithms.supported_schemes()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::CellValue;

    fn target() -> EditTarget {
        EditTarget {
            schema: Some("My Schema".into()),
            table: "Order \"Lines\"".into(),
            columns: vec![
                Some(EditColumn {
                    name: "Id".into(),
                    type_name: "integer".into(),
                }),
                Some(EditColumn {
                    name: "price".into(),
                    type_name: "money".into(),
                }),
                None,
                Some(EditColumn {
                    name: "code".into(),
                    type_name: "text".into(),
                }),
            ],
            key: vec![0, 3],
        }
    }

    fn cell(column: usize, value: Option<&str>) -> CellValue {
        CellValue {
            column,
            value: value.map(str::to_string),
        }
    }

    fn some(v: &str) -> Option<String> {
        Some(v.to_string())
    }

    #[test]
    fn quotes_identifiers() {
        assert_eq!(quote_ident("a"), "\"a\"");
        assert_eq!(quote_ident("Mixed Case"), "\"Mixed Case\"");
        assert_eq!(quote_ident("say \"hi\""), "\"say \"\"hi\"\"\"");
        assert_eq!(
            qualified_table(&target()),
            "\"My Schema\".\"Order \"\"Lines\"\"\""
        );
    }

    #[test]
    fn builds_update_with_null_key_and_money() {
        let change = RowChange::Update {
            key: vec![some("7"), None],
            values: vec![cell(1, Some("12.34")), cell(3, None)],
        };
        let built = build_change(&target(), &change).unwrap().unwrap();
        assert_eq!(built.kind, ChangeKind::Update);
        assert_eq!(
            built.sql,
            "UPDATE \"My Schema\".\"Order \"\"Lines\"\"\" SET \"price\" = $1::numeric::money, \
             \"code\" = $2 WHERE \"Id\" = $3 AND \"code\" IS NULL"
        );
        assert_eq!(built.params, vec![some("12.34"), None, some("7")]);
    }

    #[test]
    fn update_keeps_last_value_of_a_column_and_skips_empty() {
        let change = RowChange::Update {
            key: vec![some("1"), some("a")],
            values: vec![cell(3, Some("x")), cell(3, Some("y"))],
        };
        let built = build_change(&target(), &change).unwrap().unwrap();
        assert!(built.sql.contains("SET \"code\" = $1 WHERE"));
        assert_eq!(built.params, vec![some("y"), some("1"), some("a")]);
        let empty = RowChange::Update {
            key: vec![some("1"), some("a")],
            values: vec![],
        };
        assert!(build_change(&target(), &empty).unwrap().is_none());
    }

    #[test]
    fn builds_delete_and_insert() {
        let delete = RowChange::Delete {
            key: vec![some("1"), some("a'b")],
        };
        let built = build_change(&target(), &delete).unwrap().unwrap();
        assert_eq!(
            built.sql,
            "DELETE FROM \"My Schema\".\"Order \"\"Lines\"\"\" WHERE \"Id\" = $1 AND \"code\" = $2"
        );
        assert_eq!(built.params, vec![some("1"), some("a'b")]);

        let insert = RowChange::Insert {
            values: vec![cell(0, Some("5")), cell(1, Some("1.00")), cell(3, None)],
        };
        let built = build_change(&target(), &insert).unwrap().unwrap();
        assert_eq!(
            built.sql,
            "INSERT INTO \"My Schema\".\"Order \"\"Lines\"\"\" (\"Id\", \"price\", \"code\") \
             VALUES ($1, $2::numeric::money, $3)"
        );
        assert_eq!(built.params, vec![some("5"), some("1.00"), None]);

        let defaults = RowChange::Insert { values: vec![] };
        let built = build_change(&target(), &defaults).unwrap().unwrap();
        assert_eq!(
            built.sql,
            "INSERT INTO \"My Schema\".\"Order \"\"Lines\"\"\" DEFAULT VALUES"
        );
        assert!(built.params.is_empty());
    }

    #[test]
    fn rejects_invalid_changes() {
        let preview = RowChange::Update {
            key: vec![some("1"), some("a")],
            values: vec![cell(3, Some("\\x0102… (40000 bytes)"))],
        };
        let err = build_change(&target(), &preview).unwrap_err();
        assert_eq!(err.kind, ErrorKind::Invalid);
        assert_eq!(
            err.message,
            "This value is only a preview and cannot be saved."
        );

        let expression = RowChange::Insert {
            values: vec![cell(2, Some("x"))],
        };
        assert_eq!(
            build_change(&target(), &expression).unwrap_err().kind,
            ErrorKind::Invalid
        );
        let out_of_range = RowChange::Insert {
            values: vec![cell(9, Some("x"))],
        };
        assert_eq!(
            build_change(&target(), &out_of_range).unwrap_err().kind,
            ErrorKind::Invalid
        );
        let short_key = RowChange::Delete {
            key: vec![some("1")],
        };
        assert_eq!(
            build_change(&target(), &short_key).unwrap_err().kind,
            ErrorKind::Invalid
        );
    }

    fn meta(kind: &str, key: Option<Vec<i16>>, primary: bool) -> TableMeta {
        TableMeta {
            schema: "public".into(),
            name: "orders".into(),
            kind: kind.into(),
            columns: vec![
                (1, "id".into(), "integer".into()),
                (2, "name".into(), "character varying(20)".into()),
                (3, "total".into(), "numeric(10,2)".into()),
            ],
            key,
            primary,
        }
    }

    #[test]
    fn detects_editable_results() {
        let origins = [
            (Some(10), Some(2)),
            (None, None),
            (Some(10), Some(1)),
            (Some(10), Some(3)),
        ];
        let t = target_for(&meta("r", Some(vec![1]), true), 10, &origins).unwrap();
        assert_eq!(t.schema.as_deref(), Some("public"));
        assert_eq!(t.table, "orders");
        assert_eq!(t.key, vec![2]);
        assert!(t.columns[1].is_none());
        let name = t.columns[0].as_ref().unwrap();
        assert_eq!(
            (name.name.as_str(), name.type_name.as_str()),
            ("name", "character varying(20)")
        );

        // Key in key order, not result order.
        let t = target_for(&meta("p", Some(vec![3, 1]), false), 10, &origins).unwrap();
        assert_eq!(t.key, vec![3, 2]);
    }

    #[test]
    fn explains_read_only_results() {
        let origins = [(Some(10), Some(2)), (Some(10), Some(3))];
        assert_eq!(
            target_for(&meta("r", Some(vec![1]), true), 10, &origins).unwrap_err(),
            "Read-only: add the primary key columns (id) to the query to edit rows."
        );
        assert_eq!(
            target_for(&meta("r", Some(vec![1, 2]), false), 10, &origins).unwrap_err(),
            "Read-only: add the key columns (id, name) to the query to edit rows."
        );
        assert_eq!(
            target_for(&meta("r", None, false), 10, &origins).unwrap_err(),
            "Read-only: table orders has no primary key."
        );
        assert_eq!(
            target_for(&meta("v", Some(vec![2]), true), 10, &origins).unwrap_err(),
            NOT_A_TABLE
        );
        assert_eq!(single_table(&[(None, None)]).unwrap_err(), NOT_A_TABLE);
        assert_eq!(
            single_table(&[(Some(1), Some(1)), (None, None), (Some(2), Some(1))]).unwrap_err(),
            "Read-only: the result combines columns from several tables."
        );
        assert_eq!(
            single_table(&[(None, None), (Some(5), Some(1)), (Some(5), Some(2))]),
            Ok(5)
        );
    }

    #[test]
    fn parses_catalog_rows() {
        let row = |attnum: &str, name: &str, ty: &str| {
            vec![
                some("public"),
                some("orders"),
                some("r"),
                some(attnum),
                some(name),
                some(ty),
                some("2,1"),
                some("t"),
            ]
        };
        let m = parse_table_meta(&[row("1", "id", "integer"), row("2", "Name", "text")]).unwrap();
        assert_eq!(m.key, Some(vec![2, 1]));
        assert!(m.primary);
        assert_eq!(m.columns[1], (2, "Name".to_string(), "text".to_string()));

        let no_key = vec![
            some("s"),
            some("t"),
            some("r"),
            some("1"),
            some("a"),
            some("integer"),
            None,
            None,
        ];
        let m = parse_table_meta(&[no_key]).unwrap();
        assert_eq!(m.key, None);
        assert!(!m.primary);
        assert!(parse_table_meta(&[]).is_none());
    }

    fn union(sql: &str) -> bool {
        has_union(&sql_tokens(sql))
    }

    fn joins(sql: &str) -> bool {
        joins_tables(&sql_tokens(sql))
    }

    #[test]
    fn finds_joins_at_any_depth() {
        assert!(joins(
            "SELECT a.id, a.x FROM t a JOIN t b ON b.id = a.parent"
        ));
        assert!(joins("select a.id from t a cross join t b"));
        assert!(joins("SELECT a.id FROM t a NATURAL JOIN t b"));
        assert!(joins("SELECT a.id, b.x FROM t a, t b WHERE a.id = b.id"));
        assert!(joins(
            "SELECT * FROM (SELECT a.id, b.x FROM t a, t b) s ORDER BY 1"
        ));
        assert!(joins(
            "SELECT * FROM (SELECT a.id FROM t a JOIN t b USING (id)) s"
        ));
        assert!(joins("SELECT a.id FROM t a, LATERAL (SELECT 1) l"));
        assert!(!joins("select a, b from t"));
        assert!(!joins("select f(a, b) from t where x in (1,2)"));
        assert!(!joins("select * from t order by a, b"));
        assert!(!joins("SELECT a, b FROM t GROUP BY a, b LIMIT 5"));
        assert!(!joins("SELECT extract(year FROM d), b FROM t"));
        assert!(!joins(
            "SELECT a FROM t WHERE b IN (SELECT x FROM u) AND c = 'x, y'"
        ));
        assert!(!joins("SELECT a FROM t FOR UPDATE OF t, u"));
        assert!(!joins(
            "SELECT a FROM \"join, x\" -- join
 WHERE b = 'join'"
        ));
        assert!(!joins("WITH c AS (SELECT 1) SELECT a FROM t"));
        assert!(!joins(
            "SELECT a, (SELECT max(b) FROM u WHERE u.a = t.a) FROM t"
        ));
    }

    #[test]
    fn finds_union_outside_literals() {
        assert!(union("SELECT 1 UNION SELECT 2"));
        assert!(union("select a from t\nunion all select a from u"));
        assert!(!union("SELECT 'union' FROM t"));
        assert!(!union("SELECT \"union\" FROM t"));
        assert!(!union("SELECT 1 -- union\nFROM t"));
        assert!(!union("SELECT 1 /* a /* union */ b */ FROM t"));
        assert!(!union("SELECT $$ union $$, $q$ union $q$ FROM t"));
        assert!(!union("SELECT E'it\\'s union' FROM t"));
        assert!(!union(
            "SELECT reunion, union_id, x$union FROM t WHERE a = $1"
        ));
        assert!(union("SELECT 'a''b' union SELECT 'c'"));
    }
}

/// Need `LQ_PG_HOST` (and optionally `LQ_PG_PORT`, `LQ_PG_USER`,
/// `LQ_PG_PASSWORD`, `LQ_PG_DB`), like `integration_tests`.
#[cfg(test)]
mod edit_integration_tests {
    use super::*;
    use crate::model::CellValue;

    fn env(name: &str) -> Option<String> {
        std::env::var(name).ok().filter(|v| !v.is_empty())
    }

    async fn session() -> Option<PgSession> {
        let profile = ConnectionProfile {
            name: "test".into(),
            host: env("LQ_PG_HOST")?,
            port: env("LQ_PG_PORT")
                .and_then(|p| p.parse().ok())
                .unwrap_or(5432),
            user: env("LQ_PG_USER").unwrap_or_else(|| "postgres".into()),
            database: env("LQ_PG_DB").unwrap_or_else(|| "postgres".into()),
            ssl_mode: SslMode::Prefer,
            ..Default::default()
        };
        Some(
            connect(&profile, env("LQ_PG_PASSWORD").as_deref())
                .await
                .expect("connection"),
        )
    }

    async fn exec(s: &PgSession, sql: &str) -> ExecResult {
        s.execute(&ExecRequest {
            sql: sql.into(),
            max_rows: 1000,
        })
        .await
        .unwrap_or_else(|e| panic!("{sql}: {e}"))
    }

    async fn rows(s: &PgSession, sql: &str) -> Vec<Vec<Option<String>>> {
        exec(s, sql).await.rows
    }

    async fn reason(s: &PgSession, sql: &str) -> String {
        let r = exec(s, sql).await;
        assert!(r.editable.is_none(), "{sql} should be read-only");
        r.read_only_reason
            .unwrap_or_else(|| panic!("{sql}: no reason"))
    }

    fn cell(column: usize, value: Option<&str>) -> CellValue {
        CellValue {
            column,
            value: value.map(str::to_string),
        }
    }

    fn update(key: &str, values: Vec<CellValue>) -> RowChange {
        RowChange::Update {
            key: vec![Some(key.into())],
            values,
        }
    }

    fn insert(key: &str, value: Option<&str>) -> RowChange {
        let mut values = vec![cell(0, Some(key))];
        if let Some(v) = value {
            values.push(cell(1, Some(v)));
        }
        RowChange::Insert { values }
    }

    async fn apply(
        s: &PgSession,
        target: &EditTarget,
        changes: Vec<RowChange>,
    ) -> AppResult<ApplyResult> {
        s.apply_changes(&ApplyRequest {
            target: target.clone(),
            changes,
        })
        .await
    }

    #[tokio::test]
    #[ignore]
    async fn postgres_edit_detection() {
        let Some(s) = session().await else { return };
        for sql in [
            "DROP VIEW IF EXISTS lq_edit_view",
            "DROP TABLE IF EXISTS lq_edit_det, lq_edit_det2, lq_edit_nokey, lq_edit_uniq",
            "CREATE TABLE lq_edit_det (id int PRIMARY KEY, name text, qty numeric(10,2))",
            "CREATE TABLE lq_edit_det2 (id int PRIMARY KEY, det_id int)",
            "CREATE TABLE lq_edit_nokey (a int, b text)",
            "CREATE TABLE lq_edit_uniq (a int NOT NULL, b int NOT NULL, c text, UNIQUE (a, b))",
            "CREATE VIEW lq_edit_view AS SELECT * FROM lq_edit_det",
            "INSERT INTO lq_edit_det VALUES (1, 'x', 1.5)",
        ] {
            exec(&s, sql).await;
        }

        let r = exec(
            &s,
            "SELECT name, upper(name) AS u, id, qty FROM lq_edit_det",
        )
        .await;
        assert!(r.read_only_reason.is_none());
        let t = r.editable.expect("editable");
        assert_eq!(t.table, "lq_edit_det");
        assert_eq!(t.schema, s.current_schema.clone());
        assert_eq!(t.key, vec![2]);
        assert!(t.columns[1].is_none());
        assert_eq!(t.columns[0].as_ref().unwrap().name, "name");
        assert_eq!(t.columns[3].as_ref().unwrap().type_name, "numeric(10,2)");

        let t = exec(&s, "SELECT c, b, a FROM lq_edit_uniq")
            .await
            .editable
            .expect("unique key");
        assert_eq!(t.key, vec![2, 1]);

        assert!(reason(
            &s,
            "SELECT d.id, d.name, e.id FROM lq_edit_det d JOIN lq_edit_det2 e ON e.det_id = d.id"
        )
        .await
        .contains("several tables"));
        // Self-joins report one table OID for both aliases.
        assert_eq!(
            reason(
                &s,
                "SELECT a.id, a.name, b.qty FROM lq_edit_det a JOIN lq_edit_det b ON b.id = a.id"
            )
            .await,
            JOINS_TABLES
        );
        assert_eq!(
            reason(&s, "SELECT a.id, b.name FROM lq_edit_det a, lq_edit_det b").await,
            JOINS_TABLES
        );
        assert_eq!(
            reason(&s, "SELECT * FROM lq_edit_nokey").await,
            "Read-only: table lq_edit_nokey has no primary key."
        );
        assert_eq!(
            reason(&s, "SELECT name FROM lq_edit_det").await,
            "Read-only: add the primary key columns (id) to the query to edit rows."
        );
        assert_eq!(reason(&s, "SELECT * FROM lq_edit_view").await, NOT_A_TABLE);
        assert_eq!(reason(&s, "SELECT 1").await, NOT_A_TABLE);
        // The server reports no source table for UNION columns; the scan
        // covers servers that report the first branch's table.
        let union = reason(
            &s,
            "SELECT id, name FROM lq_edit_det UNION ALL SELECT id, name FROM lq_edit_det",
        )
        .await;
        assert!(
            union == NOT_A_TABLE || union.contains("several queries"),
            "{union}"
        );

        // Detection inside a manual transaction (cache miss) keeps it usable.
        s.set_autocommit(false).await.unwrap();
        let r = exec(&s, "SELECT id, det_id FROM lq_edit_det2").await;
        assert!(r.in_transaction);
        assert_eq!(r.editable.expect("editable").key, vec![0]);
        assert_eq!(rows(&s, "SELECT 1").await, vec![vec![Some("1".into())]]);
        assert!(s.tx_state().in_transaction);
        s.end_transaction(false).await.unwrap();
        s.set_autocommit(true).await.unwrap();

        // DDL clears the cache: a key added later is seen.
        exec(&s, "ALTER TABLE lq_edit_nokey ADD PRIMARY KEY (a)").await;
        let t = exec(&s, "SELECT b, a FROM lq_edit_nokey")
            .await
            .editable
            .expect("editable");
        assert_eq!(t.key, vec![1]);

        exec(&s, "DROP VIEW lq_edit_view").await;
        exec(
            &s,
            "DROP TABLE lq_edit_det, lq_edit_det2, lq_edit_nokey, lq_edit_uniq",
        )
        .await;
    }

    #[tokio::test]
    #[ignore]
    async fn postgres_edit_round_trips_types() {
        let Some(s) = session().await else { return };
        exec(&s, "DROP TABLE IF EXISTS lq_edit_types").await;
        exec(
            &s,
            "CREATE TABLE lq_edit_types (id serial PRIMARY KEY, n numeric(12,3), nn numeric,
                i2 smallint, i4 integer, i8 bigint, f4 real, f8 double precision, b boolean,
                d date, ts timestamp(6), tstz timestamptz, tm time, ttz timetz, iv interval,
                t text, vc varchar(20), ch char(5), by bytea, arr int[], tarr text[], j jsonb,
                u uuid, m money, ip inet, tsarr timestamptz[])",
        )
        .await;
        exec(
            &s,
            r#"INSERT INTO lq_edit_types (n, nn, i2, i4, i8, f4, f8, b, d, ts, tstz, tm, ttz, iv,
                t, vc, ch, by, arr, tarr, j, u, m, ip, tsarr)
             VALUES (12345.678, 'NaN', -32768, 2147483647, -9223372036854775808, 1.1, 0.1, true,
                '2024-02-29', '2024-02-29 13:45:10.123456', '2024-02-29 13:45:10.25+00',
                '23:59:59.999999', '13:00:01+05:30', '1 year 2 mons -3 days 04:05:06.5',
                E'it''s\n"quoted" \\ end', 'abc', 'ab', '\xdeadbeef00', '{1,NULL,3}',
                '{"a b","c,d","","NULL",NULL}', '{"a": [1, 2]}',
                'a0eebc99-9c0b-4ef8-bb6d-6bb9bd380a11', 12.34::numeric::money,
                '192.168.0.1/24', '{"2024-01-01 00:00:00+00","1999-12-31 23:59:59.5-03"}')"#,
        )
        .await;
        exec(&s, "INSERT INTO lq_edit_types DEFAULT VALUES").await;

        let sql = "SELECT * FROM lq_edit_types ORDER BY id";
        let before = exec(&s, sql).await;
        let target = before.editable.clone().expect("editable");
        assert_eq!(target.key, vec![0]);
        let changes = before
            .rows
            .iter()
            .map(|row| {
                update(
                    row[0].as_deref().unwrap(),
                    (1..row.len()).map(|i| cell(i, row[i].as_deref())).collect(),
                )
            })
            .collect();
        let r = apply(&s, &target, changes).await.expect("apply");
        assert_eq!(r.updated, 2);
        assert!(!r.in_transaction);
        assert_eq!(exec(&s, sql).await.rows, before.rows);

        // The stored values themselves did not move.
        let check = rows(
            &s,
            r#"SELECT count(*) FROM lq_edit_types WHERE n = 12345.678 AND nn = 'NaN'
                AND f4 = 1.1::real AND f8 = 0.1::float8
                AND tstz = '2024-02-29 13:45:10.25+00' AND ttz = '13:00:01+05:30'
                AND iv = '1 year 2 mons -3 days 04:05:06.5'::interval
                AND t = E'it''s\n"quoted" \\ end' AND by = '\xdeadbeef00'::bytea
                AND m = 12.34::numeric::money AND ch = 'ab'
                AND tarr = '{"a b","c,d","","NULL",NULL}'::text[]
                AND tsarr[2] = '1999-12-31 23:59:59.5-03'"#,
        )
        .await;
        assert_eq!(check, vec![vec![Some("1".into())]]);

        // NULL in one row, a value in the row of NULLs.
        let id = before.rows[0][0].clone().unwrap();
        apply(
            &s,
            &target,
            vec![update(&id, vec![cell(15, None), cell(8, Some("false"))])],
        )
        .await
        .expect("apply");
        let row = rows(
            &s,
            &format!("SELECT t, b FROM lq_edit_types WHERE id = {id}"),
        )
        .await;
        assert_eq!(row, vec![vec![None, Some("false".into())]]);
        let id2 = before.rows[1][0].clone().unwrap();
        apply(
            &s,
            &target,
            vec![update(&id2, vec![cell(18, Some("\\x00ff"))])],
        )
        .await
        .expect("apply");
        let row = rows(
            &s,
            &format!("SELECT by FROM lq_edit_types WHERE id = {id2}"),
        )
        .await;
        assert_eq!(row, vec![vec![Some("\\x00ff".into())]]);

        // Too long for varchar(20) fails instead of being cut.
        let long = "x".repeat(21);
        let err = apply(&s, &target, vec![update(&id, vec![cell(16, Some(&long))])])
            .await
            .unwrap_err();
        assert_eq!(err.code.as_deref(), Some("22001"));

        exec(&s, "DROP TABLE lq_edit_types").await;
    }

    #[tokio::test]
    #[ignore]
    async fn postgres_edit_changes_and_transactions() {
        let Some(s) = session().await else { return };
        exec(
            &s,
            r#"DROP TABLE IF EXISTS "lq_edit_Mixed Case", lq_edit_defaults"#,
        )
        .await;
        exec(
            &s,
            r#"CREATE TABLE "lq_edit_Mixed Case" ("Key Col" int PRIMARY KEY,
                "Value ""Q""" text, created int DEFAULT 7)"#,
        )
        .await;
        exec(
            &s,
            "CREATE TABLE lq_edit_defaults (id serial PRIMARY KEY, v int DEFAULT 5)",
        )
        .await;

        let select = r#"SELECT "Key Col", "Value ""Q""" FROM "lq_edit_Mixed Case" ORDER BY 1"#;
        let target = exec(&s, select).await.editable.expect("editable");
        assert_eq!(target.table, "lq_edit_Mixed Case");
        assert_eq!(target.columns[1].as_ref().unwrap().name, "Value \"Q\"");

        let r = apply(
            &s,
            &target,
            vec![
                insert("1", Some("one")),
                insert("2", Some("two")),
                insert("3", None),
            ],
        )
        .await
        .expect("insert");
        assert_eq!(r.inserted, 3);
        assert_eq!(
            rows(
                &s,
                r#"SELECT created FROM "lq_edit_Mixed Case" WHERE "Key Col" = 3"#
            )
            .await,
            vec![vec![Some("7".into())]]
        );

        let r = apply(
            &s,
            &target,
            vec![
                update("1", vec![cell(1, Some("uno"))]),
                RowChange::Delete {
                    key: vec![Some("2".into())],
                },
            ],
        )
        .await
        .expect("update and delete");
        assert_eq!((r.updated, r.deleted), (1, 1));
        let expected = vec![
            vec![Some("1".to_string()), Some("uno".to_string())],
            vec![Some("3".to_string()), None],
        ];
        assert_eq!(rows(&s, select).await, expected);

        // A key that matches nothing undoes the whole batch.
        let err = apply(
            &s,
            &target,
            vec![
                update("1", vec![cell(1, Some("changed"))]),
                update("99", vec![cell(1, Some("x"))]),
            ],
        )
        .await
        .unwrap_err();
        assert_eq!(err.kind, ErrorKind::Database);
        assert!(
            err.message
                .starts_with("Row 2 was not saved: 0 rows matched"),
            "{err}"
        );
        assert_eq!(rows(&s, select).await, expected);

        // Server errors keep their code.
        let err = apply(&s, &target, vec![insert("1", Some("dup"))])
            .await
            .unwrap_err();
        assert_eq!(err.code.as_deref(), Some("23505"));
        assert!(err.message.starts_with("Row 1 was not saved"), "{err}");
        assert_eq!(rows(&s, select).await, expected);

        // Manual commit: the edits open a transaction and stay in it.
        s.set_autocommit(false).await.unwrap();
        let r = apply(
            &s,
            &target,
            vec![update("1", vec![cell(1, Some("manual"))])],
        )
        .await
        .expect("manual");
        assert!(r.in_transaction);
        assert!(s.tx_state().in_transaction);
        s.end_transaction(false).await.unwrap();
        assert_eq!(rows(&s, select).await, expected);
        s.end_transaction(false).await.unwrap();

        // A failed batch inside an open transaction keeps the transaction
        // and its earlier work.
        exec(
            &s,
            r#"UPDATE "lq_edit_Mixed Case" SET created = 8 WHERE "Key Col" = 3"#,
        )
        .await;
        assert!(s.tx_state().in_transaction);
        let err = apply(
            &s,
            &target,
            vec![
                update("3", vec![cell(1, Some("three"))]),
                update("99", vec![cell(1, Some("x"))]),
            ],
        )
        .await
        .unwrap_err();
        assert_eq!(err.kind, ErrorKind::Database);
        assert!(s.tx_state().in_transaction);
        s.end_transaction(true).await.unwrap();
        s.set_autocommit(true).await.unwrap();
        assert_eq!(
            rows(
                &s,
                r#"SELECT created, "Value ""Q""" FROM "lq_edit_Mixed Case" WHERE "Key Col" = 3"#
            )
            .await,
            vec![vec![Some("8".into()), None]]
        );

        // No values: all defaults.
        let target = exec(&s, "SELECT id, v FROM lq_edit_defaults")
            .await
            .editable
            .expect("editable");
        let r = apply(&s, &target, vec![RowChange::Insert { values: vec![] }])
            .await
            .expect("defaults");
        assert_eq!(r.inserted, 1);
        assert_eq!(
            rows(&s, "SELECT v FROM lq_edit_defaults").await,
            vec![vec![Some("5".into())]]
        );

        exec(&s, r#"DROP TABLE "lq_edit_Mixed Case", lq_edit_defaults"#).await;
    }
}
