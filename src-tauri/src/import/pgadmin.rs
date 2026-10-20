//! pgAdmin 4: the `servers.json` export (Tools > Import/Export Servers) and
//! the desktop configuration database `pgadmin4.db` (`server` and
//! `servergroup` tables), read without writing to it. Saved passwords are
//! encrypted with the pgAdmin master password and are not imported; a
//! `passfile` (pgpass format) set on a server is read.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::time::Duration;

use rusqlite::types::Value as SqlValue;
use rusqlite::{Connection, ErrorCode, OpenFlags};
use serde_json::{Map, Value};

use super::datagrip::{hex_color, invalid_reason, read_bytes, read_text};
use super::{jdbc, ImportCandidate, Parsed};
use crate::error::{AppError, AppResult};
use crate::model::{ConnectionProfile, Engine};

/// `DATA_DIR` of desktop mode in pgAdmin's `config.py`: `%APPDATA%\pgAdmin`
/// on Windows, `~/.pgadmin` elsewhere. `SQLITE_PATH` overrides the file.
pub fn default_files() -> Vec<(String, PathBuf)> {
    let mut candidates = Vec::new();
    if let Some(p) = std::env::var_os("SQLITE_PATH").filter(|p| !p.is_empty()) {
        candidates.push(PathBuf::from(p));
    }
    if cfg!(windows) {
        if let Some(dir) = super::config_dir() {
            candidates.push(dir.join("pgAdmin").join("pgadmin4.db"));
        }
    } else if let Some(home) = super::home() {
        candidates.push(home.join(".pgadmin").join("pgadmin4.db"));
    }
    let mut seen = HashSet::new();
    candidates
        .into_iter()
        .filter(|p| p.is_file())
        .filter(|p| {
            let id = std::fs::canonicalize(p).unwrap_or_else(|_| p.clone());
            seen.insert(id.to_string_lossy().to_lowercase())
        })
        .map(|p| ("pgAdmin 4".to_string(), p))
        .collect()
}

const NOT_OURS: &str = "This is not a pgAdmin 4 servers.json export or pgadmin4.db file.";

pub fn parse(path: &Path) -> AppResult<Parsed> {
    let mut head = [0u8; 16];
    let is_sqlite = {
        use std::io::Read;
        let mut f = std::fs::File::open(path)?;
        f.read(&mut head).map(|n| n == 16)? && &head == b"SQLite format 3\0"
    };
    let (servers, from_db) = if is_sqlite {
        (read_db(path)?, true)
    } else {
        (read_json(path)?, false)
    };
    let mut parsed = Parsed::default();
    for s in servers {
        convert(s, &mut parsed);
    }
    if parsed.candidates.len() > parsed.passwords.len() {
        parsed.notes.push(if from_db {
            "pgAdmin encrypts saved passwords with its master password, so LowQuery does not import them: type them on first connect.".into()
        } else {
            "pgAdmin exports do not include passwords: type them on first connect.".into()
        });
    }
    Ok(parsed)
}

/// One server as both formats describe it.
#[derive(Default)]
struct Server {
    key: String,
    name: String,
    group: Option<String>,
    host: Option<String>,
    port: Option<i64>,
    database: Option<String>,
    user: Option<String>,
    shared_user: Option<String>,
    role: Option<String>,
    bgcolor: Option<String>,
    service: Option<String>,
    ssh: bool,
    tunnel_host: Option<String>,
    kerberos: bool,
    passexec: Option<String>,
    post_sql: Option<String>,
    save_password: Option<bool>,
    /// libpq parameters (sslmode, hostaddr, passfile, sslcert...).
    params: Map<String, Value>,
}

