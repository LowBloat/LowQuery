//! DataGrip and other JetBrains IDEs: `dataSources.xml` (global, in the IDE
//! config folder, or per project in `.idea`) with user names and SSH/SSL
//! settings in `dataSources.local.xml` next to it. Passwords live in the OS
//! keychain or KeePass and are not imported.
//!
//! Also holds the small XML reader and checks shared by the pgAdmin and SQL
//! Developer importers.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use quick_xml::events::{BytesStart, Event};
use quick_xml::{Reader, XmlVersion};

use super::{jdbc, pgadmin, ImportCandidate, Parsed};
use crate::error::{AppError, AppResult};
use crate::model::{ConnectionProfile, Engine, OracleConnectType, OraclePrivilege, SslMode};

pub fn default_files() -> Vec<(String, PathBuf)> {
    let mut found = Found::default();
    let home = super::home();
    for (label, root) in product_dirs(home.as_deref()) {
        let options = root.join("options");
        found.add(format!("{label} · Global"), options.join("dataSources.xml"));
        for project in subdirs(&root.join("projects")) {
            found.add_project(&label, &project);
        }
        for project in recent_projects(&options.join("recentProjects.xml"), home.as_deref()) {
            found.add_project(&label, &project);
        }
    }
    if let Some(home) = &home {
        for project in subdirs(&home.join("DataGripProjects")) {
            found.add_project("DataGrip", &project);
        }
    }
    found.files
}

#[derive(Default)]
struct Found {
    files: Vec<(String, PathBuf)>,
    seen: HashSet<String>,
}

impl Found {
    fn add(&mut self, label: String, path: PathBuf) {
        if !path.is_file() {
            return;
        }
        let id = std::fs::canonicalize(&path).unwrap_or_else(|_| path.clone());
        let mut id = id.to_string_lossy().replace('\\', "/");
        if cfg!(windows) || cfg!(target_os = "macos") {
            id = id.to_lowercase();
        }
        if self.seen.insert(id) {
            self.files.push((label, path));
        }
    }

    fn add_project(&mut self, product: &str, project: &Path) {
        let name = project
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| project.display().to_string());
        self.add(
            format!("{product} · {name}"),
            project.join(".idea").join("dataSources.xml"),
        );
    }
}

/// IDE config folders as (label, folder holding `options`).
fn product_dirs(home: Option<&Path>) -> Vec<(String, PathBuf)> {
    let mut bases = Vec::new();
    if let Some(dir) = super::config_dir() {
        bases.push(dir.join("JetBrains"));
    }
    if let Some(home) = home {
        if cfg!(target_os = "linux") {
            // Flatpak builds keep their own XDG config folder.
            for app in subdirs(&home.join(".var").join("app")) {
                bases.push(app.join("config").join("JetBrains"));
            }
        }
    }
    let mut out = Vec::new();
    for base in bases {
        for dir in subdirs(&base) {
            if dir.join("options").is_dir() {
                out.push((product_label(&dir_name(&dir)), dir));
            }
        }
    }
    // Before 2020.1: ~/.DataGrip2019.3/config (Windows, Linux) and
    // ~/Library/Preferences/DataGrip2019.3 (macOS).
    if let Some(home) = home {
        for dir in subdirs(home) {
            let name = dir_name(&dir);
            if let Some(product) = name.strip_prefix('.') {
                let config = dir.join("config");
                if config.join("options").is_dir() && product.contains(char::is_numeric) {
                    out.push((product_label(product), config));
                }
            }
        }
        if cfg!(target_os = "macos") {
            for dir in subdirs(&home.join("Library").join("Preferences")) {
                let name = dir_name(&dir);
                if dir.join("options").join("dataSources.xml").is_file() {
                    out.push((product_label(&name), dir));
                }
            }
        }
    }
    out
}

fn dir_name(dir: &Path) -> String {
    dir.file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default()
}

/// "DataGrip2024.1" → "DataGrip 2024.1".
fn product_label(name: &str) -> String {
    match name.find(|c: char| c.is_ascii_digit()) {
        Some(i) if i > 0 => format!("{} {}", &name[..i], &name[i..]),
        _ => name.to_string(),
    }
}

/// Subfolders sorted by name; empty when the folder cannot be read.
pub(super) fn subdirs(dir: &Path) -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut out: Vec<PathBuf> = entries
        .flatten()
        .filter(|e| e.file_type().map(|t| t.is_dir()).unwrap_or(false))
        .map(|e| e.path())
        .collect();
    out.sort();
    out
}

/// Project folders listed in the IDE's `recentProjects.xml`.
fn recent_projects(file: &Path, home: Option<&Path>) -> Vec<PathBuf> {
    let Ok(text) = read_text(file) else {
        return Vec::new();
    };
    let Some(root) = parse_xml(&text) else {
        return Vec::new();
    };
    let mut paths = Vec::new();
    collect_recent(&root, false, &mut paths);
    paths
        .into_iter()
        .map(|p| {
            let p = match home {
                Some(h) => p.replace("$USER_HOME$", &h.to_string_lossy()),
                None => p,
            };
            PathBuf::from(p)
        })
        .filter(|p| p.is_absolute())
        .collect()
}

fn collect_recent(node: &XmlNode, in_recent_paths: bool, out: &mut Vec<String>) {
    for child in &node.children {
        match child.name.as_str() {
            // additionalInfo map: <entry key="$USER_HOME$/project">.
            "entry" => {
                if let Some(k) = child.attr("key").filter(|k| !k.is_empty()) {
                    out.push(k.to_string());
                }
            }
            // recentPaths list: <option value="$USER_HOME$/project"/>.
            "option" if in_recent_paths => {
                if let Some(v) = child.attr("value").filter(|v| !v.is_empty()) {
                    out.push(v.to_string());
                }
            }
            _ => {}
        }
        let recent = in_recent_paths || child.attr("name") == Some("recentPaths");
        collect_recent(child, recent, out);
    }
}

/// User-level settings from `dataSources.local.xml`.
#[derive(Default)]
struct Local {
    user: Option<String>,
    secret_storage: Option<String>,
    auth_provider: Option<String>,
    product: Option<String>,
    /// SSH tunnel on, with its host when written inline.
    ssh: Option<Option<String>>,
    ssl: Option<Ssl>,
}

struct Ssl {
    mode: Option<String>,
    ca_cert: Option<String>,
    client_cert: bool,
}

