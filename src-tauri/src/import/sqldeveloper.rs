//! Oracle SQL Developer: `connections.json` (19.x and later) or the older
//! `connections.xml` (JNDI `References`), both in
//! `system<version>/o.jdeveloper.db.connection*/` under the user folder.
//! Saved passwords are encrypted with a key derived from the installation's
//! `db.system.id` and are not imported.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use serde_json::Value;

use super::datagrip::{
    apply_jdbc, invalid_reason, oracle_privilege, parse_xml, read_text, subdirs, UrlError,
};
use super::{ImportCandidate, Parsed};
use crate::error::{AppError, AppResult};
use crate::model::{ConnectionProfile, Engine, OracleConnectType};

/// `%APPDATA%\SQL Developer` on Windows, `~/.sqldeveloper` on macOS and
/// Linux, or `SQLDEVELOPER_USER_DIR`; one file per `system*` folder.
pub fn default_files() -> Vec<(String, PathBuf)> {
    let mut roots = Vec::new();
    if let Some(dir) = std::env::var_os("SQLDEVELOPER_USER_DIR").filter(|d| !d.is_empty()) {
        roots.push(PathBuf::from(dir));
    }
    if cfg!(windows) {
        if let Some(dir) = super::config_dir() {
            roots.push(dir.join("SQL Developer"));
        }
    }
    if let Some(home) = super::home() {
        roots.push(home.join(".sqldeveloper"));
    }
    files_in(&roots)
}

fn files_in(roots: &[PathBuf]) -> Vec<(String, PathBuf)> {
    let mut seen = HashSet::new();
    let mut out = Vec::new();
    for root in roots {
        for system in subdirs(root) {
            let name = system
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default();
            let Some(version) = name.strip_prefix("system") else {
                continue;
            };
            for conn_dir in subdirs(&system) {
                let is_conn = conn_dir.file_name().is_some_and(|n| {
                    n.to_string_lossy()
                        .starts_with("o.jdeveloper.db.connection")
                });
                if !is_conn {
                    continue;
                }
                let json = conn_dir.join("connections.json");
                let xml = conn_dir.join("connections.xml");
                let file = if json.is_file() {
                    json
                } else if xml.is_file() {
                    xml
                } else {
                    continue;
                };
                let id = std::fs::canonicalize(&file).unwrap_or_else(|_| file.clone());
                if seen.insert(id.to_string_lossy().to_lowercase()) {
                    out.push((version_label(version), file));
                }
            }
        }
    }
    out
}

/// "23.1.0.097.1607" → "SQL Developer 23.1", "23.1.1.345.2114" →
/// "SQL Developer 23.1.1".
fn version_label(version: &str) -> String {
    let parts: Vec<&str> = version.split('.').take(3).collect();
    let short = match parts[..] {
        [a, b, "0"] => format!("{a}.{b}"),
        _ if parts.iter().all(|p| !p.is_empty()) && !parts.is_empty() => parts.join("."),
        _ => return "SQL Developer".into(),
    };
    format!("SQL Developer {short}")
}

const NOT_OURS: &str = "This is not a SQL Developer connections.json or connections.xml file.";

pub fn parse(path: &Path) -> AppResult<Parsed> {
    let text = read_text(path)?;
    let conns = match text.trim_start().chars().next() {
        Some('{') => from_json(&text)?,
        Some('<') => from_xml(&text)?,
        _ => return Err(AppError::invalid(NOT_OURS)),
    };
    let mut parsed = Parsed::default();
    let mut keys = HashSet::new();
    let mut saved = false;
    for (i, (name, info)) in conns.into_iter().enumerate() {
        let name = if name.is_empty() {
            format!("Connection {}", i + 1)
        } else {
            name
        };
        let mut key = name.clone();
        let mut n = 1;
        while !keys.insert(key.clone()) {
            n += 1;
            key = format!("{name}#{n}");
        }
        if let Some(c) = convert(key, &name, &info, &mut parsed) {
            saved |= c.profile.save_password;
            parsed.candidates.push(c);
        }
    }
    if saved {
        parsed.notes.push(
            "SQL Developer encrypts saved passwords with a key tied to its installation (db.system.id), so LowQuery does not import them: type them on first connect."
                .into(),
        );
    }
    Ok(parsed)
}

type Info = HashMap<String, String>;

