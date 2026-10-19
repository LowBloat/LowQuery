//! Redis (and Valkey) session over the `redis` crate (RESP protocol).
//!
//! - Each `execute` call is one command written like redis-cli input, split
//!   with redis-cli's rules (`sdssplitargs`) and sent as raw arguments.
//! - The connection speaks RESP3 so maps, sets, doubles and booleans keep
//!   their types; servers without HELLO (Redis < 6) fall back to RESP2.
//! - Replies become a table: scalars one `value` cell, maps `field`/`value`,
//!   arrays `#`/`value` (or one column per element of uniform tuples), INFO
//!   `section`/`key`/`value`. Nested values are compact JSON-like text and
//!   binary strings use redis-cli escapes (`"\xff"`), so nothing is lossy.
//! - Redis has no autocommit mode: the transaction state is MULTI, tracked
//!   by command name.
//! - Cancel runs `CLIENT UNBLOCK <id> ERROR` over a short-lived second
//!   connection, which only interrupts blocking commands.

use std::fmt::Write as _;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use parking_lot::Mutex;
use redis::aio::MultiplexedConnection;
use redis::{
    AsyncConnectionConfig, Client, Cmd, ConnectionAddr, IntoConnectionInfo, ProtocolVersion,
    RedisConnectionInfo, RedisError, ServerError, TlsCertificates, Value,
};

use super::TxState;
use crate::error::{AppError, AppResult, ErrorKind};
use crate::model::{
    ColumnInfo, ConnectionProfile, DbColumn, DbObject, ExecRequest, ExecResult, SslMode, ValueKind,
};

const DEFAULT_PORT: u16 = 6379;
const CONNECT_TIMEOUT: Duration = Duration::from_secs(15);
/// TLS attempt of `prefer`: a plain server may wait for a line that never
/// comes, so the attempt gives up early and falls back to plain TCP.
const PREFER_TLS_TIMEOUT: Duration = Duration::from_secs(5);
/// Keys listed per database in the tree.
pub const MAX_KEYS: usize = 1000;
const SCAN_COUNT: usize = 500;
const NOT_BLOCKING: &str = "Redis can only interrupt blocking commands such as BLPOP or XREAD.";
/// Commands that would leave the connection in a mode where replies no
/// longer match requests.
const UNSUPPORTED: &[&str] = &[
    "SUBSCRIBE",
    "PSUBSCRIBE",
    "SSUBSCRIBE",
    "MONITOR",
    "SYNC",
    "PSYNC",
];

#[derive(Default)]
struct State {
    /// Between MULTI and EXEC/DISCARD.
    in_multi: bool,
    /// Logical database selected on the connection.
    db: i64,
    /// SELECT queued inside MULTI, applied when EXEC runs.
    queued_select: Option<i64>,
}

pub struct RedisSession {
    conn: tokio::sync::Mutex<MultiplexedConnection>,
    /// Client that opened the session, reused by `cancel`.
    client: Client,
    client_id: Option<i64>,
    busy: AtomicBool,
    state: Mutex<State>,
    pub server_version: String,
    pub current_user: Option<String>,
    pub current_schema: Option<String>,
}

pub async fn connect(
    profile: &ConnectionProfile,
    password: Option<&str>,
) -> AppResult<RedisSession> {
    // rustls has more than one crypto provider compiled in; the redis crate
    // builds its TLS config with the process default.
    let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();

    let db = parse_db(&profile.database)
        .ok_or_else(|| AppError::invalid("The database must be a number (0, 1, 2...)."))?;
    let mut auth = RedisConnectionInfo::default().set_protocol(ProtocolVersion::RESP3);
    let user = profile.user.trim();
    if !user.is_empty() {
        auth = auth.set_username(user);
    }
    if let Some(p) = password.filter(|p| !p.is_empty()) {
        auth = auth.set_password(p);
    }
    let target = Target {
        host: profile.host.trim().to_string(),
        port: if profile.port == 0 {
            DEFAULT_PORT
        } else {
            profile.port
        },
        auth,
    };

    let (client, mut conn) = match profile.ssl_mode {
        SslMode::Disable => target.open(&Tls::Off, CONNECT_TIMEOUT).await,
        SslMode::Prefer => match target.open(&Tls::Lenient, PREFER_TLS_TIMEOUT).await {
            Err(e) if !is_server_reply(&e) => target.open(&Tls::Off, CONNECT_TIMEOUT).await,
            other => other,
        },
        SslMode::Require => target.open(&Tls::Lenient, CONNECT_TIMEOUT).await,
        SslMode::VerifyFull => target.open_verified().await,
    }
    .map_err(connect_error)?;

    let _ = call(&mut conn, &command(&["CLIENT", "SETNAME", "LowQuery"])).await;
    if db != 0 {
        call(&mut conn, &command(&["SELECT", &db.to_string()]))
            .await
            .map_err(as_connection)?;
    }
    let info = match call(&mut conn, &command(&["INFO", "server"])).await {
        Ok(v) => text_of(&v),
        Err(e) if e.kind == ErrorKind::Connection || is_auth_code(&e) => {
            return Err(as_connection(e))
        }
        Err(_) => None,
    };
    let client_id = match call(&mut conn, &command(&["CLIENT", "ID"])).await {
        Ok(Value::Int(id)) => Some(id),
        _ => None,
    };
    let current_user = call(&mut conn, &command(&["ACL", "WHOAMI"]))
        .await
        .ok()
        .and_then(|v| text_of(&v));

    Ok(RedisSession {
        conn: tokio::sync::Mutex::new(conn),
        client,
        client_id,
        busy: AtomicBool::new(false),
        state: Mutex::new(State {
            db,
            ..State::default()
        }),
        server_version: product_version(info.as_deref().unwrap_or_default()),
        current_user,
        current_schema: Some(format!("db{db}")),
    })
}

enum Tls {
    Off,
    /// Encrypted without validating the certificate (`prefer`/`require`).
    Lenient,
    /// Validated against these PEM roots, or the bundled ones when `None`.
    Verified(Option<Vec<u8>>),
}

struct Target {
    host: String,
    port: u16,
    auth: RedisConnectionInfo,
}

impl Target {
    fn client(&self, tls: &Tls, auth: RedisConnectionInfo) -> Result<Client, RedisError> {
        let addr = match tls {
            Tls::Off => ConnectionAddr::Tcp(self.host.clone(), self.port),
            Tls::Lenient | Tls::Verified(_) => ConnectionAddr::TcpTls {
                host: self.host.clone(),
                port: self.port,
                insecure: matches!(tls, Tls::Lenient),
                tls_params: None,
            },
        };
        let info = addr.into_connection_info()?.set_redis_settings(auth);
        match tls {
            Tls::Verified(Some(pem)) => Client::build_with_tls(
                info,
                TlsCertificates {
                    client_tls: None,
                    root_cert: Some(pem.clone()),
                },
            ),
            _ => Client::open(info),
        }
    }

    /// Opens with RESP3, or RESP2 when the server has no HELLO.
    async fn open(
        &self,
        tls: &Tls,
        timeout: Duration,
    ) -> Result<(Client, MultiplexedConnection), RedisError> {
        let client = self.client(tls, self.auth.clone())?;
        match open_connection(&client, timeout).await {
            Ok(conn) => Ok((client, conn)),
            Err(e) if resp3_unsupported(&e) => {
                let auth = self.auth.clone().set_protocol(ProtocolVersion::RESP2);
                let client = self.client(tls, auth)?;
                let conn = open_connection(&client, timeout).await?;
                Ok((client, conn))
            }
            Err(e) => Err(e),
        }
    }

    /// Validates against the OS store first, then the bundled roots (the
    /// OS store may lack a public root it would only fetch on demand).
    async fn open_verified(&self) -> Result<(Client, MultiplexedConnection), RedisError> {
        if let Some(pem) = native_roots_pem() {
            match self.open(&Tls::Verified(Some(pem)), CONNECT_TIMEOUT).await {
                Err(e) if describe(&e).to_lowercase().contains("certificate") => {}
                other => return other,
            }
        }
        self.open(&Tls::Verified(None), CONNECT_TIMEOUT).await
    }
}

async fn open_connection(
    client: &Client,
    timeout: Duration,
) -> Result<MultiplexedConnection, RedisError> {
    // No response timeout: blocking commands may wait as long as they ask.
    let config = AsyncConnectionConfig::new()
        .set_connection_timeout(Some(timeout))
        .set_response_timeout(None);
    client
        .get_multiplexed_async_connection_with_config(&config)
        .await
}

