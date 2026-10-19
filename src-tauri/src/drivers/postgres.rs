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
use tokio_postgres::{AsyncMessage, CancelToken, Client, Statement};
use tokio_postgres_rustls::MakeRustlsConnect;

use super::pg_value::{type_label, value_kind, PgCell};
use super::{first_keywords, TxState};
use crate::error::{AppError, AppResult, ErrorKind};
use crate::model::{
    ColumnInfo, ConnectionProfile, DbColumn, DbObject, ExecRequest, ExecResult, SslMode,
};

pub struct PgSession {
    client: tokio::sync::Mutex<Client>,
    cancel: CancelToken,
    tls: MakeRustlsConnect,
    notices: Arc<Mutex<Vec<String>>>,
    closed: Arc<AtomicBool>,
    state: Mutex<TxState>,
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
        Ok(ExecResult {
            columns: Some(columns),
            rows: out,
            truncated,
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