/// Where the file sits, to expand path macros.
struct Ctx {
    project_dir: Option<PathBuf>,
    home: Option<PathBuf>,
}

impl Ctx {
    fn expand(&self, s: &str) -> String {
        let mut s = s.to_string();
        if let Some(dir) = &self.project_dir {
            let dir = dir.to_string_lossy();
            s = s
                .replace("$PROJECT_DIR$", &dir)
                .replace("$ProjectFileDir$", &dir);
        }
        if let Some(home) = &self.home {
            s = s.replace("$USER_HOME$", &home.to_string_lossy());
        }
        s
    }
}

pub fn parse(path: &Path) -> AppResult<Parsed> {
    let not_ours =
        || AppError::invalid("This is not a DataGrip or JetBrains dataSources.xml file.");
    let text = read_text(path)?;
    let root = parse_xml(&text).ok_or_else(not_ours)?;
    if root.name != "project" && root.name != "application" {
        return Err(not_ours());
    }
    let component = root.children("component").find(|c| {
        matches!(
            c.attr("name"),
            Some("DataSourceManagerImpl" | "dataSourceStorage")
        )
    });
    let Some(component) = component else {
        if root
            .children("component")
            .any(|c| c.attr("name") == Some("dataSourceStorageLocal"))
        {
            return Err(AppError::invalid(
                "This is DataGrip's local settings file. Open dataSources.xml next to it.",
            ));
        }
        return Err(not_ours());
    };

    let parent = path.parent();
    let project_dir = parent
        .filter(|p| p.file_name().is_some_and(|n| n == ".idea"))
        .and_then(Path::parent)
        .map(Path::to_path_buf);
    let ctx = Ctx {
        project_dir,
        home: super::home(),
    };
    let locals = parent.and_then(|p| read_locals(&p.join("dataSources.local.xml")));

    let mut parsed = Parsed::default();
    let mut keys = HashSet::new();
    let mut keychain = false;
    for (i, ds) in component.children("data-source").enumerate() {
        let empty = Local::default();
        let uuid = ds.attr("uuid").unwrap_or_default();
        let local = locals.as_ref().and_then(|l| l.get(uuid)).unwrap_or(&empty);
        if let Some(c) = convert(ds, i, local, &ctx, &mut parsed) {
            if !keys.insert(c.key.clone()) {
                continue;
            }
            let no_auth = local.auth_provider.as_deref() == Some("no-auth");
            keychain |= !no_auth && !parsed.passwords.contains_key(&c.key);
            parsed.candidates.push(c);
        }
    }
    if parsed.candidates.is_empty() {
        return Ok(parsed);
    }
    if locals.is_none() {
        parsed.notes.push(
            "dataSources.local.xml was not found or could not be read next to this file, so user names are empty."
                .into(),
        );
    }
    if keychain {
        parsed.notes.push(
            "DataGrip keeps passwords in the system keychain or KeePass, which LowQuery does not read: type them on first connect."
                .into(),
        );
    }
    Ok(parsed)
}

/// Settings by data source uuid; `None` when the file is missing or broken.
fn read_locals(path: &Path) -> Option<HashMap<String, Local>> {
    let text = read_text(path).ok()?;
    let root = parse_xml(&text)?;
    let component = root
        .children("component")
        .find(|c| c.attr("name") == Some("dataSourceStorageLocal"))?;
    let mut out = HashMap::new();
    for ds in component.children("data-source") {
        let Some(uuid) = ds.attr("uuid") else {
            continue;
        };
        let ssh = ds
            .child("ssh-properties")
            .filter(|s| s.child_text("enabled") == Some("true"))
            .map(|s| s.child_text("proxy-host").map(str::to_string));
        let ssl = ds
            .child("ssl-config")
            .filter(|s| s.child_text("enabled") == Some("true"))
            .map(|s| Ssl {
                mode: s.child_text("mode").map(str::to_string),
                ca_cert: s.child_text("ca-cert").map(str::to_string),
                client_cert: s.child_text("client-cert").is_some()
                    || s.child_text("client-key").is_some(),
            });
        out.insert(
            uuid.to_string(),
            Local {
                user: ds.child_text("user-name").map(str::to_string),
                secret_storage: ds.child_text("secret-storage").map(str::to_string),
                auth_provider: ds.child_text("auth-provider").map(str::to_string),
                product: ds
                    .child("database-info")
                    .and_then(|d| d.attr("product"))
                    .filter(|p| !p.trim().is_empty())
                    .map(str::to_string),
                ssh,
                ssl,
            },
        );
    }
    Some(out)
}