/// `{"connections": [{"name", "type", "info": {...}}]}`.
fn from_json(text: &str) -> AppResult<Vec<(String, Info)>> {
    let root: Value = serde_json::from_str(text).map_err(|_| AppError::invalid(NOT_OURS))?;
    let Some(list) = root.get("connections").and_then(Value::as_array) else {
        return Err(AppError::invalid(NOT_OURS));
    };
    let mut out = Vec::new();
    for c in list {
        let mut info = Info::new();
        if let Some(map) = c.get("info").and_then(Value::as_object) {
            for (k, v) in map {
                let v = match v {
                    Value::String(s) => s.clone(),
                    Value::Number(n) => n.to_string(),
                    Value::Bool(b) => b.to_string(),
                    _ => continue,
                };
                info.insert(k.clone(), v);
            }
        }
        if let Some(t) = c.get("type").and_then(Value::as_str) {
            info.insert("type".into(), t.to_string());
        }
        let name = c
            .get("name")
            .and_then(Value::as_str)
            .map(str::to_string)
            .or_else(|| info.get("ConnName").cloned())
            .unwrap_or_default();
        out.push((name.trim().to_string(), info));
    }
    Ok(out)
}

/// `<References><Reference name><RefAddresses><StringRefAddr addrType>
/// <Contents>`.
fn from_xml(text: &str) -> AppResult<Vec<(String, Info)>> {
    let root = parse_xml(text).ok_or_else(|| AppError::invalid(NOT_OURS))?;
    if root.name != "References" {
        return Err(AppError::invalid(NOT_OURS));
    }
    let mut out = Vec::new();
    for r in root.children("Reference") {
        let mut info = Info::new();
        for addrs in r.children("RefAddresses") {
            for a in addrs.children.iter() {
                if let Some(k) = a.attr("addrType") {
                    let v = a
                        .child("Contents")
                        .map(|c| c.text.trim())
                        .unwrap_or_default();
                    info.insert(k.to_string(), v.to_string());
                }
            }
        }
        let name = r
            .attr("name")
            .map(str::to_string)
            .or_else(|| info.get("ConnName").cloned())
            .unwrap_or_default();
        out.push((name.trim().to_string(), info));
    }
    Ok(out)
}