fn read_json(path: &Path) -> AppResult<Vec<Server>> {
    let text = read_text(path)?;
    let root: Value = serde_json::from_str(&text).map_err(|_| AppError::invalid(NOT_OURS))?;
    let Some(map) = root.get("Servers").and_then(Value::as_object) else {
        return Err(AppError::invalid(NOT_OURS));
    };
    let mut out = Vec::new();
    for (id, s) in map {
        let Some(o) = s.as_object() else {
            continue;
        };
        let text = |k: &str| o.get(k).and_then(value_text);
        let mut params = o
            .get("ConnectionParameters")
            .and_then(Value::as_object)
            .cloned()
            .unwrap_or_default();
        // Before pgAdmin 6.0 libpq settings were top-level keys.
        for (old, new) in [
            ("SSLMode", "sslmode"),
            ("HostAddr", "hostaddr"),
            ("PassFile", "passfile"),
            ("SSLCert", "sslcert"),
            ("SSLKey", "sslkey"),
            ("SSLRootCert", "sslrootcert"),
        ] {
            if let Some(v) = text(old) {
                params.entry(new).or_insert(Value::String(v));
            }
        }
        out.push(Server {
            key: format!("server-{id}"),
            name: text("Name").unwrap_or_default(),
            group: text("Group"),
            host: text("Host"),
            port: o.get("Port").and_then(value_int),
            database: text("MaintenanceDB"),
            user: text("Username"),
            shared_user: text("SharedUsername"),
            role: text("Role"),
            bgcolor: text("BGColor"),
            service: text("Service"),
            ssh: o.get("UseSSHTunnel").is_some_and(truthy),
            tunnel_host: text("TunnelHost"),
            kerberos: o.get("KerberosAuthentication").is_some_and(truthy),
            passexec: text("PasswordExecCommand"),
            post_sql: text("PostConnectionSQL"),
            save_password: None,
            params,
        });
    }
    Ok(out)
}

fn value_text(v: &Value) -> Option<String> {
    let s = match v {
        Value::String(s) => s.trim().to_string(),
        Value::Number(n) => n.to_string(),
        Value::Bool(b) => b.to_string(),
        _ => return None,
    };
    (!s.is_empty()).then_some(s)
}

fn value_int(v: &Value) -> Option<i64> {
    match v {
        Value::Number(n) => n.as_i64(),
        Value::String(s) => s.trim().parse().ok(),
        _ => None,
    }
}

fn truthy(v: &Value) -> bool {
    match v {
        Value::Bool(b) => *b,
        Value::Number(n) => n.as_i64().is_some_and(|n| n != 0),
        Value::String(s) => matches!(s.trim().to_ascii_lowercase().as_str(), "1" | "true"),
        _ => false,
    }
}

/// Columns read from `server`; missing ones (older schemas) read as NULL.
const COLUMNS: &[&str] = &[
    "id",
    "servergroup_id",
    "name",
    "host",
    "port",
    "maintenance_db",
    "username",
    "shared_username",
    "role",
    "bgcolor",
    "service",
    "use_ssh_tunnel",
    "tunnel_host",
    "kerberos_conn",
    "passexec_cmd",
    "post_connection_sql",
    "save_password",
    "connection_params",
    "is_adhoc",
    // Before connection_params (schema 30, pgAdmin 6.0).
    "ssl_mode",
    "hostaddr",
    "passfile",
    "sslcert",
    "sslkey",
    "sslrootcert",
];

fn read_db(path: &Path) -> AppResult<Vec<Server>> {
    let failed = |e: rusqlite::Error| match e {
        rusqlite::Error::SqliteFailure(f, _) if f.code == ErrorCode::NotADatabase => {
            AppError::invalid(NOT_OURS)
        }
        e => AppError::internal(format!("pgAdmin's database could not be read: {e}")),
    };
    let first = open_db(path, false).and_then(|c| query_db(&c));
    let rows = match first {
        // A running pgAdmin may hold a write lock: read the file as it is.
        Err(e) if is_busy(&e) => open_db(path, true).and_then(|c| query_db(&c)),
        other => other,
    }
    .map_err(failed)?;
    rows.ok_or_else(|| AppError::invalid(NOT_OURS))
}

fn is_busy(e: &rusqlite::Error) -> bool {
    matches!(e, rusqlite::Error::SqliteFailure(f, _)
        if matches!(f.code, ErrorCode::DatabaseBusy | ErrorCode::DatabaseLocked))
}

