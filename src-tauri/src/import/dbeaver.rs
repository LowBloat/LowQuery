//! DBeaver (Community and PRO). Each project keeps its connections in
//! `<workspace6>/<project>/.dbeaver/data-sources*.json`; user names and saved
//! passwords live next to it in `credentials-config*.json`, encrypted with
//! DBeaver's built-in key (AES-128-CBC, the first 16 bytes are the IV).
//!
//! Key names follow DBeaver's `DataSourceSerializerModern`,
//! `RegistryConstants` and the Oracle, SQL Server, PostgreSQL and MySQL
//! plugin constants.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use serde_json::{Map, Value};

use super::{jdbc, ImportCandidate, Parsed};
use crate::error::{AppError, AppResult};
use crate::model::{ConnectionProfile, Engine, OracleConnectType, OraclePrivilege, SslMode};

type Obj = Map<String, Value>;

/// `BaseProjectImpl.LOCAL_KEY_CACHE` in DBeaver.
const CREDENTIALS_KEY: [u8; 16] = [
    0xba, 0xbb, 0x4a, 0x9f, 0x77, 0x4a, 0xb8, 0x53, 0xc9, 0x6c, 0x2d, 0x65, 0x3d, 0xfe, 0x54, 0x4a,
];
const MAX_FILE_BYTES: u64 = 32 * 1024 * 1024;
const RED: &str = "#dc2626";
const GREEN: &str = "#16a34a";

// Provider properties (`DBConstants.INTERNAL_PROP_PREFIX` is "@dbeaver-").
const ORA_CONNECTION_TYPE: &str = "@dbeaver-connection-type@";
const ORA_SID_SERVICE: &str = "@dbeaver-sid-service@";
const ORA_TNS_PATH: &str = "@dbeaver-tns-path@";
const ORA_INTERNAL_LOGON: &str = "@dbeaver-internal-logon@";
const ORA_LOGON_AS: &str = "oracle.logon-as";
const ORA_OS_AUTH: &str = "@dbeaver-os-authentication@";
const MSSQL_WINDOWS_AUTH: &str = "@dbeaver-connection-windows-auth@";

/// Driver properties that do not change how LowQuery connects.
const HARMLESS_PROPERTIES: &[&str] = &[
    "allowPublicKeyRetrieval",
    "ApplicationName",
    "applicationName",
    "connectTimeout",
    "loginTimeout",
    "socketTimeout",
    "tcpKeepAlive",
    "characterEncoding",
    "useUnicode",
];

pub fn default_files() -> Vec<(String, PathBuf)> {
    let mut seen = HashSet::new();
    let mut out = Vec::new();
    for (variant, root) in data_roots() {
        let workspace = root.join("workspace6");
        if seen.insert(workspace.clone()) {
            out.extend(workspace_files(&workspace, variant));
        }
    }
    out
}

/// `DBeaverData` folders: `%APPDATA%` on Windows, `~/Library` on macOS,
/// `$XDG_DATA_HOME` or `~/.local/share` on Linux, plus Snap and Flatpak.
fn data_roots() -> Vec<(&'static str, PathBuf)> {
    let mut roots = Vec::new();
    if let Some(data) = super::data_dir() {
        roots.push(("", data.join("DBeaverData")));
    }
    if let Some(home) = super::home() {
        let under = |parts: &[&str]| parts.iter().fold(home.clone(), |p, s| p.join(s));
        roots.push(("", under(&["Library", "DBeaverData"])));
        roots.push(("", under(&[".local", "share", "DBeaverData"])));
        roots.push((
            " (Snap)",
            under(&[
                "snap",
                "dbeaver-ce",
                "current",
                ".local",
                "share",
                "DBeaverData",
            ]),
        ));
        roots.push((
            " (Flatpak)",
            under(&[
                ".var",
                "app",
                "io.dbeaver.DBeaverCommunity",
                "data",
                "DBeaverData",
            ]),
        ));
    }
    roots
}

/// The `data-sources*.json` files of every project in a workspace.
fn workspace_files(workspace: &Path, variant: &str) -> Vec<(String, PathBuf)> {
    let mut projects: Vec<(String, PathBuf)> = match std::fs::read_dir(workspace) {
        Ok(rd) => rd
            .flatten()
            .filter(|e| e.path().is_dir())
            .map(|e| (e.file_name().to_string_lossy().into_owned(), e.path()))
            .filter(|(name, _)| !name.starts_with('.'))
            .collect(),
        Err(_) => return Vec::new(),
    };
    projects.sort();
    let mut out = Vec::new();
    for (project, dir) in projects {
        let Ok(rd) = std::fs::read_dir(dir.join(".dbeaver")) else {
            continue;
        };
        let mut files: Vec<(String, PathBuf)> = rd
            .flatten()
            .filter(|e| e.path().is_file())
            .map(|e| (e.file_name().to_string_lossy().into_owned(), e.path()))
            .filter(|(name, _)| is_data_sources_name(name))
            .collect();
        files.sort();
        let several = files.len() > 1;
        for (name, path) in files {
            let mut label = format!("DBeaver{variant} · {project}");
            if several {
                label.push_str(&format!(" · {name}"));
            }
            out.push((label, path));
        }
    }
    out
}

fn is_data_sources_name(name: &str) -> bool {
    name.starts_with("data-sources") && name.ends_with(".json")
}

/// `data-sources-2.json` keeps its credentials in `credentials-config-2.json`
/// (`DataSourceFileStorage.configSuffix`).
fn credentials_path(path: &Path) -> Option<PathBuf> {
    let name = path.file_name()?.to_str()?;
    let suffix = name.strip_prefix("data-sources")?.strip_suffix(".json")?;
    Some(path.with_file_name(format!("credentials-config{suffix}.json")))
}

fn not_dbeaver() -> AppError {
    AppError::invalid("This is not a DBeaver data-sources.json file.")
}

pub fn parse(path: &Path) -> AppResult<Parsed> {
    let size = std::fs::metadata(path)?.len();
    if size > MAX_FILE_BYTES {
        return Err(not_dbeaver());
    }
    let bytes = std::fs::read(path)?;
    let text = String::from_utf8_lossy(&bytes);
    let root: Value =
        serde_json::from_str(text.trim_start_matches('\u{feff}')).map_err(|_| not_dbeaver())?;
    let root = root.as_object().ok_or_else(not_dbeaver)?;
    if !root.contains_key("connections") && !root.contains_key("folders") {
        return Err(not_dbeaver());
    }
    let empty = Obj::new();
    let connections = root
        .get("connections")
        .and_then(Value::as_object)
        .unwrap_or(&empty);
    let types = root
        .get("connection-types")
        .and_then(Value::as_object)
        .unwrap_or(&empty);

    let mut parsed = Parsed::default();
    let creds = match credentials_path(path) {
        Some(p) if p.is_file() => match read_credentials(&p) {
            Some(c) => Credentials::Read(c),
            None => Credentials::Unreadable,
        },
        _ => Credentials::Missing,
    };
    let any_saved = connections
        .values()
        .any(|c| c.get("save-password").is_some_and(truthy));
    match creds {
        Credentials::Unreadable if !connections.is_empty() => parsed.notes.push(
            "credentials-config.json could not be decrypted (the project may be protected by a \
             password), so user names and passwords must be typed on first connect."
                .into(),
        ),
        Credentials::Missing if any_saved => parsed.notes.push(
            "DBeaver keeps the saved credentials of this project in its secure storage, so \
             user names and passwords must be typed on first connect."
                .into(),
        ),
        _ => {}
    }
    let creds = match &creds {
        Credentials::Read(c) => c,
        _ => &empty,
    };

    for (id, conn) in connections {
        let Some(conn) = conn.as_object() else {
            parsed.skip(id.clone(), "The connection entry is not readable.");
            continue;
        };
        let secret = creds
            .get(id)
            .and_then(|c| c.get("#connection"))
            .and_then(Value::as_object)
            .unwrap_or(&empty);
        connection(id, conn, secret, types, &mut parsed);
    }
    Ok(parsed)
}