fn resp3_unsupported(e: &RedisError) -> bool {
    if e.kind() == redis::ErrorKind::RESP3NotSupported {
        return true;
    }
    let detail = e.detail().unwrap_or_default().to_ascii_lowercase();
    e.code() == Some("ERR") && detail.contains("unknown command") && detail.contains("hello")
}

/// The server answered (an auth failure, for example): TLS itself worked.
fn is_server_reply(e: &RedisError) -> bool {
    matches!(
        e.kind(),
        redis::ErrorKind::Server(_)
            | redis::ErrorKind::Extension
            | redis::ErrorKind::AuthenticationFailed
    )
}

/// Certificates of the OS store as PEM, skipping those rustls cannot parse.
fn native_roots_pem() -> Option<Vec<u8>> {
    let mut store = rustls::RootCertStore::empty();
    let mut pem = String::new();
    for cert in rustls_native_certs::load_native_certs().certs {
        if store.add(cert.clone()).is_ok() {
            pem.push_str(&pem_certificate(&cert));
        }
    }
    (!pem.is_empty()).then(|| pem.into_bytes())
}

fn pem_certificate(der: &[u8]) -> String {
    let encoded = base64(der);
    let mut out = String::from("-----BEGIN CERTIFICATE-----\n");
    for line in encoded.as_bytes().chunks(64) {
        out.push_str(std::str::from_utf8(line).unwrap_or_default());
        out.push('\n');
    }
    out.push_str("-----END CERTIFICATE-----\n");
    out
}

fn base64(bytes: &[u8]) -> String {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let n = (u32::from(chunk[0]) << 16)
            | (u32::from(*chunk.get(1).unwrap_or(&0)) << 8)
            | u32::from(*chunk.get(2).unwrap_or(&0));
        for i in 0..4 {
            if i <= chunk.len() {
                out.push(TABLE[(n >> (18 - 6 * i) & 63) as usize] as char);
            } else {
                out.push('=');
            }
        }
    }
    out
}

/// `""` → 0, `"3"` or `"db3"` → 3.
fn parse_db(text: &str) -> Option<i64> {
    let t = text.trim();
    if t.is_empty() {
        return Some(0);
    }
    let digits = if t.len() > 2 && t[..2].eq_ignore_ascii_case("db") {
        &t[2..]
    } else {
        t
    };
    digits.parse().ok().filter(|n| *n >= 0)
}

/// `Redis 7.4.1`, `Valkey 8.1.0` or `Dragonfly 1.27.1`, from `INFO server`.
pub fn product_version(info: &str) -> String {
    let field = |name: &str| {
        info.lines()
            .filter_map(|l| l.trim().split_once(':'))
            .find(|(k, _)| *k == name)
            .map(|(_, v)| v.trim().to_string())
    };
    let redis = field("redis_version");
    let valkey = field("valkey_version");
    if valkey.is_some() || field("server_name").as_deref() == Some("valkey") {
        return format!("Valkey {}", valkey.or(redis).unwrap_or_default())
            .trim_end()
            .to_string();
    }
    if let Some(v) = field("dragonfly_version") {
        return format!("Dragonfly {}", v.trim_start_matches("df-v"));
    }
    format!("Redis {}", redis.unwrap_or_default())
        .trim_end()
        .to_string()
}

fn command(parts: &[&str]) -> Cmd {
    let mut cmd = Cmd::new();
    for p in parts {
        cmd.arg(*p);
    }
    cmd
}

/// Sends one command; an error reply becomes an `AppError`.
async fn call(conn: &mut MultiplexedConnection, cmd: &Cmd) -> AppResult<Value> {
    match conn.send_packed_command(cmd).await.map_err(map_error)? {
        Value::ServerError(e) => Err(server_error(&e)),
        v => Ok(v),
    }
}

impl RedisSession {
    pub fn tx_state(&self) -> TxState {
        TxState {
            autocommit: true,
            in_transaction: self.state.lock().in_multi,
        }
    }

    pub async fn execute(&self, req: &ExecRequest) -> AppResult<ExecResult> {
        let text = req.sql.trim();
        let args = if text.starts_with('#') || text.starts_with("//") {
            Vec::new()
        } else {
            split_args(&req.sql)?
        };
        if args.is_empty() {
            return Ok(ExecResult {
                in_transaction: self.tx_state().in_transaction,
                ..ExecResult::default()
            });
        }
        let name = String::from_utf8_lossy(&args[0]).to_ascii_uppercase();
        let reply_off = name == "CLIENT" && arg_is(&args, 1, "REPLY");
        if UNSUPPORTED.contains(&name.as_str()) || reply_off {
            return Err(AppError::invalid(format!(
                "{} is not supported in the query editor.",
                if reply_off { "CLIENT REPLY" } else { &name }
            )));
        }
        let mut cmd = Cmd::new();
        for a in &args {
            cmd.arg(a.as_slice());
        }

        let mut conn = self.conn.lock().await;
        self.busy.store(true, Ordering::SeqCst);
        let started = Instant::now();
        let reply = conn.send_packed_command(&cmd).await;
        let elapsed_ms = started.elapsed().as_millis() as u64;
        self.busy.store(false, Ordering::SeqCst);
        track(&mut self.state.lock(), &name, &args, &reply);
        drop(conn);

        let value = reply.map_err(map_error)?;
        if let Value::ServerError(e) = &value {
            return Err(server_error(e));
        }
        let grid = tabulate(&name, &args, value, req.max_rows);
        let (columns, rows) = grid.finish();
        Ok(ExecResult {
            columns: Some(columns),
            rows,
            truncated: grid.truncated,
            rows_affected: None,
            elapsed_ms,
            notices: grid.notices,
            in_transaction: self.tx_state().in_transaction,
        })
    }

    pub async fn cancel(&self) -> AppResult<()> {
        if !self.busy.load(Ordering::SeqCst) {
            return Ok(());
        }
        let Some(id) = self.client_id else {
            return Err(AppError::invalid(NOT_BLOCKING));
        };
        let mut conn = open_connection(&self.client, CONNECT_TIMEOUT)
            .await
            .map_err(connect_error)?;
        let reply = call(
            &mut conn,
            &command(&["CLIENT", "UNBLOCK", &id.to_string(), "ERROR"]),
        )
        .await?;
        match reply {
            Value::Int(0) => Err(AppError::invalid(NOT_BLOCKING)),
            _ => Ok(()),
        }
    }

    pub async fn set_autocommit(&self, on: bool) -> AppResult<TxState> {
        if on {
            Ok(self.tx_state())
        } else {
            Err(AppError::invalid(
                "Redis has no autocommit mode; use MULTI and EXEC.",
            ))
        }
    }

    pub async fn end_transaction(&self, commit: bool) -> AppResult<TxState> {
        if !self.tx_state().in_transaction {
            return Ok(self.tx_state());
        }
        let name = if commit { "EXEC" } else { "DISCARD" };
        let mut conn = self.conn.lock().await;
        let reply = conn.send_packed_command(&command(&[name])).await;
        track(&mut self.state.lock(), name, &[], &reply);
        drop(conn);
        if let Value::ServerError(e) = reply.map_err(map_error)? {
            return Err(server_error(&e));
        }
        Ok(self.tx_state())
    }

    fn check_not_in_multi(&self) -> AppResult<()> {
        if self.tx_state().in_transaction {
            return Err(AppError::invalid(
                "Finish the transaction with EXEC or DISCARD first.",
            ));
        }
        Ok(())
    }

    /// The current database first, then those with keys, then the empty
    /// ones (when CONFIG GET databases is allowed).
    pub async fn list_schemas(&self) -> AppResult<Vec<String>> {
        self.check_not_in_multi()?;
        let mut conn = self.conn.lock().await;
        // A user without INFO or CONFIG rights still sees its database.
        let keyspace = match call(&mut conn, &command(&["INFO", "keyspace"])).await {
            Ok(v) => text_of(&v).unwrap_or_default(),
            Err(e) if e.kind == ErrorKind::Connection => return Err(e),
            Err(_) => String::new(),
        };
        let mut used: Vec<i64> = info_rows(&keyspace)
            .into_iter()
            .filter_map(|[_, key, _]| key.strip_prefix("db").and_then(|n| n.parse().ok()))
            .collect();
        let count = match call(&mut conn, &command(&["CONFIG", "GET", "databases"])).await {
            Ok(v) => config_value(v).and_then(|n| n.parse::<i64>().ok()),
            Err(_) => None,
        };
        drop(conn);

        let current = self.state.lock().db;
        used.sort_unstable();
        let mut dbs = vec![current];
        dbs.extend(used.iter().filter(|n| **n != current));
        dbs.extend((0..count.unwrap_or(0)).filter(|n| *n != current && !used.contains(n)));
        Ok(dbs.into_iter().map(|n| format!("db{n}")).collect())
    }