fn open_db(path: &Path, immutable: bool) -> rusqlite::Result<Connection> {
    let flags = OpenFlags::SQLITE_OPEN_READ_ONLY
        | OpenFlags::SQLITE_OPEN_URI
        | OpenFlags::SQLITE_OPEN_NO_MUTEX;
    let conn = if immutable {
        Connection::open_with_flags(file_uri(path, "immutable=1"), flags)?
    } else {
        Connection::open_with_flags(path, flags)?
    };
    conn.busy_timeout(Duration::from_millis(500))?;
    Ok(conn)
}

/// `file:` URI for SQLite with the reserved characters escaped.
fn file_uri(path: &Path, query: &str) -> String {
    let mut s = String::from("file:");
    let p = path.to_string_lossy().replace('\\', "/");
    if !p.starts_with('/') {
        s.push_str("///");
    } else {
        s.push_str("//");
    }
    for c in p.chars() {
        match c {
            '%' => s.push_str("%25"),
            '?' => s.push_str("%3f"),
            '#' => s.push_str("%23"),
            ' ' => s.push_str("%20"),
            c => s.push(c),
        }
    }
    s.push('?');
    s.push_str(query);
    s
}

/// `None` when the database has no pgAdmin `server` table.
fn query_db(conn: &Connection) -> rusqlite::Result<Option<Vec<Server>>> {
    let mut stmt = conn.prepare("SELECT name FROM pragma_table_info('server')")?;
    let present: HashSet<String> = stmt
        .query_map([], |r| r.get::<_, String>(0))?
        .collect::<Result<_, _>>()?;
    if !present.contains("name") || !present.contains("id") {
        return Ok(None);
    }
    let groups: HashMap<i64, String> = {
        let has_groups = conn
            .prepare("SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = 'servergroup'")?
            .exists([])?;
        if has_groups {
            let mut stmt = conn.prepare("SELECT id, name FROM servergroup")?;
            let rows = stmt.query_map([], |r| {
                Ok((r.get::<_, SqlValue>(0)?, r.get::<_, SqlValue>(1)?))
            })?;
            rows.filter_map(Result::ok)
                .filter_map(|(id, name)| Some((sql_int(&id)?, sql_text(&name)?)))
                .collect()
        } else {
            HashMap::new()
        }
    };
    let select = COLUMNS
        .iter()
        .map(|c| {
            if present.contains(*c) {
                format!("\"{c}\"")
            } else {
                "NULL".into()
            }
        })
        .collect::<Vec<_>>()
        .join(", ");
    let mut stmt = conn.prepare(&format!("SELECT {select} FROM server ORDER BY id"))?;
    let rows = stmt.query_map([], |r| {
        (0..COLUMNS.len())
            .map(|i| r.get::<_, SqlValue>(i))
            .collect::<rusqlite::Result<Vec<_>>>()
    })?;
    let mut out = Vec::new();
    for row in rows {
        let row = row?;
        let col = |name: &str| {
            let i = COLUMNS.iter().position(|c| *c == name).unwrap_or(0);
            &row[i]
        };
        let text = |name: &str| sql_text(col(name));
        let flag = |name: &str| {
            sql_int(col(name)).is_some_and(|v| v != 0)
                || text(name).is_some_and(|t| t.eq_ignore_ascii_case("true"))
        };
        // Ad hoc servers come from the Workspace and are not in the tree.
        if flag("is_adhoc") {
            continue;
        }
        let Some(id) = sql_int(col("id")) else {
            continue;
        };
        let mut params = text("connection_params")
            .and_then(|t| serde_json::from_str::<Value>(&t).ok())
            .and_then(|v| v.as_object().cloned())
            .unwrap_or_default();
        for (old, new) in [
            ("ssl_mode", "sslmode"),
            ("hostaddr", "hostaddr"),
            ("passfile", "passfile"),
            ("sslcert", "sslcert"),
            ("sslkey", "sslkey"),
            ("sslrootcert", "sslrootcert"),
        ] {
            if let Some(v) = text(old) {
                params.entry(new).or_insert(Value::String(v));
            }
        }
        out.push(Server {
            key: format!("server-{id}"),
            name: text("name").unwrap_or_default(),
            group: sql_int(col("servergroup_id")).and_then(|g| groups.get(&g).cloned()),
            host: text("host"),
            port: sql_int(col("port")),
            database: text("maintenance_db"),
            user: text("username"),
            shared_user: text("shared_username"),
            role: text("role"),
            bgcolor: text("bgcolor"),
            service: text("service"),
            ssh: flag("use_ssh_tunnel"),
            tunnel_host: text("tunnel_host"),
            kerberos: flag("kerberos_conn"),
            passexec: text("passexec_cmd"),
            post_sql: text("post_connection_sql"),
            save_password: Some(flag("save_password")),
            params,
        });
    }
    Ok(Some(out))
}

