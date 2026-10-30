//! Apache Cassandra session over the native CQL protocol, through the
//! `scylla` driver (it speaks to Apache Cassandra 3.x, 4.x and 5.x as well).
//!
//! - Every statement is sent unprepared as text, like cqlsh does. Rows are
//!   read with paging: the page size comes from the row limit and reading
//!   stops at `max_rows + 1`, so a `SELECT` on a big table never streams
//!   everything to show a few rows.
//! - Values keep their exact CQL text (`varint` and `decimal` keep every
//!   digit, floats are shortest round-trip, blobs are `0x...`). Collections,
//!   tuples, UDTs and vectors are written as valid CQL literals, so a cell can
//!   be pasted into an `INSERT` or `UPDATE` as it is.
//! - There are no transactions: the session is always in autocommit mode and
//!   `BEGIN BATCH ... APPLY BATCH` is just one statement. Saving edits from
//!   the grid sends one logged batch.
//! - CQL has no server-side cancel. `cancel` drops the request future, so the
//!   interface gets its answer at once, but the coordinator keeps working
//!   until its own request timeout.
//! - Server warnings (tombstone thresholds, large batches, aggregations
//!   without a partition key) arrive as messages, like PostgreSQL notices.
//! - Requests run at consistency `LOCAL_ONE`, the cqlsh default. TLS uses
//!   rustls; with the local datacenter set, only its nodes get requests.
//! - The schema comes from `system_schema` (and `system_virtual_schema` for
//!   the virtual keyspaces of 4.0+), which every 3.0+ server has.
//!   `DESCRIBE` is a cqlsh command, not part of the protocol, so the CREATE
//!   statements are built from the catalog.

use std::collections::HashMap;
use std::fmt::Write as _;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use futures_util::StreamExt;
use parking_lot::Mutex;
use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::client::WebPkiServerVerifier;
use rustls::pki_types::{CertificateDer, ServerName, UnixTime};
use rustls::{CertificateError, DigitallySignedStruct, SignatureScheme};
use scylla::client::execution_profile::ExecutionProfile;
use scylla::client::session::Session as Cql;
use scylla::client::session_builder::SessionBuilder;
use scylla::errors::{
    DbError, ExecutionError, IntoRowsResultError, NewSessionError, NextPageError, NextRowError,
    PagerExecutionError, RequestAttemptError, RequestError,
};
use scylla::frame::response::result::{CollectionType, ColumnType, NativeType};
use scylla::policies::load_balancing::DefaultPolicy;
use scylla::response::PagingState;
use scylla::statement::{Consistency, Statement};
use scylla::value::{CqlValue, Row};
use tokio::sync::Notify;

use super::{first_keywords, RowSink, StreamEvent, TxState, STREAM_BATCH};
use crate::error::{AppError, AppResult, ErrorKind};
use crate::model::{
    ApplyRequest, ApplyResult, ColumnInfo, ConnectionProfile, DbColumn, DbObject, EditColumn,
    EditTarget, ExecRequest, ExecResult, RowChange, SslMode, ValueKind,
};

const DEFAULT_PORT: u16 = 9042;
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
/// TLS attempt of `prefer`: a server without TLS gives up early and the
/// session falls back to plain TCP.
const PREFER_TLS_TIMEOUT: Duration = Duration::from_secs(8);
/// Largest page asked for in one request.
const PAGE_CAP: usize = 5000;
/// Page size of the catalog queries.
const CATALOG_PAGE: i32 = 1000;
const BINARY_PREVIEW_BYTES: usize = 32 * 1024;

/// Keyspaces that belong to the server, listed after the user's.
const SYSTEM_KEYSPACES: &[&str] = &[
    "system",
    "system_schema",
    "system_auth",
    "system_distributed",
    "system_traces",
    "system_views",
    "system_virtual_schema",
];
const VIRTUAL_KEYSPACES: &[&str] = &["system_views", "system_virtual_schema"];

pub struct CassSession {
    cql: Cql,
    busy: AtomicBool,
    cancel: Arc<Notify>,
    /// Key information per table, for result editing. Cleared by any
    /// statement that is not a SELECT.
    tables: Mutex<HashMap<(String, String), Arc<TableMeta>>>,
    pub server_version: String,
    pub current_user: Option<String>,
    pub current_schema: Option<String>,
}

pub async fn connect(
    profile: &ConnectionProfile,
    password: Option<&str>,
) -> AppResult<CassSession> {
    // rustls has more than one crypto provider compiled in.
    let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();

    let port = if profile.port == 0 {
        DEFAULT_PORT
    } else {
        profile.port
    };
    let contact_points: Vec<String> = profile
        .host
        .split(',')
        .map(str::trim)
        .filter(|h| !h.is_empty())
        .map(|h| contact_point(h, port))
        .collect();
    if contact_points.is_empty() {
        return Err(AppError::invalid("Enter at least one contact point."));
    }

    let build = |tls: Option<Arc<rustls::ClientConfig>>| {
        let mut builder = SessionBuilder::new()
            .known_nodes(&contact_points)
            .connection_timeout(CONNECT_TIMEOUT)
            .fetch_schema_metadata(false)
            .default_execution_profile_handle(execution_profile(&profile.datacenter));
        let keyspace = profile.database.trim();
        if !keyspace.is_empty() {
            builder = builder.use_keyspace(keyspace, true);
        }
        let user = profile.user.trim();
        if !user.is_empty() {
            builder = builder.user(user, password.unwrap_or_default());
        }
        if let Some(config) = tls {
            builder = builder.tls_context(Some(config));
        }
        async move { builder.build().await }
    };

    let cql = match profile.ssl_mode {
        SslMode::Disable => build(None).await,
        SslMode::Prefer => {
            let attempt = tokio::time::timeout(
                PREFER_TLS_TIMEOUT,
                build(Some(tls_config(SslMode::Prefer)?)),
            )
            .await;
            match attempt {
                Ok(Ok(cql)) => Ok(cql),
                Ok(Err(e)) if is_auth_text(&chain(&e)) => Err(e),
                _ => build(None).await,
            }
        }
        SslMode::Require => build(Some(tls_config(SslMode::Require)?)).await,
        SslMode::VerifyFull => build(Some(tls_config(SslMode::VerifyFull)?)).await,
    }
    .map_err(connect_error)?;

    let version = fetch(&cql, "SELECT release_version FROM system.local")
        .await
        .ok()
        .and_then(|f| f.rows.into_iter().next())
        .and_then(|mut r| r.pop().flatten())
        .and_then(|v| text_of(&Some(v)));
    let user = profile.user.trim();
    Ok(CassSession {
        current_schema: cql.get_keyspace().map(|k| (*k).clone()),
        cql,
        busy: AtomicBool::new(false),
        cancel: Arc::new(Notify::new()),
        tables: Mutex::new(HashMap::new()),
        server_version: match version {
            Some(v) => format!("Apache Cassandra {v}"),
            None => "Apache Cassandra".into(),
        },
        current_user: (!user.is_empty()).then(|| user.to_string()),
    })
}

/// `host` or `host:port` as the driver wants it; a bare IPv6 address is
/// bracketed.
fn contact_point(host: &str, default_port: u16) -> String {
    if host.starts_with('[') {
        return if host.contains("]:") {
            host.to_string()
        } else {
            format!("{host}:{default_port}")
        };
    }
    match host.matches(':').count() {
        0 => format!("{host}:{default_port}"),
        1 => host.to_string(),
        _ => format!("[{host}]:{default_port}"),
    }
}

/// Requests go to the local datacenter only, when one is named.
fn execution_profile(
    datacenter: &str,
) -> scylla::client::execution_profile::ExecutionProfileHandle {
    let mut policy = DefaultPolicy::builder();
    let dc = datacenter.trim();
    if !dc.is_empty() {
        policy = policy
            .prefer_datacenter(dc.to_string())
            .permit_dc_failover(false);
    }
    ExecutionProfile::builder()
        .load_balancing_policy(policy.build())
        .consistency(Consistency::LocalOne)
        // The server's own timeouts apply; Cancel stops waiting.
        .request_timeout(None)
        .build()
        .into_handle()
}

// ---------------------------------------------------------------------------
// TLS

fn tls_config(mode: SslMode) -> AppResult<Arc<rustls::ClientConfig>> {
    if mode != SslMode::VerifyFull {
        // `prefer` and `require` encrypt without validating the certificate,
        // as everywhere else in the app.
        return super::postgres::tls_config(mode).map(Arc::new);
    }
    let provider = Arc::new(rustls::crypto::aws_lc_rs::default_provider());
    let mut roots = rustls::RootCertStore::empty();
    roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
    for cert in rustls_native_certs::load_native_certs().certs {
        let _ = roots.add(cert);
    }
    let inner = WebPkiServerVerifier::builder_with_provider(Arc::new(roots), provider.clone())
        .build()
        .map_err(|e| AppError::internal(e.to_string()))?;
    let config = rustls::ClientConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .map_err(|e| AppError::internal(e.to_string()))?
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(ChainVerifier(inner)))
        .with_no_client_auth();
    Ok(Arc::new(config))
}

/// Validates the chain, expiry and signatures against the trusted roots but
/// not the host name: the driver connects to the addresses the cluster
/// reports (`system.peers`), which certificates rarely list.
#[derive(Debug)]
struct ChainVerifier(Arc<WebPkiServerVerifier>);

impl ServerCertVerifier for ChainVerifier {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        intermediates: &[CertificateDer<'_>],
        server_name: &ServerName<'_>,
        ocsp_response: &[u8],
        now: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        match self
            .0
            .verify_server_cert(end_entity, intermediates, server_name, ocsp_response, now)
        {
            Err(rustls::Error::InvalidCertificate(
                CertificateError::NotValidForName | CertificateError::NotValidForNameContext { .. },
            )) => Ok(ServerCertVerified::assertion()),
            other => other,
        }
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        self.0.verify_tls12_signature(message, cert, dss)
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        self.0.verify_tls13_signature(message, cert, dss)
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.0.supported_verify_schemes()
    }
}

// ---------------------------------------------------------------------------
// Queries

/// Rows of a catalog query, values as the driver decoded them.
struct Fetched {
    names: Vec<String>,
    rows: Vec<Vec<Option<CqlValue>>>,
}

impl Fetched {
    fn index(&self, name: &str) -> Option<usize> {
        self.names.iter().position(|n| n == name)
    }

    fn get<'a>(&self, row: &'a [Option<CqlValue>], name: &str) -> Option<&'a CqlValue> {
        self.index(name)
            .and_then(|i| row.get(i))
            .and_then(Option::as_ref)
    }

    fn text(&self, row: &[Option<CqlValue>], name: &str) -> Option<String> {
        self.index(name).and_then(|i| row.get(i)).and_then(text_of)
    }
}

fn text_of(v: &Option<CqlValue>) -> Option<String> {
    match v {
        Some(CqlValue::Text(s) | CqlValue::Ascii(s)) => Some(s.clone()),
        _ => None,
    }
}

