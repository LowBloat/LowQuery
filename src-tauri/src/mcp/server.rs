//! MCP over stdio: JSON-RPC 2.0, one message per line. Tool calls run
//! concurrently, serialized per connection, each on the connection's own
//! agent session (separate from the app's) under a time limit.

use std::collections::HashMap;
use std::future::Future;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use parking_lot::Mutex;
use serde_json::{json, Map, Value};
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::sync::{mpsc, Notify};
use tokio::task::JoinSet;

use super::audit::{self, AuditEntry};
use super::tools;
use crate::drivers::Session;
use crate::error::{AppError, AppResult, ErrorKind};
use crate::model::{AiAccess, ConnectionProfile, Engine, ExecRequest};
use crate::profiles;

/// Supported revisions, newest first.
pub const PROTOCOL_VERSIONS: &[&str] = &["2025-11-25", "2025-06-18", "2025-03-26", "2024-11-05"];

const DEFAULT_TIMEOUT: Duration = Duration::from_secs(30);
/// Time a cancelled call gets to stop before its session is dropped.
const GRACE: Duration = Duration::from_secs(5);

const INSTRUCTIONS: &str = "LowQuery gives you the database connections the user opened to AI agents. \
Start with list_connections to see each connection's engine, access (read or write) and SQL dialect. \
Find tables with search_objects (or list_schemas and list_tables), and read describe_table before \
writing SQL so you use the real column names, keys and foreign keys. Write SQL in the connection's own \
dialect (FETCH FIRST n ROWS ONLY on Oracle, TOP (n) on SQL Server, LIMIT n elsewhere) and send one \
statement per call. Keep result sets small: select only the columns you need and filter or aggregate in \
SQL. Use explain to check the plan of a heavy query before running it. Use execute only when the user \
asked for a change; read-only connections refuse changes.";

/// `lowquery mcp`: serves stdin/stdout until stdin closes.
pub fn serve(config_dir: PathBuf) {
    let server = Arc::new(Server::new(config_dir, timeout_from_env()));
    tauri::async_runtime::block_on(serve_io(server, tokio::io::stdin(), tokio::io::stdout()));
}

fn timeout_from_env() -> Duration {
    std::env::var("LOWQUERY_MCP_TIMEOUT")
        .ok()
        .and_then(|v| v.trim().parse::<u64>().ok())
        .filter(|&s| s > 0)
        .map_or(DEFAULT_TIMEOUT, Duration::from_secs)
}

/// A cancel request for one tool call (`notifications/cancelled`).
#[derive(Default)]
pub struct Cancel {
    flag: AtomicBool,
    notify: Notify,
}

impl Cancel {
    fn cancel(&self) {
        self.flag.store(true, Ordering::SeqCst);
        self.notify.notify_one();
    }

    fn is_cancelled(&self) -> bool {
        self.flag.load(Ordering::SeqCst)
    }

    async fn cancelled(&self) {
        if !self.is_cancelled() {
            self.notify.notified().await;
        }
    }
}

/// How a call uses the session.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// Reads: inside a read-only transaction rolled back afterwards on
    /// read-only connections.
    Read,
    /// Like `Read`, but Oracle skips `SET TRANSACTION READ ONLY`: EXPLAIN
    /// PLAN writes PLAN_TABLE (and is rolled back).
    Explain,
    /// `execute` on a write connection: autocommit, no transaction left open.
    Write,
}

#[derive(Default)]
struct Slot {
    session: Option<Arc<Session>>,
    /// The profile the session was opened with.
    fingerprint: String,
}

/// What a tool returns.
pub struct Reply {
    pub text: String,
    pub structured: Option<Value>,
    pub is_error: bool,
    /// Rows returned or affected, for the activity log.
    pub rows: Option<u64>,
}

impl Reply {
    pub fn ok(text: String, structured: Value) -> Self {
        Self {
            text,
            structured: Some(structured),
            is_error: false,
            rows: None,
        }
    }

    pub fn error(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            structured: None,
            is_error: true,
            rows: None,
        }
    }

    pub fn rows(mut self, n: usize) -> Self {
        self.rows = Some(n as u64);
        self
    }

    fn into_result(self, structured: bool) -> Value {
        let mut out = Map::new();
        out.insert(
            "content".into(),
            json!([{ "type": "text", "text": self.text }]),
        );
        if let (true, Some(s)) = (structured, self.structured) {
            out.insert("structuredContent".into(), s);
        }
        if self.is_error {
            out.insert("isError".into(), Value::Bool(true));
        }
        Value::Object(out)
    }
}

/// What a call is about, for the activity log.
#[derive(Default)]
pub struct CallInfo {
    pub connection_id: Option<String>,
    pub connection_name: Option<String>,
    pub detail: Option<String>,
}

pub struct Server {
    pub config_dir: PathBuf,
    timeout: Duration,
    /// clientInfo of `initialize`, as "name version".
    client: Mutex<String>,
    version: Mutex<&'static str>,
    slots: Mutex<HashMap<String, Arc<tokio::sync::Mutex<Slot>>>>,
    inflight: Mutex<HashMap<String, Arc<Cancel>>>,
}