enum Credentials {
    Read(Obj),
    Missing,
    Unreadable,
}

/// Decrypts `credentials-config.json`: `{ id: { "#connection": { user,
/// password, auth properties... } } }`.
fn read_credentials(path: &Path) -> Option<Obj> {
    let meta = std::fs::metadata(path).ok()?;
    if meta.len() > MAX_FILE_BYTES {
        return None;
    }
    let data = std::fs::read(path).ok()?;
    let plain = decrypt(&data)?;
    match serde_json::from_slice::<Value>(&plain).ok()? {
        Value::Object(m) => Some(m),
        _ => None,
    }
}

fn decrypt(data: &[u8]) -> Option<Vec<u8>> {
    use cbc::cipher::{block_padding::Pkcs7, BlockModeDecrypt, KeyIvInit};
    if data.len() < 32 || !data.len().is_multiple_of(16) {
        return None;
    }
    let (iv, body) = data.split_at(16);
    let mut buf = body.to_vec();
    let dec = cbc::Decryptor::<aes::Aes128>::new_from_slices(&CREDENTIALS_KEY, iv).ok()?;
    let plain = dec.decrypt_padded::<Pkcs7>(&mut buf).ok()?;
    Some(plain.to_vec())
}

/// A JSON scalar as text (DBeaver writes ports and flags as strings, older
/// files sometimes as numbers or booleans).
fn text(v: Option<&Value>) -> String {
    match v {
        Some(Value::String(s)) => s.trim().to_string(),
        Some(Value::Number(n)) => n.to_string(),
        Some(Value::Bool(b)) => b.to_string(),
        _ => String::new(),
    }
}

/// A string exactly as stored (passwords keep their spaces).
fn raw(v: Option<&Value>) -> String {
    match v {
        Some(Value::String(s)) => s.clone(),
        _ => String::new(),
    }
}

fn truthy(v: &Value) -> bool {
    match v {
        Value::Bool(b) => *b,
        Value::String(s) => s.trim().eq_ignore_ascii_case("true"),
        _ => false,
    }
}

fn obj<'a>(m: &'a Obj, key: &str) -> Option<&'a Obj> {
    m.get(key).and_then(Value::as_object)
}

fn connection(id: &str, conn: &Obj, secret: &Obj, types: &Obj, parsed: &mut Parsed) {
    let empty = Obj::new();
    let name = text(conn.get("name"));
    let display = if name.is_empty() {
        id.to_string()
    } else {
        name.clone()
    };
    let provider = text(conn.get("provider")).to_ascii_lowercase();
    let driver = text(conn.get("driver"));
    let cfg = obj(conn, "configuration").unwrap_or(&empty);
    let url = text(cfg.get("url"));
    let url_mode = text(cfg.get("configurationType")).eq_ignore_ascii_case("URL");

    let engine = match engine_for(&provider, &driver) {
        Ok(Some(e)) => e,
        Ok(None) => {
            // Generic or custom driver: the URL tells the database.
            let mut probe = ConnectionProfile::default();
            match url_target(&url, &mut probe) {
                Ok(()) => probe.engine,
                Err(_) => {
                    parsed.skip(
                        display,
                        format!("{} is not supported by LowQuery.", product(&driver)),
                    );
                    return;
                }
            }
        }
        Err(reason) => {
            parsed.skip(display, reason);
            return;
        }
    };

    let mut p = ConnectionProfile {
        name,
        engine,
        port: default_port(engine),
        save_password: conn.get("save-password").is_some_and(truthy),
        ..ConnectionProfile::default()
    };
    let mut warnings = Vec::new();

    // User: the credentials file first, then a plain `user` (DBeaver #6564).
    let mut user = text(secret.get("user"));
    if user.is_empty() {
        user = text(cfg.get("user"));
    }
    let mut password = raw(secret.get("password"));
    if password.is_empty() {
        password = raw(cfg.get("password"));
    }
    p.user = user;

    let provider_props = obj(cfg, "provider-properties").unwrap_or(&empty);
    let auth_props = obj(cfg, "auth-properties").unwrap_or(&empty);
    let driver_props = obj(cfg, "properties").unwrap_or(&empty);
    let handlers = obj(cfg, "handlers").unwrap_or(&empty);
    let ssl_handler = ssl_handler_id(engine).and_then(|h| enabled_handler(handlers, h));

    let oracle_type = text(provider_props.get(ORA_CONNECTION_TYPE)).to_ascii_uppercase();
    let address = if url_mode || (engine == Engine::Oracle && oracle_type == "CUSTOM") {
        url_target(&url, &mut p)
    } else {
        manual_target(engine, cfg, provider_props, &mut p);
        Ok(())
    };
    if let Err(reason) = address {
        parsed.skip(
            display,
            format!("Uses a JDBC URL LowQuery cannot read: {reason}."),
        );
        return;
    }

    match p.engine {
        Engine::Postgres => postgres_ssl(&mut p, ssl_handler, driver_props, url_mode),
        Engine::Mysql | Engine::Mariadb => mysql_ssl(&mut p, ssl_handler, driver_props, url_mode),
        Engine::Sqlserver => sqlserver_options(
            &mut p,
            &driver,
            ssl_handler,
            driver_props,
            provider_props,
            url_mode,
        ),
        Engine::Oracle => {
            oracle_privilege(&mut p, secret, auth_props, provider_props);
            if provider_props.get(ORA_OS_AUTH).is_some_and(truthy) {
                warnings
                    .push("Uses Oracle OS authentication, which LowQuery does not support.".into());
            }
        }
        Engine::Sqlite | Engine::Redis => {}
    }

    auth_model_warnings(&mut p, &text(cfg.get("auth-model")), &mut warnings);
    handler_warnings(handlers, ssl_handler_id(p.engine), &mut warnings);
    if let Some(h) = ssl_handler {
        certificate_warnings(h, &mut warnings);
    }
    property_warnings(p.engine, driver_props, &mut warnings);
    other_warnings(conn, cfg, &mut warnings);

    p.color = color(cfg, types);
    if p.name.trim().is_empty() && p.engine == Engine::Sqlite {
        p.name = Path::new(&p.database)
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
    }
    if let Some(reason) = incomplete(&p) {
        parsed.skip(display, reason);
        return;
    }

    if !password.is_empty() {
        // DBeaver turns "save password" on when it finds one.
        p.save_password = true;
        parsed.passwords.insert(id.to_string(), password);
    }
    let folder = text(conn.get("folder"));
    parsed.candidates.push(ImportCandidate {
        key: id.to_string(),
        profile: p,
        folder: (!folder.is_empty()).then_some(folder),
        has_password: false,
        warnings,
    });
}