fn text_list(v: Option<&CqlValue>) -> Vec<String> {
    match v {
        Some(CqlValue::List(items) | CqlValue::Set(items)) => items
            .iter()
            .filter_map(|i| text_of(&Some(i.clone())))
            .collect(),
        _ => Vec::new(),
    }
}

/// Runs a catalog statement and reads every page.
async fn fetch(cql: &Cql, sql: &str) -> AppResult<Fetched> {
    let mut names;
    let mut rows = Vec::new();
    let mut state = PagingState::start();
    loop {
        let statement = Statement::new(sql).with_page_size(CATALOG_PAGE);
        let (result, flow) = cql
            .query_single_page(statement, &[], state)
            .await
            .map_err(|e| map_execution(e, Some(sql)))?;
        let page = result.into_rows_result().map_err(|e| match e {
            IntoRowsResultError::ResultNotRows(_) => {
                AppError::internal("The catalog statement returned no rows.")
            }
            other => AppError::internal(other.to_string()),
        })?;
        names = page
            .column_specs()
            .iter()
            .map(|c| c.name().to_string())
            .collect();
        for row in page
            .rows::<Row>()
            .map_err(|e| AppError::internal(e.to_string()))?
        {
            rows.push(row.map_err(|e| AppError::internal(e.to_string()))?.columns);
        }
        match flow.into_paging_control_flow() {
            std::ops::ControlFlow::Break(()) => break,
            std::ops::ControlFlow::Continue(next) => state = next,
        }
    }
    Ok(Fetched { names, rows })
}

/// Names a catalog query matches on, as a CQL string literal.
fn lit(s: &str) -> String {
    format!("'{}'", s.replace('\'', "''"))
}

fn is_virtual(keyspace: &str) -> bool {
    VIRTUAL_KEYSPACES.contains(&keyspace)
}

fn is_system(keyspace: &str) -> bool {
    SYSTEM_KEYSPACES.contains(&keyspace) || keyspace.starts_with("system_")
}

/// One result column: what the editor needs to know about its origin.
struct ResultColumn {
    name: String,
    keyspace: String,
    table: String,
}

struct TableMeta {
    columns: Vec<DbColumn>,
    view: bool,
}

impl CassSession {
    /// Cassandra 3.x, whose table options differ from 4.0 and later.
    fn legacy(&self) -> bool {
        self.server_version.starts_with("Apache Cassandra 3.")
    }

    pub fn tx_state(&self) -> TxState {
        TxState {
            autocommit: true,
            in_transaction: false,
        }
    }

    pub async fn execute(&self, req: &ExecRequest) -> AppResult<ExecResult> {
        self.busy.store(true, Ordering::SeqCst);
        let cancelled = self.cancel.notified();
        tokio::pin!(cancelled);
        cancelled.as_mut().enable();
        let outcome = tokio::select! {
            r = self.run(req) => r,
            () = &mut cancelled => Err(AppError::new(ErrorKind::Cancelled, "Cancelled.")),
        };
        self.busy.store(false, Ordering::SeqCst);
        outcome
    }

    /// CQL cannot ask the server to stop a request: the wait ends here and the
    /// coordinator finishes (or times out) on its own.
    pub async fn cancel(&self) -> AppResult<()> {
        if self.busy.load(Ordering::SeqCst) {
            self.cancel.notify_waiters();
        }
        Ok(())
    }

    pub async fn set_autocommit(&self, on: bool) -> AppResult<TxState> {
        if on {
            Ok(self.tx_state())
        } else {
            Err(AppError::invalid(
                "Cassandra has no transactions: every statement commits on its own.",
            ))
        }
    }

    pub async fn end_transaction(&self, _commit: bool) -> AppResult<TxState> {
        Err(AppError::invalid(
            "Cassandra has no transactions: every statement commits on its own.",
        ))
    }

    async fn run(&self, req: &ExecRequest) -> AppResult<ExecResult> {
        let started = Instant::now();
        let sql = req.sql.as_str();
        let limit = req.max_rows;
        let mut result = ExecResult::default();
        let mut columns: Option<Vec<ColumnInfo>> = None;
        let mut origins: Vec<ResultColumn> = Vec::new();
        let mut rows: Vec<Vec<Option<String>>> = Vec::new();
        let mut state = PagingState::start();
        loop {
            let size = if limit == 0 {
                PAGE_CAP
            } else {
                (limit + 1 - rows.len().min(limit + 1)).clamp(1, PAGE_CAP)
            };
            let statement = Statement::new(sql).with_page_size(size as i32);
            let (page, flow) = self
                .cql
                .query_single_page(statement, &[], state)
                .await
                .map_err(|e| map_execution(e, Some(sql)))?;
            match page.into_rows_result() {
                Ok(page) => {
                    for w in page.warnings() {
                        result.notices.push(format!("WARNING: {w}"));
                    }
                    if columns.is_none() {
                        columns = Some(
                            page.column_specs()
                                .iter()
                                .map(|c| ColumnInfo {
                                    name: c.name().to_string(),
                                    type_name: type_name(c.typ()),
                                    kind: column_kind(c.typ()),
                                })
                                .collect(),
                        );
                        origins = page
                            .column_specs()
                            .iter()
                            .map(|c| ResultColumn {
                                name: c.name().to_string(),
                                keyspace: c.table_spec().ks_name().to_string(),
                                table: c.table_spec().table_name().to_string(),
                            })
                            .collect();
                    }
                    let iter = page
                        .rows::<Row>()
                        .map_err(|e| AppError::internal(e.to_string()))?;
                    for row in iter {
                        let row = row.map_err(|e| AppError::internal(e.to_string()))?;
                        rows.push(row.columns.iter().map(cell).collect());
                    }
                }
                Err(IntoRowsResultError::ResultNotRows(page)) => {
                    for w in page.warnings() {
                        result.notices.push(format!("WARNING: {w}"));
                    }
                    break;
                }
                Err(e) => return Err(AppError::internal(e.to_string())),
            }
            if limit > 0 && rows.len() > limit {
                break;
            }
            match flow.into_paging_control_flow() {
                std::ops::ControlFlow::Break(()) => break,
                std::ops::ControlFlow::Continue(next) => state = next,
            }
        }
        if limit > 0 && rows.len() > limit {
            rows.truncate(limit);
            result.truncated = true;
        }

        let is_select = first_keywords(sql, 1).first().map(String::as_str) == Some("SELECT");
        if !is_select {
            self.tables.lock().clear();
        }
        if let Some(cols) = columns {
            let (editable, reason) = self.editable(sql, is_select, &origins).await;
            result.editable = editable;
            result.read_only_reason = reason;
            result.columns = Some(cols);
            result.rows = rows;
        }
        result.elapsed_ms = started.elapsed().as_millis() as u64;
        Ok(result)
    }

    pub async fn list_schemas(&self) -> AppResult<Vec<String>> {
        let mut names: Vec<String> = fetch(
            &self.cql,
            "SELECT keyspace_name FROM system_schema.keyspaces",
        )
        .await?
        .rows
        .into_iter()
        .filter_map(|mut r| r.pop().and_then(|v| text_of(&v)))
        .collect();
        // Virtual keyspaces exist from Cassandra 4.0.
        if let Ok(f) = fetch(
            &self.cql,
            "SELECT keyspace_name FROM system_virtual_schema.keyspaces",
        )
        .await
        {
            names.extend(
                f.rows
                    .into_iter()
                    .filter_map(|mut r| r.pop().and_then(|v| text_of(&v))),
            );
        }
        names.sort();
        names.dedup();
        names.sort_by_key(|n| is_system(n));
        Ok(names)
    }

    pub async fn list_objects(&self, schema: &str) -> AppResult<Vec<DbObject>> {
        let ks = lit(schema);
        let mut out = Vec::new();
        let mut add = |kind: &str, names: Vec<String>| {
            let mut names = names;
            names.sort();
            out.extend(names.into_iter().map(|name| DbObject {
                name,
                kind: kind.into(),
            }));
        };
        let names_of = |f: Fetched, column: &str| -> Vec<String> {
            f.rows.iter().filter_map(|r| f.text(r, column)).collect()
        };
        if is_virtual(schema) {
            let f = fetch(
                &self.cql,
                &format!(
                    "SELECT table_name FROM system_virtual_schema.tables WHERE keyspace_name = {ks}"
                ),
            )
            .await?;
            add("table", names_of(f, "table_name"));
            return Ok(out);
        }
        let f = fetch(
            &self.cql,
            &format!("SELECT table_name FROM system_schema.tables WHERE keyspace_name = {ks}"),
        )
        .await?;
        add("table", names_of(f, "table_name"));
        let f = fetch(
            &self.cql,
            &format!("SELECT view_name FROM system_schema.views WHERE keyspace_name = {ks}"),
        )
        .await?;
        add("matview", names_of(f, "view_name"));
        let f = fetch(
            &self.cql,
            &format!("SELECT type_name FROM system_schema.types WHERE keyspace_name = {ks}"),
        )
        .await?;
        add("type", names_of(f, "type_name"));
        for (kind, table, column) in [
            ("function", "functions", "function_name"),
            ("aggregate", "aggregates", "aggregate_name"),
        ] {
            let f = fetch(
                &self.cql,
                &format!(
                    "SELECT {column}, argument_types FROM system_schema.{table} WHERE keyspace_name = {ks}"
                ),
            )
            .await?;
            let names = f
                .rows
                .iter()
                .filter_map(|r| {
                    let name = f.text(r, column)?;
                    Some(format!(
                        "{name}({})",
                        text_list(f.get(r, "argument_types")).join(", ")
                    ))
                })
                .collect();
            add(kind, names);
        }
        let f = fetch(
            &self.cql,
            &format!("SELECT index_name FROM system_schema.indexes WHERE keyspace_name = {ks}"),
        )
        .await?;
        add("index", names_of(f, "index_name"));
        Ok(out)
    }

    pub async fn list_columns(&self, schema: &str, table: &str) -> AppResult<Vec<DbColumn>> {
        let (ks, t) = (lit(schema), lit(table));
        let f = if is_virtual(schema) {
            fetch(
                &self.cql,
                &format!(
                    "SELECT column_name, type, kind, position FROM system_virtual_schema.columns \
                     WHERE keyspace_name = {ks} AND table_name = {t}"
                ),
            )
            .await?
        } else {
            fetch(
                &self.cql,
                &format!(
                    "SELECT column_name, type, kind, position FROM system_schema.columns \
                     WHERE keyspace_name = {ks} AND table_name = {t}"
                ),
            )
            .await?
        };
        let mut cols: Vec<(u8, i32, DbColumn)> = f
            .rows
            .iter()
            .filter_map(|r| {
                let name = f.text(r, "column_name")?;
                let kind = f.text(r, "kind").unwrap_or_default().to_ascii_lowercase();
                let position = match f.get(r, "position") {
                    Some(CqlValue::Int(p)) => *p,
                    _ => 0,
                };
                let (order, role) = match kind.as_str() {
                    "partition_key" => (0, "partition"),
                    "clustering" => (1, "clustering"),
                    "static" => (2, "static"),
                    _ => (3, "regular"),
                };
                let key = order < 2;
                Some((
                    order,
                    if key { position } else { 0 },
                    DbColumn {
                        name,
                        type_name: f.text(r, "type").unwrap_or_default(),
                        nullable: !key,
                        primary_key: key,
                        default_value: None,
                        key_role: Some(role.into()),
                    },
                ))
            })
            .collect();
        cols.sort_by(|a, b| (a.0, a.1, &a.2.name).cmp(&(b.0, b.1, &b.2.name)));
        Ok(cols.into_iter().map(|c| c.2).collect())
    }