fn convert(
    ds: &XmlNode,
    index: usize,
    local: &Local,
    ctx: &Ctx,
    parsed: &mut Parsed,
) -> Option<ImportCandidate> {
    let name = ds
        .attr("name")
        .map(str::trim)
        .filter(|n| !n.is_empty())
        .map(str::to_string)
        .unwrap_or_else(|| format!("Data source {}", index + 1));
    let key = ds
        .attr("uuid")
        .filter(|u| !u.trim().is_empty())
        .map(str::to_string)
        .unwrap_or_else(|| format!("data-source-{index}"));
    let Some(url) = ds.child_text("jdbc-url") else {
        parsed.skip(&name, "Has no JDBC URL.");
        return None;
    };
    let url = ctx.expand(url);
    let mut p = ConnectionProfile {
        name: name.clone(),
        ..ConnectionProfile::default()
    };
    if let Err(e) = apply_jdbc(&url, &mut p) {
        let reason = match e {
            UrlError::Unsupported(product) => {
                let product = local.product.clone().unwrap_or(product);
                format!("{product} is not supported by LowQuery.")
            }
            UrlError::Invalid(r) => format!("Uses a JDBC URL LowQuery cannot read: {r}."),
        };
        parsed.skip(&name, reason);
        return None;
    }
    if p.engine == Engine::Sqlserver && has_entra_auth(&url) {
        parsed.skip(
            &name,
            "Uses Microsoft Entra ID authentication, which LowQuery does not support.",
        );
        return None;
    }

    if let Some(dir) = &p.config_dir {
        p.config_dir = Some(native_path(dir));
    }
    let mut warnings = Vec::new();
    if let Some(user) = &local.user {
        p.user = user.clone();
    }
    if p.engine == Engine::Oracle {
        let (user, privilege) = split_oracle_role(&p.user);
        p.user = user;
        if let Some(privilege) = privilege {
            p.oracle_privilege = privilege;
        }
    }
    if p.engine == Engine::Sqlite {
        let file = Path::new(&p.database);
        if file.is_relative() {
            if let Some(dir) = &ctx.project_dir {
                p.database = dir.join(file).to_string_lossy().into_owned();
            }
        }
        p.database = native_path(&p.database);
    }

    // Driver properties LowQuery understands; the rest is reported.
    let mut ignored = Vec::new();
    for prop in ds
        .child("driver-properties")
        .map(|d| d.children("property").collect::<Vec<_>>())
        .unwrap_or_default()
    {
        let Some(pname) = prop.attr("name").filter(|n| !n.is_empty()) else {
            continue;
        };
        let value = prop.attr("value").unwrap_or_default().trim();
        match (p.engine, pname) {
            (Engine::Oracle, "internal_logon") => {
                if let Some(privilege) = oracle_privilege(value) {
                    p.oracle_privilege = privilege;
                }
            }
            (Engine::Oracle, "oracle.net.tns_admin") if !value.is_empty() => {
                p.config_dir = Some(native_path(&ctx.expand(value)));
            }
            (Engine::Postgres, "sslmode") => {
                if let Some(mode) = jdbc::pg_ssl_mode(value) {
                    p.ssl_mode = mode;
                }
            }
            (Engine::Sqlserver, "integratedSecurity") => {
                p.windows_auth = value.eq_ignore_ascii_case("true");
            }
            (_, n) if is_cosmetic_property(n) => {}
            _ => ignored.push(pname.to_string()),
        }
    }

    match local.auth_provider.as_deref() {
        None | Some("no-auth") => {}
        Some("ms-sso") if p.engine == Engine::Sqlserver => p.windows_auth = true,
        Some("pgpass") if p.engine == Engine::Postgres => {
            let db = if p.database.is_empty() {
                p.user.clone()
            } else {
                p.database.clone()
            };
            let found = pgadmin::default_pgpass()
                .and_then(|f| pgadmin::pgpass_password(&f, &p.host, p.port, &db, &p.user));
            if let Some(password) = found {
                parsed.passwords.insert(key.clone(), password);
            }
        }
        Some(other) => warnings.push(format!(
            "The {other} authentication is not carried over: LowQuery signs in with a user and password."
        )),
    }
    if p.windows_auth {
        p.user.clear();
    }

    if let Some(ssl) = &local.ssl {
        if p.engine != Engine::Sqlite {
            p.ssl_mode = match ssl.mode.as_deref().map(str::to_ascii_uppercase).as_deref() {
                Some("VERIFY_FULL") => SslMode::VerifyFull,
                Some("PREFER") => SslMode::Prefer,
                Some("DISABLE") => SslMode::Disable,
                _ => SslMode::Require,
            };
        }
        if let Some(ca) = &ssl.ca_cert {
            warnings.push(format!(
                "The CA certificate {} is not carried over.",
                native_path(&ctx.expand(ca))
            ));
        }
        if ssl.client_cert {
            warnings.push("The SSL client certificate is not carried over.".into());
        }
    } else if p.engine == Engine::Redis && !url.to_ascii_lowercase().starts_with("jdbc:rediss:") {
        p.ssl_mode = SslMode::Disable;
    }
    if let Some(host) = &local.ssh {
        warnings.push(match host {
            Some(h) => format!("The SSH tunnel through {h} is not carried over."),
            None => "The SSH tunnel is not carried over.".into(),
        });
    }
    if ds.attr("read-only") == Some("true") {
        warnings.push("Read-only mode is not carried over.".into());
    }
    if ds.child_text("auto-commit") == Some("false") {
        warnings.push("Manual commit mode is not carried over.".into());
    }
    let kubernetes = ds
        .child("jdbc-additional-properties")
        .map(|d| {
            d.children("property").any(|prop| {
                prop.attr("name") == Some("com.intellij.clouds.kubernetes.db.enabled")
                    && prop.attr("value") == Some("true")
            })
        })
        .unwrap_or(false);
    if kubernetes {
        warnings.push("Kubernetes port forwarding is not carried over.".into());
    }
    if !ignored.is_empty() {
        warnings.push(format!(
            "Driver properties are not carried over: {}.",
            ignored.join(", ")
        ));
    }

    p.save_password = local.secret_storage.as_deref() != Some("forget")
        && local.auth_provider.as_deref() != Some("no-auth");
    if let Some(reason) = invalid_reason(&p) {
        parsed.passwords.remove(&key);
        parsed.skip(&name, reason);
        return None;
    }
    Some(ImportCandidate {
        key,
        profile: p,
        folder: ds
            .attr("group")
            .map(str::trim)
            .filter(|g| !g.is_empty())
            .map(str::to_string),
        has_password: false,
        warnings,
    })
}

/// Driver properties that do not change how LowQuery connects.
fn is_cosmetic_property(name: &str) -> bool {
    name.starts_with("com.intellij.")
        || matches!(
            name,
            "applicationName"
                | "ApplicationName"
                | "v$session.program"
                | "defaultRowFetchSize"
                | "oracle.jdbc.fetchSizeTuning"
                | "connectTimeout"
                | "loginTimeout"
                | "socketTimeout"
        )
}

/// Uses the platform separator in a path built from macros (`$PROJECT_DIR$/x`).
fn native_path(s: &str) -> String {
    if cfg!(windows) {
        s.replace('/', "\\")
    } else {
        s.to_string()
    }
}

fn has_entra_auth(url: &str) -> bool {
    url.to_ascii_lowercase()
        .contains("authentication=activedirectory")
}

/// "sys as sysdba" → ("sys", SYSDBA).
pub(super) fn split_oracle_role(user: &str) -> (String, Option<OraclePrivilege>) {
    let lower = user.to_ascii_lowercase();
    if let Some(i) = lower.rfind(" as ") {
        if let Some(privilege) = oracle_privilege(&user[i + 4..]) {
            return (user[..i].trim().to_string(), Some(privilege));
        }
    }
    (user.to_string(), None)
}