fn sql_text(v: &SqlValue) -> Option<String> {
    let s = match v {
        SqlValue::Text(s) => s.trim().to_string(),
        SqlValue::Integer(i) => i.to_string(),
        SqlValue::Blob(b) => String::from_utf8_lossy(b).trim().to_string(),
        _ => return None,
    };
    (!s.is_empty()).then_some(s)
}

fn sql_int(v: &SqlValue) -> Option<i64> {
    match v {
        SqlValue::Integer(i) => Some(*i),
        SqlValue::Real(r) => Some(*r as i64),
        SqlValue::Text(s) => s.trim().parse().ok(),
        _ => None,
    }
}

fn param(s: &Server, k: &str) -> Option<String> {
    s.params.get(k).and_then(value_text)
}

fn convert(s: Server, parsed: &mut Parsed) {
    let name = if s.name.is_empty() {
        s.host.clone().unwrap_or_else(|| s.key.clone())
    } else {
        s.name.clone()
    };
    let hostaddr = param(&s, "hostaddr");
    let Some(hosts) = s.host.clone().or(hostaddr) else {
        let reason = match &s.service {
            Some(svc) => {
                format!("Uses the pg_service.conf service {svc}, which LowQuery cannot read.")
            }
            None => "Has no host.".into(),
        };
        parsed.skip(name, reason);
        return;
    };
    let mut warnings = Vec::new();
    // libpq accepts a comma-separated list of hosts and ports.
    let host = hosts
        .split(',')
        .next()
        .unwrap_or_default()
        .trim()
        .to_string();
    if hosts.contains(',') {
        warnings.push(format!("Only the first host ({host}) is used."));
    }
    let port = s
        .port
        .and_then(|p| u16::try_from(p).ok())
        .filter(|p| *p != 0)
        .unwrap_or(5432);
    let mut p = ConnectionProfile {
        name: name.clone(),
        engine: Engine::Postgres,
        host,
        port,
        database: s.database.clone().unwrap_or_else(|| "postgres".into()),
        user: s
            .user
            .clone()
            .or_else(|| s.shared_user.clone())
            .unwrap_or_default(),
        color: s.bgcolor.as_deref().and_then(hex_color),
        ..ConnectionProfile::default()
    };
    if let Some(mode) = param(&s, "sslmode").and_then(|m| jdbc::pg_ssl_mode(&m)) {
        p.ssl_mode = mode;
    }
    if let Some(save) = s.save_password {
        p.save_password = save;
    }

    if let Some(svc) = &s.service {
        warnings.push(format!("The pg_service.conf service {svc} is not used."));
    }
    if s.ssh {
        warnings.push(match &s.tunnel_host {
            Some(h) => format!("The SSH tunnel through {h} is not carried over."),
            None => "The SSH tunnel is not carried over.".into(),
        });
    }
    if s.kerberos {
        warnings.push("Kerberos authentication is not carried over.".into());
    }
    if s.passexec.is_some() {
        warnings.push("The password exec command is not carried over.".into());
    }
    if s.post_sql.is_some() {
        warnings.push("The post-connection SQL is not carried over.".into());
    }
    if let Some(role) = &s.role {
        warnings.push(format!("The role {role} is not set after connecting."));
    }
    if param(&s, "sslcert").is_some() || param(&s, "sslkey").is_some() {
        warnings.push("The SSL client certificate is not carried over.".into());
    }
    if let Some(ca) = param(&s, "sslrootcert") {
        warnings.push(format!("The CA certificate {ca} is not carried over."));
    }
    if let Some(options) = param(&s, "options") {
        warnings.push(format!(
            "The connection options {options} are not carried over."
        ));
    }
    if let Some(file) = param(&s, "passfile") {
        let file = PathBuf::from(file);
        if file.is_absolute() {
            if let Some(pw) = pgpass_password(&file, &p.host, p.port, &p.database, &p.user) {
                parsed.passwords.insert(s.key.clone(), pw);
            }
        }
    }
    if let Some(reason) = invalid_reason(&p) {
        parsed.passwords.remove(&s.key);
        parsed.skip(name, reason);
        return;
    }
    let folder = s.group.filter(|g| g != "Servers");
    parsed.candidates.push(ImportCandidate {
        key: s.key,
        profile: p,
        folder,
        has_password: false,
        warnings,
    });
}