/// Engine of a DBeaver provider and driver. `Ok(None)` means a generic or
/// custom driver whose URL decides.
fn engine_for(provider: &str, driver: &str) -> Result<Option<Engine>, String> {
    let d = driver.to_ascii_lowercase();
    let unsupported = |what: &str| Err(format!("{what} is not supported by LowQuery."));
    if provider == "redis" || d.contains("redis") {
        return Ok(Some(Engine::Redis));
    }
    match provider {
        "postgresql" | "greenplum" => {
            for (id, what) in [
                ("redshift", "Amazon Redshift"),
                ("cockroach", "CockroachDB"),
                ("materialize", "Materialize"),
                ("risingwave", "RisingWave"),
                ("cratedb", "CrateDB"),
                ("yellowbrick", "Yellowbrick"),
                ("gcloud", "The Google Cloud SQL connector"),
            ] {
                if d.contains(id) {
                    return unsupported(what);
                }
            }
            Ok(Some(Engine::Postgres))
        }
        "oracle" => Ok(Some(Engine::Oracle)),
        "mysql" => match d.as_str() {
            "mariadb" => Ok(Some(Engine::Mariadb)),
            "starrocks" => unsupported("StarRocks"),
            _ => Ok(Some(Engine::Mysql)),
        },
        "mariadb" => Ok(Some(Engine::Mariadb)),
        "sqlserver" | "mssql" if d.contains("sybase") || d.contains("sypase") => {
            unsupported("Sybase")
        }
        "sqlserver" | "mssql" => Ok(Some(Engine::Sqlserver)),
        "sqlite" if d.contains("libsql") => unsupported("libSQL"),
        "sqlite" => Ok(Some(Engine::Sqlite)),
        "generic" => Ok(match d.as_str() {
            "sqlite_jdbc" => Some(Engine::Sqlite),
            "postgresql" | "postgresql_generic" => Some(Engine::Postgres),
            "mysql3" => Some(Engine::Mysql),
            "mssql" | "mssql_ms" => Some(Engine::Sqlserver),
            "oracle" => Some(Engine::Oracle),
            _ => None,
        }),
        "" => Err("The connection has no DBeaver driver.".into()),
        other => unsupported(&product(other)),
    }
}

/// Product name for a DBeaver provider or driver id.
fn product(id: &str) -> String {
    let l = id.to_ascii_lowercase();
    const NAMES: &[(&str, &str)] = &[
        ("db2", "DB2"),
        ("clickhouse", "ClickHouse"),
        ("snowflake", "Snowflake"),
        ("mongo", "MongoDB"),
        ("cassandra", "Cassandra"),
        ("hana", "SAP HANA"),
        ("vertica", "Vertica"),
        ("exasol", "Exasol"),
        ("firebird", "Firebird"),
        ("duckdb", "DuckDB"),
        ("bigquery", "Google BigQuery"),
        ("redshift", "Amazon Redshift"),
        ("athena", "Amazon Athena"),
        ("databricks", "Databricks"),
        ("teradata", "Teradata"),
        ("informix", "Informix"),
        ("sybase", "Sybase"),
        ("hive", "Apache Hive"),
        ("elastic", "Elasticsearch"),
        ("dynamo", "DynamoDB"),
        ("couchbase", "Couchbase"),
        ("neo4j", "Neo4j"),
        ("trino", "Trino"),
        ("presto", "Presto"),
        ("derby", "Apache Derby"),
        ("hsqldb", "HSQLDB"),
        ("access", "Microsoft Access"),
        ("csv", "CSV files"),
    ];
    if l == "h2" || l.starts_with("h2_") {
        return "H2".into();
    }
    match NAMES.iter().find(|(k, _)| l.contains(k)) {
        Some((_, name)) => (*name).to_string(),
        None if id.is_empty() => "This driver".into(),
        None => format!("The DBeaver driver \"{id}\""),
    }
}

fn default_port(engine: Engine) -> u16 {
    match engine {
        Engine::Postgres => 5432,
        Engine::Oracle => 1521,
        Engine::Mysql | Engine::Mariadb => 3306,
        Engine::Sqlserver => 1433,
        Engine::Sqlite => 0,
        Engine::Redis => 6379,
    }
}

fn port_or(s: &str, default: u16) -> u16 {
    s.trim()
        .parse::<u16>()
        .ok()
        .filter(|p| *p > 0)
        .unwrap_or(default)
}

/// Address from a JDBC URL (`configurationType: URL` and Oracle "Custom").
fn url_target(url: &str, p: &mut ConnectionProfile) -> Result<(), String> {
    let url = url.trim();
    if url.is_empty() {
        return Err("the URL is empty".into());
    }
    if url.contains('{') {
        return Err("the URL is a DBeaver template".into());
    }
    // The OCI driver takes the same addresses as the thin one.
    let thin = match url.get(..16) {
        Some(head) if head.eq_ignore_ascii_case("jdbc:oracle:oci:") => {
            format!("jdbc:oracle:thin:{}", &url[16..])
        }
        _ => url.to_string(),
    };
    jdbc::apply_url(&thin, p)?;
    if p.engine == Engine::Sqlserver {
        // DBeaver writes `jdbc:sqlserver://;serverName=host\inst;port=...`.
        for kv in url.split(';').skip(1) {
            if let Some((k, v)) = kv.split_once('=') {
                if k.trim().eq_ignore_ascii_case("servername") && !v.trim().is_empty() {
                    let (host, instance) = split_instance(v.trim());
                    p.host = host;
                    if !instance.is_empty() {
                        p.instance = instance;
                    }
                }
            }
        }
    }
    Ok(())
}

fn split_instance(server: &str) -> (String, String) {
    match server.split_once('\\') {
        Some((h, i)) => (h.trim().to_string(), i.trim().to_string()),
        None => (server.to_string(), String::new()),
    }
}

/// Address from the host, port and database fields.
fn manual_target(engine: Engine, cfg: &Obj, provider_props: &Obj, p: &mut ConnectionProfile) {
    let host = text(cfg.get("host"));
    let port = text(cfg.get("port"));
    let database = text(cfg.get("database"));
    match engine {
        Engine::Sqlite => {
            p.host.clear();
            p.port = 0;
            p.database = database;
            if p.database.is_empty() {
                let url = text(cfg.get("url"));
                let mut probe = ConnectionProfile::default();
                if jdbc::apply_url(&url, &mut probe).is_ok() && probe.engine == Engine::Sqlite {
                    p.database = probe.database;
                }
            }
        }
        Engine::Oracle => {
            let kind = text(provider_props.get(ORA_CONNECTION_TYPE)).to_ascii_uppercase();
            if kind == "TNS" {
                p.oracle_connect_type = OracleConnectType::Tns;
                p.tns_alias = database;
                let tns = text(provider_props.get(ORA_TNS_PATH));
                if !tns.is_empty() {
                    p.config_dir = Some(tns_dir(&tns));
                }
                if !host.is_empty() {
                    p.host = host;
                }
            } else {
                let sid = text(provider_props.get(ORA_SID_SERVICE)).eq_ignore_ascii_case("SID");
                p.oracle_connect_type = if sid {
                    OracleConnectType::Sid
                } else {
                    OracleConnectType::Service
                };
                p.host = host;
                p.port = port_or(&port, p.port);
                p.database = database;
            }
        }
        Engine::Sqlserver => {
            // A named instance is typed as `host\INSTANCE`.
            let (h, instance) = split_instance(&host);
            p.host = h;
            p.instance = instance;
            p.port = port_or(&port, p.port);
            p.database = database;
        }
        Engine::Redis => {
            p.host = host;
            p.port = port_or(&port, p.port);
            p.database = if database.parse::<u32>().is_ok() {
                database
            } else {
                String::new()
            };
        }
        Engine::Postgres | Engine::Mysql | Engine::Mariadb => {
            p.host = host;
            p.port = port_or(&port, p.port);
            p.database = database;
        }
    }
}

/// DBeaver's TNS path is the folder of tnsnames.ora; accept the file too.
fn tns_dir(path: &str) -> String {
    let p = Path::new(path);
    let is_file = p
        .file_name()
        .is_some_and(|n| n.to_string_lossy().eq_ignore_ascii_case("tnsnames.ora"));
    match p.parent() {
        Some(parent) if is_file => parent.to_string_lossy().into_owned(),
        _ => path.to_string(),
    }
}

