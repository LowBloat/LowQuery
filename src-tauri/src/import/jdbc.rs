//! JDBC URLs, the common address format of Java database tools.

use crate::model::{ConnectionProfile, Engine, OracleConnectType, SslMode};

/// Fills engine, address, database and TLS settings of `profile` from a JDBC
/// URL. Returns a short reason when the URL is not one LowQuery can use.
pub fn apply_url(url: &str, profile: &mut ConnectionProfile) -> Result<(), String> {
    let url = url.trim();
    let rest = strip_prefix_ci(url, "jdbc:").ok_or("not a JDBC URL")?;
    if let Some(r) = strip_prefix_ci(rest, "postgresql:") {
        return host_db(r, Engine::Postgres, 5432, profile, pg_params);
    }
    if let Some(r) = strip_prefix_ci(rest, "mysql:") {
        return host_db(r, Engine::Mysql, 3306, profile, mysql_params);
    }
    if let Some(r) = strip_prefix_ci(rest, "mariadb:") {
        return host_db(r, Engine::Mariadb, 3306, profile, mysql_params);
    }
    if let Some(r) = strip_prefix_ci(rest, "sqlserver:") {
        return sqlserver(r, profile);
    }
    if let Some(r) = strip_prefix_ci(rest, "jtds:sqlserver:") {
        return jtds(r, profile);
    }
    if let Some(r) = strip_prefix_ci(rest, "oracle:thin:") {
        return oracle(r, profile);
    }
    if let Some(r) = strip_prefix_ci(rest, "sqlite:") {
        profile.engine = Engine::Sqlite;
        let path = r.split('?').next().unwrap_or_default();
        if path.is_empty() || path == ":memory:" {
            return Err("in-memory SQLite databases cannot be imported".into());
        }
        profile.database = path.to_string();
        profile.host.clear();
        profile.port = 0;
        return Ok(());
    }
    let scheme = rest.split(':').next().unwrap_or_default();
    Err(format!("{scheme} URLs are not supported"))
}

fn strip_prefix_ci<'a>(s: &'a str, prefix: &str) -> Option<&'a str> {
    let head = s.get(..prefix.len())?;
    head.eq_ignore_ascii_case(prefix)
        .then(|| &s[prefix.len()..])
}

/// Splits `host[:port]`, including `[ipv6]:port`.
pub fn split_host_port(s: &str, default_port: u16) -> (String, u16) {
    let s = s.trim();
    if let Some(r) = s.strip_prefix('[') {
        if let Some((h, tail)) = r.split_once(']') {
            let port = tail.strip_prefix(':').and_then(|p| p.parse().ok());
            return (h.to_string(), port.unwrap_or(default_port));
        }
    }
    match s.rsplit_once(':') {
        Some((h, p)) if !h.contains(':') => match p.parse() {
            Ok(port) => (h.to_string(), port),
            Err(_) => (s.to_string(), default_port),
        },
        _ => (s.to_string(), default_port),
    }
}

/// `//host[:port][,host2...][/database][?params]`.
fn host_db(
    rest: &str,
    engine: Engine,
    default_port: u16,
    profile: &mut ConnectionProfile,
    params: fn(&str, &str, &mut ConnectionProfile),
) -> Result<(), String> {
    profile.engine = engine;
    let r = rest.strip_prefix("//").ok_or("missing //host")?;
    let (main, query) = r.split_once('?').unwrap_or((r, ""));
    let (hosts, db) = main.split_once('/').unwrap_or((main, ""));
    // Failover lists keep the first host.
    let first = hosts.split(',').next().unwrap_or_default();
    let (host, port) = split_host_port(first, default_port);
    profile.host = if host.is_empty() {
        "localhost".into()
    } else {
        host
    };
    profile.port = port;
    profile.database = percent_decode(db);
    for pair in query.split('&').filter(|p| !p.is_empty()) {
        let (k, v) = pair.split_once('=').unwrap_or((pair, ""));
        params(k, &percent_decode(v), profile);
    }
    Ok(())
}