    /// Runs a query and sends its rows to `sink` in batches, one page at a time.
    pub async fn stream(&self, sql: &str, sink: RowSink) -> AppResult<u64> {
        let statement = Statement::new(sql).with_page_size(STREAM_BATCH as i32);
        let pager = self
            .cql
            .query_iter(statement, &[])
            .await
            .map_err(map_pager)?;
        let info: Vec<ColumnInfo> = pager
            .column_specs()
            .iter()
            .map(|c| ColumnInfo {
                name: c.name().to_string(),
                type_name: type_name(c.typ()),
                kind: column_kind(c.typ()),
            })
            .collect();
        if info.is_empty() {
            return Err(AppError::invalid("The statement returns no rows."));
        }
        sink.send(StreamEvent::Columns(info))
            .await
            .map_err(|_| super::stream_stopped())?;
        let mut rows = pager
            .rows_stream::<Row>()
            .map_err(|e| AppError::internal(e.to_string()))?;
        let mut total = 0u64;
        let mut batch = Vec::with_capacity(STREAM_BATCH);
        while let Some(row) = rows.next().await {
            let row = row.map_err(map_next_row)?;
            batch.push(super::full_values(|| {
                row.columns.iter().map(cell).collect::<Vec<_>>()
            }));
            total += 1;
            if batch.len() == STREAM_BATCH {
                let full = std::mem::replace(&mut batch, Vec::with_capacity(STREAM_BATCH));
                sink.send(StreamEvent::Rows(full))
                    .await
                    .map_err(|_| super::stream_stopped())?;
            }
        }
        if !batch.is_empty() {
            sink.send(StreamEvent::Rows(batch))
                .await
                .map_err(|_| super::stream_stopped())?;
        }
        Ok(total)
    }

    // -----------------------------------------------------------------------
    // Result editing

    /// The table behind a result, when each row can be addressed by its full
    /// primary key.
    async fn editable(
        &self,
        sql: &str,
        is_select: bool,
        origins: &[ResultColumn],
    ) -> (Option<EditTarget>, Option<String>) {
        let reason = |text: &str| (None, Some(text.to_string()));
        if !is_select {
            return reason("Only the rows of a SELECT can be edited.");
        }
        let Some(first) = origins.first() else {
            return reason("The result has no columns.");
        };
        if origins
            .iter()
            .any(|c| (&c.keyspace, &c.table) != (&first.keyspace, &first.table))
        {
            return reason("The result does not come from a single table.");
        }
        if is_system(&first.keyspace) {
            return reason("System tables are read-only.");
        }
        if has_alias(sql) {
            return reason("Columns with an alias (AS) cannot be matched to the table.");
        }
        let meta = match self.table_meta(&first.keyspace, &first.table).await {
            Ok(m) => m,
            Err(_) => return reason("The table's key could not be read."),
        };
        if meta.view {
            return reason("Materialized views cannot be edited.");
        }
        if meta.columns.iter().any(|c| c.type_name == "counter") {
            return reason("Counter tables cannot be edited in the grid.");
        }
        let mut key = Vec::new();
        let mut missing = Vec::new();
        for c in meta.columns.iter().filter(|c| c.primary_key) {
            match origins.iter().position(|o| o.name == c.name) {
                Some(i) => key.push(i),
                None => missing.push(quote_name(&c.name)),
            }
        }
        if key.is_empty() && missing.is_empty() {
            return reason("The table has no primary key.");
        }
        if !missing.is_empty() {
            return reason(&format!(
                "The result must include the whole primary key (missing {}).",
                missing.join(", ")
            ));
        }
        let columns = origins
            .iter()
            .map(|o| {
                meta.columns
                    .iter()
                    .find(|c| c.name == o.name)
                    .map(|c| EditColumn {
                        name: c.name.clone(),
                        type_name: c.type_name.clone(),
                    })
            })
            .collect();
        (
            Some(EditTarget {
                schema: Some(first.keyspace.clone()),
                table: first.table.clone(),
                columns,
                key,
            }),
            None,
        )
    }

    async fn table_meta(&self, keyspace: &str, table: &str) -> AppResult<Arc<TableMeta>> {
        let id = (keyspace.to_string(), table.to_string());
        if let Some(m) = self.tables.lock().get(&id) {
            return Ok(m.clone());
        }
        let columns = self.list_columns(keyspace, table).await?;
        let views = fetch(
            &self.cql,
            &format!(
                "SELECT view_name FROM system_schema.views WHERE keyspace_name = {} AND view_name = {}",
                lit(keyspace),
                lit(table)
            ),
        )
        .await?;
        let meta = Arc::new(TableMeta {
            columns,
            view: !views.rows.is_empty(),
        });
        self.tables.lock().insert(id, meta.clone());
        Ok(meta)
    }

    /// Saves the grid's changes as one logged batch, which Cassandra applies
    /// completely or not at all. CQL writes are upserts, so an UPDATE or
    /// DELETE whose row was removed meanwhile does not fail: the counts are
    /// the statements sent.
    pub async fn apply_changes(&self, req: &ApplyRequest) -> AppResult<ApplyResult> {
        let (statements, mut result) = plan_changes(&req.target, &req.changes)?;
        let started = Instant::now();
        let sql = match statements.len() {
            0 => return Ok(result),
            1 => statements[0].clone(),
            _ => format!("BEGIN BATCH\n{};\nAPPLY BATCH", statements.join(";\n")),
        };
        self.execute(&ExecRequest { sql, max_rows: 0 }).await?;
        result.elapsed_ms = started.elapsed().as_millis() as u64;
        Ok(result)
    }

    // -----------------------------------------------------------------------
    // DDL

    pub async fn object_ddl(&self, schema: &str, name: &str, kind: &str) -> AppResult<String> {
        match kind {
            "keyspace" => self.keyspace_ddl(name).await,
            "table" => self.table_ddl(schema, name).await,
            "matview" => self.view_ddl(schema, name).await,
            "type" => self.type_ddl(schema, name).await,
            "function" => self.function_ddl(schema, name).await,
            "aggregate" => self.aggregate_ddl(schema, name).await,
            "index" => self.index_ddl(schema, name).await,
            other => Err(AppError::invalid(format!(
                "LowQuery cannot script a {other} on Cassandra."
            ))),
        }
    }

    async fn keyspace_ddl(&self, name: &str) -> AppResult<String> {
        let f = fetch(
            &self.cql,
            &format!(
                "SELECT * FROM system_schema.keyspaces WHERE keyspace_name = {}",
                lit(name)
            ),
        )
        .await?;
        let Some(row) = f.rows.first() else {
            return Ok(String::new());
        };
        let replication = f
            .get(row, "replication")
            .map_or_else(|| "{}".to_string(), literal);
        let durable = f
            .get(row, "durable_writes")
            .map_or_else(|| "true".to_string(), literal);
        Ok(format!(
            "CREATE KEYSPACE {} WITH replication = {replication}\n    AND durable_writes = {durable};\n",
            quote_name(name)
        ))
    }

    async fn type_ddl(&self, keyspace: &str, name: &str) -> AppResult<String> {
        let f = fetch(
            &self.cql,
            &format!(
                "SELECT * FROM system_schema.types WHERE keyspace_name = {} AND type_name = {}",
                lit(keyspace),
                lit(name)
            ),
        )
        .await?;
        let Some(row) = f.rows.first() else {
            return Ok(String::new());
        };
        let names = text_list(f.get(row, "field_names"));
        let types = text_list(f.get(row, "field_types"));
        let fields: Vec<String> = names
            .iter()
            .zip(&types)
            .map(|(n, t)| format!("    {} {t}", quote_name(n)))
            .collect();
        Ok(format!(
            "CREATE TYPE {}.{} (\n{}\n);\n",
            quote_name(keyspace),
            quote_name(name),
            fields.join(",\n")
        ))
    }

    async fn table_ddl(&self, keyspace: &str, name: &str) -> AppResult<String> {
        let f = fetch(
            &self.cql,
            &format!(
                "SELECT * FROM system_schema.tables WHERE keyspace_name = {} AND table_name = {}",
                lit(keyspace),
                lit(name)
            ),
        )
        .await?;
        let Some(row) = f.rows.first() else {
            return Ok(String::new());
        };
        let mut out = String::new();
        let columns = self.catalog_columns(keyspace, name).await?;
        write_create(
            &mut out,
            &format!("CREATE TABLE {}.{}", quote_name(keyspace), quote_name(name)),
            &columns,
            &table_options(&f, row, self.legacy()),
        );
        out.push('\n');
        for statement in self.index_statements(keyspace, Some(name), None).await? {
            out.push_str(&statement);
            out.push('\n');
        }
        Ok(out)
    }

    async fn view_ddl(&self, keyspace: &str, name: &str) -> AppResult<String> {
        let f = fetch(
            &self.cql,
            &format!(
                "SELECT * FROM system_schema.views WHERE keyspace_name = {} AND view_name = {}",
                lit(keyspace),
                lit(name)
            ),
        )
        .await?;
        let Some(row) = f.rows.first() else {
            return Ok(String::new());
        };
        let columns = self.catalog_columns(keyspace, name).await?;
        let base = f.text(row, "base_table_name").unwrap_or_default();
        let all = matches!(
            f.get(row, "include_all_columns"),
            Some(CqlValue::Boolean(true))
        );
        let select = if all {
            "*".to_string()
        } else {
            columns
                .iter()
                .map(|c| quote_name(&c.name))
                .collect::<Vec<_>>()
                .join(", ")
        };
        let header = format!(
            "CREATE MATERIALIZED VIEW {}.{} AS\n    SELECT {select}\n    FROM {}.{}\n    WHERE {}",
            quote_name(keyspace),
            quote_name(name),
            quote_name(keyspace),
            quote_name(&base),
            f.text(row, "where_clause").unwrap_or_default()
        );
        let mut out = String::new();
        write_primary_key(
            &mut out,
            &header,
            &columns,
            &table_options(&f, row, self.legacy()),
        );
        Ok(out)
    }