fn ssl_handler_id(engine: Engine) -> Option<&'static str> {
    match engine {
        Engine::Postgres => Some("postgre_ssl"),
        Engine::Mysql | Engine::Mariadb => Some("mysql_ssl"),
        Engine::Sqlserver => Some("mssql_ssl"),
        _ => None,
    }
}

fn enabled_handler<'a>(handlers: &'a Obj, id: &str) -> Option<&'a Obj> {
    let h = obj(handlers, id)?;
    h.get("enabled").is_some_and(truthy).then_some(h)
}

fn handler_prop(h: &Obj, key: &str) -> String {
    obj(h, "properties")
        .map(|p| text(p.get(key)))
        .unwrap_or_default()
}

fn prop<'a>(props: &'a Obj, key: &str) -> Option<&'a Value> {
    props
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case(key))
        .map(|(_, v)| v)
}

/// `PostgreDataSource.initServerSSL`: the handler sets `ssl=true` (pgjdbc
/// then verifies the certificate) unless it names an `sslMode`.
fn postgres_ssl(p: &mut ConnectionProfile, handler: Option<&Obj>, props: &Obj, url_mode: bool) {
    if let Some(h) = handler {
        p.ssl_mode = jdbc::pg_ssl_mode(&handler_prop(h, "sslMode")).unwrap_or(SslMode::VerifyFull);
        return;
    }
    if url_mode {
        return;
    }
    if let Some(mode) = prop(props, "sslmode").and_then(|v| jdbc::pg_ssl_mode(&text(Some(v)))) {
        p.ssl_mode = mode;
    } else if prop(props, "ssl").is_some_and(truthy) {
        p.ssl_mode = SslMode::VerifyFull;
    }
}

/// `MySQLDataSource`: without the SSL handler DBeaver sends `useSSL=false`.
fn mysql_ssl(p: &mut ConnectionProfile, handler: Option<&Obj>, props: &Obj, url_mode: bool) {
    if let Some(h) = handler {
        let verify = handler_prop(h, "ssl.verify.server").eq_ignore_ascii_case("true");
        p.ssl_mode = if verify {
            SslMode::VerifyFull
        } else {
            SslMode::Require
        };
        return;
    }
    if url_mode {
        return;
    }
    p.ssl_mode = SslMode::Disable;
    if let Some(v) = prop(props, "sslMode") {
        p.ssl_mode = match text(Some(v)).to_ascii_uppercase().as_str() {
            "DISABLED" => SslMode::Disable,
            "REQUIRED" | "VERIFY_CA" => SslMode::Require,
            "VERIFY_IDENTITY" => SslMode::VerifyFull,
            _ => SslMode::Prefer,
        };
    } else if prop(props, "useSSL").is_some_and(truthy) {
        p.ssl_mode = SslMode::Require;
    }
}

fn sqlserver_options(
    p: &mut ConnectionProfile,
    driver: &str,
    handler: Option<&Obj>,
    props: &Obj,
    provider_props: &Obj,
    url_mode: bool,
) {
    let mut encrypt = prop(props, "encrypt").map(|v| {
        matches!(
            text(Some(v)).to_ascii_lowercase().as_str(),
            "true" | "mandatory" | "strict"
        )
    });
    let mut trust = prop(props, "trustServerCertificate").is_some_and(truthy);
    if let Some(h) = handler {
        encrypt = Some(true);
        trust |= handler_prop(h, "sslTrustServerCertificate").eq_ignore_ascii_case("true");
    }
    // The Azure driver template adds `encrypt=true`.
    if encrypt.is_none() && driver.to_ascii_lowercase().contains("azure") {
        encrypt = Some(true);
    }
    if !url_mode || encrypt.is_some() || trust {
        p.ssl_mode = jdbc::sqlserver_ssl(encrypt, trust);
    }
    if prop(props, "integratedSecurity").is_some_and(truthy)
        || provider_props.get(MSSQL_WINDOWS_AUTH).is_some_and(truthy)
    {
        p.windows_auth = true;
    }
}

/// "Logon as" from the auth properties (kept in the credentials file), the
/// older provider properties, or a `user AS SYSDBA` user name.
fn oracle_privilege(p: &mut ConnectionProfile, secret: &Obj, auth: &Obj, provider: &Obj) {
    let role = [
        secret.get(ORA_LOGON_AS),
        auth.get(ORA_LOGON_AS),
        provider.get(ORA_LOGON_AS),
        provider.get(ORA_INTERNAL_LOGON),
    ]
    .into_iter()
    .map(text)
    .find_map(|r| privilege(&r));
    if let Some(r) = role {
        p.oracle_privilege = r;
    }
    let lower = p.user.to_ascii_lowercase();
    if let Some(i) = lower.rfind(" as ") {
        if let Some(r) = privilege(&p.user[i + 4..]) {
            p.oracle_privilege = r;
            p.user = p.user[..i].trim().to_string();
        }
    }
}

fn privilege(s: &str) -> Option<OraclePrivilege> {
    Some(match s.trim().to_ascii_uppercase().as_str() {
        "SYSDBA" => OraclePrivilege::Sysdba,
        "SYSOPER" => OraclePrivilege::Sysoper,
        "SYSBACKUP" => OraclePrivilege::Sysbackup,
        "SYSDG" => OraclePrivilege::Sysdg,
        "SYSKM" => OraclePrivilege::Syskm,
        "SYSRAC" => OraclePrivilege::Sysrac,
        "SYSASM" => OraclePrivilege::Sysasm,
        _ => return None,
    })
}

fn auth_model_warnings(p: &mut ConnectionProfile, model: &str, warnings: &mut Vec<String>) {
    match model {
        "" | "native" | "oracle_native" | "sqlserver_database" => {}
        "sqlserver_windows" => p.windows_auth = true,
        "sqlserver_ntlm" => {
            p.windows_auth = true;
            warnings.push(
                "Uses NTLM with a named account; LowQuery signs in as the current Windows user."
                    .into(),
            );
        }
        "oracle_os" => {
            warnings.push("Uses Oracle OS authentication, which LowQuery does not support.".into())
        }
        "postgres_pgpass" => warnings.push(
            "Reads its password from .pgpass in DBeaver; LowQuery asks for it on first connect."
                .into(),
        ),
        m if m.starts_with("sqlserver_ad") || m == "sqlserver_msi" || m == "sqlserver_mfa" => {
            warnings.push(
                "Uses Microsoft Entra ID (Azure AD) sign-in, which LowQuery does not support."
                    .into(),
            )
        }
        m => warnings.push(format!(
            "Uses the DBeaver sign-in method \"{m}\", which is not carried over."
        )),
    }
}

/// Enabled network handlers other than the engine's SSL handler.
fn handler_warnings(handlers: &Obj, ssl: Option<&str>, warnings: &mut Vec<String>) {
    for (id, h) in handlers {
        let Some(h) = h.as_object() else { continue };
        if !h.get("enabled").is_some_and(truthy) || Some(id.as_str()) == ssl {
            continue;
        }
        let kind = text(h.get("type")).to_ascii_uppercase();
        let message = if id.contains("ssh") || kind == "TUNNEL" {
            "Uses an SSH tunnel, which LowQuery does not support; open the tunnel yourself and \
             point the connection at it."
                .to_string()
        } else if id.contains("proxy") || kind == "PROXY" {
            "Connects through a proxy, which LowQuery does not support.".to_string()
        } else if id.contains("ssl") {
            "Uses SSL settings LowQuery does not read for this database.".to_string()
        } else {
            format!("Uses the DBeaver network handler \"{id}\", which is not carried over.")
        };
        if !warnings.contains(&message) {
            warnings.push(message);
        }
    }
}