    /// Up to [`MAX_KEYS`] keys of the database with their types, sorted by
    /// type then name.
    pub async fn list_objects(&self, schema: &str) -> AppResult<Vec<DbObject>> {
        let db = parse_db(schema)
            .ok_or_else(|| AppError::invalid(format!("Unknown database: {schema}")))?;
        self.check_not_in_multi()?;
        let mut conn = self.conn.lock().await;
        let current = self.state.lock().db;
        if db != current {
            call(&mut conn, &command(&["SELECT", &db.to_string()])).await?;
        }
        let outcome = key_types(&mut conn).await;
        if db != current {
            let back = call(&mut conn, &command(&["SELECT", &current.to_string()])).await;
            if back.is_err() {
                self.state.lock().db = db;
            }
        }
        let mut objects: Vec<DbObject> = outcome?
            .into_iter()
            .filter_map(|(key, kind)| {
                kind.map(|kind| DbObject {
                    name: text(&key),
                    kind,
                })
            })
            .collect();
        objects.sort_by(|a, b| (&a.kind, &a.name).cmp(&(&b.kind, &b.name)));
        Ok(objects)
    }

    pub async fn list_columns(&self, _schema: &str, _table: &str) -> AppResult<Vec<DbColumn>> {
        Ok(Vec::new())
    }
}

/// SCAN until [`MAX_KEYS`] keys or the end, then TYPE for each key in one
/// pipeline. Keys removed meanwhile have no type.
async fn key_types(conn: &mut MultiplexedConnection) -> AppResult<Vec<(Vec<u8>, Option<String>)>> {
    let mut cursor = b"0".to_vec();
    let mut keys: Vec<Vec<u8>> = Vec::new();
    loop {
        let mut cmd = Cmd::new();
        cmd.arg("SCAN")
            .arg(cursor.as_slice())
            .arg("COUNT")
            .arg(SCAN_COUNT);
        let reply = call(conn, &cmd).await?;
        let Value::Array(mut parts) = reply else {
            return Err(AppError::internal("Unexpected SCAN reply."));
        };
        if parts.len() != 2 {
            return Err(AppError::internal("Unexpected SCAN reply."));
        }
        if let Value::Array(found) = parts.pop().unwrap_or_default() {
            keys.extend(found.into_iter().filter_map(|k| bytes_of(&k)));
        }
        cursor = bytes_of(&parts[0]).unwrap_or_default();
        if cursor == b"0" || keys.len() >= MAX_KEYS {
            break;
        }
    }
    keys.sort_unstable();
    keys.dedup();
    keys.truncate(MAX_KEYS);
    if keys.is_empty() {
        return Ok(Vec::new());
    }

    let mut pipe = redis::pipe();
    for k in &keys {
        pipe.cmd("TYPE").arg(k.as_slice());
    }
    let types = conn
        .send_packed_commands(&pipe, 0, keys.len())
        .await
        .map_err(map_error)?;
    Ok(keys
        .into_iter()
        .zip(types.iter().map(|t| text_of(t).and_then(|t| key_kind(&t))))
        .collect())
}

/// Tree kind of a TYPE reply: core types as they are, module types
/// lowercased (`ReJSON-RL` → `json`).
fn key_kind(type_name: &str) -> Option<String> {
    let lower = type_name.trim().to_ascii_lowercase();
    match lower.as_str() {
        "" | "none" => None,
        "rejson-rl" => Some("json".into()),
        _ => Some(lower),
    }
}

/// Updates the MULTI state and the selected database after a command.
fn track(state: &mut State, name: &str, args: &[Vec<u8>], reply: &Result<Value, RedisError>) {
    let value = match reply {
        Ok(v) => v,
        // The connection is gone, and the transaction with it.
        Err(_) => {
            state.in_multi = false;
            state.queued_select = None;
            return;
        }
    };
    let failed = matches!(value, Value::ServerError(_));
    match name {
        // EXEC and DISCARD end the transaction even when they fail
        // (EXECABORT, or no MULTI at all).
        "EXEC" => {
            let select = state.queued_select.take();
            if let (Some(db), Value::Array(_)) = (select, value) {
                state.db = db;
            }
            state.in_multi = false;
        }
        "DISCARD" => {
            state.in_multi = false;
            state.queued_select = None;
        }
        "MULTI" if !failed => state.in_multi = true,
        "RESET" if !failed => *state = State::default(),
        "SELECT" if !failed => {
            if let Some(db) = args
                .get(1)
                .and_then(|a| std::str::from_utf8(a).ok()?.parse().ok())
            {
                if state.in_multi {
                    state.queued_select = Some(db);
                } else {
                    state.db = db;
                }
            }
        }
        _ => {}
    }
}

fn arg_is(args: &[Vec<u8>], i: usize, word: &str) -> bool {
    args.get(i)
        .is_some_and(|a| a.eq_ignore_ascii_case(word.as_bytes()))
}

fn has_arg(args: &[Vec<u8>], word: &str) -> bool {
    args.iter()
        .skip(1)
        .any(|a| a.eq_ignore_ascii_case(word.as_bytes()))
}

// ---------------------------------------------------------------------------
// Argument parsing (redis-cli's sdssplitargs)

fn is_space(c: u8) -> bool {
    matches!(c, b' ' | b'\t' | b'\n' | b'\r' | 0x0b | 0x0c)
}

fn hex_value(c: u8) -> Option<u8> {
    (c as char).to_digit(16).map(|d| d as u8)
}

/// Splits a command line into arguments the way redis-cli does: double
/// quotes take `\n \r \t \b \a \\ \"` and `\xHH`, single quotes only `\'`,
/// and a closing quote must be followed by a space or the end.
pub fn split_args(line: &str) -> AppResult<Vec<Vec<u8>>> {
    let b = line.as_bytes();
    let invalid_at = |message: &str, byte: usize| {
        let mut err = AppError::invalid(message);
        err.position = Some(line[..byte].chars().count());
        err
    };
    let mut args = Vec::new();
    let mut p = 0;
    loop {
        while p < b.len() && is_space(b[p]) {
            p += 1;
        }
        if p >= b.len() {
            return Ok(args);
        }
        let mut current = Vec::new();
        let mut in_double = false;
        let mut in_single = false;
        let mut quote_start = 0;
        loop {
            if in_double {
                if p >= b.len() {
                    return Err(invalid_at("Unbalanced quotes in the command.", quote_start));
                }
                let next = b.get(p + 1).copied();
                if b[p] == b'\\' && next == Some(b'x') {
                    let hi = b.get(p + 2).copied().and_then(hex_value);
                    let lo = b.get(p + 3).copied().and_then(hex_value);
                    if let (Some(hi), Some(lo)) = (hi, lo) {
                        current.push(hi * 16 + lo);
                        p += 3;
                    } else {
                        p += 1;
                        current.push(b'x');
                    }
                } else if b[p] == b'\\' && next.is_some() {
                    p += 1;
                    current.push(match b[p] {
                        b'n' => b'\n',
                        b'r' => b'\r',
                        b't' => b'\t',
                        b'b' => 0x08,
                        b'a' => 0x07,
                        c => c,
                    });
                } else if b[p] == b'"' {
                    if b.get(p + 1).is_some_and(|c| !is_space(*c)) {
                        return Err(invalid_at(
                            "A closing quote must be followed by a space.",
                            p,
                        ));
                    }
                    p += 1;
                    break;
                } else {
                    current.push(b[p]);
                }
            } else if in_single {
                if p >= b.len() {
                    return Err(invalid_at("Unbalanced quotes in the command.", quote_start));
                }
                if b[p] == b'\\' && b.get(p + 1) == Some(&b'\'') {
                    p += 1;
                    current.push(b'\'');
                } else if b[p] == b'\'' {
                    if b.get(p + 1).is_some_and(|c| !is_space(*c)) {
                        return Err(invalid_at(
                            "A closing quote must be followed by a space.",
                            p,
                        ));
                    }
                    p += 1;
                    break;
                } else {
                    current.push(b[p]);
                }
            } else if p >= b.len() || is_space(b[p]) {
                break;
            } else if b[p] == b'"' {
                in_double = true;
                quote_start = p;
            } else if b[p] == b'\'' {
                in_single = true;
                quote_start = p;
            } else {
                current.push(b[p]);
            }
            p += 1;
        }
        args.push(current);
    }
}

// ---------------------------------------------------------------------------
// Replies as tables