    async fn catalog_columns(&self, keyspace: &str, table: &str) -> AppResult<Vec<CatalogColumn>> {
        let f = fetch(
            &self.cql,
            &format!(
                "SELECT column_name, type, kind, position, clustering_order \
                 FROM system_schema.columns WHERE keyspace_name = {} AND table_name = {}",
                lit(keyspace),
                lit(table)
            ),
        )
        .await?;
        let mut cols: Vec<CatalogColumn> = f
            .rows
            .iter()
            .filter_map(|r| {
                Some(CatalogColumn {
                    name: f.text(r, "column_name")?,
                    type_name: f.text(r, "type")?,
                    kind: f.text(r, "kind").unwrap_or_default().to_ascii_lowercase(),
                    position: match f.get(r, "position") {
                        Some(CqlValue::Int(p)) => *p,
                        _ => 0,
                    },
                    descending: f
                        .text(r, "clustering_order")
                        .is_some_and(|o| o.eq_ignore_ascii_case("desc")),
                })
            })
            .collect();
        cols.sort_by(|a, b| (a.rank(), a.position, &a.name).cmp(&(b.rank(), b.position, &b.name)));
        Ok(cols)
    }

    /// CREATE INDEX statements of a table (or the one index named `only`).
    async fn index_statements(
        &self,
        keyspace: &str,
        table: Option<&str>,
        only: Option<&str>,
    ) -> AppResult<Vec<String>> {
        let f = fetch(
            &self.cql,
            &format!(
                "SELECT * FROM system_schema.indexes WHERE keyspace_name = {}",
                lit(keyspace)
            ),
        )
        .await?;
        let mut out = Vec::new();
        for row in &f.rows {
            let (Some(index), Some(on)) = (f.text(row, "index_name"), f.text(row, "table_name"))
            else {
                continue;
            };
            if table.is_some_and(|t| t != on) || only.is_some_and(|o| o != index) {
                continue;
            }
            let kind = f.text(row, "kind").unwrap_or_default();
            let mut options: Vec<(String, String)> = match f.get(row, "options") {
                Some(CqlValue::Map(m)) => m
                    .iter()
                    .filter_map(|(k, v)| {
                        Some((text_of(&Some(k.clone()))?, text_of(&Some(v.clone()))?))
                    })
                    .collect(),
                _ => Vec::new(),
            };
            let target = options
                .iter()
                .position(|(k, _)| k == "target")
                .map(|i| options.remove(i).1)
                .unwrap_or_default();
            let class = options
                .iter()
                .position(|(k, _)| k == "class_name")
                .map(|i| options.remove(i).1);
            let mut statement = format!(
                "CREATE {}INDEX {} ON {}.{} ({target})",
                if kind == "CUSTOM" { "CUSTOM " } else { "" },
                quote_name(&index),
                quote_name(keyspace),
                quote_name(&on)
            );
            if let Some(class) = class {
                let _ = write!(statement, " USING {}", quote_str(&class));
                if !options.is_empty() {
                    options.sort();
                    let pairs: Vec<String> = options
                        .iter()
                        .map(|(k, v)| format!("{}: {}", quote_str(k), quote_str(v)))
                        .collect();
                    let _ = write!(statement, " WITH OPTIONS = {{{}}}", pairs.join(", "));
                }
            }
            statement.push(';');
            out.push(statement);
        }
        Ok(out)
    }

    async fn index_ddl(&self, keyspace: &str, name: &str) -> AppResult<String> {
        Ok(self
            .index_statements(keyspace, None, Some(name))
            .await?
            .join("\n"))
    }

    /// Functions and aggregates are named `name(argument types)` in the tree.
    async fn routine_row(
        &self,
        table: &str,
        column: &str,
        keyspace: &str,
        name: &str,
    ) -> AppResult<Option<(Fetched, usize)>> {
        let (bare, args) = match name.split_once('(') {
            Some((b, rest)) => (b, rest.trim_end_matches(')')),
            None => (name, ""),
        };
        let f = fetch(
            &self.cql,
            &format!(
                "SELECT * FROM system_schema.{table} WHERE keyspace_name = {} AND {column} = {}",
                lit(keyspace),
                lit(bare)
            ),
        )
        .await?;
        let found = f
            .rows
            .iter()
            .position(|r| text_list(f.get(r, "argument_types")).join(", ") == args.trim());
        Ok(found.map(|i| (f, i)))
    }

    async fn function_ddl(&self, keyspace: &str, name: &str) -> AppResult<String> {
        let Some((f, i)) = self
            .routine_row("functions", "function_name", keyspace, name)
            .await?
        else {
            return Ok(String::new());
        };
        let row = &f.rows[i];
        let args: Vec<String> = text_list(f.get(row, "argument_names"))
            .iter()
            .zip(text_list(f.get(row, "argument_types")))
            .map(|(n, t)| format!("{} {t}", quote_name(n)))
            .collect();
        let on_null = if matches!(
            f.get(row, "called_on_null_input"),
            Some(CqlValue::Boolean(true))
        ) {
            "CALLED ON NULL INPUT"
        } else {
            "RETURNS NULL ON NULL INPUT"
        };
        let body = f.text(row, "body").unwrap_or_default();
        Ok(format!(
            "CREATE OR REPLACE FUNCTION {}.{} ({})\n    {on_null}\n    RETURNS {}\n    LANGUAGE {}\n    AS {};\n",
            quote_name(keyspace),
            quote_name(f.text(row, "function_name").as_deref().unwrap_or(name)),
            args.join(", "),
            f.text(row, "return_type").unwrap_or_default(),
            f.text(row, "language").unwrap_or_default(),
            dollar_body(&body)
        ))
    }

    async fn aggregate_ddl(&self, keyspace: &str, name: &str) -> AppResult<String> {
        let Some((f, i)) = self
            .routine_row("aggregates", "aggregate_name", keyspace, name)
            .await?
        else {
            return Ok(String::new());
        };
        let row = &f.rows[i];
        let mut out = format!(
            "CREATE OR REPLACE AGGREGATE {}.{} ({})\n    SFUNC {}\n    STYPE {}",
            quote_name(keyspace),
            quote_name(f.text(row, "aggregate_name").as_deref().unwrap_or(name)),
            text_list(f.get(row, "argument_types")).join(", "),
            quote_name(&f.text(row, "state_func").unwrap_or_default()),
            f.text(row, "state_type").unwrap_or_default()
        );
        if let Some(fin) = f.text(row, "final_func") {
            let _ = write!(out, "\n    FINALFUNC {}", quote_name(&fin));
        }
        if let Some(init) = f.text(row, "initcond") {
            let _ = write!(out, "\n    INITCOND {init}");
        }
        out.push_str(";\n");
        Ok(out)
    }
}

struct CatalogColumn {
    name: String,
    type_name: String,
    /// `partition_key`, `clustering`, `static` or `regular`.
    kind: String,
    position: i32,
    descending: bool,
}

impl CatalogColumn {
    fn rank(&self) -> u8 {
        match self.kind.as_str() {
            "partition_key" => 0,
            "clustering" => 1,
            "static" => 2,
            _ => 3,
        }
    }
}

/// Options a `CREATE TABLE`/`CREATE MATERIALIZED VIEW` can carry, in the
/// order cqlsh writes them, taken from the row the catalog holds.
fn table_options(f: &Fetched, row: &[Option<CqlValue>], legacy: bool) -> Vec<String> {
    const OPTIONS: &[&str] = &[
        "additional_write_policy",
        "allow_auto_snapshot",
        "bloom_filter_fp_chance",
        "caching",
        "comment",
        "compaction",
        "compression",
        "crc_check_chance",
        "default_time_to_live",
        "gc_grace_seconds",
        "incremental_backups",
        "max_index_interval",
        "memtable",
        "memtable_flush_period_in_ms",
        "min_index_interval",
        "read_repair",
        "speculative_retry",
    ];
    // Cassandra 3.x still has the read repair chances that 4.0 removed (it
    // refuses them as options).
    let legacy_options: &[&str] = if legacy {
        &["dclocal_read_repair_chance", "read_repair_chance"]
    } else {
        &[]
    };
    OPTIONS
        .iter()
        .chain(legacy_options)
        .filter_map(|name| {
            let value = f.get(row, name)?;
            Some(format!("{name} = {}", literal(value)))
        })
        .collect()
}

fn write_create(out: &mut String, header: &str, columns: &[CatalogColumn], options: &[String]) {
    let _ = writeln!(out, "{header} (");
    for c in columns {
        let _ = writeln!(
            out,
            "    {} {}{},",
            quote_name(&c.name),
            c.type_name,
            if c.kind == "static" { " static" } else { "" }
        );
    }
    let _ = write!(out, "    PRIMARY KEY ({})\n)", primary_key(columns));
    write_with(out, columns, options);
}

/// Materialized views list their key after the select, not in a column list.
fn write_primary_key(
    out: &mut String,
    header: &str,
    columns: &[CatalogColumn],
    options: &[String],
) {
    let _ = write!(out, "{header}\n    PRIMARY KEY ({})", primary_key(columns));
    write_with(out, columns, options);
}

fn write_with(out: &mut String, columns: &[CatalogColumn], options: &[String]) {
    let clustering: Vec<String> = columns
        .iter()
        .filter(|c| c.kind == "clustering")
        .map(|c| {
            format!(
                "{} {}",
                quote_name(&c.name),
                if c.descending { "DESC" } else { "ASC" }
            )
        })
        .collect();
    let mut parts = Vec::new();
    if !clustering.is_empty() {
        parts.push(format!("CLUSTERING ORDER BY ({})", clustering.join(", ")));
    }
    parts.extend(options.iter().cloned());
    for (i, part) in parts.iter().enumerate() {
        let _ = write!(
            out,
            "{}{part}",
            if i == 0 { "\n    WITH " } else { "\n    AND " }
        );
    }
    out.push_str(";\n");
}

/// `(a, b), c, d` for a table keyed by (a, b) with clustering columns c, d.
fn primary_key(columns: &[CatalogColumn]) -> String {
    let partition: Vec<String> = columns
        .iter()
        .filter(|c| c.kind == "partition_key")
        .map(|c| quote_name(&c.name))
        .collect();
    let clustering: Vec<String> = columns
        .iter()
        .filter(|c| c.kind == "clustering")
        .map(|c| quote_name(&c.name))
        .collect();
    let head = if partition.len() == 1 {
        partition[0].clone()
    } else {
        format!("({})", partition.join(", "))
    };
    std::iter::once(head)
        .chain(clustering)
        .collect::<Vec<_>>()
        .join(", ")
}

/// A function body as `$$...$$`, or a quoted string when it holds `$$`.
fn dollar_body(body: &str) -> String {
    if body.contains("$$") {
        quote_str(body)
    } else {
        format!("$${body}$$")
    }
}

// ---------------------------------------------------------------------------
// Edits