fn pg_params(k: &str, v: &str, p: &mut ConnectionProfile) {
    match k {
        "sslmode" => {
            if let Some(mode) = pg_ssl_mode(v) {
                p.ssl_mode = mode;
            }
        }
        "ssl" if v.eq_ignore_ascii_case("true") && p.ssl_mode == SslMode::Prefer => {
            p.ssl_mode = SslMode::Require;
        }
        "user" if p.user.is_empty() => p.user = v.to_string(),
        _ => {}
    }
}

/// libpq `sslmode` names (also used by pgAdmin and DBeaver).
pub fn pg_ssl_mode(v: &str) -> Option<SslMode> {
    Some(match v.to_ascii_lowercase().as_str() {
        "disable" | "allow" => SslMode::Disable,
        "prefer" => SslMode::Prefer,
        "require" | "verify-ca" => SslMode::Require,
        "verify-full" => SslMode::VerifyFull,
        _ => return None,
    })
}

fn mysql_params(k: &str, v: &str, p: &mut ConnectionProfile) {
    match k.to_ascii_lowercase().as_str() {
        "sslmode" => {
            p.ssl_mode = match v.to_ascii_uppercase().as_str() {
                "DISABLED" => SslMode::Disable,
                "REQUIRED" | "VERIFY_CA" => SslMode::Require,
                "VERIFY_IDENTITY" => SslMode::VerifyFull,
                _ => SslMode::Prefer,
            }
        }
        "usessl" => {
            p.ssl_mode = if v.eq_ignore_ascii_case("true") {
                SslMode::Require
            } else {
                SslMode::Disable
            }
        }
        "user" if p.user.is_empty() => p.user = v.to_string(),
        _ => {}
    }
}

/// `//host[\instance][:port][;key=value...]`.
fn sqlserver(rest: &str, p: &mut ConnectionProfile) -> Result<(), String> {
    p.engine = Engine::Sqlserver;
    let r = rest.strip_prefix("//").unwrap_or(rest);
    let mut parts = r.split(';');
    let server = parts.next().unwrap_or_default();
    let (server, port) = split_host_port(server, 1433);
    let (host, instance) = match server.split_once('\\') {
        Some((h, i)) => (h.to_string(), i.to_string()),
        None => (server, String::new()),
    };
    p.host = if host.is_empty() {
        "localhost".into()
    } else {
        host
    };
    p.port = port;
    p.instance = instance;
    let mut encrypt: Option<bool> = None;
    let mut trust = false;
    for kv in parts.filter(|s| !s.is_empty()) {
        let (k, v) = kv.split_once('=').unwrap_or((kv, ""));
        match k.trim().to_ascii_lowercase().as_str() {
            "databasename" | "database" => p.database = v.trim().to_string(),
            "instancename" => p.instance = v.trim().to_string(),
            "portnumber" | "port" => p.port = v.trim().parse().unwrap_or(p.port),
            "integratedsecurity" => p.windows_auth = v.trim().eq_ignore_ascii_case("true"),
            "encrypt" => {
                let v = v.trim().to_ascii_lowercase();
                encrypt = Some(matches!(v.as_str(), "true" | "mandatory" | "strict"));
            }
            "trustservercertificate" => trust = v.trim().eq_ignore_ascii_case("true"),
            "user" if p.user.is_empty() => p.user = v.trim().to_string(),
            _ => {}
        }
    }
    p.ssl_mode = sqlserver_ssl(encrypt, trust);
    Ok(())
}

/// Microsoft driver `encrypt` / `trustServerCertificate` as LowQuery modes.
/// The driver encrypts by default since version 10.
pub fn sqlserver_ssl(encrypt: Option<bool>, trust_server_certificate: bool) -> SslMode {
    match encrypt {
        Some(false) => SslMode::Prefer,
        Some(true) | None if trust_server_certificate => SslMode::Require,
        Some(true) => SslMode::VerifyFull,
        None => SslMode::Prefer,
    }
}