fn convert(key: String, name: &str, info: &Info, parsed: &mut Parsed) -> Option<ImportCandidate> {
    let get = |k: &str| info.get(k).map(|v| v.trim()).filter(|v| !v.is_empty());
    let flag = |k: &str| get(k).is_some_and(|v| v.eq_ignore_ascii_case("true"));
    let mut p = ConnectionProfile {
        name: name.to_string(),
        save_password: flag("SavePassword"),
        ..ConnectionProfile::default()
    };
    let mut warnings = Vec::new();
    let url = get("customUrl");
    let jdbc_url = url.filter(|u| u.get(..5).is_some_and(|h| h.eq_ignore_ascii_case("jdbc:")));
    let raptor = get("RaptorConnectionType").unwrap_or("Oracle");
    let skip = |parsed: &mut Parsed, reason: String| {
        parsed.skip(name, reason);
        None
    };
    let url_error = |e: UrlError| match e {
        UrlError::Unsupported(product) => format!("{product} is not supported by LowQuery."),
        UrlError::Invalid(r) => format!("Uses a JDBC URL LowQuery cannot read: {r}."),
    };

    if raptor.eq_ignore_ascii_case("Oracle") {
        p.engine = Engine::Oracle;
        p.port = 1521;
        let kind = get("OracleConnectionType")
            .unwrap_or("BASIC")
            .to_ascii_uppercase();
        match kind.as_str() {
            "BASIC" => {
                if let (Some(host), Some(target)) =
                    (get("hostname"), get("sid").or(get("serviceName")))
                {
                    p.host = host.to_string();
                    p.port = get("port").and_then(|v| v.parse().ok()).unwrap_or(1521);
                    p.database = target.to_string();
                    p.oracle_connect_type = if get("sid").is_some() {
                        OracleConnectType::Sid
                    } else {
                        OracleConnectType::Service
                    };
                } else if let Some(u) = jdbc_url {
                    if let Err(e) = apply_jdbc(u, &mut p) {
                        return skip(parsed, url_error(e));
                    }
                } else {
                    return skip(parsed, "Has no host and SID or service name.".into());
                }
            }
            "TNS" => {
                // The alias is stored in customUrl.
                match (jdbc_url, url) {
                    (Some(u), _) => {
                        if let Err(e) = apply_jdbc(u, &mut p) {
                            return skip(parsed, url_error(e));
                        }
                    }
                    (None, Some(alias)) => {
                        p.oracle_connect_type = OracleConnectType::Tns;
                        p.tns_alias = alias.to_string();
                    }
                    (None, None) => return skip(parsed, "Has no TNS alias.".into()),
                }
            }
            "LDAP" => {
                return skip(parsed, "LDAP naming is not supported by LowQuery.".into());
            }
            k if k.contains("WALLET") || k.contains("CLOUD") => {
                return skip(
                    parsed,
                    "Cloud Wallet connections are not supported by LowQuery.".into(),
                );
            }
            // ADVANCED is the Custom JDBC URL type.
            other => match jdbc_url {
                Some(u) => {
                    if let Err(e) = apply_jdbc(u, &mut p) {
                        return skip(parsed, url_error(e));
                    }
                }
                None => {
                    return skip(
                        parsed,
                        format!("The {other} connection type is not supported by LowQuery."),
                    )
                }
            },
        }
        if p.engine != Engine::Oracle {
            return skip(parsed, "Uses a JDBC URL that is not Oracle's.".into());
        }
        if let Some(privilege) = get("role").and_then(oracle_privilege) {
            p.oracle_privilege = privilege;
        }
        if flag("OS_AUTHENTICATION") {
            warnings.push(
                "OS authentication is not carried over: LowQuery signs in with a user and password."
                    .into(),
            );
        }
        if flag("KERBEROS_AUTHENTICATION") {
            warnings.push("Kerberos authentication is not carried over.".into());
        }
        if flag("IS_PROXY") {
            warnings.push(match get("PROXY_USER_NAME") {
                Some(u) => format!("Proxy authentication as {u} is not carried over."),
                None => "Proxy authentication is not carried over.".into(),
            });
        }
    } else {
        let lower = raptor.to_ascii_lowercase();
        if lower.contains("sybase") {
            return skip(parsed, "Sybase is not supported by LowQuery.".into());
        }
        if let Some(u) = jdbc_url {
            if let Err(e) = apply_jdbc(u, &mut p) {
                return skip(parsed, url_error(e));
            }
        } else {
            let (engine, port) = if lower.contains("mysql") {
                (Engine::Mysql, 3306)
            } else if lower.contains("postgres") {
                (Engine::Postgres, 5432)
            } else if lower.contains("sqlserver") || lower.contains("jtds") {
                (Engine::Sqlserver, 1433)
            } else {
                return skip(parsed, format!("{raptor} is not supported by LowQuery."));
            };
            let Some(host) = get("hostname") else {
                return skip(parsed, "Has no host.".into());
            };
            p.engine = engine;
            p.host = host.to_string();
            p.port = get("port").and_then(|v| v.parse().ok()).unwrap_or(port);
            p.database = get("databaseName")
                .or(get("database"))
                .unwrap_or_default()
                .to_string();
        }
    }

    if let Some(user) = get("user") {
        p.user = user.to_string();
    }
    p.color = get("Connection-Color-For-Editors").and_then(java_color);
    if let Some(reason) = invalid_reason(&p) {
        return skip(parsed, reason.into());
    }
    Some(ImportCandidate {
        key,
        profile: p,
        folder: get("NAV_FOLDER").map(str::to_string),
        has_password: false,
        warnings,
    })
}