fn plan_changes(
    target: &EditTarget,
    changes: &[RowChange],
) -> AppResult<(Vec<String>, ApplyResult)> {
    let table = match &target.schema {
        Some(schema) => format!("{}.{}", quote_name(schema), quote_name(&target.table)),
        None => quote_name(&target.table),
    };
    let key_columns: Vec<&EditColumn> = target
        .key
        .iter()
        .map(|&i| column_at(target, i))
        .collect::<AppResult<_>>()?;
    let condition = |key: &[Option<String>]| -> AppResult<String> {
        if key.len() != key_columns.len() {
            return Err(AppError::invalid("The row's key is incomplete."));
        }
        key_columns
            .iter()
            .zip(key)
            .map(|(c, v)| match v {
                Some(v) => Ok(format!(
                    "{} = {}",
                    quote_name(&c.name),
                    cql_literal(&c.type_name, Some(v))
                )),
                None => Err(AppError::invalid(format!(
                    "The key column {} is empty.",
                    c.name
                ))),
            })
            .collect::<AppResult<Vec<_>>>()
            .map(|parts| parts.join(" AND "))
    };
    let mut statements = Vec::new();
    let mut result = ApplyResult::default();
    for change in changes {
        match change {
            RowChange::Update { values, .. } if values.is_empty() => {}
            RowChange::Update { key, values } => {
                let mut sets = Vec::with_capacity(values.len());
                for v in values {
                    let c = column_at(target, v.column)?;
                    if key_columns.iter().any(|k| k.name == c.name) {
                        return Err(AppError::invalid(format!(
                            "{} is part of the primary key, which CQL cannot update. \
                             Delete the row and insert it again.",
                            c.name
                        )));
                    }
                    sets.push(format!(
                        "{} = {}",
                        quote_name(&c.name),
                        cql_literal(&c.type_name, v.value.as_deref())
                    ));
                }
                statements.push(format!(
                    "UPDATE {table} SET {} WHERE {}",
                    sets.join(", "),
                    condition(key)?
                ));
                result.updated += 1;
            }
            RowChange::Delete { key } => {
                statements.push(format!("DELETE FROM {table} WHERE {}", condition(key)?));
                result.deleted += 1;
            }
            RowChange::Insert { values } => {
                if values.is_empty() {
                    return Err(AppError::invalid("Enter a value for the new row."));
                }
                let mut names = Vec::with_capacity(values.len());
                let mut literals = Vec::with_capacity(values.len());
                for v in values {
                    let c = column_at(target, v.column)?;
                    names.push(quote_name(&c.name));
                    literals.push(cql_literal(&c.type_name, v.value.as_deref()));
                }
                statements.push(format!(
                    "INSERT INTO {table} ({}) VALUES ({})",
                    names.join(", "),
                    literals.join(", ")
                ));
                result.inserted += 1;
            }
        }
    }
    Ok((statements, result))
}

fn column_at(target: &EditTarget, index: usize) -> AppResult<&EditColumn> {
    target
        .columns
        .get(index)
        .and_then(Option::as_ref)
        .ok_or_else(|| {
            AppError::invalid(format!(
                "Column {} is not a column of the table and cannot be saved.",
                index + 1
            ))
        })
}

/// True when the text has the word `AS` outside strings, quoted names and
/// comments.
fn has_alias(sql: &str) -> bool {
    let b = sql.as_bytes();
    let mut i = 0;
    let mut word = String::new();
    let mut found = false;
    let mut flush = |word: &mut String| {
        if word.eq_ignore_ascii_case("as") {
            found = true;
        }
        word.clear();
    };
    while i < b.len() {
        let c = b[i];
        if c == b'\'' || c == b'"' {
            flush(&mut word);
            i += 1;
            while i < b.len() {
                if b[i] == c {
                    if b.get(i + 1) == Some(&c) {
                        i += 1;
                    } else {
                        break;
                    }
                }
                i += 1;
            }
        } else if c == b'$' && b.get(i + 1) == Some(&b'$') {
            flush(&mut word);
            i = sql[i + 2..].find("$$").map_or(b.len(), |p| i + 2 + p + 1);
        } else if (c == b'-' && b.get(i + 1) == Some(&b'-'))
            || (c == b'/' && b.get(i + 1) == Some(&b'/'))
        {
            flush(&mut word);
            while i < b.len() && b[i] != b'\n' {
                i += 1;
            }
        } else if c == b'/' && b.get(i + 1) == Some(&b'*') {
            flush(&mut word);
            i = sql[i + 2..].find("*/").map_or(b.len(), |p| i + 2 + p + 1);
        } else if c.is_ascii_alphanumeric() || c == b'_' {
            word.push(c as char);
        } else {
            flush(&mut word);
        }
        i += 1;
    }
    flush(&mut word);
    found
}

// ---------------------------------------------------------------------------
// Names and types

/// CQL's reserved words (Cassandra 3.0 to 5.0): they need quotes as names.
const RESERVED: &[&str] = &[
    "ADD",
    "ALLOW",
    "ALTER",
    "AND",
    "APPLY",
    "ASC",
    "AUTHORIZE",
    "BATCH",
    "BEGIN",
    "BY",
    "COLUMNFAMILY",
    "CREATE",
    "DEFAULT",
    "DELETE",
    "DESC",
    "DESCRIBE",
    "DROP",
    "ENTRIES",
    "EXECUTE",
    "FROM",
    "FULL",
    "GRANT",
    "IF",
    "IN",
    "INDEX",
    "INFINITY",
    "INSERT",
    "INTO",
    "IS",
    "KEYSPACE",
    "LIMIT",
    "MATERIALIZED",
    "MODIFY",
    "NAN",
    "NORECURSIVE",
    "NOT",
    "NULL",
    "OF",
    "ON",
    "OR",
    "ORDER",
    "PRIMARY",
    "RENAME",
    "REPLACE",
    "REVOKE",
    "SCHEMA",
    "SELECT",
    "SET",
    "TABLE",
    "TO",
    "TOKEN",
    "TRUNCATE",
    "UNLOGGED",
    "UPDATE",
    "USE",
    "USING",
    "VIEW",
    "WHERE",
    "WITH",
];

/// An identifier as CQL reads it back: bare when it is lowercase and not a
/// reserved word, otherwise double-quoted.
pub fn quote_name(name: &str) -> String {
    let bare = name.bytes().next().is_some_and(|c| c.is_ascii_lowercase())
        && name
            .bytes()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == b'_')
        && !RESERVED.contains(&name.to_ascii_uppercase().as_str());
    if bare {
        name.to_string()
    } else {
        format!("\"{}\"", name.replace('"', "\"\""))
    }
}

fn quote_str(s: &str) -> String {
    format!("'{}'", s.replace('\'', "''"))
}

fn native_name(t: &NativeType) -> &'static str {
    match t {
        NativeType::Ascii => "ascii",
        NativeType::Boolean => "boolean",
        NativeType::Blob => "blob",
        NativeType::Counter => "counter",
        NativeType::Date => "date",
        NativeType::Decimal => "decimal",
        NativeType::Double => "double",
        NativeType::Duration => "duration",
        NativeType::Float => "float",
        NativeType::Int => "int",
        NativeType::BigInt => "bigint",
        NativeType::Text => "text",
        NativeType::Timestamp => "timestamp",
        NativeType::Inet => "inet",
        NativeType::SmallInt => "smallint",
        NativeType::TinyInt => "tinyint",
        NativeType::Time => "time",
        NativeType::Timeuuid => "timeuuid",
        NativeType::Uuid => "uuid",
        NativeType::Varint => "varint",
        _ => "custom",
    }
}

/// The type as `system_schema` writes it: `list<int>`,
/// `frozen<map<text, int>>`, `vector<float, 3>`, a UDT by its name.
fn type_name(t: &ColumnType<'_>) -> String {
    let frozen = |frozen: bool, inner: String| {
        if frozen {
            format!("frozen<{inner}>")
        } else {
            inner
        }
    };
    match t {
        ColumnType::Native(n) => native_name(n).into(),
        ColumnType::Collection { frozen: f, typ } => frozen(
            *f,
            match typ {
                CollectionType::List(e) => format!("list<{}>", type_name(e)),
                CollectionType::Set(e) => format!("set<{}>", type_name(e)),
                CollectionType::Map(k, v) => format!("map<{}, {}>", type_name(k), type_name(v)),
                _ => "custom".into(),
            },
        ),
        ColumnType::Vector { typ, dimensions } => {
            format!("vector<{}, {dimensions}>", type_name(typ))
        }
        ColumnType::UserDefinedType {
            frozen: f,
            definition,
        } => frozen(*f, quote_name(&definition.name)),
        ColumnType::Tuple(items) => format!(
            "tuple<{}>",
            items.iter().map(type_name).collect::<Vec<_>>().join(", ")
        ),
        _ => "custom".into(),
    }
}

fn column_kind(t: &ColumnType<'_>) -> ValueKind {
    match t {
        ColumnType::Native(n) => match n {
            NativeType::TinyInt
            | NativeType::SmallInt
            | NativeType::Int
            | NativeType::BigInt
            | NativeType::Varint
            | NativeType::Decimal
            | NativeType::Float
            | NativeType::Double
            | NativeType::Counter => ValueKind::Number,
            NativeType::Boolean => ValueKind::Bool,
            NativeType::Timestamp | NativeType::Date | NativeType::Time | NativeType::Duration => {
                ValueKind::Temporal
            }
            NativeType::Blob => ValueKind::Binary,
            NativeType::Text
            | NativeType::Ascii
            | NativeType::Uuid
            | NativeType::Timeuuid
            | NativeType::Inet => ValueKind::Text,
            _ => ValueKind::Other,
        },
        _ => ValueKind::Other,
    }
}

// ---------------------------------------------------------------------------
// Values

/// A value as the grid shows it: text as it is, everything else as its CQL
/// text (collections and UDTs as valid CQL literals).
fn cell(v: &Option<CqlValue>) -> Option<String> {
    let v = v.as_ref()?;
    Some(match v {
        CqlValue::Text(s) | CqlValue::Ascii(s) => s.clone(),
        CqlValue::Timestamp(_) | CqlValue::Date(_) | CqlValue::Time(_) | CqlValue::Inet(_) => {
            plain(v)
        }
        CqlValue::Blob(bytes) => blob(bytes, super::binary_limit(BINARY_PREVIEW_BYTES)),
        CqlValue::Empty => String::new(),
        other => literal(other),
    })
}

/// The text of a temporal value or an address, unquoted.
fn plain(v: &CqlValue) -> String {
    match v {
        CqlValue::Timestamp(t) => timestamp_text(t.0),
        CqlValue::Date(d) => date_text(i64::from(d.0) - (1 << 31)),
        CqlValue::Time(t) => time_text(t.0),
        CqlValue::Inet(ip) => ip.to_string(),
        _ => String::new(),
    }
}

/// `0x00ff`, cut to `limit` bytes with the full size noted.
fn blob(bytes: &[u8], limit: usize) -> String {
    let shown = &bytes[..bytes.len().min(limit)];
    let mut s = String::with_capacity(shown.len() * 2 + 2);
    s.push_str("0x");
    for b in shown {
        let _ = write!(s, "{b:02x}");
    }
    if bytes.len() > shown.len() {
        let _ = write!(s, "… ({} bytes)", bytes.len());
    }
    s
}