/// `//host[:port][/database][;instance=x;...]`.
fn jtds(rest: &str, p: &mut ConnectionProfile) -> Result<(), String> {
    p.engine = Engine::Sqlserver;
    let r = rest.strip_prefix("//").ok_or("missing //host")?;
    let mut parts = r.split(';');
    let main = parts.next().unwrap_or_default();
    let (server, db) = main.split_once('/').unwrap_or((main, ""));
    let (host, port) = split_host_port(server, 1433);
    p.host = host;
    p.port = port;
    p.database = db.to_string();
    for kv in parts {
        let (k, v) = kv.split_once('=').unwrap_or((kv, ""));
        match k.to_ascii_lowercase().as_str() {
            "instance" => p.instance = v.to_string(),
            "user" if p.user.is_empty() => p.user = v.to_string(),
            _ => {}
        }
    }
    Ok(())
}

/// Thin driver addresses after `jdbc:oracle:thin:`: `[user/pass]@host:port:SID`,
/// `@//host:port/service`, `@host:port/service`, a descriptor, an Easy Connect
/// Plus string or a TNS alias.
fn oracle(rest: &str, p: &mut ConnectionProfile) -> Result<(), String> {
    p.engine = Engine::Oracle;
    let target = match rest.split_once('@') {
        Some((creds, t)) => {
            if let Some((user, _)) = creds.split_once('/') {
                if p.user.is_empty() {
                    p.user = user.to_string();
                }
            }
            t.trim()
        }
        None => rest.trim(),
    };
    apply_oracle_target(target, p);
    Ok(())
}

/// Sets the Oracle connect type and fields from an address written the way
/// Oracle tools accept it.
pub fn apply_oracle_target(target: &str, p: &mut ConnectionProfile) {
    p.engine = Engine::Oracle;
    let t = target.trim();
    let custom = |p: &mut ConnectionProfile| {
        p.oracle_connect_type = OracleConnectType::Custom;
        p.connect_string = t.to_string();
    };
    if t.starts_with('(') || t.contains("://") || t.contains('?') {
        return custom(p);
    }
    if let Some(r) = t.strip_prefix("//") {
        // //host[:port]/service[:server][/instance]
        let (addr, service) = r.split_once('/').unwrap_or((r, ""));
        let (host, port) = split_host_port(addr, 1521);
        let service = service.split(['/', ':']).next().unwrap_or_default();
        p.oracle_connect_type = OracleConnectType::Service;
        p.host = host;
        p.port = port;
        p.database = service.to_string();
        return;
    }
    if !t.contains([':', '/']) {
        // A bare name is a TNS alias.
        p.oracle_connect_type = OracleConnectType::Tns;
        p.tns_alias = t.to_string();
        return;
    }
    if let Some((addr, service)) = t.split_once('/') {
        let (host, port) = split_host_port(addr, 1521);
        p.oracle_connect_type = OracleConnectType::Service;
        p.host = host;
        p.port = port;
        p.database = service
            .split(['/', ':'])
            .next()
            .unwrap_or_default()
            .to_string();
        return;
    }
    // host:port:SID
    let parts: Vec<&str> = t.split(':').collect();
    if let [host, port, sid] = parts[..] {
        if let Ok(port) = port.parse() {
            p.oracle_connect_type = OracleConnectType::Sid;
            p.host = host.to_string();
            p.port = port;
            p.database = sid.to_string();
            return;
        }
    }
    custom(p)
}

fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let Some(b) = s
                .get(i + 1..i + 3)
                .and_then(|h| u8::from_str_radix(h, 16).ok())
            {
                out.push(b);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(url: &str) -> ConnectionProfile {
        let mut p = ConnectionProfile::default();
        apply_url(url, &mut p).unwrap();
        p
    }

    #[test]
    fn postgres_and_mysql() {
        let p = parse("jdbc:postgresql://db.internal:5433/sales?sslmode=verify-full&user=ana");
        assert_eq!(
            (p.engine, p.host.as_str(), p.port),
            (Engine::Postgres, "db.internal", 5433)
        );
        assert_eq!(p.database, "sales");
        assert_eq!(p.ssl_mode, SslMode::VerifyFull);
        assert_eq!(p.user, "ana");
        let p = parse("jdbc:postgresql://a,b:5432/x");
        assert_eq!((p.host.as_str(), p.port), ("a", 5432));
        let p = parse("jdbc:mysql://[::1]:3307/shop?useSSL=false");
        assert_eq!(
            (p.engine, p.host.as_str(), p.port),
            (Engine::Mysql, "::1", 3307)
        );
        assert_eq!(p.ssl_mode, SslMode::Disable);
        let p = parse("jdbc:mariadb://m/db%20x");
        assert_eq!(
            (p.engine, p.port, p.database.as_str()),
            (Engine::Mariadb, 3306, "db x")
        );
    }

    #[test]
    fn sqlserver_forms() {
        let p = parse(
            "jdbc:sqlserver://fin-sql\\REPORTING;databaseName=Finance;integratedSecurity=true;encrypt=true;trustServerCertificate=true",
        );
        assert_eq!(p.engine, Engine::Sqlserver);
        assert_eq!(
            (p.host.as_str(), p.instance.as_str()),
            ("fin-sql", "REPORTING")
        );
        assert_eq!(p.database, "Finance");
        assert!(p.windows_auth);
        assert_eq!(p.ssl_mode, SslMode::Require);
        let p = parse("jdbc:sqlserver://h:1500;database=d;encrypt=false");
        assert_eq!(
            (p.port, p.database.as_str(), p.ssl_mode),
            (1500, "d", SslMode::Prefer)
        );
        let p = parse("jdbc:jtds:sqlserver://h:1433/db;instance=SQLEXPRESS");
        assert_eq!(
            (p.database.as_str(), p.instance.as_str()),
            ("db", "SQLEXPRESS")
        );
    }

    #[test]
    fn oracle_forms() {
        let p = parse("jdbc:oracle:thin:@erp:1521:ERPPRD");
        assert_eq!(p.oracle_connect_type, OracleConnectType::Sid);
        assert_eq!(
            (p.host.as_str(), p.port, p.database.as_str()),
            ("erp", 1521, "ERPPRD")
        );
        let p = parse("jdbc:oracle:thin:@//erp.internal:1522/erppdb");
        assert_eq!(p.oracle_connect_type, OracleConnectType::Service);
        assert_eq!(
            (p.host.as_str(), p.port, p.database.as_str()),
            ("erp.internal", 1522, "erppdb")
        );
        let p = parse("jdbc:oracle:thin:@erp/erppdb");
        assert_eq!((p.port, p.database.as_str()), (1521, "erppdb"));
        let p = parse("jdbc:oracle:thin:@ERPPRD");
        assert_eq!(p.oracle_connect_type, OracleConnectType::Tns);
        assert_eq!(p.tns_alias, "ERPPRD");
        let d = "(DESCRIPTION=(ADDRESS=(PROTOCOL=TCP)(HOST=h)(PORT=1521))(CONNECT_DATA=(SERVICE_NAME=s)))";
        let p = parse(&format!("jdbc:oracle:thin:@{d}"));
        assert_eq!(p.oracle_connect_type, OracleConnectType::Custom);
        assert_eq!(p.connect_string, d);
        let p = parse("jdbc:oracle:thin:scott/tiger@h:1521:X");
        assert_eq!(p.user, "scott");
    }

    #[test]
    fn sqlite_and_unsupported() {
        let p = parse("jdbc:sqlite:C:\\data\\app.db");
        assert_eq!(
            (p.engine, p.database.as_str()),
            (Engine::Sqlite, "C:\\data\\app.db")
        );
        let mut p = ConnectionProfile::default();
        assert!(apply_url("jdbc:db2://h:50000/x", &mut p)
            .unwrap_err()
            .contains("db2"));
        assert!(apply_url("jdbc:sqlite::memory:", &mut p).is_err());
        assert!(apply_url("postgres://h/x", &mut p).is_err());
    }
}