/// Java `Color.getRGB()` (signed ARGB) as `#rrggbb`.
fn java_color(v: &str) -> Option<String> {
    let n: i64 = v.trim().parse().ok()?;
    let rgb = (n as u32) & 0x00ff_ffff;
    Some(format!("#{rgb:06x}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{OraclePrivilege, SslMode};

    struct TempDir(PathBuf);

    impl TempDir {
        fn new(tag: &str) -> Self {
            let dir = std::env::temp_dir().join(format!(
                "lowquery-sqldev-{tag}-{}-{}",
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

    const JSON: &str = r#"{"connections":[
{"info":{"role":"SYSDBA","SavePassword":"true","OracleConnectionType":"BASIC","PROXY_TYPE":"USER NAME","RaptorConnectionType":"Oracle","sid":"FREE","NAV_FOLDER":"OraDBA/cdbfree","customUrl":"jdbc:oracle:thin:@localhost:1526:FREE","oraDriverType":"thin","NoPasswordConnection":"TRUE","hostname":"localhost","password":"v2_ZmFrZQ==","driver":"oracle.jdbc.OracleDriver","subtype":"oraJDBC","port":"1526","OS_AUTHENTICATION":"false","IS_PROXY":"false","KERBEROS_AUTHENTICATION":"false","user":"sys","PROXY_USER_NAME":""},"name":"cdbfree CDB$ROOT","type":"jdbc"},
{"info":{"role":"","SavePassword":"false","OracleConnectionType":"BASIC","RaptorConnectionType":"Oracle","serviceName":"erppdb","Connection-Color-For-Editors":"-2350809","customUrl":"jdbc:oracle:thin:@//erp-db:1522/erppdb","hostname":"erp-db","port":"1522","OS_AUTHENTICATION":"false","IS_PROXY":"true","PROXY_USER_NAME":"app_proxy","KERBEROS_AUTHENTICATION":"true","user":"erp"},"name":"ERP","type":"jdbc"},
{"info":{"OracleConnectionType":"TNS","RaptorConnectionType":"Oracle","customUrl":"ERPPRD.WORLD","user":"app","OS_AUTHENTICATION":"true"},"name":"tns","type":"jdbc"},
{"info":{"OracleConnectionType":"ADVANCED","RaptorConnectionType":"Oracle","customUrl":"jdbc:oracle:thin:@(DESCRIPTION=(ADDRESS=(PROTOCOL=TCP)(HOST=a)(PORT=1521))(CONNECT_DATA=(SERVICE_NAME=eam)))","user":"maximo"},"name":"advanced","type":"jdbc"},
{"info":{"OracleConnectionType":"ADVANCED","RaptorConnectionType":"Oracle","customUrl":"jdbc:oracle:thin:@db_high?TNS_ADMIN=/opt/wallet","user":"admin"},"name":"url-tns","type":"jdbc"},
{"info":{"OracleConnectionType":"LDAP","RaptorConnectionType":"Oracle","user":"x"},"name":"ldap","type":"jdbc"},
{"info":{"OracleConnectionType":"CLOUD_WALLET","RaptorConnectionType":"Oracle","user":"x"},"name":"cloud","type":"jdbc"},
{"info":{"RaptorConnectionType":"MySQL","customUrl":"jdbc:mysql://my.local:3307/shop","hostname":"my.local","port":"3307","user":"root","SavePassword":"true"},"name":"mysql","type":"jdbc"},
{"info":{"RaptorConnectionType":"SQLServer","customUrl":"jdbc:jtds:sqlserver://mssql.local:1433/Sales;instance=SQL2019","user":"sa"},"name":"mssql","type":"jdbc"},
{"info":{"RaptorConnectionType":"PostgreSQL","hostname":"pg.local","port":"5433","databaseName":"app","user":"app"},"name":"pg","type":"jdbc"},
{"info":{"RaptorConnectionType":"DB2","hostname":"db2.local","port":"50000","user":"x"},"name":"db2","type":"jdbc"},
{"info":{"RaptorConnectionType":"Sybase","hostname":"syb.local"},"name":"sybase","type":"jdbc"},
{"info":{"OracleConnectionType":"BASIC","RaptorConnectionType":"Oracle","hostname":"h"},"name":"no-target","type":"jdbc"},
{"info":{"OracleConnectionType":"BASIC"},"name":"ERP","type":"jdbc"},
{"name":"broken","info":"x"},
{"info":{"OracleConnectionType":"BASIC","sid":"X","hostname":"h","port":"x","role":"default"},"type":"jdbc","name":""}
]}"#;

    #[test]
    fn connections_json() {
        let dir = TempDir::new("json");
        let path = dir.write("connections.json", JSON);
        let parsed = parse(&path).unwrap();

        let cdb = find(&parsed, "cdbfree CDB$ROOT");
        assert_eq!(cdb.profile.engine, Engine::Oracle);
        assert_eq!(cdb.profile.oracle_connect_type, OracleConnectType::Sid);
        assert_eq!(
            (
                cdb.profile.host.as_str(),
                cdb.profile.port,
                cdb.profile.database.as_str()
            ),
            ("localhost", 1526, "FREE")
        );
        assert_eq!(cdb.profile.user, "sys");
        assert_eq!(cdb.profile.oracle_privilege, OraclePrivilege::Sysdba);
        assert_eq!(cdb.folder.as_deref(), Some("OraDBA/cdbfree"));
        assert!(cdb.profile.save_password);
        assert!(cdb.warnings.is_empty());

        let erp = find(&parsed, "ERP");
        assert_eq!(erp.profile.oracle_connect_type, OracleConnectType::Service);
        assert_eq!(
            (erp.profile.port, erp.profile.database.as_str()),
            (1522, "erppdb")
        );
        assert_eq!(erp.profile.color.as_deref(), Some("#dc2127"));
        assert!(!erp.profile.save_password);
        assert_eq!(erp.warnings.len(), 2, "{:?}", erp.warnings);
        assert!(erp.warnings.iter().any(|w| w.contains("app_proxy")));

        let tns = find(&parsed, "tns");
        assert_eq!(tns.profile.oracle_connect_type, OracleConnectType::Tns);
        assert_eq!(tns.profile.tns_alias, "ERPPRD.WORLD");
        assert!(tns.warnings[0].contains("OS authentication"));

        let adv = find(&parsed, "advanced");
        assert_eq!(adv.profile.oracle_connect_type, OracleConnectType::Custom);
        assert!(adv.profile.connect_string.starts_with("(DESCRIPTION="));
        let url_tns = find(&parsed, "url-tns");
        assert_eq!(url_tns.profile.tns_alias, "db_high");
        assert_eq!(url_tns.profile.config_dir.as_deref(), Some("/opt/wallet"));

        assert!(skipped(&parsed, "ldap").contains("LDAP"));
        assert!(skipped(&parsed, "cloud").contains("Cloud Wallet"));

        let my = find(&parsed, "mysql");
        assert_eq!(
            (
                my.profile.engine,
                my.profile.port,
                my.profile.database.as_str()
            ),
            (Engine::Mysql, 3307, "shop")
        );
        assert_eq!(my.profile.ssl_mode, SslMode::Prefer);
        let ms = find(&parsed, "mssql");
        assert_eq!(ms.profile.engine, Engine::Sqlserver);
        assert_eq!(
            (ms.profile.instance.as_str(), ms.profile.database.as_str()),
            ("SQL2019", "Sales")
        );
        assert_eq!(ms.profile.user, "sa");
        let pg = find(&parsed, "pg");
        assert_eq!(
            (
                pg.profile.engine,
                pg.profile.port,
                pg.profile.database.as_str()
            ),
            (Engine::Postgres, 5433, "app")
        );

        assert_eq!(skipped(&parsed, "db2"), "DB2 is not supported by LowQuery.");
        assert_eq!(
            skipped(&parsed, "sybase"),
            "Sybase is not supported by LowQuery."
        );
        assert!(skipped(&parsed, "no-target").contains("SID or service"));
        // The second "ERP" gets its own key.
        assert!(parsed.skipped.iter().any(|s| s.name == "ERP"));
        assert!(skipped(&parsed, "broken").contains("SID or service"));
        let unnamed = find(&parsed, "Connection 16");
        assert_eq!(unnamed.profile.name, "Connection 16");
        assert_eq!(unnamed.profile.port, 1521);
        assert_eq!(unnamed.profile.oracle_privilege, OraclePrivilege::None);

        assert!(parsed.passwords.is_empty());
        assert_eq!(parsed.notes.len(), 1);
        assert!(parsed.notes[0].contains("db.system.id"));
    }

    const XML: &str = r#"<?xml version = '1.0' encoding = 'UTF-8'?>
<References xmlns="http://xmlns.oracle.com/adf/jndi">
   <Reference name="SMDEV1" className="oracle.jdeveloper.db.adapter.DatabaseProvider" xmlns="">
      <Factory className="oracle.jdeveloper.db.adapter.DatabaseProviderFactory"/>
      <RefAddresses>
         <StringRefAddr addrType="OracleConnectionType"><Contents>TNS</Contents></StringRefAddr>
         <StringRefAddr addrType="user"><Contents>plateau</Contents></StringRefAddr>
         <StringRefAddr addrType="role"><Contents/></StringRefAddr>
         <StringRefAddr addrType="RaptorConnectionType"><Contents>Oracle</Contents></StringRefAddr>
         <StringRefAddr addrType="customUrl"><Contents>SMDEV1</Contents></StringRefAddr>
         <StringRefAddr addrType="SavePassword"><Contents>false</Contents></StringRefAddr>
      </RefAddresses>
   </Reference>
   <Reference name="Orcl" className="oracle.jdeveloper.db.adapter.DatabaseProvider" xmlns="">
      <RefAddresses>
         <StringRefAddr addrType="OracleConnectionType"><Contents>BASIC</Contents></StringRefAddr>
         <StringRefAddr addrType="sid"><Contents>ORCL</Contents></StringRefAddr>
         <StringRefAddr addrType="hostname"><Contents>db &amp; co</Contents></StringRefAddr>
         <StringRefAddr addrType="port"><Contents>1521</Contents></StringRefAddr>
         <StringRefAddr addrType="user"><Contents>matthew</Contents></StringRefAddr>
         <StringRefAddr addrType="role"><Contents>SYSOPER</Contents></StringRefAddr>
         <StringRefAddr addrType="password"><Contents>fake-encrypted</Contents></StringRefAddr>
         <StringRefAddr addrType="SavePassword"><Contents>true</Contents></StringRefAddr>
         <StringRefAddr addrType="RaptorConnectionType"><Contents>Oracle</Contents></StringRefAddr>
      </RefAddresses>
   </Reference>
</References>"#;

    #[test]
    fn connections_xml() {
        let dir = TempDir::new("xml");
        let path = dir.write("connections.xml", XML);
        let parsed = parse(&path).unwrap();
        assert_eq!(parsed.candidates.len(), 2);
        let tns = find(&parsed, "SMDEV1");
        assert_eq!(tns.profile.oracle_connect_type, OracleConnectType::Tns);
        assert_eq!(tns.profile.tns_alias, "SMDEV1");
        assert_eq!(tns.profile.user, "plateau");
        assert!(!tns.profile.save_password);
        let orcl = find(&parsed, "Orcl");
        assert_eq!(orcl.profile.oracle_connect_type, OracleConnectType::Sid);
        assert_eq!(orcl.profile.host, "db & co");
        assert_eq!(orcl.profile.oracle_privilege, OraclePrivilege::Sysoper);
        assert!(parsed.passwords.is_empty());
        assert_eq!(parsed.notes.len(), 1);
    }

    #[test]
    fn rejects_other_files() {
        let dir = TempDir::new("bad");
        for (name, text) in [
            ("empty.json", ""),
            ("other.json", "{\"Servers\": {}}"),
            ("list.json", "{\"connections\": 3}"),
            ("cut.json", "{\"connections\": ["),
            ("other.xml", "<project/>"),
            ("cut.xml", "<References><Reference>"),
            ("text.txt", "hello"),
        ] {
            let path = dir.write(name, text);
            let err = parse(&path).expect_err(name);
            assert_eq!(err.message, NOT_OURS, "{name}");
        }
        let empty = dir.write("ok.json", "{\"connections\": []}");
        assert!(parse(&empty).unwrap().candidates.is_empty());
    }

    #[test]
    fn labels_and_colors() {
        assert_eq!(version_label("23.1.0.097.1607"), "SQL Developer 23.1");
        assert_eq!(version_label("23.1.1.345.2114"), "SQL Developer 23.1.1");
        assert_eq!(version_label("x.x"), "SQL Developer x.x");
        assert_eq!(version_label(""), "SQL Developer");
        assert_eq!(java_color("-3750202").as_deref(), Some("#c6c6c6"));
        assert_eq!(java_color("-65536").as_deref(), Some("#ff0000"));
        assert_eq!(java_color("red"), None);
        let _ = default_files();
    }

    #[test]
    fn finds_every_version() {
        let dir = TempDir::new("dirs");
        dir.write(
            "system23.1.0.097.1607/o.jdeveloper.db.connection/connections.json",
            "{\"connections\": []}",
        );
        dir.write(
            "system17.3.0.271.2323/o.jdeveloper.db.connection.17.3.0.271.2323/connections.xml",
            XML,
        );
        dir.write(
            "system4.1.5.21.78/o.sqldeveloper/product-preferences.xml",
            "<x/>",
        );
        let files = files_in(&[dir.0.clone(), dir.0.join("missing")]);
        let labels: Vec<&str> = files.iter().map(|(l, _)| l.as_str()).collect();
        assert_eq!(labels, vec!["SQL Developer 17.3", "SQL Developer 23.1"]);
    }
}