/// A value as a CQL literal, as it is written inside a collection.
fn literal(v: &CqlValue) -> String {
    match v {
        CqlValue::Text(s) | CqlValue::Ascii(s) => quote_str(s),
        CqlValue::Boolean(b) => b.to_string(),
        CqlValue::TinyInt(n) => n.to_string(),
        CqlValue::SmallInt(n) => n.to_string(),
        CqlValue::Int(n) => n.to_string(),
        CqlValue::BigInt(n) => n.to_string(),
        CqlValue::Counter(c) => c.0.to_string(),
        CqlValue::Float(f) => float_text(f64::from(*f), &f.to_string(), &format!("{f:e}")),
        CqlValue::Double(d) => float_text(*d, &d.to_string(), &format!("{d:e}")),
        CqlValue::Varint(n) => integer_text(n.as_signed_bytes_be_slice()),
        CqlValue::Decimal(d) => {
            let (bytes, scale) = d.as_signed_be_bytes_slice_and_exponent();
            decimal_text(&integer_text(bytes), scale)
        }
        CqlValue::Uuid(u) => u.to_string(),
        CqlValue::Timeuuid(u) => u.to_string(),
        CqlValue::Blob(bytes) => blob(bytes, usize::MAX),
        CqlValue::Timestamp(_) | CqlValue::Date(_) | CqlValue::Time(_) | CqlValue::Inet(_) => {
            quote_str(&plain(v))
        }
        CqlValue::Duration(d) => duration_text(d.months, d.days, d.nanoseconds),
        CqlValue::List(items) | CqlValue::Vector(items) => {
            format!(
                "[{}]",
                items.iter().map(literal).collect::<Vec<_>>().join(", ")
            )
        }
        CqlValue::Set(items) => {
            format!(
                "{{{}}}",
                items.iter().map(literal).collect::<Vec<_>>().join(", ")
            )
        }
        CqlValue::Map(pairs) => format!(
            "{{{}}}",
            pairs
                .iter()
                .map(|(k, v)| format!("{}: {}", literal(k), literal(v)))
                .collect::<Vec<_>>()
                .join(", ")
        ),
        CqlValue::Tuple(items) => format!(
            "({})",
            items
                .iter()
                .map(|i| i.as_ref().map_or_else(|| "null".into(), literal))
                .collect::<Vec<_>>()
                .join(", ")
        ),
        CqlValue::UserDefinedType { fields, .. } => format!(
            "{{{}}}",
            fields
                .iter()
                .map(|(n, v)| format!(
                    "{}: {}",
                    quote_name(n),
                    v.as_ref().map_or_else(|| "null".into(), literal)
                ))
                .collect::<Vec<_>>()
                .join(", ")
        ),
        _ => "null".into(),
    }
}

/// Shortest round-trip text; very large or small magnitudes use an exponent.
fn float_text(value: f64, plain: &str, exponent: &str) -> String {
    if value.is_nan() {
        "NaN".into()
    } else if value.is_infinite() {
        if value > 0.0 { "Infinity" } else { "-Infinity" }.into()
    } else if value != 0.0 && (value.abs() >= 1e16 || value.abs() < 1e-5) {
        exponent.to_string()
    } else {
        plain.to_string()
    }
}

/// Decimal digits of a two's complement big-endian integer.
fn integer_text(bytes: &[u8]) -> String {
    let negative = bytes.first().is_some_and(|b| b & 0x80 != 0);
    let mut magnitude = bytes.to_vec();
    if negative {
        // Negate: invert and add one.
        let mut carry = true;
        for b in magnitude.iter_mut().rev() {
            *b = !*b;
            if carry {
                let (n, c) = b.overflowing_add(1);
                *b = n;
                carry = c;
            }
        }
    }
    // Base 2^32 limbs, most significant first.
    let pad = (4 - magnitude.len() % 4) % 4;
    let mut limbs: Vec<u32> = std::iter::repeat_n(0u8, pad)
        .chain(magnitude)
        .collect::<Vec<u8>>()
        .chunks(4)
        .map(|c| u32::from_be_bytes([c[0], c[1], c[2], c[3]]))
        .collect();
    let mut parts: Vec<u32> = Vec::new();
    while limbs.iter().any(|&l| l != 0) {
        let mut rem = 0u64;
        for limb in &mut limbs {
            let cur = (rem << 32) | u64::from(*limb);
            *limb = (cur / 1_000_000_000) as u32;
            rem = cur % 1_000_000_000;
        }
        parts.push(rem as u32);
        let zeros = limbs.iter().take_while(|&&l| l == 0).count();
        limbs.drain(..zeros);
    }
    let Some(top) = parts.pop() else {
        return "0".into();
    };
    let mut out = String::new();
    if negative {
        out.push('-');
    }
    let _ = write!(out, "{top}");
    for part in parts.iter().rev() {
        let _ = write!(out, "{part:09}");
    }
    out
}

/// `unscaled × 10^-scale` the way Java's `BigDecimal.toString` writes it:
/// plain when the exponent allows, scientific otherwise.
fn decimal_text(unscaled: &str, scale: i32) -> String {
    let (sign, digits) = match unscaled.strip_prefix('-') {
        Some(d) => ("-", d),
        None => ("", unscaled),
    };
    let adjusted = -i64::from(scale) + digits.len() as i64 - 1;
    if scale == 0 {
        return unscaled.to_string();
    }
    if scale > 0 && adjusted >= -6 {
        let scale = scale as usize;
        return if digits.len() > scale {
            let (int, frac) = digits.split_at(digits.len() - scale);
            format!("{sign}{int}.{frac}")
        } else {
            format!("{sign}0.{}{digits}", "0".repeat(scale - digits.len()))
        };
    }
    let (head, rest) = digits.split_at(1);
    let mantissa = if rest.is_empty() {
        head.to_string()
    } else {
        format!("{head}.{rest}")
    };
    format!(
        "{sign}{mantissa}E{}{adjusted}",
        if adjusted >= 0 { "+" } else { "" }
    )
}

/// Year, month and day of a day count since 1970-01-01.
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

fn date_text(days: i64) -> String {
    let (y, m, d) = civil_from_days(days);
    if y < 0 {
        format!("-{:04}-{m:02}-{d:02}", -y)
    } else {
        format!("{y:04}-{m:02}-{d:02}")
    }
}

/// `2024-05-06 07:08:09.123+0000`.
fn timestamp_text(millis: i64) -> String {
    let secs = millis.div_euclid(1000);
    let ms = millis.rem_euclid(1000);
    let day = secs.div_euclid(86_400);
    let tod = secs.rem_euclid(86_400);
    format!(
        "{} {:02}:{:02}:{:02}.{ms:03}+0000",
        date_text(day),
        tod / 3600,
        tod % 3600 / 60,
        tod % 60
    )
}

/// `07:08:09.123456789`.
fn time_text(nanos: i64) -> String {
    let secs = nanos.div_euclid(1_000_000_000);
    let frac = nanos.rem_euclid(1_000_000_000);
    format!(
        "{:02}:{:02}:{:02}.{frac:09}",
        secs / 3600,
        secs % 3600 / 60,
        secs % 60
    )
}

/// `1y2mo3d4h5m6s7ms8us9ns`, the form CQL reads back.
fn duration_text(months: i32, days: i32, nanos: i64) -> String {
    let negative = months < 0 || days < 0 || nanos < 0;
    let months = i64::from(months).unsigned_abs();
    let days = i64::from(days).unsigned_abs();
    let nanos = nanos.unsigned_abs();
    let mut out = String::new();
    let mut put = |n: u64, unit: &str| {
        if n > 0 {
            let _ = write!(out, "{n}{unit}");
        }
    };
    put(months / 12, "y");
    put(months % 12, "mo");
    put(days, "d");
    put(nanos / 3_600_000_000_000, "h");
    put(nanos / 60_000_000_000 % 60, "m");
    put(nanos / 1_000_000_000 % 60, "s");
    put(nanos / 1_000_000 % 1000, "ms");
    put(nanos / 1000 % 1000, "us");
    put(nanos % 1000, "ns");
    if out.is_empty() {
        out.push_str("0s");
    }
    if negative {
        out.insert(0, '-');
    }
    out
}

/// A cell's text as a CQL literal for a column of `type_name` (the type as
/// `system_schema` writes it). Numbers, booleans, uuids, blobs and durations
/// are bare, text and temporal values quoted; collections, tuples, vectors and
/// UDTs are already CQL literals in cell text and stay as they are. `None` is
/// `null`.
pub fn cql_literal(type_name: &str, value: Option<&str>) -> String {
    let Some(v) = value else {
        return "null".into();
    };
    let mut t = type_name.trim().to_ascii_lowercase();
    while let Some(inner) = t
        .strip_prefix("frozen<")
        .and_then(|rest| rest.strip_suffix('>'))
    {
        t = inner.trim().to_string();
    }
    let trimmed = v.trim();
    let bare = match t.as_str() {
        "tinyint" | "smallint" | "int" | "bigint" | "varint" | "decimal" | "float" | "double"
        | "counter" => is_number(trimmed).then(|| trimmed.to_string()),
        "boolean" => match trimmed.to_ascii_lowercase().as_str() {
            b @ ("true" | "false") => Some(b.to_string()),
            _ => None,
        },
        "uuid" | "timeuuid" => is_uuid(trimmed).then(|| trimmed.to_string()),
        "blob" => is_blob(trimmed).then(|| trimmed.to_string()),
        "duration" => (!trimmed.is_empty() && !trimmed.contains(char::is_whitespace))
            .then(|| trimmed.to_string()),
        "text" | "varchar" | "ascii" | "inet" | "timestamp" | "date" | "time" => {
            return quote_str(v);
        }
        // Collections, tuples, vectors and UDTs.
        _ => return v.to_string(),
    };
    bare.unwrap_or_else(|| quote_str(v))
}

fn is_number(v: &str) -> bool {
    if matches!(v, "NaN" | "Infinity" | "-Infinity") {
        return true;
    }
    let body = v.strip_prefix(['-', '+']).unwrap_or(v);
    let (mantissa, exponent) = match body.find(['e', 'E']) {
        Some(i) => (&body[..i], Some(&body[i + 1..])),
        None => (body, None),
    };
    let digits = mantissa.replacen('.', "", 1);
    !digits.is_empty()
        && digits.bytes().all(|b| b.is_ascii_digit())
        && mantissa.matches('.').count() <= 1
        && exponent.is_none_or(|e| {
            let e = e.strip_prefix(['-', '+']).unwrap_or(e);
            !e.is_empty() && e.bytes().all(|b| b.is_ascii_digit())
        })
}

fn is_uuid(v: &str) -> bool {
    v.len() == 36
        && v.bytes().enumerate().all(|(i, b)| {
            if matches!(i, 8 | 13 | 18 | 23) {
                b == b'-'
            } else {
                b.is_ascii_hexdigit()
            }
        })
}

fn is_blob(v: &str) -> bool {
    v.strip_prefix("0x")
        .is_some_and(|h| h.len() % 2 == 0 && h.bytes().all(|b| b.is_ascii_hexdigit()))
}

// ---------------------------------------------------------------------------
// Errors