/// Rows of reply values before formatting.
#[derive(Default)]
struct Grid {
    names: Vec<String>,
    rows: Vec<Vec<Value>>,
    truncated: bool,
    notices: Vec<String>,
}

impl Grid {
    fn new(names: &[&str]) -> Self {
        Grid {
            names: names.iter().map(|n| n.to_string()).collect(),
            ..Grid::default()
        }
    }

    fn finish(&self) -> (Vec<ColumnInfo>, Vec<Vec<Option<String>>>) {
        let columns = self
            .names
            .iter()
            .enumerate()
            .map(|(i, name)| {
                let values: Vec<&Value> = self
                    .rows
                    .iter()
                    .filter_map(|r| r.get(i))
                    .filter(|v| !matches!(v, Value::Nil))
                    .collect();
                ColumnInfo {
                    name: name.clone(),
                    type_name: column_type(&values),
                    kind: if name == "#" || name == "score" {
                        ValueKind::Number
                    } else {
                        column_kind(&values)
                    },
                }
            })
            .collect();
        let rows = self
            .rows
            .iter()
            .map(|r| r.iter().map(cell).collect())
            .collect();
        (columns, rows)
    }
}

/// Keeps at most `max_rows` items (0 = all); true when some were dropped.
fn limit<T>(items: &mut Vec<T>, max_rows: usize) -> bool {
    if max_rows > 0 && items.len() > max_rows {
        items.truncate(max_rows);
        true
    } else {
        false
    }
}

fn is_aggregate(v: &Value) -> bool {
    matches!(
        v,
        Value::Array(_)
            | Value::Set(_)
            | Value::Map(_)
            | Value::Push { .. }
            | Value::Attribute { .. }
    )
}

fn unwrap_attribute(v: Value) -> Value {
    match v {
        Value::Attribute { data, .. } => unwrap_attribute(*data),
        other => other,
    }
}

/// Sorted-set replies whose pairs are member and score.
fn has_scores(name: &str, args: &[Vec<u8>]) -> bool {
    name.starts_with('Z') && (has_arg(args, "WITHSCORES") || matches!(name, "ZPOPMIN" | "ZPOPMAX"))
}

/// Commands whose RESP2 reply is a flat field/value list.
fn has_field_pairs(name: &str, args: &[Vec<u8>]) -> bool {
    match name {
        "HGETALL" => true,
        "CONFIG" => arg_is(args, 1, "GET"),
        "HRANDFIELD" => has_arg(args, "WITHVALUES"),
        _ => false,
    }
}

fn tabulate(name: &str, args: &[Vec<u8>], value: Value, max_rows: usize) -> Grid {
    let value = unwrap_attribute(value);
    if name == "INFO" {
        if let Some(text) = text_of(&value) {
            let mut rows = info_rows(&text);
            let mut grid = Grid::new(&["section", "key", "value"]);
            grid.truncated = limit(&mut rows, max_rows);
            grid.rows = rows
                .into_iter()
                .map(|r| r.into_iter().map(Value::SimpleString).collect())
                .collect();
            return grid;
        }
    }
    match value {
        Value::Array(mut parts)
            if matches!(name, "SCAN" | "SSCAN" | "HSCAN" | "ZSCAN")
                && parts.len() == 2
                && matches!(parts[1], Value::Array(_)) =>
        {
            let Value::Array(items) = parts.pop().unwrap_or_default() else {
                unreachable!()
            };
            let cursor = cell(&parts[0]).unwrap_or_default();
            let mut grid = match name {
                "HSCAN" if !has_arg(args, "NOVALUES") => pairs(items, "field", "value", max_rows),
                "ZSCAN" => pairs(items, "member", "score", max_rows),
                _ => list(items, max_rows),
            };
            grid.notices.push(if cursor == "0" {
                "Next cursor: 0 (the scan is complete).".into()
            } else {
                format!("Next cursor: {cursor}")
            });
            grid
        }
        Value::Map(mut entries) => {
            let mut grid = Grid::new(&["field", "value"]);
            grid.truncated = limit(&mut entries, max_rows);
            grid.rows = entries.into_iter().map(|(k, v)| vec![k, v]).collect();
            grid
        }
        Value::Array(items) | Value::Set(items) | Value::Push { data: items, .. } => {
            if let Some(width) = tuple_width(&items) {
                let names: Vec<String> = if width == 2 && has_scores(name, args) {
                    vec!["member".into(), "score".into()]
                } else if width == 2 && matches!(name, "XRANGE" | "XREVRANGE") {
                    vec!["id".into(), "fields".into()]
                } else {
                    (1..=width).map(|i| i.to_string()).collect()
                };
                let mut items = items;
                let mut grid = Grid {
                    names,
                    ..Grid::default()
                };
                grid.truncated = limit(&mut items, max_rows);
                grid.rows = items
                    .into_iter()
                    .map(|t| match t {
                        Value::Array(cells) => cells,
                        other => vec![other],
                    })
                    .collect();
                return grid;
            }
            let flat = items.len().is_multiple_of(2) && !items.iter().any(is_aggregate);
            if flat && has_scores(name, args) {
                pairs(items, "member", "score", max_rows)
            } else if flat && has_field_pairs(name, args) {
                pairs(items, "field", "value", max_rows)
            } else {
                list(items, max_rows)
            }
        }
        scalar => {
            let mut grid = Grid::new(&["value"]);
            grid.rows = vec![vec![scalar]];
            grid
        }
    }
}

/// Width of a non-empty list whose items are all arrays of the same length
/// (at least 2).
fn tuple_width(items: &[Value]) -> Option<usize> {
    let mut width = None;
    for item in items {
        let Value::Array(cells) = item else {
            return None;
        };
        if cells.len() < 2 || width.is_some_and(|w| w != cells.len()) {
            return None;
        }
        width = Some(cells.len());
    }
    width
}

fn list(mut items: Vec<Value>, max_rows: usize) -> Grid {
    let mut grid = Grid::new(&["#", "value"]);
    grid.truncated = limit(&mut items, max_rows);
    grid.rows = items
        .into_iter()
        .enumerate()
        .map(|(i, v)| vec![Value::Int(i as i64 + 1), v])
        .collect();
    grid
}

/// A flat `[a1, b1, a2, b2...]` list as two columns.
fn pairs(items: Vec<Value>, a: &str, b: &str, max_rows: usize) -> Grid {
    if !items.len().is_multiple_of(2) {
        return list(items, max_rows);
    }
    let mut grid = Grid::new(&[a, b]);
    let mut it = items.into_iter();
    let mut rows = Vec::new();
    while let (Some(x), Some(y)) = (it.next(), it.next()) {
        rows.push(vec![x, y]);
    }
    grid.truncated = limit(&mut rows, max_rows);
    grid.rows = rows;
    grid
}

/// `[section, key, value]` rows from INFO text (`# Section` and
/// `key:value` lines).
fn info_rows(text: &str) -> Vec<[String; 3]> {
    let mut section = String::new();
    let mut rows = Vec::new();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        if let Some(name) = line.strip_prefix('#') {
            section = name.trim().to_string();
            continue;
        }
        let (key, value) = line.split_once(':').unwrap_or((line, ""));
        rows.push([section.clone(), key.to_string(), value.to_string()]);
    }
    rows
}

/// Value of `CONFIG GET <one parameter>` (a map in RESP3, a pair in RESP2).
fn config_value(v: Value) -> Option<String> {
    match v {
        Value::Map(mut entries) => entries.pop().and_then(|(_, v)| text_of(&v)),
        Value::Array(items) if items.len() == 2 => text_of(&items[1]),
        _ => None,
    }
}

fn type_name(v: &Value) -> &'static str {
    match v {
        Value::Nil => "nil",
        Value::Int(_) => "integer",
        Value::Double(_) => "double",
        Value::Boolean(_) => "boolean",
        Value::BigNumber(_) => "big number",
        Value::BulkString(_) | Value::SimpleString(_) | Value::Okay => "string",
        Value::VerbatimString { .. } => "verbatim string",
        Value::Array(_) | Value::Push { .. } => "array",
        Value::Set(_) => "set",
        Value::Map(_) | Value::Attribute { .. } => "map",
        Value::ServerError(_) => "error",
        _ => "other",
    }
}

fn column_type(values: &[&Value]) -> String {
    let mut names = values.iter().map(|v| type_name(v));
    match names.next() {
        None => "nil".into(),
        Some(first) if names.all(|n| n == first) => first.into(),
        Some(_) => "mixed".into(),
    }
}