/// The libpq password file: `PGPASSFILE`, else `%APPDATA%\postgresql\pgpass.conf`
/// on Windows and `~/.pgpass` elsewhere.
pub(super) fn default_pgpass() -> Option<PathBuf> {
    if let Some(p) = std::env::var_os("PGPASSFILE").filter(|p| !p.is_empty()) {
        return Some(PathBuf::from(p));
    }
    if cfg!(windows) {
        super::config_dir().map(|d| d.join("postgresql").join("pgpass.conf"))
    } else {
        super::home().map(|h| h.join(".pgpass"))
    }
}

/// First `hostname:port:database:username:password` line of a pgpass file
/// that matches (`*` matches anything, `\` escapes `:` and `\`).
pub(super) fn pgpass_password(
    file: &Path,
    host: &str,
    port: u16,
    database: &str,
    user: &str,
) -> Option<String> {
    if user.is_empty() {
        return None;
    }
    let bytes = read_bytes(file).ok()?;
    let text = String::from_utf8_lossy(&bytes);
    let port = port.to_string();
    for line in text.lines() {
        let line = line.trim_end_matches('\r');
        if line.trim().is_empty() || line.trim_start().starts_with('#') {
            continue;
        }
        let fields = pgpass_fields(line);
        let [h, pt, db, u, pw, ..] = &fields[..] else {
            continue;
        };
        let m = |pattern: &str, value: &str| pattern == "*" || pattern == value;
        if m(h, host) && m(pt, &port) && m(db, database) && m(u, user) {
            return (!pw.is_empty()).then(|| pw.clone());
        }
    }
    None
}