/// Every message in the chain of an error, joined.
fn chain(e: &dyn std::error::Error) -> String {
    let mut text = e.to_string();
    let mut source = e.source();
    while let Some(s) = source {
        let part = s.to_string();
        if !text.contains(&part) {
            text.push_str(": ");
            text.push_str(&part);
        }
        source = s.source();
    }
    text
}

fn is_auth_text(text: &str) -> bool {
    let t = text.to_ascii_lowercase();
    t.contains("authenticat") || t.contains("credentials") || t.contains("username and/or password")
}

fn connect_error(e: NewSessionError) -> AppError {
    let text = chain(&e);
    let mut err = AppError::connection(match &e {
        NewSessionError::EmptyKnownNodesList => "Enter at least one contact point.".to_string(),
        _ => text.clone(),
    });
    if is_auth_text(&text) {
        err.code = Some("AuthenticationError".into());
    }
    err
}

/// The server's error behind a failed request, when there is one.
fn attempt_error(e: &RequestAttemptError) -> Option<(&DbError, &str)> {
    match e {
        RequestAttemptError::DbError(db, message) => Some((db, message)),
        _ => None,
    }
}

/// `line 1:7 no viable alternative at input ...` as a 0-based character
/// offset into `sql`.
fn error_position(sql: &str, message: &str) -> Option<usize> {
    let rest = &message[message.find("line ")? + 5..];
    let (line, rest) = rest.split_once(':')?;
    let line: usize = line.trim().parse().ok()?;
    let col: usize = rest
        .chars()
        .take_while(char::is_ascii_digit)
        .collect::<String>()
        .parse()
        .ok()?;
    let before: usize = sql
        .split('\n')
        .take(line.checked_sub(1)?)
        .map(|l| l.chars().count() + 1)
        .sum();
    Some(before + col)
}

fn db_error(db: &DbError, message: &str, sql: Option<&str>) -> AppError {
    let debug = format!("{db:?}");
    let code: String = debug
        .chars()
        .take_while(|c| c.is_ascii_alphanumeric())
        .collect();
    let kind = match db {
        DbError::AuthenticationError => ErrorKind::Connection,
        _ => ErrorKind::Database,
    };
    let mut err = AppError::new(kind, message);
    err.code = Some(code);
    if matches!(db, DbError::SyntaxError) {
        err.position = sql.and_then(|s| error_position(s, message));
    }
    err
}

fn timeout_error(d: Duration) -> AppError {
    AppError::new(
        ErrorKind::Database,
        format!("The request took longer than {} ms.", d.as_millis()),
    )
}

fn map_request(e: &RequestError, sql: Option<&str>) -> AppError {
    match e {
        RequestError::LastAttemptError(a) => match attempt_error(a) {
            Some((db, message)) => db_error(db, message, sql),
            None => AppError::connection(chain(a)),
        },
        RequestError::RequestTimeout(d) => timeout_error(*d),
        RequestError::EmptyPlan => no_node(),
        other => AppError::connection(chain(other)),
    }
}

fn no_node() -> AppError {
    AppError::connection(
        "No node accepts requests. If a local datacenter is set, check its name: only its nodes are used.",
    )
}

fn map_execution(e: ExecutionError, sql: Option<&str>) -> AppError {
    match &e {
        ExecutionError::LastAttemptError(a) => match attempt_error(a) {
            Some((db, message)) => db_error(db, message, sql),
            None => AppError::connection(chain(a)),
        },
        ExecutionError::RequestTimeout(d) => timeout_error(*d),
        ExecutionError::EmptyPlan => no_node(),
        ExecutionError::BadQuery(_) => AppError::invalid(e.to_string()),
        ExecutionError::ConnectionPoolError(_) => AppError::connection(chain(&e)),
        _ => AppError::new(ErrorKind::Database, chain(&e)),
    }
}

fn map_next_page(e: &NextPageError) -> AppError {
    match e {
        NextPageError::RequestFailure(r) => map_request(r, None),
        other => AppError::new(ErrorKind::Database, chain(other)),
    }
}

fn map_next_row(e: NextRowError) -> AppError {
    match &e {
        NextRowError::NextPageError(p) => map_next_page(p),
        other => AppError::internal(other.to_string()),
    }
}