fn column_kind(values: &[&Value]) -> ValueKind {
    if values.is_empty() {
        ValueKind::Text
    } else if values
        .iter()
        .all(|v| matches!(v, Value::Int(_) | Value::Double(_) | Value::BigNumber(_)))
    {
        ValueKind::Number
    } else if values.iter().all(|v| matches!(v, Value::Boolean(_))) {
        ValueKind::Bool
    } else if values.iter().all(|v| is_aggregate(v)) {
        ValueKind::Json
    } else {
        ValueKind::Text
    }
}

/// Text of a cell: scalars as they are, nested values as compact JSON-like
/// text, errors inside a reply (EXEC) as `(error) CODE message`.
fn cell(v: &Value) -> Option<String> {
    match v {
        Value::Nil => None,
        Value::ServerError(e) => Some(error_text(e)),
        v if is_aggregate(v) => {
            let mut out = String::new();
            write_nested(&mut out, v);
            Some(out)
        }
        v => Some(scalar_text(v)),
    }
}

fn scalar_text(v: &Value) -> String {
    match v {
        Value::Int(i) => i.to_string(),
        Value::Double(d) => double_text(*d),
        Value::Boolean(b) => b.to_string(),
        Value::BigNumber(n) => String::from_utf8_lossy(n).into_owned(),
        Value::BulkString(b) => text(b),
        Value::SimpleString(s) => s.clone(),
        Value::Okay => "OK".into(),
        Value::VerbatimString { text, .. } => text.clone(),
        Value::Nil => String::new(),
        other => cell(other).unwrap_or_default(),
    }
}

fn double_text(d: f64) -> String {
    if d.is_nan() {
        "nan".into()
    } else if d.is_infinite() {
        if d > 0.0 { "inf" } else { "-inf" }.into()
    } else {
        let plain = d.to_string();
        if plain.len() > 24 {
            format!("{d:e}")
        } else {
            plain
        }
    }
}

fn error_text(e: &ServerError) -> String {
    match e.details() {
        Some(d) => format!("(error) {} {d}", e.code()),
        None => format!("(error) {}", e.code()),
    }
}

/// Bytes as text when they are readable UTF-8, otherwise redis-cli's
/// quoted form with escapes (`"\xff"`), which never loses data.
fn text(bytes: &[u8]) -> String {
    match std::str::from_utf8(bytes) {
        Ok(s)
            if !s
                .chars()
                .any(|c| c.is_control() && !matches!(c, '\n' | '\r' | '\t')) =>
        {
            s.to_string()
        }
        _ => repr(bytes),
    }
}

/// redis-cli's `sdscatrepr`.
pub fn repr(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() + 2);
    out.push('"');
    for &b in bytes {
        match b {
            b'\\' => out.push_str("\\\\"),
            b'"' => out.push_str("\\\""),
            b'\n' => out.push_str("\\n"),
            b'\r' => out.push_str("\\r"),
            b'\t' => out.push_str("\\t"),
            0x07 => out.push_str("\\a"),
            0x08 => out.push_str("\\b"),
            0x20..=0x7e => out.push(b as char),
            _ => {
                let _ = write!(out, "\\x{b:02x}");
            }
        }
    }
    out.push('"');
    out
}

fn json_string(out: &mut String, s: &str) {
    out.push_str(&serde_json::to_string(s).unwrap_or_default());
}

/// Compact JSON-like text. Binary strings keep redis-cli escapes.
fn write_nested(out: &mut String, v: &Value) {
    match v {
        Value::Nil => out.push_str("null"),
        Value::Int(i) => {
            let _ = write!(out, "{i}");
        }
        Value::Double(d) if d.is_finite() => out.push_str(&double_text(*d)),
        Value::Double(d) => json_string(out, &double_text(*d)),
        Value::Boolean(b) => {
            let _ = write!(out, "{b}");
        }
        Value::BigNumber(n) => out.push_str(&String::from_utf8_lossy(n)),
        Value::BulkString(b) => match std::str::from_utf8(b) {
            Ok(s) => json_string(out, s),
            Err(_) => out.push_str(&repr(b)),
        },
        Value::SimpleString(s) => json_string(out, s),
        Value::Okay => json_string(out, "OK"),
        Value::VerbatimString { text, .. } => json_string(out, text),
        Value::Array(items) | Value::Set(items) | Value::Push { data: items, .. } => {
            out.push('[');
            for (i, item) in items.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                write_nested(out, item);
            }
            out.push(']');
        }
        Value::Map(entries) => {
            out.push('{');
            for (i, (k, v)) in entries.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                match k {
                    Value::BulkString(_)
                    | Value::SimpleString(_)
                    | Value::VerbatimString { .. } => write_nested(out, k),
                    other => {
                        let mut key = String::new();
                        write_nested(&mut key, other);
                        json_string(out, &key);
                    }
                }
                out.push(':');
                write_nested(out, v);
            }
            out.push('}');
        }
        Value::Attribute { data, .. } => write_nested(out, data),
        Value::ServerError(e) => json_string(out, &error_text(e)),
        _ => out.push_str("null"),
    }
}

/// Text of a string-like reply.
fn text_of(v: &Value) -> Option<String> {
    match v {
        Value::BulkString(b) => Some(String::from_utf8_lossy(b).into_owned()),
        Value::SimpleString(s) => Some(s.clone()),
        Value::VerbatimString { text, .. } => Some(text.clone()),
        Value::Okay => Some("OK".into()),
        _ => None,
    }
}