type Outbox = mpsc::UnboundedSender<Value>;

fn response(id: Value, result: Value) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "result": result })
}

fn error_response(id: Value, code: i64, message: impl Into<String>) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "error": { "code": code, "message": message.into() } })
}

/// Serves JSON-RPC lines from `reader` until it closes, then waits for the
/// calls still running.
pub async fn serve_io<R, W>(server: Arc<Server>, reader: R, writer: W)
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin + Send + 'static,
{
    let (tx, mut rx) = mpsc::unbounded_channel::<Value>();
    let writer = tokio::spawn(async move {
        let mut writer = writer;
        while let Some(message) = rx.recv().await {
            let mut line = serde_json::to_vec(&message).unwrap_or_default();
            line.push(b'\n');
            if writer.write_all(&line).await.is_err() || writer.flush().await.is_err() {
                eprintln!("lowquery mcp: stdout closed");
                break;
            }
        }
    });

    let mut reader = BufReader::new(reader);
    let mut buf = Vec::new();
    let mut calls = JoinSet::new();
    loop {
        buf.clear();
        match reader.read_until(b'\n', &mut buf).await {
            Ok(0) => break,
            Ok(_) => {}
            Err(e) => {
                eprintln!("lowquery mcp: reading stdin failed: {e}");
                break;
            }
        }
        while calls.try_join_next().is_some() {}
        match std::str::from_utf8(&buf) {
            Ok(text) if text.trim().is_empty() => {}
            Ok(text) => server.handle(text.trim(), &tx, &mut calls),
            Err(_) => {
                let _ = tx.send(error_response(
                    Value::Null,
                    -32700,
                    "Parse error: the message is not UTF-8.",
                ));
            }
        }
    }
    while calls.join_next().await.is_some() {}
    drop(tx);
    let _ = writer.await;
}

impl Server {
    pub fn new(config_dir: PathBuf, timeout: Duration) -> Self {
        Self {
            config_dir,
            timeout,
            client: Mutex::new("unknown".into()),
            version: Mutex::new(PROTOCOL_VERSIONS[0]),
            slots: Mutex::default(),
            inflight: Mutex::default(),
        }
    }