fn map_pager(e: PagerExecutionError) -> AppError {
    match &e {
        PagerExecutionError::NextPageError(p) => map_next_page(p),
        other => AppError::new(ErrorKind::Database, chain(other)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use scylla::value::{
        Counter, CqlDate, CqlDecimal, CqlDuration, CqlTime, CqlTimestamp, CqlTimeuuid, CqlVarint,
    };

    fn text(s: &str) -> CqlValue {
        CqlValue::Text(s.into())
    }

    #[test]
    fn integers_keep_every_digit() {
        assert_eq!(integer_text(&[]), "0");
        assert_eq!(integer_text(&[0]), "0");
        assert_eq!(integer_text(&[0x7f]), "127");
        assert_eq!(integer_text(&[0xff]), "-1");
        assert_eq!(integer_text(&[0x80]), "-128");
        assert_eq!(integer_text(&[0x00, 0xff]), "255");
        // 2^64 and -(2^64) - 1
        assert_eq!(
            integer_text(&[0x01, 0, 0, 0, 0, 0, 0, 0, 0]),
            "18446744073709551616"
        );
        assert_eq!(
            integer_text(&[0xfe, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff]),
            "-18446744073709551617"
        );
        // 123456789012345678901234567890
        let n = CqlVarint::from_signed_bytes_be(vec![
            0x01, 0x8e, 0xe9, 0x0f, 0xf6, 0xc3, 0x73, 0xe0, 0xee, 0x4e, 0x3f, 0x0a, 0xd2,
        ]);
        assert_eq!(
            literal(&CqlValue::Varint(n)),
            "123456789012345678901234567890"
        );
    }

    #[test]
    fn decimals_are_exact() {
        assert_eq!(decimal_text("12345", 2), "123.45");
        assert_eq!(decimal_text("-5", 3), "-0.005");
        assert_eq!(decimal_text("0", 2), "0.00");
        assert_eq!(decimal_text("7", 0), "7");
        assert_eq!(decimal_text("1", -3), "1E+3");
        assert_eq!(decimal_text("-12", -3), "-1.2E+4");
        assert_eq!(decimal_text("1", 10), "1E-10");
        let d = CqlDecimal::from_signed_be_bytes_and_exponent(vec![0x30, 0x39], 2);
        assert_eq!(literal(&CqlValue::Decimal(d)), "123.45");
    }

    #[test]
    fn floats_round_trip() {
        assert_eq!(literal(&CqlValue::Double(0.1)), "0.1");
        assert_eq!(literal(&CqlValue::Float(0.1)), "0.1");
        assert_eq!(literal(&CqlValue::Double(1.0)), "1");
        assert_eq!(literal(&CqlValue::Double(f64::NAN)), "NaN");
        assert_eq!(literal(&CqlValue::Double(f64::INFINITY)), "Infinity");
        assert_eq!(literal(&CqlValue::Double(f64::NEG_INFINITY)), "-Infinity");
        assert_eq!(literal(&CqlValue::Double(1.5e300)), "1.5e300");
        assert_eq!(literal(&CqlValue::Double(1e-7)), "1e-7");
        assert_eq!(literal(&CqlValue::Double(0.0)), "0");
    }

    #[test]
    fn temporal_values() {
        assert_eq!(
            cell(&Some(CqlValue::Timestamp(CqlTimestamp(0)))).unwrap(),
            "1970-01-01 00:00:00.000+0000"
        );
        assert_eq!(
            timestamp_text(1_714_979_289_123),
            "2024-05-06 07:08:09.123+0000"
        );
        assert_eq!(timestamp_text(-1), "1969-12-31 23:59:59.999+0000");
        assert_eq!(date_text(0), "1970-01-01");
        assert_eq!(date_text(19_849), "2024-05-06");
        assert_eq!(
            cell(&Some(CqlValue::Date(CqlDate(1 << 31)))).unwrap(),
            "1970-01-01"
        );
        assert_eq!(
            cell(&Some(CqlValue::Time(CqlTime(25_689_123_456_789)))).unwrap(),
            "07:08:09.123456789"
        );
        assert_eq!(
            literal(&CqlValue::Timestamp(CqlTimestamp(0))),
            "'1970-01-01 00:00:00.000+0000'"
        );
    }

    #[test]
    fn durations_read_back() {
        assert_eq!(duration_text(0, 0, 0), "0s");
        assert_eq!(
            duration_text(14, 3, 14_706_007_008_009),
            "1y2mo3d4h5m6s7ms8us9ns"
        );
        assert_eq!(duration_text(-1, -2, -3_000_000_000), "-1mo2d3s");
        assert_eq!(
            literal(&CqlValue::Duration(CqlDuration {
                months: 1,
                days: 0,
                nanoseconds: 0
            })),
            "1mo"
        );
    }

    #[test]
    fn scalars() {
        assert_eq!(cell(&None), None);
        assert_eq!(cell(&Some(text("it's"))).unwrap(), "it's");
        assert_eq!(literal(&text("it's")), "'it''s'");
        assert_eq!(cell(&Some(CqlValue::Boolean(true))).unwrap(), "true");
        assert_eq!(cell(&Some(CqlValue::Counter(Counter(-4)))).unwrap(), "-4");
        assert_eq!(cell(&Some(CqlValue::Blob(vec![0, 255]))).unwrap(), "0x00ff");
        assert_eq!(cell(&Some(CqlValue::Blob(vec![]))).unwrap(), "0x");
        assert_eq!(
            cell(&Some(CqlValue::Inet("127.0.0.1".parse().unwrap()))).unwrap(),
            "127.0.0.1"
        );
        assert_eq!(literal(&CqlValue::Inet("::1".parse().unwrap())), "'::1'");
        let id = uuid::Uuid::parse_str("123e4567-e89b-12d3-a456-426614174000").unwrap();
        assert_eq!(
            cell(&Some(CqlValue::Uuid(id))).unwrap(),
            "123e4567-e89b-12d3-a456-426614174000"
        );
        assert_eq!(
            cell(&Some(CqlValue::Timeuuid(CqlTimeuuid::from(id)))).unwrap(),
            "123e4567-e89b-12d3-a456-426614174000"
        );
    }

    #[test]
    fn composites_are_cql_literals() {
        let list = CqlValue::List(vec![CqlValue::Int(1), CqlValue::Int(2)]);
        assert_eq!(cell(&Some(list)).unwrap(), "[1, 2]");
        let set = CqlValue::Set(vec![text("a"), text("b'")]);
        assert_eq!(cell(&Some(set)).unwrap(), "{'a', 'b'''}");
        let map = CqlValue::Map(vec![(text("a"), CqlValue::Int(1))]);
        assert_eq!(cell(&Some(map)).unwrap(), "{'a': 1}");
        let tuple = CqlValue::Tuple(vec![Some(CqlValue::Int(1)), Some(text("x")), None]);
        assert_eq!(cell(&Some(tuple)).unwrap(), "(1, 'x', null)");
        let udt = CqlValue::UserDefinedType {
            keyspace: "ks".into(),
            name: "address".into(),
            fields: vec![
                ("street".into(), Some(text("Main"))),
                ("Zip".into(), Some(CqlValue::Int(12345))),
                ("floor".into(), None),
            ],
        };
        assert_eq!(
            cell(&Some(udt)).unwrap(),
            "{street: 'Main', \"Zip\": 12345, floor: null}"
        );
        let vector = CqlValue::Vector(vec![CqlValue::Float(0.5), CqlValue::Float(1.5)]);
        assert_eq!(cell(&Some(vector)).unwrap(), "[0.5, 1.5]");
        let nested = CqlValue::Map(vec![(
            text("k"),
            CqlValue::List(vec![CqlValue::Blob(vec![1])]),
        )]);
        assert_eq!(literal(&nested), "{'k': [0x01]}");
    }

    #[test]
    fn blob_preview_is_cut() {
        let bytes = vec![0xab; 10];
        assert_eq!(blob(&bytes, 4), "0xabababab… (10 bytes)");
        assert_eq!(blob(&bytes, usize::MAX).len(), 22);
    }

    #[test]
    fn literals_follow_the_column_type() {
        assert_eq!(cql_literal("int", Some("42")), "42");
        assert_eq!(
            cql_literal("varint", Some("-12345678901234567890")),
            "-12345678901234567890"
        );
        assert_eq!(cql_literal("double", Some("NaN")), "NaN");
        assert_eq!(cql_literal("decimal", Some("1.50")), "1.50");
        assert_eq!(cql_literal("boolean", Some("TRUE")), "true");
        assert_eq!(cql_literal("text", Some("it's")), "'it''s'");
        assert_eq!(cql_literal("ascii", Some("x")), "'x'");
        assert_eq!(
            cql_literal("timestamp", Some("2024-05-06 07:08:09.123+0000")),
            "'2024-05-06 07:08:09.123+0000'"
        );
        assert_eq!(cql_literal("date", Some("2024-05-06")), "'2024-05-06'");
        assert_eq!(cql_literal("time", Some("07:08:09")), "'07:08:09'");
        assert_eq!(cql_literal("inet", Some("::1")), "'::1'");
        assert_eq!(
            cql_literal("uuid", Some("123e4567-e89b-12d3-a456-426614174000")),
            "123e4567-e89b-12d3-a456-426614174000"
        );
        assert_eq!(cql_literal("blob", Some("0x00ff")), "0x00ff");
        assert_eq!(cql_literal("duration", Some("1mo2d")), "1mo2d");
        assert_eq!(cql_literal("int", None), "null");
        // Not a value of the type: quoted, so the server explains the error.
        assert_eq!(cql_literal("int", Some("abc")), "'abc'");
        assert_eq!(cql_literal("blob", Some("xyz")), "'xyz'");
        // Collections, tuples, UDTs and vectors are literals already.
        assert_eq!(cql_literal("list<int>", Some("[1, 2]")), "[1, 2]");
        assert_eq!(
            cql_literal("frozen<map<text, int>>", Some("{'a': 1}")),
            "{'a': 1}"
        );
        assert_eq!(cql_literal("frozen<address>", Some("{zip: 1}")), "{zip: 1}");
        assert_eq!(
            cql_literal("tuple<int, text>", Some("(1, 'x')")),
            "(1, 'x')"
        );
        assert_eq!(
            cql_literal("vector<float, 2>", Some("[0.5, 1.5]")),
            "[0.5, 1.5]"
        );
        assert_eq!(cql_literal("FROZEN<LIST<TEXT>>", Some("['a']")), "['a']");
    }

    #[test]
    fn names_are_quoted_when_needed() {
        assert_eq!(quote_name("users"), "users");
        assert_eq!(quote_name("user_id2"), "user_id2");
        assert_eq!(quote_name("Users"), "\"Users\"");
        assert_eq!(quote_name("2fast"), "\"2fast\"");
        assert_eq!(quote_name("select"), "\"select\"");
        assert_eq!(quote_name("a\"b"), "\"a\"\"b\"");
        assert_eq!(quote_name("with space"), "\"with space\"");
        assert_eq!(quote_name(""), "\"\"");
    }

    #[test]
    fn type_names() {
        assert_eq!(type_name(&ColumnType::Native(NativeType::Int)), "int");
        let list = ColumnType::Collection {
            frozen: false,
            typ: CollectionType::List(Box::new(ColumnType::Native(NativeType::Text))),
        };
        assert_eq!(type_name(&list), "list<text>");
        let map = ColumnType::Collection {
            frozen: true,
            typ: CollectionType::Map(
                Box::new(ColumnType::Native(NativeType::Text)),
                Box::new(ColumnType::Native(NativeType::Int)),
            ),
        };
        assert_eq!(type_name(&map), "frozen<map<text, int>>");
        assert_eq!(column_kind(&map), ValueKind::Other);
        assert_eq!(
            type_name(&ColumnType::Vector {
                typ: Box::new(ColumnType::Native(NativeType::Float)),
                dimensions: 3
            }),
            "vector<float, 3>"
        );
        assert_eq!(
            type_name(&ColumnType::Tuple(vec![
                ColumnType::Native(NativeType::Int),
                ColumnType::Native(NativeType::Text)
            ])),
            "tuple<int, text>"
        );
        assert_eq!(
            column_kind(&ColumnType::Native(NativeType::Varint)),
            ValueKind::Number
        );
        assert_eq!(
            column_kind(&ColumnType::Native(NativeType::Blob)),
            ValueKind::Binary
        );
        assert_eq!(
            column_kind(&ColumnType::Native(NativeType::Uuid)),
            ValueKind::Text
        );
        assert_eq!(
            column_kind(&ColumnType::Native(NativeType::Timestamp)),
            ValueKind::Temporal
        );
        assert_eq!(
            column_kind(&ColumnType::Native(NativeType::Boolean)),
            ValueKind::Bool
        );
    }

    #[test]
    fn contact_points() {
        assert_eq!(contact_point("db1", 9042), "db1:9042");
        assert_eq!(contact_point("db1:9043", 9042), "db1:9043");
        assert_eq!(contact_point("::1", 9042), "[::1]:9042");
        assert_eq!(contact_point("[::1]", 9042), "[::1]:9042");
        assert_eq!(contact_point("[::1]:9043", 9042), "[::1]:9043");
    }

    #[test]
    fn syntax_error_positions() {
        let sql = "SELECT *\nFROM  WHERE";
        let message = "line 2:6 no viable alternative at input 'WHERE'";
        // "SELECT *" is 8 chars plus the newline, then column 6.
        assert_eq!(error_position(sql, message), Some(15));
        assert_eq!(error_position("x", "line 1:0 mismatched input"), Some(0));
        assert_eq!(error_position("x", "Unknown table"), None);
    }

    #[test]
    fn aliases_are_found_outside_strings() {
        assert!(has_alias("SELECT id AS key FROM t"));
        assert!(has_alias("select id as key from t"));
        assert!(!has_alias("SELECT id FROM t WHERE name = 'a as b'"));
        assert!(!has_alias("SELECT \"as\", assigned FROM t -- as x\n"));
        assert!(!has_alias("SELECT id FROM base"));
    }

    #[test]
    fn primary_keys_and_options() {
        let col = |name: &str, kind: &str, position: i32, descending: bool| CatalogColumn {
            name: name.into(),
            type_name: "int".into(),
            kind: kind.into(),
            position,
            descending,
        };
        let columns = vec![
            col("a", "partition_key", 0, false),
            col("b", "partition_key", 1, false),
            col("c", "clustering", 0, true),
            col("d", "regular", -1, false),
        ];
        assert_eq!(primary_key(&columns), "(a, b), c");
        assert_eq!(primary_key(&columns[1..]), "b, c");
        let mut out = String::new();
        write_create(
            &mut out,
            "CREATE TABLE ks.t",
            &columns,
            &[
                "comment = ''".to_string(),
                "gc_grace_seconds = 864000".to_string(),
            ],
        );
        assert_eq!(
            out,
            "CREATE TABLE ks.t (\n    a int,\n    b int,\n    c int,\n    d int,\n    \
             PRIMARY KEY ((a, b), c)\n)\n    WITH CLUSTERING ORDER BY (c DESC)\n    \
             AND comment = ''\n    AND gc_grace_seconds = 864000;\n"
        );
        assert_eq!(dollar_body("return a;"), "$$return a;$$");
        assert_eq!(dollar_body("a $$ b"), "'a $$ b'");
    }

    fn target() -> EditTarget {
        let col = |name: &str, ty: &str| {
            Some(EditColumn {
                name: name.into(),
                type_name: ty.into(),
            })
        };
        EditTarget {
            schema: Some("My Ks".into()),
            table: "items".into(),
            columns: vec![
                col("id", "int"),
                col("day", "date"),
                col("tags", "set<text>"),
                None,
            ],
            key: vec![0, 1],
        }
    }

    #[test]
    fn edits_become_cql() {
        use crate::model::CellValue;
        let changes = vec![
            RowChange::Update {
                key: vec![Some("1".into()), Some("2024-05-06".into())],
                values: vec![CellValue {
                    column: 2,
                    value: Some("{'a', 'b'}".into()),
                }],
            },
            RowChange::Delete {
                key: vec![Some("2".into()), Some("2024-05-07".into())],
            },
            RowChange::Insert {
                values: vec![
                    CellValue {
                        column: 0,
                        value: Some("3".into()),
                    },
                    CellValue {
                        column: 1,
                        value: Some("2024-05-08".into()),
                    },
                    CellValue {
                        column: 2,
                        value: None,
                    },
                ],
            },
        ];
        let (statements, result) = plan_changes(&target(), &changes).unwrap();
        assert_eq!(
            statements,
            [
                "UPDATE \"My Ks\".items SET tags = {'a', 'b'} WHERE id = 1 AND day = '2024-05-06'",
                "DELETE FROM \"My Ks\".items WHERE id = 2 AND day = '2024-05-07'",
                "INSERT INTO \"My Ks\".items (id, day, tags) VALUES (3, '2024-05-08', null)",
            ]
        );
        assert_eq!((result.updated, result.deleted, result.inserted), (1, 1, 1));
    }

    #[test]
    fn edits_refuse_key_changes_and_expressions() {
        use crate::model::CellValue;
        let key = vec![Some("1".into()), Some("2024-05-06".into())];
        let change = |column| RowChange::Update {
            key: key.clone(),
            values: vec![CellValue {
                column,
                value: Some("5".into()),
            }],
        };
        let err = plan_changes(&target(), &[change(0)]).unwrap_err();
        assert!(err.message.contains("primary key"), "{err}");
        let err = plan_changes(&target(), &[change(3)]).unwrap_err();
        assert!(err.message.contains("not a column"), "{err}");
        let short = RowChange::Delete {
            key: vec![Some("1".into())],
        };
        assert!(plan_changes(&target(), &[short]).is_err());
        let null_key = RowChange::Delete {
            key: vec![Some("1".into()), None],
        };
        assert!(plan_changes(&target(), &[null_key]).is_err());
    }
}