/// SYSDBA, SYSOPER... (the JDBC `internal_logon` values and SQL Developer
/// roles). `None` for an empty or unknown value.
pub(super) fn oracle_privilege(v: &str) -> Option<OraclePrivilege> {
    Some(match v.trim().to_ascii_uppercase().as_str() {
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

pub(super) enum UrlError {
    /// The database LowQuery does not support, by display name.
    Unsupported(String),
    Invalid(String),
}

/// Fills `p` from a JDBC URL, including the driver variants Java tools use
/// (AWS wrapper, MySQL load balancing, Oracle OCI, Redis).
pub(super) fn apply_jdbc(url: &str, p: &mut ConnectionProfile) -> Result<(), UrlError> {
    let url = url.trim();
    let Some(rest) = strip_prefix_ci(url, "jdbc:") else {
        return Err(UrlError::Invalid("not a JDBC URL".into()));
    };
    let rest = strip_prefix_ci(rest, "aws-wrapper:").unwrap_or(rest);
    let scheme = rest
        .split(':')
        .next()
        .unwrap_or_default()
        .to_ascii_lowercase();
    let mut rest = rest.to_string();
    match scheme.as_str() {
        "redis" | "rediss" => return redis_url(&rest, p),
        "mysql" => {
            // jdbc:mysql:aws://, jdbc:mysql:loadbalance://...
            let after = rest.get(6..).unwrap_or_default().to_string();
            if let Some((_, tail)) = after
                .split_once(':')
                .filter(|(v, t)| !v.is_empty() && !v.contains('/') && t.starts_with("//"))
            {
                rest = format!("mysql:{tail}");
            }
        }
        "oracle" => {
            for driver in ["oracle:oci8:", "oracle:oci:"] {
                if let Some(tail) = strip_prefix_ci(&rest, driver) {
                    rest = format!("oracle:thin:{tail}");
                    break;
                }
            }
            let (clean, tns_admin) = split_tns_admin(&rest);
            rest = clean;
            if let Some(dir) = tns_admin {
                p.config_dir = Some(dir);
            }
        }
        "jtds" if strip_prefix_ci(&rest, "jtds:sybase:").is_some() => {
            return Err(UrlError::Unsupported("Sybase".into()));
        }
        "postgresql" | "mariadb" | "sqlserver" | "jtds" | "sqlite" => {}
        other => return Err(UrlError::Unsupported(product_name(other))),
    }
    jdbc::apply_url(&format!("jdbc:{rest}"), p).map_err(UrlError::Invalid)
}

fn strip_prefix_ci<'a>(s: &'a str, prefix: &str) -> Option<&'a str> {
    let head = s.get(..prefix.len())?;
    head.eq_ignore_ascii_case(prefix)
        .then(|| &s[prefix.len()..])
}

/// Moves `TNS_ADMIN=dir` out of an Oracle URL query (`@alias?TNS_ADMIN=...`).
fn split_tns_admin(url: &str) -> (String, Option<String>) {
    let Some((base, query)) = url.split_once('?') else {
        return (url.to_string(), None);
    };
    let mut dir = None;
    let mut kept = Vec::new();
    for pair in query.split('&').filter(|s| !s.is_empty()) {
        match pair.split_once('=') {
            Some((k, v)) if k.eq_ignore_ascii_case("TNS_ADMIN") && !v.is_empty() => {
                dir = Some(v.to_string());
            }
            _ => kept.push(pair),
        }
    }
    if dir.is_none() {
        return (url.to_string(), None);
    }
    let url = if kept.is_empty() {
        base.to_string()
    } else {
        format!("{base}?{}", kept.join("&"))
    };
    (url, dir)
}

/// `redis://[user[:password]@]host[:port][/db]`.
fn redis_url(rest: &str, p: &mut ConnectionProfile) -> Result<(), UrlError> {
    let (scheme, tail) = rest.split_once(':').unwrap_or((rest, ""));
    let tail = tail
        .strip_prefix("//")
        .ok_or_else(|| UrlError::Invalid("missing //host".into()))?;
    let (main, _) = tail.split_once('?').unwrap_or((tail, ""));
    let (authority, db) = main.split_once('/').unwrap_or((main, ""));
    let address = match authority.rsplit_once('@') {
        Some((creds, address)) => {
            let user = creds.split(':').next().unwrap_or_default();
            if !user.is_empty() {
                p.user = user.to_string();
            }
            address
        }
        None => authority,
    };
    let (host, port) = jdbc::split_host_port(address, 6379);
    p.engine = Engine::Redis;
    p.host = if host.is_empty() {
        "localhost".into()
    } else {
        host
    };
    p.port = port;
    let db = db.trim();
    p.database = if db.is_empty() { "0".into() } else { db.into() };
    p.ssl_mode = if scheme.eq_ignore_ascii_case("rediss") {
        SslMode::Require
    } else {
        SslMode::Disable
    };
    Ok(())
}

/// Display name of a JDBC subprotocol LowQuery does not support.
fn product_name(scheme: &str) -> String {
    match scheme {
        "db2" | "as400" => "DB2",
        "clickhouse" | "ch" => "ClickHouse",
        "h2" => "H2",
        "hsqldb" => "HSQLDB",
        "derby" => "Apache Derby",
        "snowflake" => "Snowflake",
        "redshift" => "Amazon Redshift",
        "bigquery" => "BigQuery",
        "mongodb" | "mongo" => "MongoDB",
        "cassandra" => "Cassandra",
        "hive2" => "Apache Hive",
        "sap" => "SAP HANA",
        "exa" => "Exasol",
        "vertica" => "Vertica",
        "trino" => "Trino",
        "presto" => "Presto",
        "firebirdsql" | "firebird" => "Firebird",
        "informix-sqli" => "Informix",
        "sybase" => "Sybase",
        "databricks" | "spark" => "Databricks",
        "duckdb" => "DuckDB",
        "teradata" => "Teradata",
        "awsathena" | "athena" => "Amazon Athena",
        "dynamodb" => "DynamoDB",
        "elasticsearch" | "es" | "opensearch" => "Elasticsearch",
        "neo4j" => "Neo4j",
        "" => "This database",
        other => return other.to_string(),
    }
    .into()
}

/// Why `profiles::validate` would refuse the profile.
pub(super) fn invalid_reason(p: &ConnectionProfile) -> Option<&'static str> {
    if p.name.trim().is_empty() {
        return Some("Has no name.");
    }
    if p.engine == Engine::Oracle {
        match p.oracle_connect_type {
            OracleConnectType::Custom if p.connect_string.trim().is_empty() => {
                return Some("Has no connect descriptor.")
            }
            OracleConnectType::Tns if p.tns_alias.trim().is_empty() => {
                return Some("Has no TNS alias.")
            }
            OracleConnectType::Custom | OracleConnectType::Tns => return None,
            _ => {}
        }
    }
    if p.engine == Engine::Sqlite {
        return p
            .database
            .trim()
            .is_empty()
            .then_some("Has no database file.");
    }
    p.host.trim().is_empty().then_some("Has no host.")
}