fn pgpass_fields(line: &str) -> Vec<String> {
    let mut fields = vec![String::new()];
    let mut chars = line.chars();
    while let Some(c) = chars.next() {
        match c {
            '\\' => {
                if let Some(next) = chars.next() {
                    fields.last_mut().unwrap().push(next);
                }
            }
            ':' => fields.push(String::new()),
            c => fields.last_mut().unwrap().push(c),
        }
    }
    fields
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::SslMode;

    struct TempDir(PathBuf);

    impl TempDir {
        fn new(tag: &str) -> Self {
            let dir = std::env::temp_dir().join(format!(
                "lowquery-pgadmin-{tag}-{}-{}",
                std::process::id(),
                uuid::Uuid::new_v4().simple()
            ));
            std::fs::create_dir_all(&dir).unwrap();
            Self(dir)
        }

        fn write(&self, rel: &str, text: &str) -> PathBuf {
            let path = self.0.join(rel);
            std::fs::write(&path, text).unwrap();
            path
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn find<'a>(p: &'a Parsed, key: &str) -> &'a ImportCandidate {
        p.candidates
            .iter()
            .find(|c| c.key == key)
            .unwrap_or_else(|| panic!("missing {key}"))
    }

    fn json_with_passfile(passfile: &str) -> String {
        let passfile = passfile.replace('\\', "\\\\");
        format!(
            r##"{{
  "Servers": {{
    "1": {{
      "Name": "ERP production",
      "Group": "Production/ERP",
      "Host": "erp-db.internal",
      "Port": 5433,
      "MaintenanceDB": "erp",
      "Username": "erp_app",
      "Role": "erp_owner",
      "Comment": "main",
      "BGColor": "#DC2626",
      "FGColor": "#FFFFFF",
      "UseSSHTunnel": 1,
      "TunnelHost": "bastion.example.com",
      "TunnelPort": "22",
      "TunnelUsername": "ops",
      "TunnelAuthentication": 1,
      "TunnelIdentityFile": "/home/ana/.ssh/id_ed25519",
      "KerberosAuthentication": false,
      "ConnectionParameters": {{
        "sslmode": "verify-full",
        "connect_timeout": 10,
        "sslrootcert": "/etc/ssl/erp-ca.pem",
        "passfile": "{passfile}"
      }},
      "Tags": [{{"color": "#000", "text": "prod"}}],
      "PostConnectionSQL": "set search_path to erp"
    }},
    "2": {{
      "Name": "Local",
      "Group": "Servers",
      "Host": "localhost",
      "Port": "5432",
      "MaintenanceDB": "postgres",
      "Username": "postgres",
      "SSLMode": "disable",
      "PassFile": "{passfile}"
    }},
    "3": {{
      "Name": "From service",
      "Group": "Servers",
      "Port": 5432,
      "MaintenanceDB": "postgres",
      "Username": "x",
      "Service": "erp"
    }},
    "4": {{
      "Name": "Kerberos",
      "Group": "Servers",
      "Host": "a.example.com,b.example.com",
      "Port": 5432,
      "MaintenanceDB": "postgres",
      "Shared": true,
      "SharedUsername": "ana",
      "KerberosAuthentication": true,
      "PasswordExecCommand": "vault read pg",
      "ConnectionParameters": {{"sslmode": "require", "sslcert": "/c.crt", "sslkey": "/c.key", "options": "-c statement_timeout=0"}}
    }},
    "5": "not an object"
  }}
}}"##
        )
    }

    #[test]
    fn servers_json_export() {
        let dir = TempDir::new("json");
        let pass = dir.write(
            "pgpass",
            "localhost:5432:*:postgres:fake-pass-1\nerp-db.internal:5433:erp:erp_app:fake\\:pass-2\n",
        );
        let path = dir.write("servers.json", &json_with_passfile(&pass.to_string_lossy()));
        let parsed = parse(&path).unwrap();
        assert_eq!(parsed.candidates.len(), 3);

        let erp = find(&parsed, "server-1");
        assert_eq!(erp.profile.engine, Engine::Postgres);
        assert_eq!(
            (erp.profile.host.as_str(), erp.profile.port),
            ("erp-db.internal", 5433)
        );
        assert_eq!(erp.profile.database, "erp");
        assert_eq!(erp.profile.user, "erp_app");
        assert_eq!(erp.profile.ssl_mode, SslMode::VerifyFull);
        assert_eq!(erp.profile.color.as_deref(), Some("#dc2626"));
        assert_eq!(erp.folder.as_deref(), Some("Production/ERP"));
        assert_eq!(erp.warnings.len(), 4, "{:?}", erp.warnings);
        assert!(erp
            .warnings
            .iter()
            .any(|w| w.contains("bastion.example.com")));
        assert!(erp.warnings.iter().any(|w| w.contains("erp_owner")));
        assert!(erp.warnings.iter().any(|w| w.contains("erp-ca.pem")));
        assert!(erp.warnings.iter().any(|w| w.contains("post-connection")));
        assert_eq!(
            parsed.passwords.get("server-1").map(String::as_str),
            Some("fake:pass-2")
        );

        let local = find(&parsed, "server-2");
        assert_eq!(local.profile.port, 5432);
        assert_eq!(local.profile.ssl_mode, SslMode::Disable);
        assert_eq!(local.folder, None);
        assert!(local.warnings.is_empty());
        assert_eq!(
            parsed.passwords.get("server-2").map(String::as_str),
            Some("fake-pass-1")
        );

        let krb = find(&parsed, "server-4");
        assert_eq!(krb.profile.host, "a.example.com");
        assert_eq!(krb.profile.user, "ana");
        assert_eq!(krb.profile.ssl_mode, SslMode::Require);
        assert_eq!(krb.warnings.len(), 5, "{:?}", krb.warnings);
        assert!(!parsed.passwords.contains_key("server-4"));

        assert_eq!(parsed.skipped.len(), 1);
        assert!(parsed.skipped[0].reason.contains("pg_service.conf"));
        assert_eq!(parsed.notes.len(), 1);
        assert!(parsed.notes[0].contains("do not include passwords"));
    }

    const SCHEMA: &str = r#"