/// Certificates in the SSL handler (`SSLHandlerTrustStoreImpl` and the
/// older per-driver keys).
fn certificate_warnings(h: &Obj, warnings: &mut Vec<String>) {
    let any = |keys: &[&str]| keys.iter().any(|k| !handler_prop(h, k).is_empty());
    if any(&["ssl.client.cert", "clientCert", "ssl.keystore"]) {
        warnings.push("Uses a client certificate, which is not carried over.".into());
    }
    if any(&["ssl.ca.cert", "rootCert"]) {
        warnings.push(
            "Trusts a custom CA certificate; LowQuery checks the server against the system \
             certificates."
                .into(),
        );
    }
}

fn property_warnings(engine: Engine, props: &Obj, warnings: &mut Vec<String>) {
    let used: &[&str] = match engine {
        Engine::Postgres => &["sslmode", "ssl", "user"],
        Engine::Mysql | Engine::Mariadb => &["sslMode", "useSSL", "user"],
        Engine::Sqlserver => &[
            "encrypt",
            "trustServerCertificate",
            "integratedSecurity",
            "user",
        ],
        _ => &[],
    };
    let mut kerberos = false;
    let mut entra = false;
    let mut others = Vec::new();
    for (k, v) in props {
        if used
            .iter()
            .chain(HARMLESS_PROPERTIES)
            .any(|u| u.eq_ignore_ascii_case(k))
        {
            continue;
        }
        let value = text(Some(v));
        if engine == Engine::Sqlserver && k.eq_ignore_ascii_case("authenticationScheme") {
            kerberos |= value.eq_ignore_ascii_case("JavaKerberos");
            continue;
        }
        if engine == Engine::Sqlserver && k.eq_ignore_ascii_case("authentication") {
            entra |= value.to_ascii_lowercase().starts_with("activedirectory");
            continue;
        }
        others.push(k.clone());
    }
    if kerberos {
        warnings.push("Uses Kerberos authentication, which LowQuery does not support.".into());
    }
    if entra {
        warnings.push(
            "Uses Microsoft Entra ID (Azure AD) sign-in, which LowQuery does not support.".into(),
        );
    }
    if !others.is_empty() {
        let shown: Vec<&str> = others.iter().take(5).map(String::as_str).collect();
        let more = if others.len() > shown.len() {
            ", ..."
        } else {
            ""
        };
        warnings.push(format!(
            "Driver properties are not carried over: {}{more}.",
            shown.join(", ")
        ));
    }
}

fn other_warnings(conn: &Obj, cfg: &Obj, warnings: &mut Vec<String>) {
    if conn.get("read-only").is_some_and(truthy) {
        warnings.push("Is read-only in DBeaver; LowQuery does not enforce that.".into());
    }
    if let Some(b) = obj(cfg, "bootstrap") {
        let queries = b
            .get("initQueries")
            .or_else(|| b.get("query"))
            .and_then(Value::as_array)
            .is_some_and(|q| !q.is_empty());
        if queries {
            warnings
                .push("Runs SQL on connect (bootstrap queries), which is not carried over.".into());
        }
        if !text(b.get("defaultCatalog")).is_empty() || !text(b.get("defaultSchema")).is_empty() {
            warnings.push("Opens a default catalog or schema, which is not carried over.".into());
        }
        if b.get("autocommit") == Some(&Value::Bool(false)) {
            warnings.push("Turns auto-commit off on connect, which is not carried over.".into());
        }
    }
    let events = obj(cfg, "events")
        .is_some_and(|e| e.values().any(|ev| ev.get("enabled").is_some_and(truthy)));
    if events {
        warnings
            .push("Runs shell commands on connection events, which are not carried over.".into());
    }
}

/// Production is red; otherwise the connection's own color, then the
/// connection type's.
fn color(cfg: &Obj, types: &Obj) -> Option<String> {
    let kind = text(cfg.get("type"));
    if kind.eq_ignore_ascii_case("prod") {
        return Some(RED.into());
    }
    if let Some(c) = rgb(&text(cfg.get("color"))) {
        return Some(c);
    }
    match kind.to_ascii_lowercase().as_str() {
        "" | "dev" => None,
        "test" => Some(GREEN.into()),
        other => {
            let t = obj(types, other).or_else(|| obj(types, &kind))?;
            let name = text(t.get("name")).to_ascii_lowercase();
            if other.contains("prod") || name.contains("prod") {
                Some(RED.into())
            } else {
                rgb(&text(t.get("color")))
            }
        }
    }
}

/// `r,g,b` (DBeaver's format) or `#rrggbb`; white means no color.
fn rgb(s: &str) -> Option<String> {
    let s = s.trim();
    let hex = if let Some(h) = s.strip_prefix('#') {
        (h.len() == 6 && h.chars().all(|c| c.is_ascii_hexdigit()))
            .then(|| h.to_ascii_lowercase())?
    } else {
        let parts: Vec<u8> = s
            .split(',')
            .map(|p| p.trim().parse::<u8>())
            .collect::<Result<_, _>>()
            .ok()?;
        let [r, g, b] = parts[..] else { return None };
        format!("{r:02x}{g:02x}{b:02x}")
    };
    (hex != "ffffff").then(|| format!("#{hex}"))
}