/// `#rrggbb` from `#rgb` or `#rrggbb` (with or without `#`).
pub(super) fn hex_color(s: &str) -> Option<String> {
    let h = s.trim().trim_start_matches('#');
    if !h.chars().all(|c| c.is_ascii_hexdigit()) {
        return None;
    }
    match h.len() {
        6 => Some(format!("#{}", h.to_ascii_lowercase())),
        3 => Some(
            h.chars()
                .flat_map(|c| [c, c])
                .fold(String::from("#"), |mut s, c| {
                    s.push(c.to_ascii_lowercase());
                    s
                }),
        ),
        _ => None,
    }
}

/// Larger files are not connection lists.
const MAX_FILE: u64 = 32 << 20;

/// Reads a text file of at most [`MAX_FILE`] bytes, dropping a BOM.
pub(super) fn read_text(path: &Path) -> AppResult<String> {
    let bytes = read_bytes(path)?;
    let text = String::from_utf8_lossy(&bytes);
    Ok(text.trim_start_matches('\u{feff}').to_string())
}

pub(super) fn read_bytes(path: &Path) -> AppResult<Vec<u8>> {
    let meta = std::fs::metadata(path)?;
    if meta.len() > MAX_FILE {
        return Err(AppError::invalid(
            "The file is too large to be a connection list.",
        ));
    }
    Ok(std::fs::read(path)?)
}

/// An XML element with its text (entities resolved) and children.
#[derive(Debug, Default)]
pub(super) struct XmlNode {
    pub name: String,
    pub attrs: Vec<(String, String)>,
    pub text: String,
    pub children: Vec<XmlNode>,
}

impl XmlNode {
    pub fn attr(&self, name: &str) -> Option<&str> {
        self.attrs
            .iter()
            .find(|(k, _)| k == name)
            .map(|(_, v)| v.as_str())
    }

    pub fn child(&self, name: &str) -> Option<&XmlNode> {
        self.children.iter().find(|c| c.name == name)
    }

    pub fn children<'a>(&'a self, name: &'a str) -> impl Iterator<Item = &'a XmlNode> + 'a {
        self.children.iter().filter(move |c| c.name == name)
    }

    /// Trimmed text of the first `name` child, when not empty.
    pub fn child_text(&self, name: &str) -> Option<&str> {
        self.child(name)
            .map(|c| c.text.trim())
            .filter(|t| !t.is_empty())
    }
}

/// Deeper elements are dropped (connection files are shallow).
const MAX_DEPTH: usize = 64;

/// The root element, or `None` when the text is not well-formed XML.
pub(super) fn parse_xml(text: &str) -> Option<XmlNode> {
    let mut reader = Reader::from_str(text);
    let mut stack: Vec<XmlNode> = Vec::new();
    let mut too_deep = 0usize;
    loop {
        let event = reader.read_event().ok()?;
        match event {
            Event::Start(e) => {
                if too_deep > 0 || stack.len() >= MAX_DEPTH {
                    too_deep += 1;
                } else {
                    stack.push(node(&e));
                }
            }
            Event::Empty(e) => {
                if too_deep == 0 && stack.len() < MAX_DEPTH {
                    let n = node(&e);
                    match stack.last_mut() {
                        Some(parent) => parent.children.push(n),
                        None => return Some(n),
                    }
                }
            }
            Event::End(_) => {
                if too_deep > 0 {
                    too_deep -= 1;
                    continue;
                }
                let n = stack.pop()?;
                match stack.last_mut() {
                    Some(parent) => parent.children.push(n),
                    None => return Some(n),
                }
            }
            Event::Text(t) if too_deep == 0 => {
                if let Some(n) = stack.last_mut() {
                    n.text.push_str(&t.xml10_content());
                }
            }
            Event::CData(t) if too_deep == 0 => {
                if let Some(n) = stack.last_mut() {
                    n.text.push_str(&t.xml10_content());
                }
            }
            Event::GeneralRef(r) if too_deep == 0 => {
                if let Some(n) = stack.last_mut() {
                    if let Ok(Some(c)) = r.resolve_char_ref() {
                        n.text.push(c);
                    } else if let Some(s) =
                        quick_xml::escape::resolve_predefined_entity(&r.xml10_content())
                    {
                        n.text.push_str(s);
                    }
                }
            }
            Event::Eof => return None,
            _ => {}
        }
    }
}