    /// Negotiated protocol revision.
    fn version(&self) -> &'static str {
        *self.version.lock()
    }

    fn handle(self: &Arc<Self>, text: &str, tx: &Outbox, calls: &mut JoinSet<()>) {
        let send = |v: Value| {
            let _ = tx.send(v);
        };
        let message: Value = match serde_json::from_str(text) {
            Ok(v) => v,
            Err(e) => {
                return send(error_response(
                    Value::Null,
                    -32700,
                    format!("Parse error: {e}"),
                ))
            }
        };
        let Value::Object(message) = message else {
            let why = if message.is_array() {
                "Invalid request: batches are not supported; send one message per line."
            } else {
                "Invalid request: expected a JSON object."
            };
            return send(error_response(Value::Null, -32600, why));
        };
        let id = message.get("id").filter(|v| !v.is_null()).cloned();
        let Some(method) = message.get("method").and_then(Value::as_str) else {
            // Responses to server requests: this server sends none.
            if !message.contains_key("result") && !message.contains_key("error") {
                if let Some(id) = id {
                    send(error_response(
                        id,
                        -32600,
                        "Invalid request: method is missing.",
                    ));
                }
            }
            return;
        };
        let params = message.get("params").cloned().unwrap_or(Value::Null);
        let Some(id) = id else {
            return self.notification(method, &params);
        };
        if !(id.is_string() || id.is_number()) {
            return send(error_response(
                Value::Null,
                -32600,
                "Invalid request: id must be a string or a number.",
            ));
        }
        if message.get("jsonrpc").and_then(Value::as_str) != Some("2.0") {
            return send(error_response(
                id,
                -32600,
                "Invalid request: jsonrpc must be \"2.0\".",
            ));
        }
        if !(params.is_object() || params.is_null()) {
            return send(error_response(
                id,
                -32602,
                "Invalid params: params must be an object.",
            ));
        }
        match method {
            "initialize" => send(self.initialize(id, &params)),
            "ping" => send(response(id, json!({}))),
            "tools/list" => send(response(id, tools::list(self.version()))),
            "tools/call" => self.start_call(id, &params, tx, calls),
            _ => send(error_response(
                id,
                -32601,
                format!("Method not found: {method}"),
            )),
        }
    }

    fn notification(&self, method: &str, params: &Value) {
        if method == "notifications/cancelled" {
            if let Some(request) = params.get("requestId") {
                if let Some(c) = self.inflight.lock().get(&request.to_string()) {
                    c.cancel();
                }
            }
        }
        // notifications/initialized and the rest need nothing.
    }

    fn initialize(&self, id: Value, params: &Value) -> Value {
        let Some(requested) = params.get("protocolVersion").and_then(Value::as_str) else {
            return error_response(id, -32602, "Invalid params: protocolVersion is required.");
        };
        let version = PROTOCOL_VERSIONS
            .iter()
            .find(|v| **v == requested)
            .copied()
            .unwrap_or(PROTOCOL_VERSIONS[0]);
        *self.version.lock() = version;
        let info = params.get("clientInfo");
        let field = |k: &str| {
            info.and_then(|i| i.get(k))
                .and_then(Value::as_str)
                .unwrap_or("")
                .trim()
                .to_string()
        };
        let client = format!("{} {}", field("name"), field("version"));
        if !client.trim().is_empty() {
            *self.client.lock() = client.trim().to_string();
        }
        response(
            id,
            json!({
                "protocolVersion": version,
                "capabilities": { "tools": { "listChanged": false } },
                "serverInfo": {
                    "name": "lowquery",
                    "title": "LowQuery",
                    "version": env!("CARGO_PKG_VERSION"),
                },
                "instructions": INSTRUCTIONS,
            }),
        )
    }

    fn start_call(
        self: &Arc<Self>,
        id: Value,
        params: &Value,
        tx: &Outbox,
        calls: &mut JoinSet<()>,
    ) {
        let Some(name) = params.get("name").and_then(Value::as_str) else {
            let _ = tx.send(error_response(
                id,
                -32602,
                "Invalid params: name is required.",
            ));
            return;
        };
        let args = match params.get("arguments") {
            None | Some(Value::Null) => Map::new(),
            Some(Value::Object(m)) => m.clone(),
            Some(_) => {
                let _ = tx.send(error_response(
                    id,
                    -32602,
                    "Invalid params: arguments must be an object.",
                ));
                return;
            }
        };
        if !tools::exists(name) {
            self.log(
                name,
                &CallInfo::default(),
                &Reply::error("Unknown tool."),
                0,
            );
            let _ = tx.send(error_response(id, -32602, format!("Unknown tool: {name}")));
            return;
        }
        let key = id.to_string();
        let cancel = Arc::new(Cancel::default());
        self.inflight.lock().insert(key.clone(), cancel.clone());
        let server = self.clone();
        let name = name.to_string();
        let tx = tx.clone();
        calls.spawn(async move {
            let result = server.call(&name, &args, &cancel).await;
            server.inflight.lock().remove(&key);
            // A cancelled request gets no response.
            if !cancel.is_cancelled() {
                let _ = tx.send(response(id, result));
            }
        });
    }

    async fn call(&self, name: &str, args: &Map<String, Value>, cancel: &Cancel) -> Value {
        let started = Instant::now();
        let mut info = CallInfo::default();
        let reply = tools::run(self, name, args, cancel, &mut info).await;
        self.log(name, &info, &reply, started.elapsed().as_millis() as u64);
        reply.into_result(self.version() >= "2025-06-18")
    }

    fn log(&self, tool: &str, info: &CallInfo, reply: &Reply, elapsed_ms: u64) {
        let cap = |s: &str, n: usize| -> String {
            if s.chars().count() <= n {
                s.to_string()
            } else {
                format!("{}…", s.chars().take(n).collect::<String>())
            }
        };
        audit::append(
            &self.config_dir,
            &AuditEntry {
                at: chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
                client: self.client.lock().clone(),
                tool: tool.to_string(),
                connection_id: info.connection_id.clone(),
                connection_name: info.connection_name.clone(),
                detail: info.detail.as_deref().map(|d| cap(d, 4000)),
                ok: !reply.is_error,
                rows: reply.rows,
                error: reply.is_error.then(|| cap(&reply.text, 2000)),
                elapsed_ms,
            },
        );
    }

    /// Profiles open to agents, read again on every call.
    pub fn agent_profiles(&self) -> Result<Vec<ConnectionProfile>, String> {
        profiles::ProfileStore::new(self.config_dir.clone())
            .and_then(|s| s.list())
            .map(|all| {
                all.into_iter()
                    .filter(|p| p.ai_access != AiAccess::Off)
                    .collect()
            })
            .map_err(|e| format!("Could not read the LowQuery connections: {e}"))
    }

    fn slot(&self, id: &str) -> Arc<tokio::sync::Mutex<Slot>> {
        self.slots.lock().entry(id.to_string()).or_default().clone()
    }

    /// Awaits `work` until the time limit or a cancel request; then asks the
    /// session to cancel and gives it [`GRACE`] to stop. The flag says
    /// whether the session is still usable.
    async fn bounded<T>(
        &self,
        session: Option<&Session>,
        cancel: &Cancel,
        work: impl Future<Output = AppResult<T>>,
    ) -> (AppResult<T>, bool) {
        tokio::pin!(work);
        let timed_out = tokio::select! {
            r = &mut work => return (r, true),
            _ = tokio::time::sleep(self.timeout) => true,
            _ = cancel.cancelled() => false,
        };
        let err = if timed_out {
            AppError::new(
                ErrorKind::Cancelled,
                format!(
                    "The call took longer than {} s and was cancelled. Narrow the query, or ask the user to raise LOWQUERY_MCP_TIMEOUT.",
                    self.timeout.as_secs()
                ),
            )
        } else {
            AppError::new(ErrorKind::Cancelled, "The client cancelled the call.")
        };
        let Some(session) = session else {
            return (Err(err), false);
        };
        let cancelled = matches!(
            tokio::time::timeout(GRACE, session.cancel()).await,
            Ok(Ok(()))
        );
        let stopped = cancelled && tokio::time::timeout(GRACE, &mut work).await.is_ok();
        (Err(err), stopped)
    }

    /// Runs `op` on the agent session of `profile`, opening it when needed.
    /// Calls on one connection wait for each other. On read-only
    /// connections every call runs in a read-only transaction (where the
    /// engine has one) that is always rolled back.
    pub async fn run<T, F, Fut>(
        &self,
        profile: &ConnectionProfile,
        kind: Kind,
        cancel: &Cancel,
        op: F,
    ) -> AppResult<T>
    where
        F: Fn(Arc<Session>) -> Fut,
        Fut: Future<Output = AppResult<T>>,
    {
        let slot = self.slot(&profile.id);
        let mut slot = slot.lock().await;
        let fingerprint = serde_json::to_string(profile).unwrap_or_default();
        if slot.fingerprint != fingerprint {
            slot.session = None;
            slot.fingerprint = fingerprint;
        }
        let read_only = profile.ai_access == AiAccess::Read;
        // A stale connection fails the first read; a write is never retried.
        let attempts = if kind == Kind::Write { 1 } else { 2 };
        let mut attempt = 0;
        loop {
            attempt += 1;
            let reused = slot.session.is_some();
            let session = match &slot.session {
                Some(s) => s.clone(),
                None => {
                    let (opened, _) = self.bounded(None, cancel, open_session(profile)).await;
                    let s = Arc::new(opened?);
                    slot.session = Some(s.clone());
                    s
                }
            };

            let work = async {
                if read_only {
                    begin_read(&session, kind).await?;
                }
                op(session.clone()).await
            };
            let (mut outcome, mut healthy) = self.bounded(Some(&session), cancel, work).await;
            if healthy && read_only && session.engine() != Engine::Redis {
                healthy = matches!(
                    tokio::time::timeout(GRACE, session.end_transaction(false)).await,
                    Ok(Ok(_))
                );
            }
            if healthy && kind == Kind::Write && session.tx_state().in_transaction {
                healthy = matches!(
                    tokio::time::timeout(GRACE, session.end_transaction(false)).await,
                    Ok(Ok(_))
                );
                outcome = Err(AppError::invalid(
                    "The statement left a transaction open, so LowQuery rolled it back. Each execute call commits on its own; do not open transactions.",
                ));
            }
            // The Oracle driver does not report a transaction opened by a
            // query (SELECT ... FOR UPDATE), so its locks would outlive the
            // call; DML has already been committed by autocommit.
            if healthy
                && kind == Kind::Write
                && session.engine() == Engine::Oracle
                && outcome.is_ok()
            {
                healthy = matches!(
                    tokio::time::timeout(GRACE, session.end_transaction(true)).await,
                    Ok(Ok(_))
                );
            }
            let lost = matches!(&outcome, Err(e) if e.kind == ErrorKind::Connection);
            // SQL Server explains between SET SHOWPLAN_ALL ON and OFF: a
            // failure may leave the session returning plans instead of rows.
            let showplan =
                kind == Kind::Explain && session.engine() == Engine::Sqlserver && outcome.is_err();
            if !healthy || lost || showplan {
                slot.session = None;
            }
            if lost && reused && attempt < attempts {
                continue;
            }
            return outcome;
        }
    }
}