fn bytes_of(v: &Value) -> Option<Vec<u8>> {
    match v {
        Value::BulkString(b) => Some(b.clone()),
        Value::SimpleString(s) => Some(s.as_bytes().to_vec()),
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// Errors

/// Error reply: `code` is its first word (ERR, WRONGTYPE, NOAUTH...),
/// `message` the rest.
fn reply_error(code: &str, detail: &str) -> AppError {
    let detail = detail.trim();
    let (kind, message) = match code {
        "MOVED" | "ASK" => (
            ErrorKind::Database,
            format!(
                "The key is served by another node ({detail}). Redis Cluster is not supported yet."
            ),
        ),
        // A blocking command interrupted by `cancel`.
        "UNBLOCKED" => (ErrorKind::Cancelled, "The command was interrupted.".into()),
        _ if detail.is_empty() => (ErrorKind::Database, code.to_string()),
        _ => (ErrorKind::Database, detail.to_string()),
    };
    let mut err = AppError::new(kind, message);
    err.code = Some(code.to_string());
    err
}

fn server_error(e: &ServerError) -> AppError {
    reply_error(e.code(), e.details().unwrap_or_default())
}

fn describe(e: &RedisError) -> String {
    let mut message = e.to_string();
    let mut source = std::error::Error::source(e);
    while let Some(s) = source {
        let text = s.to_string();
        if !message.contains(&text) {
            message.push_str(": ");
            message.push_str(&text);
        }
        source = s.source();
    }
    message
}

pub fn map_error(e: RedisError) -> AppError {
    use redis::ErrorKind as K;
    match e.kind() {
        // RESP2 AUTH failure; the library drops the server's code.
        K::AuthenticationFailed => {
            let mut err = AppError::connection("Invalid username or password.");
            err.code = Some("WRONGPASS".into());
            err
        }
        K::Server(_) | K::Extension => {
            reply_error(e.code().unwrap_or("ERR"), e.detail().unwrap_or_default())
        }
        K::Io | K::Parse => AppError::connection(describe(&e)),
        K::InvalidClientConfig => AppError::invalid(describe(&e)),
        _ => AppError::new(ErrorKind::Database, describe(&e)),
    }
}

fn is_auth_code(e: &AppError) -> bool {
    matches!(e.code.as_deref(), Some("NOAUTH" | "WRONGPASS"))
}

/// Errors while opening a session are connection errors; NOAUTH and
/// WRONGPASS keep their code.
fn as_connection(mut e: AppError) -> AppError {
    e.kind = ErrorKind::Connection;
    e
}

fn connect_error(e: RedisError) -> AppError {
    if e.is_timeout() {
        return AppError::connection("Timed out connecting to the server.");
    }
    as_connection(map_error(e))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    fn args(line: &str) -> Vec<String> {
        split_args(line)
            .unwrap()
            .into_iter()
            .map(|a| String::from_utf8(a).unwrap())
            .collect()
    }

    #[test]
    fn splits_like_redis_cli() {
        assert_eq!(args("  HGETALL   user:1 "), ["HGETALL", "user:1"]);
        assert_eq!(args(r#"SET k "a b\n""#), ["SET", "k", "a b\n"]);
        assert_eq!(
            args(r#"SET "q\"x\\" 'it\'s' "\t\r\a\b""#),
            ["SET", "q\"x\\", "it's", "\t\r\x07\x08"]
        );
        // Single quotes keep backslashes except before a quote.
        assert_eq!(args(r"ECHO 'a\nb'"), ["ECHO", r"a\nb"]);
        // Quotes inside a bare word join it.
        assert_eq!(args(r#"ECHO ab"c d""#), ["ECHO", "abc d"]);
        assert_eq!(args(r#"ECHO """#), ["ECHO", ""]);
        assert_eq!(args("ECHO ação"), ["ECHO", "ação"]);
        assert!(split_args("   \n ").unwrap().is_empty());

        let raw = split_args(r#"SET k "\xff\x41\xZZ""#).unwrap();
        assert_eq!(raw[2], b"\xffAxZZ");

        let e = split_args(r#"GET "abc"#).unwrap_err();
        assert_eq!(e.kind, ErrorKind::Invalid);
        assert_eq!(e.position, Some(4));
        assert_eq!(split_args("GET 'abc").unwrap_err().kind, ErrorKind::Invalid);
        assert_eq!(
            split_args(r#"GET "a\"#).unwrap_err().kind,
            ErrorKind::Invalid
        );
        let e = split_args(r#"GET "a"b"#).unwrap_err();
        assert_eq!(e.kind, ErrorKind::Invalid);
        assert_eq!(e.position, Some(6));
    }

    #[test]
    fn sends_raw_bytes() {
        let mut cmd = Cmd::new();
        for a in split_args(r#"SET k "\xff""#).unwrap() {
            cmd.arg(a.as_slice());
        }
        assert_eq!(
            cmd.get_packed_command(),
            b"*3\r\n$3\r\nSET\r\n$1\r\nk\r\n$1\r\n\xff\r\n"
        );
    }

    fn resp(bytes: &[u8]) -> Value {
        redis::parse_redis_value(bytes).unwrap()
    }

    fn table(
        name: &str,
        line: &str,
        value: Value,
        max_rows: usize,
    ) -> (Vec<ColumnInfo>, Vec<Vec<Option<String>>>, Grid) {
        let a = split_args(line).unwrap();
        let grid = tabulate(name, &a, value, max_rows);
        let (c, r) = grid.finish();
        (c, r, grid)
    }

    fn names(cols: &[ColumnInfo]) -> Vec<&str> {
        cols.iter().map(|c| c.name.as_str()).collect()
    }

    fn row(r: &[Option<String>]) -> Vec<Option<&str>> {
        r.iter().map(|c| c.as_deref()).collect()
    }

    #[test]
    fn formats_scalars() {
        let (c, r, _) = table("GET", "GET k", Value::BulkString(b"a b\n".to_vec()), 0);
        assert_eq!(names(&c), ["value"]);
        assert_eq!(c[0].kind, ValueKind::Text);
        assert_eq!(row(&r[0]), [Some("a b\n")]);

        let (c, r, _) = table("INCR", "INCR k", Value::Int(42), 0);
        assert_eq!(
            (c[0].kind, c[0].type_name.as_str()),
            (ValueKind::Number, "integer")
        );
        assert_eq!(row(&r[0]), [Some("42")]);

        let (c, r, _) = table("ZSCORE", "ZSCORE z m", resp(b",1.5\r\n"), 0);
        assert_eq!(c[0].kind, ValueKind::Number);
        assert_eq!(row(&r[0]), [Some("1.5")]);
        assert_eq!(double_text(3.0), "3");
        assert_eq!(double_text(f64::INFINITY), "inf");
        assert_eq!(double_text(1e300), "1e300");

        let (c, r, _) = table("X", "X", resp(b"#t\r\n"), 0);
        assert_eq!(c[0].kind, ValueKind::Bool);
        assert_eq!(row(&r[0]), [Some("true")]);

        let (_, r, _) = table("GET", "GET k", Value::Nil, 0);
        assert_eq!(row(&r[0]), [None]);
        let (_, r, _) = table("SET", "SET k v", Value::Okay, 0);
        assert_eq!(row(&r[0]), [Some("OK")]);
        let (_, r, _) = table(
            "X",
            "X",
            resp(b"(3492890328409238509324850943850943825024385\r\n"),
            0,
        );
        assert_eq!(
            row(&r[0]),
            [Some("3492890328409238509324850943850943825024385")]
        );
        let (_, r, _) = table("X", "X", resp(b"=15\r\ntxt:Some string\r\n"), 0);
        assert_eq!(row(&r[0]), [Some("Some string")]);
    }

    #[test]
    fn escapes_binary_strings() {
        assert_eq!(text(b"\xff\x00a\"\\"), r#""\xff\x00a\"\\""#);
        assert_eq!(text("ação\n".as_bytes()), "ação\n");
        assert_eq!(text(b"a\x00b"), r#""a\x00b""#);
        assert_eq!(repr(b"\x07\x08\t\r\n "), r#""\a\b\t\r\n ""#);
        let (_, r, _) = table("GET", "GET k", Value::BulkString(vec![0xff, b'a']), 0);
        assert_eq!(row(&r[0]), [Some(r#""\xffa""#)]);
    }

    #[test]
    fn formats_maps_and_arrays() {
        // RESP3 HGETALL.
        let map = resp(b"%2\r\n$2\r\nf1\r\n$2\r\nv1\r\n$2\r\nf2\r\n:7\r\n");
        let (c, r, _) = table("HGETALL", "HGETALL h", map.clone(), 0);
        assert_eq!(names(&c), ["field", "value"]);
        assert_eq!(c[1].type_name, "mixed");
        assert_eq!(row(&r[1]), [Some("f2"), Some("7")]);
        let (_, r, g) = table("HGETALL", "HGETALL h", map, 1);
        assert_eq!((r.len(), g.truncated), (1, true));

        // RESP2 HGETALL: a flat list.
        let flat = resp(b"*4\r\n$1\r\na\r\n$1\r\n1\r\n$1\r\nb\r\n$1\r\n2\r\n");
        let (c, r, _) = table("HGETALL", "HGETALL h", flat.clone(), 0);
        assert_eq!(names(&c), ["field", "value"]);
        assert_eq!(row(&r[0]), [Some("a"), Some("1")]);
        // Other commands: index and value.
        let (c, r, g) = table("LRANGE", "LRANGE l 0 -1", flat, 3);
        assert_eq!(names(&c), ["#", "value"]);
        assert_eq!(c[0].kind, ValueKind::Number);
        assert_eq!(row(&r[2]), [Some("3"), Some("b")]);
        assert!(g.truncated);

        // RESP3 ZRANGE WITHSCORES: [member, score] pairs.
        let z = resp(b"*2\r\n*2\r\n$1\r\na\r\n,1\r\n*2\r\n$1\r\nb\r\n,2.5\r\n");
        let (c, r, _) = table("ZRANGE", "ZRANGE z 0 -1 WITHSCORES", z.clone(), 0);
        assert_eq!(names(&c), ["member", "score"]);
        assert_eq!(c[1].kind, ValueKind::Number);
        assert_eq!(row(&r[1]), [Some("b"), Some("2.5")]);
        let (c, _, _) = table("LMPOP", "X", z, 0);
        assert_eq!(names(&c), ["1", "2"]);
        // RESP2 ZRANGE WITHSCORES: flat, scores as strings.
        let flat = resp(b"*2\r\n$1\r\na\r\n$1\r\n1\r\n");
        let (c, r, _) = table("ZRANGE", "ZRANGE z 0 -1 withscores", flat, 0);
        assert_eq!(names(&c), ["member", "score"]);
        assert_eq!(c[1].kind, ValueKind::Number);
        assert_eq!(row(&r[0]), [Some("a"), Some("1")]);

        // Nested values: compact JSON-like text.
        let nested = resp(b"*2\r\n*2\r\n$3\r\n1-0\r\n*2\r\n$1\r\nf\r\n$3\r\na\"b\r\n$1\r\nx\r\n");
        let (c, r, _) = table("XRANGE", "XRANGE s - +", nested, 0);
        assert_eq!(names(&c), ["#", "value"]);
        assert_eq!(c[1].kind, ValueKind::Text);
        assert_eq!(row(&r[0]), [Some("1"), Some(r#"["1-0",["f","a\"b"]]"#)]);
        let deep = resp(b"*1\r\n%1\r\n$1\r\nk\r\n~2\r\n_\r\n$1\r\n\xff\r\n");
        let (c, r, _) = table("X", "X", deep, 0);
        assert_eq!(c[1].kind, ValueKind::Json);
        assert_eq!(row(&r[0])[1], Some(r#"{"k":[null,"\xff"]}"#));

        // EXEC with an error inside.
        let exec = resp(b"*2\r\n+OK\r\n-WRONGTYPE Operation against a key\r\n");
        let (_, r, _) = table("EXEC", "EXEC", exec, 0);
        assert_eq!(
            row(&r[1]),
            [Some("2"), Some("(error) WRONGTYPE Operation against a key")]
        );

        // Empty list.
        let (c, r, _) = table("KEYS", "KEYS x*", Value::Array(vec![]), 0);
        assert_eq!(names(&c), ["#", "value"]);
        assert!(r.is_empty());
    }

    #[test]
    fn scan_replies_show_the_cursor() {
        let v = resp(b"*2\r\n$2\r\n17\r\n*4\r\n$1\r\nf\r\n$1\r\nv\r\n$1\r\ng\r\n$1\r\nw\r\n");
        let (c, r, g) = table("HSCAN", "HSCAN h 0", v.clone(), 0);
        assert_eq!(names(&c), ["field", "value"]);
        assert_eq!(r.len(), 2);
        assert_eq!(g.notices, ["Next cursor: 17"]);
        let (c, r, _) = table("SSCAN", "SSCAN s 0", v, 0);
        assert_eq!(names(&c), ["#", "value"]);
        assert_eq!(r.len(), 4);
    }

    #[test]
    fn parses_info() {
        let info = "# Server\r\nredis_version:7.4.1\r\nredis_mode:standalone\r\n\r\n# Keyspace\r\ndb0:keys=3,expires=0,avg_ttl=0\r\n";
        let (c, r, _) = table(
            "INFO",
            "INFO",
            Value::BulkString(info.as_bytes().to_vec()),
            0,
        );
        assert_eq!(names(&c), ["section", "key", "value"]);
        assert_eq!(r.len(), 3);
        assert_eq!(
            row(&r[0]),
            [Some("Server"), Some("redis_version"), Some("7.4.1")]
        );
        assert_eq!(
            row(&r[2]),
            [
                Some("Keyspace"),
                Some("db0"),
                Some("keys=3,expires=0,avg_ttl=0")
            ]
        );
        let (_, r, g) = table(
            "INFO",
            "INFO",
            Value::BulkString(info.as_bytes().to_vec()),
            2,
        );
        assert_eq!((r.len(), g.truncated), (2, true));

        assert_eq!(product_version(info), "Redis 7.4.1");
        assert_eq!(
            product_version(
                "redis_version:7.2.4\r\nserver_name:valkey\r\nvalkey_version:8.1.0\r\n"
            ),
            "Valkey 8.1.0"
        );
        assert_eq!(
            product_version("dragonfly_version:df-v1.27.1\r\n"),
            "Dragonfly 1.27.1"
        );
        assert_eq!(product_version(""), "Redis");
    }

    #[test]
    fn maps_errors_and_kinds() {
        let Value::ServerError(e) = resp(b"-WRONGTYPE Operation against a key\r\n") else {
            panic!()
        };
        let err = server_error(&e);
        assert_eq!(err.kind, ErrorKind::Database);
        assert_eq!(err.code.as_deref(), Some("WRONGTYPE"));
        assert_eq!(err.message, "Operation against a key");
        let Value::ServerError(e) = resp(b"-MOVED 3999 127.0.0.1:6381\r\n") else {
            panic!()
        };
        let err = server_error(&e);
        assert_eq!(err.code.as_deref(), Some("MOVED"));
        assert!(err.message.contains("Redis Cluster is not supported yet"));
        let Value::ServerError(e) = resp(b"-UNBLOCKED client unblocked via CLIENT UNBLOCK\r\n")
        else {
            panic!()
        };
        assert_eq!(server_error(&e).kind, ErrorKind::Cancelled);

        assert_eq!(key_kind("ReJSON-RL").as_deref(), Some("json"));
        assert_eq!(key_kind("zset").as_deref(), Some("zset"));
        assert_eq!(key_kind("TSDB-TYPE").as_deref(), Some("tsdb-type"));
        assert_eq!(key_kind("none"), None);

        assert_eq!(parse_db(""), Some(0));
        assert_eq!(parse_db(" 3 "), Some(3));
        assert_eq!(parse_db("db12"), Some(12));
        assert_eq!(parse_db("x"), None);
        assert_eq!(parse_db("-1"), None);

        assert_eq!(base64(b"Man"), "TWFu");
        assert_eq!(base64(b"Ma"), "TWE=");
        assert_eq!(base64(b"M"), "TQ==");
    }

    #[test]
    fn tracks_multi() {
        let mut st = State::default();
        let ok = |s: &str| Ok(Value::SimpleString(s.into()));
        let a = |l: &str| split_args(l).unwrap();
        track(&mut st, "MULTI", &a("MULTI"), &Ok(Value::Okay));
        assert!(st.in_multi);
        track(&mut st, "SELECT", &a("SELECT 4"), &ok("QUEUED"));
        assert_eq!((st.db, st.queued_select), (0, Some(4)));
        let err = resp(b"-ERR unknown command\r\n");
        track(&mut st, "NOPE", &a("NOPE"), &Ok(err.clone()));
        assert!(st.in_multi);
        track(
            &mut st,
            "EXEC",
            &a("EXEC"),
            &Ok(Value::Array(vec![Value::Okay])),
        );
        assert!(!st.in_multi);
        assert_eq!(st.db, 4);
        track(&mut st, "MULTI", &a("MULTI"), &Ok(Value::Okay));
        track(
            &mut st,
            "EXEC",
            &a("EXEC"),
            &Ok(resp(b"-EXECABORT Transaction discarded\r\n")),
        );
        assert!(!st.in_multi);
        track(&mut st, "SELECT", &a("SELECT 2"), &Ok(Value::Okay));
        assert_eq!(st.db, 2);
        track(&mut st, "SELECT", &a("SELECT 99"), &Ok(err));
        assert_eq!(st.db, 2);
    }

    // Integration tests: need a server. Variables: LQ_REDIS_HOST,
    // LQ_REDIS_PORT (6379) and LQ_REDIS_PASSWORD (optional).

    fn env(name: &str) -> Option<String> {
        std::env::var(name).ok().filter(|v| !v.is_empty())
    }

    fn profile() -> Option<(ConnectionProfile, Option<String>)> {
        let host = env("LQ_REDIS_HOST")?;
        Some((
            ConnectionProfile {
                name: "test".into(),
                engine: crate::model::Engine::Redis,
                host,
                port: env("LQ_REDIS_PORT")
                    .and_then(|p| p.parse().ok())
                    .unwrap_or(DEFAULT_PORT),
                ssl_mode: SslMode::Disable,
                ..Default::default()
            },
            env("LQ_REDIS_PASSWORD"),
        ))
    }

    async fn session() -> Option<RedisSession> {
        let (profile, password) = profile()?;
        Some(
            connect(&profile, password.as_deref())
                .await
                .expect("connect"),
        )
    }

    fn req(line: &str, max_rows: usize) -> ExecRequest {
        ExecRequest {
            sql: line.into(),
            max_rows,
        }
    }

    async fn run(s: &RedisSession, line: &str) -> ExecResult {
        s.execute(&req(line, 100))
            .await
            .unwrap_or_else(|e| panic!("{line}: {e}"))
    }

    fn first(r: &ExecResult) -> Option<&str> {
        r.rows[0][0].as_deref()
    }

    #[tokio::test]
    #[ignore]
    async fn connects_and_reports_version() {
        let Some(s) = session().await else { return };
        assert!(
            s.server_version.starts_with("Redis ") || s.server_version.starts_with("Valkey "),
            "{}",
            s.server_version
        );
        assert_eq!(s.current_schema.as_deref(), Some("db0"));
        assert!(s.current_user.is_some());
        let st = s.tx_state();
        assert!(st.autocommit && !st.in_transaction);
        assert!(run(&s, "# comment").await.columns.is_none());
        assert!(run(&s, "// comment").await.columns.is_none());
        assert_eq!(first(&run(&s, "PING").await), Some("PONG"));

        // `prefer` falls back to plain TCP on a server without TLS.
        let (mut profile, password) = profile().unwrap();
        profile.ssl_mode = SslMode::Prefer;
        profile.database = "2".into();
        let started = Instant::now();
        let p = connect(&profile, password.as_deref())
            .await
            .expect("prefer");
        assert!(started.elapsed() < Duration::from_secs(10));
        assert_eq!(p.current_schema.as_deref(), Some("db2"));

        // ACL user: right and wrong password.
        run(&s, "ACL SETUSER lq_test on >lq-secret ~* &* +@all").await;
        profile.ssl_mode = SslMode::Disable;
        profile.user = "lq_test".into();
        let u = connect(&profile, Some("lq-secret"))
            .await
            .expect("acl user");
        assert_eq!(u.current_user.as_deref(), Some("lq_test"));
        let e = match connect(&profile, Some("wrong")).await {
            Ok(_) => panic!("connected with a wrong password"),
            Err(e) => e,
        };
        assert_eq!(e.kind, ErrorKind::Connection);
        assert_eq!(e.code.as_deref(), Some("WRONGPASS"));
        run(&s, "ACL DELUSER lq_test").await;
    }

    #[tokio::test]
    #[ignore]
    async fn sets_and_gets_strings() {
        let Some(s) = session().await else { return };
        assert_eq!(
            first(&run(&s, r#"SET "lq:t:str key" "a b\n""#).await),
            Some("OK")
        );
        let r = run(&s, r#"GET "lq:t:str key""#).await;
        assert_eq!(first(&r), Some("a b\n"));
        assert_eq!(r.columns.as_ref().unwrap()[0].name, "value");
        run(&s, r#"SET lq:t:bin "\xff\x00ok""#).await;
        assert_eq!(
            first(&run(&s, "GET lq:t:bin").await),
            Some(r#""\xff\x00ok""#)
        );
        assert_eq!(first(&run(&s, "GET lq:t:missing").await), None);
        let r = run(&s, "INCRBY lq:t:n 5").await;
        assert_eq!(r.columns.unwrap()[0].kind, ValueKind::Number);
        run(&s, "DEL lq:t:n").await;

        let e = s.execute(&req(r#"GET "abc"#, 10)).await.unwrap_err();
        assert_eq!(e.kind, ErrorKind::Invalid);
    }

    #[tokio::test]
    #[ignore]
    async fn hashes_and_sorted_sets_have_columns() {
        let Some(s) = session().await else { return };
        run(&s, "DEL lq:t:h lq:t:z").await;
        run(&s, r#"HSET lq:t:h f1 v1 f2 "v 2""#).await;
        let r = run(&s, "HGETALL lq:t:h").await;
        let cols: Vec<String> = r.columns.unwrap().into_iter().map(|c| c.name).collect();
        assert_eq!(cols, ["field", "value"]);
        let mut rows: Vec<Vec<Option<String>>> = r.rows;
        rows.sort();
        assert_eq!(rows[1], [Some("f2".to_string()), Some("v 2".to_string())]);

        run(&s, "ZADD lq:t:z 1 a 2.5 b").await;
        let r = run(&s, "ZRANGE lq:t:z 0 -1 WITHSCORES").await;
        let cols = r.columns.unwrap();
        assert_eq!(cols[0].name, "member");
        assert_eq!(cols[1].name, "score");
        assert_eq!(cols[1].kind, ValueKind::Number);
        assert_eq!(r.rows[1], [Some("b".to_string()), Some("2.5".to_string())]);

        let e = s.execute(&req("LPUSH lq:t:h x", 10)).await.unwrap_err();
        assert_eq!(e.kind, ErrorKind::Database);
        assert_eq!(e.code.as_deref(), Some("WRONGTYPE"));
        run(&s, "DEL lq:t:h lq:t:z").await;
    }

    #[tokio::test]
    #[ignore]
    async fn limits_rows_and_parses_info() {
        let Some(s) = session().await else { return };
        run(&s, "DEL lq:t:list").await;
        let values: Vec<String> = (1..=50).map(|i| i.to_string()).collect();
        run(&s, &format!("RPUSH lq:t:list {}", values.join(" "))).await;
        let r = s.execute(&req("LRANGE lq:t:list 0 -1", 10)).await.unwrap();
        assert_eq!(r.rows.len(), 10);
        assert!(r.truncated);
        assert_eq!(r.rows[9], [Some("10".to_string()), Some("10".to_string())]);
        let r = s.execute(&req("LRANGE lq:t:list 0 -1", 0)).await.unwrap();
        assert_eq!(r.rows.len(), 50);
        assert!(!r.truncated);
        run(&s, "DEL lq:t:list").await;

        let r = run(&s, "INFO server").await;
        let cols: Vec<String> = r.columns.unwrap().into_iter().map(|c| c.name).collect();
        assert_eq!(cols, ["section", "key", "value"]);
        assert!(r
            .rows
            .iter()
            .any(|row| row[0].as_deref() == Some("Server")
                && row[1].as_deref() == Some("redis_version")));
    }

    #[tokio::test]
    #[ignore]
    async fn lists_databases_and_keys() {
        let Some(s) = session().await else { return };
        run(&s, "SET lq:t:home here").await;
        run(&s, "SELECT 9").await;
        for line in [
            "DEL lq:obj:str lq:obj:hash lq:obj:list lq:obj:set lq:obj:zset lq:obj:stream",
            "SET lq:obj:str v",
            "HSET lq:obj:hash f v",
            "RPUSH lq:obj:list a",
            "SADD lq:obj:set a",
            "ZADD lq:obj:zset 1 a",
            "XADD lq:obj:stream * f v",
        ] {
            run(&s, line).await;
        }
        let schemas = s.list_schemas().await.unwrap();
        assert_eq!(schemas[0], "db9");
        run(&s, "SELECT 0").await;
        assert_eq!(s.list_schemas().await.unwrap()[0], "db0");

        let objects = s.list_objects("db9").await.unwrap();
        let kind = |name: &str| {
            objects
                .iter()
                .find(|o| o.name == name)
                .map(|o| o.kind.as_str())
        };
        assert_eq!(kind("lq:obj:str"), Some("string"));
        assert_eq!(kind("lq:obj:hash"), Some("hash"));
        assert_eq!(kind("lq:obj:list"), Some("list"));
        assert_eq!(kind("lq:obj:set"), Some("set"));
        assert_eq!(kind("lq:obj:zset"), Some("zset"));
        assert_eq!(kind("lq:obj:stream"), Some("stream"));
        let sorted = objects
            .windows(2)
            .all(|w| (&w[0].kind, &w[0].name) <= (&w[1].kind, &w[1].name));
        assert!(sorted);
        // The session is back on its own database.
        assert_eq!(first(&run(&s, "GET lq:t:home").await), Some("here"));
        assert!(s
            .list_columns("db9", "lq:obj:str")
            .await
            .unwrap()
            .is_empty());
    }

    #[tokio::test]
    #[ignore]
    async fn tracks_transactions() {
        let Some(s) = session().await else { return };
        let e = s.set_autocommit(false).await.unwrap_err();
        assert_eq!(e.kind, ErrorKind::Invalid);
        assert!(s.set_autocommit(true).await.unwrap().autocommit);

        assert!(run(&s, "MULTI").await.in_transaction);
        let r = run(&s, "SET lq:t:tx 1").await;
        assert_eq!(first(&r), Some("QUEUED"));
        assert!(r.in_transaction && s.tx_state().in_transaction);
        let r = run(&s, "EXEC").await;
        assert!(!r.in_transaction && !s.tx_state().in_transaction);
        assert_eq!(r.rows[0][1].as_deref(), Some("OK"));

        run(&s, "MULTI").await;
        run(&s, "SET lq:t:tx 2").await;
        let st = s.end_transaction(false).await.unwrap();
        assert!(!st.in_transaction);
        assert_eq!(first(&run(&s, "GET lq:t:tx").await), Some("1"));

        run(&s, "MULTI").await;
        let e = s.execute(&req("NOSUCHCOMMAND", 10)).await.unwrap_err();
        assert_eq!(e.code.as_deref(), Some("ERR"));
        assert!(s.tx_state().in_transaction);
        let e = s.end_transaction(true).await.unwrap_err();
        assert_eq!(e.code.as_deref(), Some("EXECABORT"));
        assert!(!s.tx_state().in_transaction);
        run(&s, "DEL lq:t:tx").await;
    }

    #[tokio::test]
    #[ignore]
    async fn cancels_a_blocking_command() {
        let Some(s) = session().await else { return };
        let s = Arc::new(s);
        let started = Instant::now();
        let running = {
            let s = s.clone();
            tokio::spawn(async move { s.execute(&req("BLPOP lq:never 30", 10)).await })
        };
        tokio::time::sleep(Duration::from_secs(1)).await;
        s.cancel().await.unwrap();
        let e = running.await.unwrap().unwrap_err();
        assert_eq!(e.kind, ErrorKind::Cancelled);
        assert_eq!(e.code.as_deref(), Some("UNBLOCKED"));
        assert!(started.elapsed() < Duration::from_secs(15));
        assert_eq!(first(&run(&s, "PING").await), Some("PONG"));
        // Nothing running: nothing to do.
        s.cancel().await.unwrap();
    }
}