fn node(e: &BytesStart) -> XmlNode {
    let attrs = e
        .attributes()
        .flatten()
        .map(|a| {
            let value = a
                .normalized_value(XmlVersion::Implicit1_0)
                .map(|v| v.into_owned())
                .unwrap_or_default();
            (a.key.local_name().as_ref().to_string(), value)
        })
        .collect();
    XmlNode {
        name: e.local_name().as_ref().to_string(),
        attrs,
        ..XmlNode::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct TempDir(PathBuf);

    impl TempDir {
        fn new(tag: &str) -> Self {
            let dir = std::env::temp_dir().join(format!(
                "lowquery-datagrip-{tag}-{}-{}",
                std::process::id(),
                uuid::Uuid::new_v4().simple()
            ));
            std::fs::create_dir_all(&dir).unwrap();
            Self(dir)
        }

        fn write(&self, rel: &str, text: &str) -> PathBuf {
            let path = self.0.join(rel);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(&path, text).unwrap();
            path
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    const PROJECT: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<project version="4">
  <component name="DataSourceManagerImpl" format="xml" multifile-model="true">
    <data-source source="LOCAL" name="erp@prod" group="Production/ERP" uuid="pg-1">
      <driver-ref>postgresql</driver-ref>
      <synchronize>true</synchronize>
      <jdbc-driver>org.postgresql.Driver</jdbc-driver>
      <jdbc-url>jdbc:postgresql://erp.internal:5433/erp?currentSchema=app&amp;sslmode=require</jdbc-url>
      <working-dir>$ProjectFileDir$</working-dir>
    </data-source>
    <data-source source="LOCAL" name="shop" read-only="true" uuid="my-1">
      <driver-ref>mysql.8</driver-ref>
      <jdbc-driver>com.mysql.cj.jdbc.Driver</jdbc-driver>
      <jdbc-url>jdbc:mysql://shop.internal:3306/shop</jdbc-url>
      <auto-commit>false</auto-commit>
      <driver-properties>
        <property name="useCursorFetch" value="true" />
        <property name="com.intellij.whatever" value="1" />
      </driver-properties>
    </data-source>
    <data-source source="LOCAL" name="aurora" uuid="my-2">
      <driver-ref>mysql_aurora_aws</driver-ref>
      <jdbc-url>jdbc:mysql:aws://aurora.example.com:3306/wallet</jdbc-url>
      <jdbc-additional-properties>
        <property name="com.intellij.clouds.kubernetes.db.enabled" value="true" />
      </jdbc-additional-properties>
    </data-source>
    <data-source source="LOCAL" name="maria" uuid="ma-1">
      <driver-ref>mariadb</driver-ref>
      <jdbc-url>jdbc:mariadb://maria.local/app</jdbc-url>
    </data-source>
    <data-source source="LOCAL" name="fin" group="Production" uuid="ms-1">
      <driver-ref>sqlserver.ms</driver-ref>
      <jdbc-url>jdbc:sqlserver://fin-sql\REPORTING;databaseName=Finance;encrypt=true;trustServerCertificate=true</jdbc-url>
    </data-source>
    <data-source source="LOCAL" name="legacy-ms" uuid="ms-2">
      <driver-ref>sqlserver.jtds</driver-ref>
      <jdbc-url>jdbc:jtds:sqlserver://old-sql:1433/Legacy;instance=SQLEXPRESS</jdbc-url>
    </data-source>
    <data-source source="LOCAL" name="azure-ad" uuid="ms-3">
      <driver-ref>azure.ms</driver-ref>
      <jdbc-url>jdbc:sqlserver://x.database.windows.net:1433;database=d;authentication=ActiveDirectoryInteractive</jdbc-url>
    </data-source>
    <data-source source="LOCAL" name="ora-sid" uuid="ora-1">
      <driver-ref>oracle.19</driver-ref>
      <jdbc-url>jdbc:oracle:thin:@erp-db:1521:ERPPRD</jdbc-url>
      <driver-properties>
        <property name="internal_logon" value="sysdba" />
      </driver-properties>
    </data-source>
    <data-source source="LOCAL" name="ora-svc" uuid="ora-2">
      <driver-ref>oracle</driver-ref>
      <jdbc-url>jdbc:oracle:thin:@//erp-db.internal:1522/erppdb</jdbc-url>
    </data-source>
    <data-source source="LOCAL" name="ora-tns" uuid="ora-3">
      <driver-ref>oracle</driver-ref>
      <jdbc-url>jdbc:oracle:thin:@ERPPRD?TNS_ADMIN=$USER_HOME$/tns</jdbc-url>
    </data-source>
    <data-source source="LOCAL" name="ora-desc" uuid="ora-4">
      <driver-ref>oracle</driver-ref>
      <jdbc-url>jdbc:oracle:thin:@(description=(address=(protocol=tcps)(port=1522)(host=adb.example.com))(connect_data=(service_name=svc_high)))</jdbc-url>
    </data-source>
    <data-source source="LOCAL" name="local.db" uuid="lite-1">
      <driver-ref>sqlite.xerial</driver-ref>
      <jdbc-url>jdbc:sqlite:$PROJECT_DIR$/data/app.db</jdbc-url>
    </data-source>
    <data-source source="LOCAL" name="cache" group="dev" uuid="redis-1">
      <driver-ref>redis</driver-ref>
      <jdbc-driver>jdbc.RedisDriver</jdbc-driver>
      <jdbc-url>jdbc:redis://cache.local:6380/2</jdbc-url>
    </data-source>
    <data-source source="LOCAL" name="warehouse" uuid="ch-1">
      <driver-ref>clickhouse</driver-ref>
      <jdbc-url>jdbc:clickhouse://ch.local:8123/default</jdbc-url>
    </data-source>
    <data-source source="LOCAL" name="no-url" uuid="x-1">
      <driver-ref>postgresql</driver-ref>
    </data-source>
    <data-source source="LOCAL" name="broken" uuid="x-2">
      <jdbc-url>jdbc:postgresql:</jdbc-url>
    </data-source>
  </component>
</project>"#;

    const LOCAL: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<project version="4">
  <component name="dataSourceStorageLocal" created-in="DB-241.14494.150">
    <data-source name="erp@prod" uuid="pg-1">
      <database-info product="PostgreSQL" version="16.2" dbms="POSTGRES" exact-version="16.2" />
      <secret-storage>master_key</secret-storage>
      <user-name>erp_app</user-name>
      <ssh-properties>
        <enabled>true</enabled>
        <proxy-host>bastion.example.com</proxy-host>
        <proxy-port>22</proxy-port>
        <user>ops</user>
      </ssh-properties>
      <ssl-config use-ide-store="true">
        <ca-cert>$PROJECT_DIR$/certs/ca.pem</ca-cert>
        <client-cert>$PROJECT_DIR$/certs/client.pem</client-cert>
        <enabled>true</enabled>
        <mode>VERIFY_FULL</mode>
      </ssl-config>
    </data-source>
    <data-source name="shop" uuid="my-1">
      <secret-storage>forget</secret-storage>
      <user-name>shop</user-name>
      <ssl-config>
        <client-cert>/tmp/c.pem</client-cert>
        <enabled>false</enabled>
      </ssl-config>
    </data-source>
    <data-source name="fin" uuid="ms-1">
      <secret-storage>master_key</secret-storage>
      <auth-provider>ms-sso</auth-provider>
    </data-source>
    <data-source name="ora-sid" uuid="ora-1">
      <user-name>sys</user-name>
    </data-source>
    <data-source name="ora-svc" uuid="ora-2">
      <user-name>system as SYSOPER</user-name>
    </data-source>
    <data-source name="local.db" uuid="lite-1">
      <auth-provider>no-auth</auth-provider>
    </data-source>
    <data-source name="cache" uuid="redis-1">
      <user-name>default</user-name>
      <ssl-config><enabled>true</enabled></ssl-config>
    </data-source>
    <data-source name="warehouse" uuid="ch-1">
      <database-info product="ClickHouse" version="24.3" />
    </data-source>
    <data-source name="aurora" uuid="my-2">
      <auth-provider>aws-iam</auth-provider>
    </data-source>
  </component>
</project>"#;

    fn find<'a>(p: &'a Parsed, key: &str) -> &'a ImportCandidate {
        p.candidates
            .iter()
            .find(|c| c.key == key)
            .unwrap_or_else(|| panic!("missing {key}"))
    }

    fn skipped<'a>(p: &'a Parsed, name: &str) -> &'a str {
        &p.skipped
            .iter()
            .find(|s| s.name == name)
            .unwrap_or_else(|| panic!("not skipped: {name}"))
            .reason
    }

    #[test]
    fn project_file_with_local_settings() {
        let dir = TempDir::new("project");
        let path = dir.write("app/.idea/dataSources.xml", PROJECT);
        dir.write("app/.idea/dataSources.local.xml", LOCAL);
        let parsed = parse(&path).unwrap();
        let project = dir.0.join("app");

        let pg = find(&parsed, "pg-1");
        assert_eq!(pg.profile.engine, Engine::Postgres);
        assert_eq!(
            (pg.profile.host.as_str(), pg.profile.port),
            ("erp.internal", 5433)
        );
        assert_eq!(pg.profile.database, "erp");
        assert_eq!(pg.profile.user, "erp_app");
        assert_eq!(pg.profile.ssl_mode, SslMode::VerifyFull);
        assert_eq!(pg.folder.as_deref(), Some("Production/ERP"));
        assert!(pg.profile.save_password);
        assert_eq!(pg.warnings.len(), 3, "{:?}", pg.warnings);
        assert!(pg
            .warnings
            .iter()
            .any(|w| w.contains("bastion.example.com")));
        assert!(pg.warnings.iter().any(|w| w.contains("client certificate")));
        let ca = project.join("certs").join("ca.pem");
        let ca = ca.to_string_lossy();
        assert!(
            pg.warnings.iter().any(|w| w.contains(&*ca)),
            "{:?}",
            pg.warnings
        );

        let my = find(&parsed, "my-1");
        assert_eq!(my.profile.engine, Engine::Mysql);
        assert_eq!(my.profile.user, "shop");
        assert!(!my.profile.save_password);
        assert_eq!(my.profile.ssl_mode, SslMode::Prefer);
        assert_eq!(my.warnings.len(), 3, "{:?}", my.warnings);
        assert!(my.warnings.iter().any(|w| w.contains("useCursorFetch")));
        assert!(!my.warnings.iter().any(|w| w.contains("intellij")));
        assert!(my.warnings.iter().any(|w| w.contains("Read-only")));
        assert!(my.warnings.iter().any(|w| w.contains("Manual commit")));

        let aurora = find(&parsed, "my-2");
        assert_eq!(
            (aurora.profile.engine, aurora.profile.host.as_str()),
            (Engine::Mysql, "aurora.example.com")
        );
        assert!(aurora.warnings.iter().any(|w| w.contains("aws-iam")));
        assert!(aurora.warnings.iter().any(|w| w.contains("Kubernetes")));

        let maria = find(&parsed, "ma-1");
        assert_eq!(
            (maria.profile.engine, maria.profile.port),
            (Engine::Mariadb, 3306)
        );

        let fin = find(&parsed, "ms-1");
        assert_eq!(fin.profile.engine, Engine::Sqlserver);
        assert_eq!(
            (fin.profile.host.as_str(), fin.profile.instance.as_str()),
            ("fin-sql", "REPORTING")
        );
        assert_eq!(fin.profile.database, "Finance");
        assert!(fin.profile.windows_auth);
        assert!(fin.profile.user.is_empty());
        assert_eq!(fin.profile.ssl_mode, SslMode::Require);
        let legacy = find(&parsed, "ms-2");
        assert_eq!(
            (
                legacy.profile.instance.as_str(),
                legacy.profile.database.as_str()
            ),
            ("SQLEXPRESS", "Legacy")
        );
        assert!(skipped(&parsed, "azure-ad").contains("Entra"));

        let sid = find(&parsed, "ora-1");
        assert_eq!(sid.profile.engine, Engine::Oracle);
        assert_eq!(sid.profile.oracle_connect_type, OracleConnectType::Sid);
        assert_eq!(sid.profile.database, "ERPPRD");
        assert_eq!(sid.profile.oracle_privilege, OraclePrivilege::Sysdba);
        assert!(sid.warnings.is_empty(), "{:?}", sid.warnings);
        let svc = find(&parsed, "ora-2");
        assert_eq!(svc.profile.oracle_connect_type, OracleConnectType::Service);
        assert_eq!(
            (svc.profile.port, svc.profile.database.as_str()),
            (1522, "erppdb")
        );
        assert_eq!(svc.profile.user, "system");
        assert_eq!(svc.profile.oracle_privilege, OraclePrivilege::Sysoper);
        let tns = find(&parsed, "ora-3");
        assert_eq!(tns.profile.oracle_connect_type, OracleConnectType::Tns);
        assert_eq!(tns.profile.tns_alias, "ERPPRD");
        let tns_dir = tns.profile.config_dir.as_deref().unwrap();
        assert_eq!(
            Path::new(tns_dir),
            super::super::home().unwrap().join("tns")
        );
        let desc = find(&parsed, "ora-4");
        assert_eq!(desc.profile.oracle_connect_type, OracleConnectType::Custom);
        assert!(desc.profile.connect_string.starts_with("(description="));

        let lite = find(&parsed, "lite-1");
        assert_eq!(lite.profile.engine, Engine::Sqlite);
        let db = project.join("data").join("app.db");
        assert_eq!(lite.profile.database, db.to_string_lossy());
        assert!(!lite.profile.save_password);

        let redis = find(&parsed, "redis-1");
        assert_eq!(redis.profile.engine, Engine::Redis);
        assert_eq!(
            (redis.profile.host.as_str(), redis.profile.port),
            ("cache.local", 6380)
        );
        assert_eq!(redis.profile.database, "2");
        assert_eq!(redis.profile.user, "default");
        assert_eq!(redis.profile.ssl_mode, SslMode::Require);
        assert_eq!(redis.folder.as_deref(), Some("dev"));

        assert_eq!(
            skipped(&parsed, "warehouse"),
            "ClickHouse is not supported by LowQuery."
        );
        assert_eq!(skipped(&parsed, "no-url"), "Has no JDBC URL.");
        assert!(skipped(&parsed, "broken").starts_with("Uses a JDBC URL"));
        assert_eq!(parsed.candidates.len(), 12);
        assert!(parsed.passwords.is_empty());
        assert_eq!(parsed.notes.len(), 1);
        assert!(parsed.notes[0].contains("keychain"));
    }

    #[test]
    fn global_file_without_local_settings() {
        let dir = TempDir::new("global");
        let path = dir.write(
            "DataGrip2024.1/options/dataSources.xml",
            r#"<application>
  <component name="dataSourceStorage" format="xml" multifile-model="true">
    <data-source source="LOCAL" name="Prod stats db" group="Development" uuid="372584fc">
      <driver-ref>postgresql</driver-ref>
      <jdbc-url>jdbc:postgresql://127.0.0.1:25432/stats</jdbc-url>
    </data-source>
    <data-source source="LOCAL" name="Snowflake" uuid="9561d350">
      <driver-ref>snowflake</driver-ref>
      <jdbc-url>jdbc:snowflake://acme.snowflakecomputing.com/?warehouse=W</jdbc-url>
    </data-source>
    <data-source source="LOCAL" name="dup" uuid="372584fc">
      <jdbc-url>jdbc:postgresql://h/x</jdbc-url>
    </data-source>
  </component>
</application>"#,
        );
        let parsed = parse(&path).unwrap();
        assert_eq!(parsed.candidates.len(), 1);
        let c = &parsed.candidates[0];
        assert_eq!(c.profile.name, "Prod stats db");
        assert_eq!(c.folder.as_deref(), Some("Development"));
        assert!(c.profile.user.is_empty());
        assert_eq!(
            skipped(&parsed, "Snowflake"),
            "Snowflake is not supported by LowQuery."
        );
        assert_eq!(parsed.notes.len(), 2);
        assert!(parsed.notes[0].contains("dataSources.local.xml"));
    }

    #[test]
    fn pgpass_password_for_pgpass_auth() {
        let dir = TempDir::new("pgpass");
        let pass = dir.write(
            "pgpass.conf",
            "# comment\nother:5432:*:app:nope\npg.local:5432:app:app:fake-pass-1\n",
        );
        assert_eq!(
            pgadmin::pgpass_password(&pass, "pg.local", 5432, "app", "app").as_deref(),
            Some("fake-pass-1")
        );
        assert_eq!(
            pgadmin::pgpass_password(&pass, "pg.local", 5433, "app", "app"),
            None
        );
    }

    #[test]
    fn rejects_other_files() {
        let dir = TempDir::new("bad");
        for (name, text) in [
            ("empty.xml", ""),
            ("json.xml", "{\"a\": 1}"),
            (
                "other.xml",
                "<project><component name=\"Other\"/></project>",
            ),
            (
                "cut.xml",
                "<project><component name=\"DataSourceManagerImpl\">",
            ),
            ("bad.xml", "<a><b></a>"),
        ] {
            let path = dir.write(name, text);
            let err = parse(&path).expect_err(name);
            assert!(err.message.contains("dataSources.xml"), "{name}");
        }
        let local = dir.write("x/dataSources.local.xml", LOCAL);
        assert!(parse(&local)
            .unwrap_err()
            .message
            .contains("local settings"));
        assert!(parse(&dir.0.join("missing.xml")).is_err());
        // Odd values do not panic.
        let odd = dir.write(
            "odd.xml",
            "<project><component name=\"DataSourceManagerImpl\"><data-source><jdbc-url>jdbc:redis:</jdbc-url></data-source><data-source name=\"\"><jdbc-url>jdbc:mysql:x</jdbc-url></data-source><data-source name=\"q\"><jdbc-url>jdbc:sqlserver://;</jdbc-url></data-source></component></project>",
        );
        let parsed = parse(&odd).unwrap();
        assert_eq!(parsed.skipped.len(), 2);
        let deep = format!(
            "<project><component name=\"DataSourceManagerImpl\">{}{}</component></project>",
            "<x>".repeat(5000),
            "</x>".repeat(5000)
        );
        let deep = dir.write("deep.xml", &deep);
        assert!(parse(&deep).unwrap().candidates.is_empty());
    }

    #[test]
    fn recent_projects_and_labels() {
        let dir = TempDir::new("recent");
        let file = dir.write(
            "recentProjects.xml",
            r#"<application>
  <component name="RecentProjectsManager">
    <option name="additionalInfo">
      <map>
        <entry key="$USER_HOME$/work/erp"><value><RecentProjectMetaInfo opened="true"/></value></entry>
      </map>
    </option>
    <option name="lastProjectLocation" value="$USER_HOME$/work" />
    <option name="recentPaths">
      <list>
        <option value="$USER_HOME$/work/shop" />
      </list>
    </option>
  </component>
</application>"#,
        );
        let projects = recent_projects(&file, Some(&dir.0));
        assert_eq!(
            projects,
            vec![
                dir.0.join("work").join("erp"),
                dir.0.join("work").join("shop")
            ]
        );
        assert_eq!(product_label("DataGrip2024.1"), "DataGrip 2024.1");
        assert_eq!(product_label("IntelliJIdea2026.2"), "IntelliJIdea 2026.2");
        let mut found = Found::default();
        let ds = dir.write("work/erp/.idea/dataSources.xml", PROJECT);
        found.add_project("DataGrip 2024.1", &dir.0.join("work/erp"));
        found.add("again".into(), ds);
        found.add_project("DataGrip 2024.1", &dir.0.join("work/none"));
        assert_eq!(found.files.len(), 1);
        assert_eq!(found.files[0].0, "DataGrip 2024.1 · erp");
        let _ = default_files();
    }

    #[test]
    fn helpers() {
        assert_eq!(hex_color("#E1F5FE").as_deref(), Some("#e1f5fe"));
        assert_eq!(hex_color("f00").as_deref(), Some("#ff0000"));
        assert_eq!(hex_color("red"), None);
        assert_eq!(
            split_oracle_role("SYS AS SYSDBA"),
            ("SYS".to_string(), Some(OraclePrivilege::Sysdba))
        );
        assert_eq!(split_oracle_role("as x"), ("as x".to_string(), None));
        assert_eq!(
            split_tns_admin("oracle:thin:@db_high?TNS_ADMIN=/w&x=1"),
            (
                "oracle:thin:@db_high?x=1".to_string(),
                Some("/w".to_string())
            )
        );
        let x = parse_xml("<a k=\"1 &amp; 2\">x &lt; y &#65;<![CDATA[<z>]]></a>").unwrap();
        assert_eq!(x.attr("k"), Some("1 & 2"));
        assert_eq!(x.text, "x < y A<z>");
    }
}