async fn exec(session: &Session, sql: &str) -> AppResult<()> {
    session
        .execute(&ExecRequest {
            sql: sql.into(),
            max_rows: 0,
        })
        .await
        .map(|_| ())
}

/// Starts the read-only transaction of one call.
async fn begin_read(session: &Session, kind: Kind) -> AppResult<()> {
    match session.engine() {
        Engine::Postgres => exec(session, "SET TRANSACTION READ ONLY").await,
        Engine::Mysql | Engine::Mariadb => exec(session, "START TRANSACTION READ ONLY").await,
        Engine::Oracle if kind != Kind::Explain => exec(session, "SET TRANSACTION READ ONLY").await,
        _ => Ok(()),
    }
}

/// Session settings of read-only connections: read-only by default where
/// the engine allows it, and manual commit so every call can be rolled back.
async fn make_read_only(session: &Session) -> AppResult<()> {
    match session.engine() {
        Engine::Postgres => {
            exec(
                session,
                "SET SESSION CHARACTERISTICS AS TRANSACTION READ ONLY",
            )
            .await?
        }
        Engine::Mysql | Engine::Mariadb => {
            exec(session, "SET SESSION TRANSACTION READ ONLY").await?
        }
        Engine::Sqlite => exec(session, "PRAGMA query_only = ON").await?,
        Engine::Oracle | Engine::Sqlserver => {}
        Engine::Redis => return Ok(()),
    }
    session.set_autocommit(false).await.map(|_| ())
}

fn is_auth_failure(e: &AppError) -> bool {
    matches!(
        e.code.as_deref(),
        Some(
            "28P01"
                | "28000"
                | "ORA-01017"
                | "ORA-01005"
                | "1045 (28000)"
                | "1044 (42000)"
                | "1698 (28000)"
                | "NOAUTH"
                | "WRONGPASS"
        )
    ) || e
        .code
        .as_deref()
        .is_some_and(|c| c.starts_with("Msg 18456,"))
        || e.message.to_ascii_lowercase().contains("password")
}