/// The checks `profiles::validate` makes, as a skip reason.
fn incomplete(p: &ConnectionProfile) -> Option<&'static str> {
    match p.engine {
        Engine::Oracle if p.oracle_connect_type == OracleConnectType::Custom => p
            .connect_string
            .trim()
            .is_empty()
            .then_some("The Oracle connect string is empty."),
        Engine::Oracle if p.oracle_connect_type == OracleConnectType::Tns => p
            .tns_alias
            .trim()
            .is_empty()
            .then_some("No TNS alias is set."),
        Engine::Sqlite => p
            .database
            .trim()
            .is_empty()
            .then_some("No database file is set."),
        _ if p.host.trim().is_empty() => Some("No host is set."),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::import::ImportTool;
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct TempDir(PathBuf);

    impl TempDir {
        fn new() -> Self {
            static N: AtomicUsize = AtomicUsize::new(0);
            let dir = std::env::temp_dir().join(format!(
                "lowquery-dbeaver-{}-{}-{}",
                std::process::id(),
                N.fetch_add(1, Ordering::SeqCst),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_nanos())
                    .unwrap_or_default()
            ));
            std::fs::create_dir_all(&dir).unwrap();
            Self(dir)
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn encrypt(plain: &[u8]) -> Vec<u8> {
        use cbc::cipher::{block_padding::Pkcs7, BlockModeEncrypt, KeyIvInit};
        let iv = [7u8; 16];
        let mut buf = plain.to_vec();
        buf.resize(plain.len() + 16, 0);
        let ct = cbc::Encryptor::<aes::Aes128>::new_from_slices(&CREDENTIALS_KEY, &iv)
            .unwrap()
            .encrypt_padded::<Pkcs7>(&mut buf, plain.len())
            .unwrap()
            .to_vec();
        [iv.to_vec(), ct].concat()
    }

    const DATA_SOURCES: &str = r#"{
  "folders": {
    "Production": {},
    "Production/ERP": { "description": "ERP databases" }
  },
  "connection-types": {
    "dev": { "name": "Development", "color": "255,255,255", "auto-commit": true },
    "uat": { "name": "UAT", "color": "255,200,0", "auto-commit": true },
    "live": { "name": "Production 2", "color": "247,159,129", "confirm-execute": true }
  },
  "connections": {
    "postgres-jdbc-18c2f0a1b2c-6d1e4f": {
      "provider": "postgresql",
      "driver": "postgres-jdbc",
      "name": "Sales PG",
      "save-password": true,
      "folder": "Production/ERP",
      "configuration": {
        "host": "pg.internal",
        "port": "5433",
        "database": "sales",
        "url": "jdbc:postgresql://pg.internal:5433/sales",
        "configurationType": "MANUAL",
        "home": "",
        "type": "prod",
        "provider-properties": { "@dbeaver-show-non-default-db@": "true" },
        "auth-model": "native",
        "handlers": {
          "postgre_ssl": {
            "type": "CONFIG",
            "enabled": true,
            "save-password": false,
            "properties": { "sslMode": "verify-ca", "ssl.method": "CERTIFICATES", "ssl.ca.cert": "C:\\certs\\ca.pem" }
          },
          "ssh_tunnel": {
            "type": "TUNNEL",
            "enabled": true,
            "save-password": true,
            "properties": { "host": "bastion", "port": 22, "authType": "PASSWORD" }
          }
        },
        "bootstrap": { "initQueries": ["set search_path to app"] }
      }
    },
    "oracle_thin-1a2b": {
      "provider": "oracle",
      "driver": "oracle_thin",
      "name": "ERP SID",
      "save-password": true,
      "folder": "Production/ERP",
      "configuration": {
        "host": "erp-db",
        "port": "1521",
        "database": "ERPPRD",
        "url": "jdbc:oracle:thin:@erp-db:1521:ERPPRD",
        "configurationType": "MANUAL",
        "type": "dev",
        "provider-properties": {
          "@dbeaver-connection-type@": "BASIC",
          "@dbeaver-sid-service@": "SID"
        },
        "auth-model": "oracle_native"
      }
    },
    "oracle_thin-3c4d": {
      "provider": "oracle",
      "driver": "oracle_thin",
      "name": "ERP service as SYSDBA",
      "save-password": true,
      "configuration": {
        "host": "erp-db.internal",
        "port": "1522",
        "database": "erppdb",
        "configurationType": "MANUAL",
        "type": "uat",
        "provider-properties": {
          "@dbeaver-connection-type@": "BASIC",
          "@dbeaver-sid-service@": "SERVICE"
        },
        "auth-model": "oracle_native"
      }
    },
    "oracle_thin-5e6f": {
      "provider": "oracle",
      "driver": "oracle_thin",
      "name": "ERP TNS",
      "configuration": {
        "database": "ERP_TNS",
        "configurationType": "MANUAL",
        "type": "live",
        "provider-properties": {
          "@dbeaver-connection-type@": "TNS",
          "@dbeaver-tns-path@": "C:\\oracle\\network\\admin",
          "@dbeaver-internal-logon@": "SYSOPER"
        }
      }
    },
    "oracle_thin-7a8b": {
      "provider": "oracle",
      "driver": "oracle_thin",
      "name": "ERP custom",
      "configuration": {
        "url": "jdbc:oracle:thin:@(DESCRIPTION=(ADDRESS=(PROTOCOL=TCP)(HOST=h)(PORT=1521))(CONNECT_DATA=(SERVICE_NAME=s)))",
        "configurationType": "MANUAL",
        "provider-properties": { "@dbeaver-connection-type@": "CUSTOM" },
        "user": "scott AS SYSDBA"
      }
    },
    "microsoft-9c0d": {
      "provider": "sqlserver",
      "driver": "microsoft",
      "name": "Finance",
      "save-password": false,
      "configuration": {
        "host": "fin-sql\\REPORTING",
        "port": "1433",
        "database": "Finance",
        "configurationType": "MANUAL",
        "color": "0,128,255",
        "auth-model": "sqlserver_windows",
        "properties": { "trustServerCertificate": "true", "encrypt": "true", "selectMethod": "cursor" }
      }
    },
    "mssql_jdbc_azure-1e2f": {
      "provider": "mssql",
      "driver": "mssql_jdbc_azure",
      "name": "Azure SQL",
      "save-password": true,
      "configuration": {
        "host": "corp.database.windows.net",
        "database": "app",
        "configurationType": "MANUAL",
        "type": "test",
        "auth-model": "sqlserver_ad_password"
      }
    },
    "jtds-3a4b": {
      "provider": "sqlserver",
      "driver": "jtds",
      "name": "Legacy jTDS URL",
      "configuration": {
        "url": "jdbc:jtds:sqlserver://old-sql:1500/Legacy;instance=SQLEXPRESS",
        "configurationType": "URL"
      }
    },
    "microsoft-5c6d": {
      "provider": "sqlserver",
      "driver": "microsoft",
      "name": "Generated URL",
      "configuration": {
        "url": "jdbc:sqlserver://;serverName=bi-sql\\OLAP;port=1500;databaseName=Cube",
        "configurationType": "URL"
      }
    },
    "mysql8-7e8f": {
      "provider": "mysql",
      "driver": "mysql8",
      "name": "Shop",
      "save-password": true,
      "read-only": true,
      "configuration": {
        "host": "shop-db",
        "port": "3307",
        "database": "shop",
        "configurationType": "MANUAL",
        "properties": { "allowPublicKeyRetrieval": "true" }
      }
    },
    "mariaDB-9a0b": {
      "provider": "mysql",
      "driver": "mariaDB",
      "name": "Maria",
      "configuration": {
        "host": "maria",
        "configurationType": "MANUAL",
        "handlers": {
          "mysql_ssl": {
            "type": "CONFIG",
            "enabled": true,
            "properties": { "ssl.require": true, "ssl.verify.server": true }
          }
        }
      }
    },
    "sqlite_jdbc-1c2d": {
      "provider": "sqlite",
      "driver": "sqlite_jdbc",
      "name": "",
      "configuration": {
        "database": "C:\\data\\app.db",
        "url": "jdbc:sqlite:C:\\data\\app.db",
        "configurationType": "MANUAL"
      }
    },
    "redis-3e4f": {
      "provider": "redis",
      "driver": "redis",
      "name": "Cache",
      "configuration": { "host": "cache", "database": "2", "configurationType": "MANUAL" }
    },
    "db2_luw-5a6b": {
      "provider": "db2",
      "driver": "db2_luw",
      "name": "Mainframe",
      "configuration": { "host": "db2", "port": "50000", "database": "SAMPLE" }
    },
    "postgres-redshift-jdbc-7c8d": {
      "provider": "postgresql",
      "driver": "postgres-redshift-jdbc",
      "name": "Warehouse",
      "configuration": { "host": "wh", "port": "5439", "database": "dw" }
    },
    "h2_embedded-9e0f": {
      "provider": "generic",
      "driver": "h2_embedded_v2",
      "name": "Local H2",
      "configuration": { "url": "jdbc:h2:~/test", "configurationType": "MANUAL" }
    },
    "custom-pg-1a1a": {
      "provider": "generic",
      "driver": "18f0c2a1-custom",
      "name": "Custom PG driver",
      "configuration": { "url": "jdbc:postgresql://custom:6543/db", "configurationType": "URL" }
    },
    "postgres-jdbc-nohost": {
      "provider": "postgresql",
      "driver": "postgres-jdbc",
      "name": "No host",
      "configuration": { "host": "", "configurationType": "MANUAL" }
    }
  }
}"#;

    const CREDENTIALS: &str = r##"{
  "postgres-jdbc-18c2f0a1b2c-6d1e4f": { "#connection": { "user": "sales_app", "password": "fake-pass-1" } },
  "oracle_thin-1a2b": { "#connection": { "user": "erp", "password": "fake-pass-2" } },
  "oracle_thin-3c4d": { "#connection": { "user": "sys", "password": "fake-pass-3", "oracle.logon-as": "sysdba" } },
  "mysql8-7e8f": { "#connection": { "user": "shop" } },
  "mssql_jdbc_azure-1e2f": { "#connection": { "user": "ana@corp.com", "password": "fake-pass-4" }, "network/ssh_tunnel": { "user": "tunnel", "password": "fake-pass-5" } }
}"##;

    fn project(dir: &Path, data: &str, creds: Option<&[u8]>) -> PathBuf {
        let dot = dir.join(".dbeaver");
        std::fs::create_dir_all(&dot).unwrap();
        let path = dot.join("data-sources.json");
        std::fs::write(&path, data).unwrap();
        if let Some(c) = creds {
            std::fs::write(dot.join("credentials-config.json"), c).unwrap();
        }
        path
    }

    fn find<'a>(p: &'a Parsed, key: &str) -> &'a ImportCandidate {
        p.candidates
            .iter()
            .find(|c| c.key == key)
            .unwrap_or_else(|| panic!("missing {key}"))
    }

    fn reason<'a>(p: &'a Parsed, name: &str) -> &'a str {
        &p.skipped
            .iter()
            .find(|s| s.name == name)
            .unwrap_or_else(|| panic!("not skipped: {name}"))
            .reason
    }

    #[test]
    fn full_project() {
        let tmp = TempDir::new();
        let path = project(&tmp.0, DATA_SOURCES, Some(&encrypt(CREDENTIALS.as_bytes())));
        let p = super::super::parse(ImportTool::Dbeaver, &path).unwrap();
        assert!(p.notes.is_empty(), "{:?}", p.notes);

        let pg = find(&p, "postgres-jdbc-18c2f0a1b2c-6d1e4f");
        assert_eq!(pg.profile.engine, Engine::Postgres);
        assert_eq!(
            (
                pg.profile.host.as_str(),
                pg.profile.port,
                pg.profile.database.as_str()
            ),
            ("pg.internal", 5433, "sales")
        );
        assert_eq!(pg.profile.user, "sales_app");
        assert_eq!(pg.profile.ssl_mode, SslMode::Require);
        assert_eq!(pg.profile.color.as_deref(), Some(RED));
        assert_eq!(pg.folder.as_deref(), Some("Production/ERP"));
        assert!(pg.has_password && pg.profile.save_password);
        assert!(pg.warnings.iter().any(|w| w.contains("SSH tunnel")));
        assert!(pg.warnings.iter().any(|w| w.contains("CA certificate")));
        assert!(pg.warnings.iter().any(|w| w.contains("bootstrap")));
        assert_eq!(
            p.passwords.get(&pg.key).map(String::as_str),
            Some("fake-pass-1")
        );

        let sid = find(&p, "oracle_thin-1a2b");
        assert_eq!(sid.profile.engine, Engine::Oracle);
        assert_eq!(sid.profile.oracle_connect_type, OracleConnectType::Sid);
        assert_eq!(
            (
                sid.profile.host.as_str(),
                sid.profile.port,
                sid.profile.database.as_str()
            ),
            ("erp-db", 1521, "ERPPRD")
        );
        assert_eq!(sid.profile.user, "erp");
        assert_eq!(sid.profile.color, None);
        assert!(sid.warnings.is_empty(), "{:?}", sid.warnings);

        let svc = find(&p, "oracle_thin-3c4d");
        assert_eq!(svc.profile.oracle_connect_type, OracleConnectType::Service);
        assert_eq!(
            (svc.profile.port, svc.profile.database.as_str()),
            (1522, "erppdb")
        );
        assert_eq!(svc.profile.oracle_privilege, OraclePrivilege::Sysdba);
        assert_eq!(svc.profile.color.as_deref(), Some("#ffc800"));
        assert!(svc.folder.is_none());

        let tns = find(&p, "oracle_thin-5e6f");
        assert_eq!(tns.profile.oracle_connect_type, OracleConnectType::Tns);
        assert_eq!(tns.profile.tns_alias, "ERP_TNS");
        assert_eq!(
            tns.profile.config_dir.as_deref(),
            Some("C:\\oracle\\network\\admin")
        );
        assert_eq!(tns.profile.oracle_privilege, OraclePrivilege::Sysoper);
        assert_eq!(tns.profile.color.as_deref(), Some(RED));
        assert!(!tns.profile.save_password && !tns.has_password);

        let custom = find(&p, "oracle_thin-7a8b");
        assert_eq!(
            custom.profile.oracle_connect_type,
            OracleConnectType::Custom
        );
        assert!(custom.profile.connect_string.starts_with("(DESCRIPTION="));
        assert_eq!(custom.profile.user, "scott");
        assert_eq!(custom.profile.oracle_privilege, OraclePrivilege::Sysdba);

        let fin = find(&p, "microsoft-9c0d");
        assert_eq!(fin.profile.engine, Engine::Sqlserver);
        assert_eq!(
            (fin.profile.host.as_str(), fin.profile.instance.as_str()),
            ("fin-sql", "REPORTING")
        );
        assert!(fin.profile.windows_auth);
        assert_eq!(fin.profile.ssl_mode, SslMode::Require);
        assert_eq!(fin.profile.color.as_deref(), Some("#0080ff"));
        assert!(!fin.profile.save_password);
        assert!(fin.warnings.iter().any(|w| w.contains("selectMethod")));

        let azure = find(&p, "mssql_jdbc_azure-1e2f");
        assert_eq!(azure.profile.ssl_mode, SslMode::VerifyFull);
        assert_eq!(azure.profile.port, 1433);
        assert_eq!(azure.profile.color.as_deref(), Some(GREEN));
        assert!(azure.warnings.iter().any(|w| w.contains("Entra")));
        assert_eq!(
            p.passwords.get(&azure.key).map(String::as_str),
            Some("fake-pass-4")
        );

        let jtds = find(&p, "jtds-3a4b");
        assert_eq!(
            (
                jtds.profile.host.as_str(),
                jtds.profile.port,
                jtds.profile.database.as_str(),
                jtds.profile.instance.as_str()
            ),
            ("old-sql", 1500, "Legacy", "SQLEXPRESS")
        );

        let gen = find(&p, "microsoft-5c6d");
        assert_eq!(
            (
                gen.profile.host.as_str(),
                gen.profile.instance.as_str(),
                gen.profile.port,
                gen.profile.database.as_str()
            ),
            ("bi-sql", "OLAP", 1500, "Cube")
        );

        let shop = find(&p, "mysql8-7e8f");
        assert_eq!(shop.profile.engine, Engine::Mysql);
        assert_eq!(
            (shop.profile.port, shop.profile.user.as_str()),
            (3307, "shop")
        );
        assert_eq!(shop.profile.ssl_mode, SslMode::Disable);
        assert!(!shop.has_password && shop.profile.save_password);
        assert_eq!(shop.warnings.len(), 1, "{:?}", shop.warnings);
        assert!(shop.warnings[0].contains("read-only"));

        let maria = find(&p, "mariaDB-9a0b");
        assert_eq!(maria.profile.engine, Engine::Mariadb);
        assert_eq!(
            (maria.profile.port, maria.profile.ssl_mode),
            (3306, SslMode::VerifyFull)
        );

        let lite = find(&p, "sqlite_jdbc-1c2d");
        assert_eq!(lite.profile.engine, Engine::Sqlite);
        assert_eq!(lite.profile.database, "C:\\data\\app.db");
        assert!(lite.profile.host.is_empty());
        assert!(!lite.profile.name.is_empty());

        let redis = find(&p, "redis-3e4f");
        assert_eq!(redis.profile.engine, Engine::Redis);
        assert_eq!(
            (redis.profile.port, redis.profile.database.as_str()),
            (6379, "2")
        );

        let custom_pg = find(&p, "custom-pg-1a1a");
        assert_eq!(custom_pg.profile.engine, Engine::Postgres);
        assert_eq!(
            (custom_pg.profile.host.as_str(), custom_pg.profile.port),
            ("custom", 6543)
        );

        assert_eq!(reason(&p, "Mainframe"), "DB2 is not supported by LowQuery.");
        assert_eq!(
            reason(&p, "Warehouse"),
            "Amazon Redshift is not supported by LowQuery."
        );
        assert_eq!(reason(&p, "Local H2"), "H2 is not supported by LowQuery.");
        assert_eq!(reason(&p, "No host"), "No host is set.");
        assert_eq!(p.candidates.len(), 14);
        assert_eq!(p.passwords.len(), 4);
        // Tunnel credentials are not connection passwords.
        assert!(!p.passwords.values().any(|v| v == "fake-pass-5"));
        // Passwords never leak into messages.
        for c in &p.candidates {
            assert!(c.warnings.iter().all(|w| !w.contains("fake-pass")));
        }
    }

    #[test]
    fn credentials_missing_or_broken() {
        let data = r#"{"folders":{},"connections":{"postgres-jdbc-1":{"provider":"postgresql","driver":"postgres-jdbc","name":"A","save-password":true,"configuration":{"host":"a","port":"5432","database":"x","configurationType":"MANUAL"}}}}"#;

        let tmp = TempDir::new();
        let path = project(&tmp.0, data, None);
        let p = parse(&path).unwrap();
        assert_eq!(p.candidates.len(), 1);
        assert!(p.candidates[0].profile.user.is_empty());
        assert!(p.passwords.is_empty());
        assert_eq!(p.notes.len(), 1);
        assert!(p.notes[0].contains("secure storage"));

        let tmp = TempDir::new();
        let path = project(&tmp.0, data, Some(b"not encrypted at all, 32 bytes!!"));
        let p = parse(&path).unwrap();
        assert_eq!(p.candidates.len(), 1);
        assert_eq!(p.notes.len(), 1);
        assert!(p.notes[0].contains("could not be decrypted"));

        // Decrypts but is not JSON.
        let tmp = TempDir::new();
        let path = project(&tmp.0, data, Some(&encrypt(b"garbage")));
        let p = parse(&path).unwrap();
        assert!(p.notes[0].contains("could not be decrypted"));

        // Nothing saved: no note.
        let tmp = TempDir::new();
        let path = project(&tmp.0, &data.replace("true", "false"), None);
        assert!(parse(&path).unwrap().notes.is_empty());
    }

    #[test]
    fn plain_password_in_configuration() {
        let data = r#"{"connections":{"c1":{"provider":"postgresql","driver":"postgres-jdbc","name":"Old","configuration":{"host":"h","user":"u","password":"fake-pass-6"}}}}"#;
        let tmp = TempDir::new();
        let path = project(&tmp.0, data, None);
        let p = parse(&path).unwrap();
        assert_eq!(p.candidates[0].profile.user, "u");
        assert!(p.candidates[0].profile.save_password);
        assert_eq!(
            p.passwords.get("c1").map(String::as_str),
            Some("fake-pass-6")
        );
    }

    #[test]
    fn malformed_input() {
        let tmp = TempDir::new();
        let path = tmp.0.join("data-sources.json");
        for bad in [
            "",
            "   ",
            "not json",
            "[1,2]",
            "{}",
            "{\"other\": 1}",
            "\"x\"",
        ] {
            std::fs::write(&path, bad).unwrap();
            let err = parse(&path).unwrap_err();
            assert!(err.message.contains("DBeaver"), "{bad}: {}", err.message);
        }
        // Odd types inside a valid file never panic.
        let odd = r#"{"connections": {
            "a": 5,
            "b": {"name": "", "configuration": "x"},
            "c": {"provider": "postgresql", "driver": "postgres-jdbc", "configuration": {"host": 7, "port": "99999", "handlers": [1], "properties": "x", "bootstrap": {"initQueries": "x"}}},
            "d": {"provider": "oracle", "configuration": {"provider-properties": {"@dbeaver-connection-type@": "TNS"}}},
            "e": {"provider": "sqlite", "configuration": {}},
            "f": {"provider": "sqlserver", "configuration": {"configurationType": "URL", "url": "jdbc:sqlserver://{host}"}},
            "g": {"provider": "mysql", "configuration": {"configurationType": "URL", "url": "jdbc:db2://x/y"}}
        }, "folders": []}"#;
        std::fs::write(&path, odd).unwrap();
        let p = parse(&path).unwrap();
        let c = find(&p, "c");
        assert_eq!((c.profile.host.as_str(), c.profile.port), ("7", 5432));
        assert_eq!(reason(&p, "a"), "The connection entry is not readable.");
        assert_eq!(reason(&p, "b"), "The connection has no DBeaver driver.");
        assert_eq!(reason(&p, "d"), "No TNS alias is set.");
        assert_eq!(reason(&p, "e"), "No database file is set.");
        assert!(reason(&p, "f").contains("template"));
        assert!(reason(&p, "g").starts_with("Uses a JDBC URL LowQuery cannot read"));
        assert_eq!(p.candidates.len(), 1);

        assert!(parse(&tmp.0.join("missing.json")).is_err());
    }

    #[test]
    fn workspace_layout() {
        let tmp = TempDir::new();
        let ws = tmp.0.join("workspace6");
        let general = ws.join("General").join(".dbeaver");
        let other = ws.join("Clients").join(".dbeaver");
        let meta = ws.join(".metadata").join(".dbeaver");
        for d in [&general, &other, &meta] {
            std::fs::create_dir_all(d).unwrap();
        }
        std::fs::write(general.join("data-sources.json"), "{}").unwrap();
        std::fs::write(general.join("credentials-config.json"), "x").unwrap();
        std::fs::write(other.join("data-sources.json"), "{}").unwrap();
        std::fs::write(other.join("data-sources-2.json"), "{}").unwrap();
        std::fs::write(meta.join("data-sources.json"), "{}").unwrap();
        std::fs::create_dir_all(ws.join("Empty")).unwrap();

        let files = workspace_files(&ws, " (Snap)");
        let labels: Vec<&str> = files.iter().map(|(l, _)| l.as_str()).collect();
        assert_eq!(
            labels,
            [
                "DBeaver (Snap) · Clients · data-sources-2.json",
                "DBeaver (Snap) · Clients · data-sources.json",
                "DBeaver (Snap) · General",
            ]
        );
        assert!(workspace_files(&tmp.0.join("nope"), "").is_empty());
        let _ = default_files();
    }

    #[test]
    fn credentials_file_names() {
        let p = Path::new("x").join(".dbeaver").join("data-sources-2.json");
        assert_eq!(
            credentials_path(&p).unwrap().file_name().unwrap(),
            "credentials-config-2.json"
        );
        let p = Path::new("data-sources.json");
        assert_eq!(
            credentials_path(p).unwrap(),
            Path::new("credentials-config.json")
        );
        assert!(credentials_path(Path::new("other.json")).is_none());
    }

    #[test]
    fn colors() {
        assert_eq!(rgb("255,0,0").as_deref(), Some("#ff0000"));
        assert_eq!(rgb("#00FF00").as_deref(), Some("#00ff00"));
        assert_eq!(rgb("255,255,255"), None);
        assert_eq!(rgb("1,2"), None);
        assert_eq!(rgb("red"), None);
        assert_eq!(tns_dir("/opt/oracle/tnsnames.ora"), "/opt/oracle");
    }

    #[test]
    fn decrypts_known_key() {
        let data = encrypt(br##"{"a":{"#connection":{"user":"u"}}}"##);
        let plain = decrypt(&data).unwrap();
        assert!(plain.starts_with(b"{\"a\""));
        assert!(decrypt(&data[..20]).is_none());
    }
}