CREATE TABLE version (name VARCHAR(32) NOT NULL PRIMARY KEY, value INTEGER NOT NULL);
CREATE TABLE servergroup (id INTEGER NOT NULL PRIMARY KEY, user_id INTEGER NOT NULL, name VARCHAR(128) NOT NULL);
CREATE TABLE server (
    id INTEGER NOT NULL PRIMARY KEY AUTOINCREMENT, user_id INTEGER NOT NULL, servergroup_id INTEGER NOT NULL,
    name VARCHAR(128) NOT NULL, host VARCHAR(128), port INTEGER, maintenance_db VARCHAR(64), username VARCHAR(64),
    comment VARCHAR(1024), password VARCHAR, role VARCHAR(64), discovery_id VARCHAR, db_res VARCHAR,
    bgcolor VARCHAR(10), fgcolor VARCHAR(10), service VARCHAR, use_ssh_tunnel INTEGER DEFAULT '0',
    tunnel_host VARCHAR, tunnel_port VARCHAR, tunnel_username VARCHAR, tunnel_authentication INTEGER DEFAULT '0',
    tunnel_identity_file VARCHAR, tunnel_password VARCHAR, save_password INTEGER DEFAULT '0', shared BOOLEAN,
    kerberos_conn BOOLEAN DEFAULT 'false', cloud_status INTEGER DEFAULT '0', passexec_cmd VARCHAR,
    passexec_expiration INTEGER, connection_params JSON, shared_username VARCHAR(64), prepare_threshold INTEGER,
    tunnel_keep_alive INTEGER DEFAULT '0', tags JSON, is_adhoc INTEGER DEFAULT '0', post_connection_sql VARCHAR,
    db_res_type VARCHAR(32) DEFAULT 'databases', tunnel_prompt_password INTEGER DEFAULT '0');
INSERT INTO version VALUES ('ConfigDB', 49);
INSERT INTO servergroup VALUES (1, 1, 'Servers'), (2, 1, 'Production');
INSERT INTO server (id, user_id, servergroup_id, name, host, port, maintenance_db, username, password, bgcolor,
    use_ssh_tunnel, tunnel_host, save_password, kerberos_conn, connection_params, is_adhoc)
VALUES
    (1, 1, 2, 'Billing', 'billing.internal', 5434, 'billing', 'bill', 'ENCRYPTED', '#16A34A', 0, NULL, 1, 0,
     '{"sslmode": "require", "connect_timeout": 10}', 0),
    (2, 1, 1, 'Dev', 'localhost', 5432, 'postgres', 'postgres', NULL, NULL, 1, 'jump.local', 0, 1,
     '{"sslmode": "prefer"}', 0),
    (3, 1, 1, 'Ad hoc', 'adhoc.local', 5432, 'postgres', 'x', NULL, NULL, 0, NULL, 0, 0, NULL, 1),
    (4, 1, 9, 'Odd', 'odd.local', 99999, NULL, NULL, NULL, 'blue', 0, NULL, 0, 0, 'not json', 0);