const NO_PASSWORD: &str =
    "Open this connection once in LowQuery and save its password so agents can use it.";

/// Test passwords by profile id, used instead of the OS keychain (CI has
/// none). `None` means the profile has no password.
#[cfg(test)]
static TEST_PASSWORDS: std::sync::Mutex<std::collections::BTreeMap<String, Option<String>>> =
    std::sync::Mutex::new(std::collections::BTreeMap::new());

/// Makes `open_session` use `password` for the profile `id`.
#[cfg(test)]
pub(crate) fn set_test_password(id: &str, password: Option<&str>) {
    TEST_PASSWORDS
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .insert(id.to_string(), password.map(str::to_string));
}

fn stored_password(id: &str) -> AppResult<Option<String>> {
    #[cfg(test)]
    if let Some(pw) = TEST_PASSWORDS
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get(id)
    {
        return Ok(pw.clone());
    }
    profiles::stored_password(id)
}

async fn open_session(profile: &ConnectionProfile) -> AppResult<Session> {
    let read_only = profile.ai_access == AiAccess::Read;
    let session = match profile.engine {
        Engine::Sqlite => open_sqlite(profile, read_only).await?,
        Engine::Sqlserver if profile.windows_auth => Session::open(profile, None).await?,
        _ => match stored_password(&profile.id)? {
            Some(pw) => Session::open(profile, Some(&pw)).await?,
            // Servers with trust or no authentication need none.
            None => Session::open(profile, None).await.map_err(|e| {
                if is_auth_failure(&e) {
                    let mut err = AppError::new(ErrorKind::PasswordRequired, NO_PASSWORD);
                    err.detail = Some(e.to_string());
                    err
                } else {
                    e
                }
            })?,
        },
    };
    if read_only {
        make_read_only(&session).await.map_err(|e| {
            let mut err = AppError::connection("Could not make the agent session read-only.");
            err.detail = Some(e.to_string());
            err
        })?;
    }
    Ok(session)
}

/// SQLite files of read-only connections open with `mode=ro` when the path
/// can be written as a URI, so the file itself is read-only for the session.
async fn open_sqlite(profile: &ConnectionProfile, read_only: bool) -> AppResult<Session> {
    let mut p = profile.clone();
    if read_only {
        p.create_file = false;
        if let Some(uri) = read_only_uri(&p.database) {
            let mut ro = p.clone();
            ro.database = uri;
            // A WAL database without its -shm file cannot open read-only;
            // query_only still applies to the normal open below.
            if let Ok(s) = Session::open(&ro, None).await {
                return Ok(s);
            }
        }
    }
    Session::open(&p, None).await
}

fn read_only_uri(path: &str) -> Option<String> {
    let path = path.trim();
    if path.is_empty()
        || path == ":memory:"
        || path.starts_with("file:")
        || path.starts_with("\\\\")
    {
        return None;
    }
    let path = path.replace('\\', "/");
    let encoded = path
        .replace('%', "%25")
        .replace('?', "%3f")
        .replace('#', "%23");
    let drive =
        path.len() > 1 && path.as_bytes()[0].is_ascii_alphabetic() && path.as_bytes()[1] == b':';
    let prefix = if drive {
        "file:///"
    } else if path.starts_with('/') {
        "file://"
    } else {
        "file:"
    };
    Some(format!("{prefix}{encoded}?mode=ro"))
}

#[cfg(test)]
pub(super) mod tests {
    use super::*;
    use tokio::io::{AsyncBufReadExt, BufReader, DuplexStream};

    pub(crate) struct Client {
        input: DuplexStream,
        output: BufReader<DuplexStream>,
        next_id: i64,
    }

    impl Client {
        pub(crate) fn start(dir: PathBuf, timeout: Duration) -> (Self, Arc<Server>) {
            let (input, server_in) = tokio::io::duplex(1 << 20);
            let (server_out, output) = tokio::io::duplex(1 << 20);
            let server = Arc::new(Server::new(dir, timeout));
            tokio::spawn(serve_io(server.clone(), server_in, server_out));
            (
                Self {
                    input,
                    output: BufReader::new(output),
                    next_id: 1,
                },
                server,
            )
        }

        async fn send_raw(&mut self, line: &str) {
            self.input.write_all(line.as_bytes()).await.unwrap();
            self.input.write_all(b"\n").await.unwrap();
        }

        async fn recv(&mut self) -> Value {
            let mut line = String::new();
            tokio::time::timeout(Duration::from_secs(30), self.output.read_line(&mut line))
                .await
                .expect("no reply")
                .unwrap();
            assert!(line.ends_with('\n') && !line.trim_end().contains('\n'));
            serde_json::from_str(&line).unwrap()
        }

        async fn request(&mut self, method: &str, params: Value) -> Value {
            let id = self.next_id;
            self.next_id += 1;
            let msg = json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params });
            self.send_raw(&msg.to_string()).await;
            let reply = self.recv().await;
            assert_eq!(reply["id"], json!(id));
            reply
        }

        pub(crate) async fn initialize(&mut self, version: &str) -> Value {
            let r = self
                .request(
                    "initialize",
                    json!({
                        "protocolVersion": version,
                        "capabilities": {},
                        "clientInfo": { "name": "test-client", "version": "1.0" }
                    }),
                )
                .await;
            self.send_raw(r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#)
                .await;
            r
        }

        pub(crate) async fn call(&mut self, tool: &str, args: Value) -> Value {
            let r = self
                .request("tools/call", json!({ "name": tool, "arguments": args }))
                .await;
            r["result"].clone()
        }
    }

    fn text(result: &Value) -> &str {
        result["content"][0]["text"].as_str().unwrap()
    }

    fn is_error(result: &Value) -> bool {
        result["isError"] == json!(true)
    }

    fn temp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("lq-mcp-{name}-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[tokio::test]
    async fn protocol_basics() {
        let dir = temp_dir("protocol");
        let (mut c, _) = Client::start(dir.clone(), DEFAULT_TIMEOUT);

        let init = c.initialize("2025-06-18").await;
        assert_eq!(init["result"]["protocolVersion"], "2025-06-18");
        assert_eq!(init["result"]["serverInfo"]["name"], "lowquery");
        assert_eq!(
            init["result"]["capabilities"]["tools"]["listChanged"],
            false
        );
        assert!(init["result"]["instructions"]
            .as_str()
            .unwrap()
            .contains("list_connections"));

        let init = c.initialize("1999-01-01").await;
        assert_eq!(init["result"]["protocolVersion"], PROTOCOL_VERSIONS[0]);

        let missing = c.request("initialize", json!({})).await;
        assert_eq!(missing["error"]["code"], -32602);

        // Notifications get no reply: the next line answers the ping.
        c.send_raw(r#"{"jsonrpc":"2.0","method":"notifications/whatever"}"#)
            .await;
        let pong = c.request("ping", json!({})).await;
        assert_eq!(pong["result"], json!({}));

        let unknown = c.request("resources/list", json!({})).await;
        assert_eq!(unknown["error"]["code"], -32601);

        c.send_raw("{not json").await;
        let bad = c.recv().await;
        assert_eq!(bad["error"]["code"], -32700);
        assert_eq!(bad["id"], Value::Null);

        c.send_raw(r#"[{"jsonrpc":"2.0","id":99,"method":"ping"}]"#)
            .await;
        assert_eq!(c.recv().await["error"]["code"], -32600);

        let no_tool = c
            .request("tools/call", json!({ "name": "drop_everything" }))
            .await;
        assert_eq!(no_tool["error"]["code"], -32602);
        let no_name = c.request("tools/call", json!({})).await;
        assert_eq!(no_name["error"]["code"], -32602);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn tools_list_shape() {
        let dir = temp_dir("tools");
        let (mut c, _) = Client::start(dir.clone(), DEFAULT_TIMEOUT);
        c.initialize("2025-11-25").await;
        let list = c.request("tools/list", json!({})).await;
        let tools = list["result"]["tools"].as_array().unwrap();
        let names: Vec<_> = tools.iter().map(|t| t["name"].as_str().unwrap()).collect();
        assert_eq!(
            names,
            [
                "list_connections",
                "list_schemas",
                "list_tables",
                "describe_table",
                "search_objects",
                "query",
                "explain",
                "execute"
            ]
        );
        for t in tools {
            assert_eq!(t["inputSchema"]["type"], "object");
            assert!(t["description"].as_str().unwrap().len() > 20);
            assert_eq!(t["outputSchema"]["type"], "object");
            assert_eq!(t["annotations"]["openWorldHint"], false);
            let write = t["name"] == "execute";
            assert_eq!(t["annotations"]["readOnlyHint"], !write);
            if write {
                assert_eq!(t["annotations"]["destructiveHint"], true);
                assert_eq!(t["annotations"]["idempotentHint"], false);
            }
        }

        // Older clients get the fields their revision defines.
        c.initialize("2024-11-05").await;
        let list = c.request("tools/list", json!({})).await;
        let first = &list["result"]["tools"][0];
        assert!(first.get("outputSchema").is_none() && first.get("annotations").is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn sqlite_profile(id: &str, name: &str, path: &std::path::Path, access: AiAccess) -> Value {
        json!({
            "id": id,
            "name": name,
            "engine": "sqlite",
            "database": path.to_string_lossy(),
            "aiAccess": match access {
                AiAccess::Off => "off",
                AiAccess::Read => "read",
                AiAccess::Write => "write",
            },
        })
    }

    fn make_db(path: &std::path::Path) {
        let conn = rusqlite::Connection::open(path).unwrap();
        conn.execute_batch(
            "CREATE TABLE teams (id INTEGER PRIMARY KEY, name TEXT NOT NULL);
             CREATE TABLE users (id INTEGER PRIMARY KEY, name TEXT NOT NULL,
                                 team_id INTEGER REFERENCES teams(id), bio TEXT);
             INSERT INTO teams VALUES (1, 'core');
             INSERT INTO users VALUES (1, 'Ann', 1, NULL), (2, 'Bob', 1, 'line1
line2'), (3, 'Cid', NULL, NULL);",
        )
        .unwrap();
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn end_to_end_sqlite() {
        let dir = temp_dir("e2e");
        let ro_db = dir.join("ro.db");
        let rw_db = dir.join("rw.db");
        make_db(&ro_db);
        make_db(&rw_db);
        let profiles = json!([
            sqlite_profile("p-ro", "Local RO", &ro_db, AiAccess::Read),
            sqlite_profile("p-off", "Hidden", &ro_db, AiAccess::Off),
            sqlite_profile("p-rw", "Local RW", &rw_db, AiAccess::Write),
        ]);
        std::fs::write(dir.join("connections.json"), profiles.to_string()).unwrap();

        let (mut c, server) = Client::start(dir.clone(), Duration::from_secs(2));
        c.initialize("2025-11-25").await;

        // Off connections do not exist for agents.
        let list = c.call("list_connections", json!({})).await;
        let conns = list["structuredContent"]["connections"].as_array().unwrap();
        let names: Vec<_> = conns.iter().map(|c| c["name"].as_str().unwrap()).collect();
        assert_eq!(names, ["Local RO", "Local RW"]);
        assert_eq!(conns[0]["access"], "read");
        assert_eq!(conns[1]["access"], "write");
        assert_eq!(conns[0]["where"], "ro.db");
        assert!(!text(&list).contains("Hidden"));
        let hidden = c
            .call(
                "query",
                json!({ "connection": "Hidden", "sql": "select 1" }),
            )
            .await;
        assert!(is_error(&hidden) && text(&hidden).contains("Unknown connection"));
        let hidden = c
            .call("query", json!({ "connection": "p-off", "sql": "select 1" }))
            .await;
        assert!(is_error(&hidden));

        // Query, by name in any case.
        let q = c
            .call(
                "query",
                json!({ "connection": "local ro", "sql": "SELECT id, name, bio FROM users ORDER BY id;" }),
            )
            .await;
        assert!(!is_error(&q), "{}", text(&q));
        let s = &q["structuredContent"];
        assert_eq!(s["rowCount"], 3);
        assert_eq!(s["truncated"], false);
        assert_eq!(s["columns"][1]["name"], "name");
        assert_eq!(s["rows"][1], json!(["2", "Bob", "line1\nline2"]));
        assert_eq!(s["rows"][0][2], Value::Null);
        assert!(text(&q).contains("| 2 | Bob | line1\\nline2 |"));

        let q = c
            .call(
                "query",
                json!({ "connection": "p-ro", "sql": "select * from users", "max_rows": 1 }),
            )
            .await;
        assert_eq!(q["structuredContent"]["rowCount"], 1);
        assert_eq!(q["structuredContent"]["truncated"], true);
        assert!(text(&q).contains("more rows exist"));

        let q = c
            .call(
                "query",
                json!({ "connection": "p-ro", "sql": "select nope from users" }),
            )
            .await;
        assert!(is_error(&q) && text(&q).contains("nope"), "{}", text(&q));

        // describe_table: columns always, foreign keys when the catalog has them.
        let d = c
            .call(
                "describe_table",
                json!({ "connection": "p-ro", "table": "main.users" }),
            )
            .await;
        assert!(!is_error(&d), "{}", text(&d));
        let s = &d["structuredContent"];
        assert_eq!(s["table"], "users");
        assert_eq!(
            s["columns"][0],
            json!({ "name": "id", "type": "INTEGER", "nullable": true, "primaryKey": true })
        );
        assert_eq!(s["columns"][1]["nullable"], false);
        assert!(s["foreignKeysError"].is_string() || s["foreignKeys"][0]["refTable"] == "teams");
        let d = c
            .call(
                "describe_table",
                json!({ "connection": "p-ro", "table": "missing" }),
            )
            .await;
        assert!(is_error(&d));

        // Read-only, classifier level.
        for sql in [
            "DELETE FROM users",
            "WITH x AS (SELECT 1) INSERT INTO teams SELECT 2, 'x' FROM x",
            "PRAGMA user_version = 7",
            "select 1; delete from users",
        ] {
            let r = c
                .call("query", json!({ "connection": "p-ro", "sql": sql }))
                .await;
            assert!(is_error(&r), "{sql}");
        }
        let r = c
            .call(
                "query",
                json!({ "connection": "p-ro", "sql": "update users set name = 'x'" }),
            )
            .await;
        assert!(text(&r).contains("This connection is read-only for AI agents."));
        let r = c
            .call(
                "execute",
                json!({ "connection": "p-ro", "sql": "delete from users" }),
            )
            .await;
        assert!(is_error(&r) && text(&r).contains("read-only for AI agents"));

        // Read-only, engine level: the agent session refuses writes that get
        // past the classifier.
        let profile = server
            .agent_profiles()
            .unwrap()
            .into_iter()
            .find(|p| p.id == "p-ro")
            .unwrap();
        for sql in [
            "DELETE FROM users",
            "WITH x AS (SELECT 1) INSERT INTO teams SELECT 2, 'x' FROM x",
            "PRAGMA user_version = 7",
            "PRAGMA query_only = OFF",
        ] {
            let req = ExecRequest {
                sql: sql.into(),
                max_rows: 0,
            };
            let req = &req;
            let r = server
                .run(&profile, Kind::Read, &Cancel::default(), |s| async move {
                    s.execute(req).await?;
                    s.execute(&ExecRequest {
                        sql: "DELETE FROM users".into(),
                        max_rows: 0,
                    })
                    .await
                })
                .await;
            assert!(r.is_err(), "{sql}");
        }
        let check = rusqlite::Connection::open(&ro_db).unwrap();
        let users: i64 = check
            .query_row("select count(*) from users", [], |r| r.get(0))
            .unwrap();
        let teams: i64 = check
            .query_row("select count(*) from teams", [], |r| r.get(0))
            .unwrap();
        let version: i64 = check
            .query_row("pragma user_version", [], |r| r.get(0))
            .unwrap();
        assert_eq!((users, teams, version), (3, 1, 0));

        // Time limit: the query is interrupted and the session keeps working.
        let r = c
            .call(
                "query",
                json!({ "connection": "p-ro", "sql": "with recursive c(x) as (select 1 union all select x + 1 from c) select count(*) from c" }),
            )
            .await;
        assert!(
            is_error(&r) && text(&r).contains("longer than"),
            "{}",
            text(&r)
        );
        let r = c
            .call(
                "query",
                json!({ "connection": "p-ro", "sql": "select count(*) from users" }),
            )
            .await;
        assert_eq!(r["structuredContent"]["rows"][0][0], "3");

        // Write connection.
        let w = c
            .call(
                "execute",
                json!({ "connection": "Local RW", "sql": "INSERT INTO users (name) VALUES ('Zed')" }),
            )
            .await;
        assert!(!is_error(&w), "{}", text(&w));
        assert_eq!(w["structuredContent"]["rowsAffected"], 1);
        for sql in [
            "BEGIN",
            "insert into teams values (5, 'a'); insert into teams values (6, 'b')",
        ] {
            let r = c
                .call("execute", json!({ "connection": "p-rw", "sql": sql }))
                .await;
            assert!(is_error(&r), "{sql}");
        }
        let r = c
            .call(
                "query",
                json!({ "connection": "p-rw", "sql": "delete from users" }),
            )
            .await;
        assert!(is_error(&r) && text(&r).contains("use execute"));
        let check = rusqlite::Connection::open(&rw_db).unwrap();
        let users: i64 = check
            .query_row("select count(*) from users", [], |r| r.get(0))
            .unwrap();
        let teams: i64 = check
            .query_row("select count(*) from teams", [], |r| r.get(0))
            .unwrap();
        assert_eq!((users, teams), (4, 1));

        // Activity log.
        let log = audit::read_recent(&dir, 100).unwrap();
        assert!(log.iter().all(|e| e.client == "test-client 1.0"));
        let insert = &log[0];
        assert_eq!(insert.tool, "query");
        let zed = log
            .iter()
            .find(|e| e.detail.as_deref() == Some("INSERT INTO users (name) VALUES ('Zed')"))
            .unwrap();
        assert!(zed.ok && zed.tool == "execute" && zed.rows == Some(1));
        assert_eq!(zed.connection_name.as_deref(), Some("Local RW"));
        let first = log.last().unwrap();
        assert_eq!(first.tool, "list_connections");
        let hidden = log
            .iter()
            .find(|e| e.detail.as_deref() == Some("select 1"))
            .unwrap();
        assert!(!hidden.ok && hidden.connection_id.is_none());
        let refused = log
            .iter()
            .find(|e| e.detail.as_deref() == Some("DELETE FROM users"))
            .unwrap();
        assert!(!refused.ok && refused.error.as_deref().unwrap().contains("read-only"));

        drop(c);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn read_only_sqlite_opens_the_file_read_only() {
        let dir = temp_dir("ro-open");
        let db = dir.join("my db.sqlite");
        make_db(&db);
        let profile = ConnectionProfile {
            id: "p".into(),
            name: "p".into(),
            engine: Engine::Sqlite,
            database: db.to_string_lossy().into_owned(),
            ai_access: AiAccess::Read,
            ..Default::default()
        };
        let s = open_sqlite(&profile, true).await.unwrap();
        // Even with query_only turned off, the file itself refuses writes.
        exec(&s, "PRAGMA query_only = OFF").await.unwrap();
        let e = exec(&s, "DELETE FROM users").await.unwrap_err();
        assert!(e.message.contains("readonly"), "{e}");
        drop(s);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn sqlite_uris() {
        assert_eq!(
            read_only_uri(r"C:\data\my db#1.sqlite").unwrap(),
            "file:///C:/data/my db%231.sqlite?mode=ro"
        );
        assert_eq!(
            read_only_uri("/tmp/a?b.db").unwrap(),
            "file:///tmp/a%3fb.db?mode=ro"
        );
        assert!(read_only_uri(":memory:").is_none());
        assert!(read_only_uri("file:x.db?mode=rw").is_none());
    }
}