"#;

    #[test]
    fn pgadmin4_db() {
        let dir = TempDir::new("db");
        let path = dir.0.join("pgadmin4.db");
        let conn = Connection::open(&path).unwrap();
        conn.execute_batch(SCHEMA).unwrap();
        drop(conn);

        let parsed = parse(&path).unwrap();
        assert_eq!(parsed.candidates.len(), 3);
        let billing = find(&parsed, "server-1");
        assert_eq!(
            (billing.profile.host.as_str(), billing.profile.port),
            ("billing.internal", 5434)
        );
        assert_eq!(billing.profile.database, "billing");
        assert_eq!(billing.profile.user, "bill");
        assert_eq!(billing.profile.ssl_mode, SslMode::Require);
        assert_eq!(billing.profile.color.as_deref(), Some("#16a34a"));
        assert_eq!(billing.folder.as_deref(), Some("Production"));
        assert!(billing.profile.save_password);
        assert!(billing.warnings.is_empty());

        let dev = find(&parsed, "server-2");
        assert_eq!(dev.folder, None);
        assert!(!dev.profile.save_password);
        assert_eq!(dev.warnings.len(), 2, "{:?}", dev.warnings);

        let odd = find(&parsed, "server-4");
        assert_eq!(odd.profile.port, 5432);
        assert_eq!(odd.profile.database, "postgres");
        assert_eq!(odd.profile.color, None);
        assert_eq!(odd.folder, None);

        assert!(parsed.passwords.is_empty());
        assert_eq!(parsed.notes.len(), 1);
        assert!(parsed.notes[0].contains("master password"));
    }

    #[test]
    fn locked_and_legacy_db() {
        let dir = TempDir::new("locked");
        let path = dir.0.join("pgadmin4.db");
        let conn = Connection::open(&path).unwrap();
        conn.execute_batch(
            "CREATE TABLE servergroup (id INTEGER PRIMARY KEY, user_id INTEGER, name TEXT);
             CREATE TABLE server (id INTEGER PRIMARY KEY, user_id INTEGER, servergroup_id INTEGER,
                 name TEXT NOT NULL, host TEXT, port INTEGER, maintenance_db TEXT, username TEXT,
                 ssl_mode TEXT, hostaddr TEXT, sslcert TEXT, sslkey TEXT, sslrootcert TEXT);
             INSERT INTO servergroup VALUES (1, 1, 'Old servers');
             INSERT INTO server VALUES (7, 1, 1, 'Legacy', NULL, 5432, 'app', 'app', 'verify-full',
                 '10.0.0.5', '/c.crt', '/c.key', NULL);",
        )
        .unwrap();
        // Another process in the middle of a write.
        conn.execute_batch("BEGIN EXCLUSIVE; UPDATE server SET name = 'Changing';")
            .unwrap();
        let parsed = parse(&path).unwrap();
        conn.execute_batch("ROLLBACK").unwrap();
        drop(conn);
        let c = find(&parsed, "server-7");
        assert_eq!(c.profile.name, "Legacy");
        assert_eq!(c.profile.host, "10.0.0.5");
        assert_eq!(c.profile.ssl_mode, SslMode::VerifyFull);
        assert_eq!(c.folder.as_deref(), Some("Old servers"));
        assert_eq!(c.warnings.len(), 1);
    }

    #[test]
    fn rejects_other_files() {
        let dir = TempDir::new("bad");
        for (name, text) in [
            ("empty.json", ""),
            ("array.json", "[1, 2]"),
            ("other.json", "{\"connections\": []}"),
            ("servers.txt", "Servers"),
        ] {
            let path = dir.write(name, text);
            let err = parse(&path).expect_err(name);
            assert_eq!(err.message, NOT_OURS, "{name}");
        }
        let empty = dir.write("ok.json", "{\"Servers\": {}}");
        let parsed = parse(&empty).unwrap();
        assert!(parsed.candidates.is_empty() && parsed.notes.is_empty());

        let other_db = dir.0.join("other.db");
        Connection::open(&other_db)
            .unwrap()
            .execute_batch("CREATE TABLE t (x INTEGER);")
            .unwrap();
        assert_eq!(parse(&other_db).unwrap_err().message, NOT_OURS);
        let mut fake = b"SQLite format 3\0".to_vec();
        fake.extend(std::iter::repeat_n(0xAB, 4096));
        let fake_db = dir.0.join("fake.db");
        std::fs::write(&fake_db, fake).unwrap();
        assert!(parse(&fake_db).is_err());
        assert!(parse(&dir.0.join("missing.db")).is_err());
    }

    #[test]
    fn uri_and_pgpass_details() {
        assert_eq!(
            file_uri(Path::new("C:\\Users\\a b\\pg#1.db"), "immutable=1"),
            "file:///C:/Users/a%20b/pg%231.db?immutable=1"
        );
        assert_eq!(
            file_uri(Path::new("/home/a/.pgadmin/pgadmin4.db"), "immutable=1"),
            "file:///home/a/.pgadmin/pgadmin4.db?immutable=1"
        );
        assert_eq!(
            pgpass_fields("h:5432:d\\:x:u:p\\\\w"),
            vec!["h", "5432", "d:x", "u", "p\\w"]
        );
        let _ = default_files();
    }
}
