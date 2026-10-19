//! Oracle network configuration: finds `tnsnames.ora` the way Oracle clients
//! do and reads it without requiring UTF-8.
//!
//! Search order: `TNS_ADMIN`, `ORACLE_HOME/network/admin`, the Oracle homes
//! registered in the Windows registry, Oracle clients and Instant Clients on
//! the library path, then the system folders used on Linux and macOS.
//!
//! Corporate `tnsnames.ora` files are often saved as Latin-1 (accented
//! comments). The driver's own lookup rejects such files, so aliases are
//! resolved here and the driver receives the full descriptor.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use serde::Serialize;

use crate::error::{AppError, AppResult};

const TNSNAMES: &str = "tnsnames.ora";
const MAX_IFILE_DEPTH: usize = 8;

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TnsEntry {
    pub alias: String,
    pub descriptor: String,
    pub host: Option<String>,
    pub port: Option<u16>,
    pub service_name: Option<String>,
    pub sid: Option<String>,
    pub protocol: Option<String>,
}

/// A folder with a `tnsnames.ora`, as shown in the "Oracle client" picker.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TnsSource {
    pub dir: String,
    /// Oracle home name (`OraClient19Home1`) or the folder itself.
    pub label: String,
    /// Where it was found: `TNS_ADMIN`, `ORACLE_HOME`, `Windows registry`...
    pub origin: String,
    pub entries: Vec<TnsEntry>,
    pub error: Option<String>,
}

#[derive(Debug, Clone)]
struct Candidate {
    dir: PathBuf,
    label: String,
    origin: String,
}

/// Every folder with a `tnsnames.ora` found on this machine, in search order.
pub fn discover() -> Vec<TnsSource> {
    let env = |name: &str| std::env::var(name).ok().filter(|v| !v.trim().is_empty());
    let mut seen = HashSet::new();
    candidates(&env)
        .into_iter()
        .filter(|c| c.dir.join(TNSNAMES).is_file())
        .filter(|c| seen.insert(path_key(&c.dir)))
        .map(|c| read_source(&c.dir, c.label, c.origin))
        .collect()
}

/// Reads a folder picked by the user. Accepts the `network/admin` folder,
/// an Oracle home or Instant Client folder, or the `tnsnames.ora` file.
pub fn read_folder(path: &str) -> AppResult<TnsSource> {
    let dir = normalize_dir(Path::new(path.trim()))
        .ok_or_else(|| AppError::not_found(format!("No {TNSNAMES} found in {path}.")))?;
    let label = dir.display().to_string();
    Ok(read_source(&dir, label, "Selected folder".into()))
}

/// Resolves an alias to its descriptor. With `config_dir`, only that folder
/// is used; otherwise the discovered folders are searched in order.
pub fn resolve(alias: &str, config_dir: Option<&str>) -> AppResult<(String, PathBuf)> {
    let alias = alias.trim();
    if alias.is_empty() {
        return Err(AppError::invalid("Choose a TNS alias."));
    }
    let sources = match config_dir.filter(|d| !d.trim().is_empty()) {
        Some(dir) => vec![read_folder(dir)?],
        None => discover(),
    };
    if sources.is_empty() {
        return Err(AppError::connection(format!(
            "No {TNSNAMES} found. Set TNS_ADMIN, install an Oracle client or choose the folder in the connection."
        )));
    }
    for source in &sources {
        let dir = PathBuf::from(&source.dir);
        if let Some(entry) = find(&source.entries, alias, default_domain(&dir).as_deref()) {
            return Ok((entry.descriptor.clone(), dir));
        }
    }
    let searched: Vec<String> = sources
        .iter()
        .map(|s| Path::new(&s.dir).join(TNSNAMES).display().to_string())
        .collect();
    Err(AppError::connection(format!(
        "TNS alias \"{alias}\" was not found in {}.",
        searched.join(", ")
    )))
}

/// The first discovered folder, used for `sqlnet.ora` and aliases typed in a
/// custom connect string when the profile has no folder of its own.
pub fn default_dir() -> Option<PathBuf> {
    discover().into_iter().next().map(|s| PathBuf::from(s.dir))
}

/// Aliases match without case; `NAMES.DEFAULT_DOMAIN` from `sqlnet.ora` is
/// appended when the bare name is not defined, as Oracle clients do.
fn find<'a>(entries: &'a [TnsEntry], alias: &str, domain: Option<&str>) -> Option<&'a TnsEntry> {
    let exact = entries.iter().find(|e| e.alias.eq_ignore_ascii_case(alias));
    exact.or_else(|| {
        let domain = domain?;
        if alias.contains('.') {
            return None;
        }
        let qualified = format!("{alias}.{domain}");
        entries
            .iter()
            .find(|e| e.alias.eq_ignore_ascii_case(&qualified))
    })
}

fn default_domain(dir: &Path) -> Option<String> {
    let text = decode(&std::fs::read(dir.join("sqlnet.ora")).ok()?);
    parse(&text)
        .into_iter()
        .find(|(keys, _)| {
            keys.iter()
                .any(|k| k.eq_ignore_ascii_case("names.default_domain"))
        })
        .map(|(_, value)| value.trim().to_string())
        .filter(|v| !v.is_empty())
}

fn read_source(dir: &Path, label: String, origin: String) -> TnsSource {
    let mut entries = Vec::new();
    let mut error = None;
    let mut visited = HashSet::new();
    if let Err(e) = read_file(&dir.join(TNSNAMES), dir, &mut entries, &mut visited, 0) {
        error = Some(e);
    }
    // A later definition replaces an earlier one, keeping its position.
    let mut index: HashMap<String, usize> = HashMap::new();
    let mut unique: Vec<TnsEntry> = Vec::new();
    for entry in entries {
        match index.get(&entry.alias.to_ascii_lowercase()) {
            Some(&i) => unique[i] = entry,
            None => {
                index.insert(entry.alias.to_ascii_lowercase(), unique.len());
                unique.push(entry);
            }
        }
    }
    TnsSource {
        dir: dir.display().to_string(),
        label,
        origin,
        entries: unique,
        error,
    }
}

fn read_file(
    file: &Path,
    dir: &Path,
    out: &mut Vec<TnsEntry>,
    visited: &mut HashSet<String>,
    depth: usize,
) -> Result<(), String> {
    if depth > MAX_IFILE_DEPTH || !visited.insert(path_key(file)) {
        return Err(format!(
            "IFILE cycle or nesting too deep at {}",
            file.display()
        ));
    }
    let bytes = std::fs::read(file).map_err(|e| format!("{}: {e}", file.display()))?;
    for (keys, value) in parse(&decode(&bytes)) {
        if keys.len() == 1 && keys[0].eq_ignore_ascii_case("ifile") {
            let included = dir.join(value.trim().trim_matches('"'));
            read_file(&included, dir, out, visited, depth + 1)?;
            continue;
        }
        for alias in keys {
            out.push(entry(alias, &value));
        }
    }
    Ok(())
}

fn entry(alias: String, value: &str) -> TnsEntry {
    let descriptor = compact(value);
    TnsEntry {
        host: param(&descriptor, "HOST"),
        port: param(&descriptor, "PORT").and_then(|p| p.parse().ok()),
        service_name: param(&descriptor, "SERVICE_NAME"),
        sid: param(&descriptor, "SID"),
        protocol: param(&descriptor, "PROTOCOL").map(|p| p.to_ascii_uppercase()),
        alias,
        descriptor,
    }
}

/// UTF-8 when valid, otherwise Latin-1 (every byte is a code point).
fn decode(bytes: &[u8]) -> String {
    let bytes = bytes.strip_prefix(b"\xEF\xBB\xBF").unwrap_or(bytes);
    match std::str::from_utf8(bytes) {
        Ok(s) => s.to_string(),
        Err(_) => bytes.iter().map(|&b| b as char).collect(),
    }
}

fn compact(value: &str) -> String {
    value.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// First `(KEY = value)` in a descriptor, case-insensitive.
fn param(descriptor: &str, key: &str) -> Option<String> {
    let bytes = descriptor.as_bytes();
    let mut i = 0;
    while let Some(off) = descriptor[i..].find('(') {
        let mut j = i + off + 1;
        while j < bytes.len() && bytes[j].is_ascii_whitespace() {
            j += 1;
        }
        let end = j + key.len();
        if end <= bytes.len() && bytes[j..end].eq_ignore_ascii_case(key.as_bytes()) {
            let mut k = end;
            while k < bytes.len() && bytes[k].is_ascii_whitespace() {
                k += 1;
            }
            if bytes.get(k) == Some(&b'=') {
                let rest = &descriptor[k + 1..];
                let value = rest.split(')').next().unwrap_or("").trim();
                if !value.is_empty() && !value.starts_with('(') {
                    return Some(value.to_string());
                }
            }
        }
        i = i + off + 1;
    }
    None
}

/// Parses `name[, name...] = value` entries. A value starting with `(` runs
/// until its parentheses balance; any other value ends at the line break.
/// `#` starts a comment.
fn parse(text: &str) -> Vec<(Vec<String>, String)> {
    let chars: Vec<char> = text.chars().collect();
    let n = chars.len();
    let mut i = 0;
    let mut out = Vec::new();

    let skip_comment = |i: &mut usize| {
        while *i < n && chars[*i] != '\n' {
            *i += 1;
        }
    };

    loop {
        // Whitespace and comments before the names.
        while i < n && (chars[i].is_whitespace() || chars[i] == '#') {
            if chars[i] == '#' {
                skip_comment(&mut i);
            } else {
                i += 1;
            }
        }
        if i >= n {
            break;
        }

        // Names up to '='.
        let mut names = String::new();
        let mut malformed = false;
        while i < n && chars[i] != '=' {
            match chars[i] {
                '#' => skip_comment(&mut i),
                '(' | ')' => {
                    malformed = true;
                    i += 1;
                }
                c => {
                    names.push(c);
                    i += 1;
                }
            }
        }
        if i >= n {
            break;
        }
        i += 1; // '='

        // Value.
        while i < n && chars[i].is_whitespace() {
            i += 1;
        }
        let mut value = String::new();
        if i < n && chars[i] == '(' {
            let mut depth = 0usize;
            while i < n {
                let c = chars[i];
                if c == '#' {
                    skip_comment(&mut i);
                    continue;
                }
                value.push(c);
                i += 1;
                if c == '(' {
                    depth += 1;
                } else if c == ')' {
                    depth = depth.saturating_sub(1);
                    if depth == 0 {
                        break;
                    }
                }
            }
        } else {
            while i < n && chars[i] != '\n' && chars[i] != '#' {
                value.push(chars[i]);
                i += 1;
            }
        }

        if malformed {
            continue;
        }
        // Each name is the last word before a comma; stray text before it
        // (an unterminated line) is ignored.
        let keys: Vec<String> = names
            .split(',')
            .filter_map(|part| part.split_whitespace().last().map(str::to_string))
            .collect();
        let value = value.trim().to_string();
        if !keys.is_empty() && !value.is_empty() {
            out.push((keys, value));
        }
    }
    out
}

fn path_key(path: &Path) -> String {
    let p = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    let s = p.display().to_string();
    if cfg!(windows) {
        s.to_lowercase()
    } else {
        s
    }
}

/// A folder holding `tnsnames.ora`: the folder itself, its `network/admin`
/// (Oracle home or Instant Client folder) or the parent of the file.
fn normalize_dir(path: &Path) -> Option<PathBuf> {
    if path.is_file() {
        return path.parent().map(Path::to_path_buf);
    }
    [path.to_path_buf(), path.join("network").join("admin")]
        .into_iter()
        .find(|d| d.join(TNSNAMES).is_file())
}

fn client_library_in(dir: &Path) -> bool {
    let Ok(read) = std::fs::read_dir(dir) else {
        return false;
    };
    read.flatten().any(|e| {
        let name = e.file_name().to_string_lossy().to_ascii_lowercase();
        name == "oci.dll"
            || name.starts_with("libclntsh.so")
            || name.starts_with("libclntsh.") && name.ends_with(".dylib")
    })
}

fn candidates(env: &dyn Fn(&str) -> Option<String>) -> Vec<Candidate> {
    let mut out = Vec::new();
    let mut push = |dir: PathBuf, label: String, origin: &str| {
        out.push(Candidate {
            dir,
            label,
            origin: origin.to_string(),
        });
    };

    if let Some(dir) = env("TNS_ADMIN") {
        push(
            PathBuf::from(dir.trim()),
            dir.trim().to_string(),
            "TNS_ADMIN",
        );
    }
    if let Some(home) = env("ORACLE_HOME") {
        let home = PathBuf::from(home.trim());
        push(
            home.join("network").join("admin"),
            home.display().to_string(),
            "ORACLE_HOME",
        );
    }

    for (dir, label, origin) in registry_homes() {
        push(dir, label, origin);
    }

    let mut lib_vars = vec!["PATH"];
    if !cfg!(windows) {
        lib_vars.extend(["LD_LIBRARY_PATH", "DYLD_LIBRARY_PATH"]);
    }
    for var in lib_vars {
        let Some(value) = env(var) else { continue };
        for dir in std::env::split_paths(&value) {
            if !client_library_in(&dir) {
                continue;
            }
            // Instant Client keeps network/admin next to the library; a full
            // client has it next to its bin folder.
            push(
                dir.join("network").join("admin"),
                dir.display().to_string(),
                "Instant Client on PATH",
            );
            if let Some(home) = dir.parent() {
                push(
                    home.join("network").join("admin"),
                    home.display().to_string(),
                    "Oracle client on PATH",
                );
            }
        }
    }

    if !cfg!(windows) {
        for dir in ["/etc", "/var/opt/oracle"] {
            push(PathBuf::from(dir), dir.to_string(), "System folder");
        }
    }
    out
}

/// Oracle homes registered by Windows installers (HKLM\SOFTWARE\ORACLE\KEY_*).
#[cfg(windows)]
fn registry_homes() -> Vec<(PathBuf, String, &'static str)> {
    use winreg::enums::{HKEY_LOCAL_MACHINE, KEY_READ};
    use winreg::RegKey;

    let mut out = Vec::new();
    let hklm = RegKey::predef(HKEY_LOCAL_MACHINE);
    for root in ["SOFTWARE\\ORACLE", "SOFTWARE\\WOW6432Node\\ORACLE"] {
        let Ok(oracle) = hklm.open_subkey_with_flags(root, KEY_READ) else {
            continue;
        };
        if let Ok(dir) = oracle.get_value::<String, _>("TNS_ADMIN") {
            out.push((PathBuf::from(&dir), dir, "Windows registry"));
        }
        for name in oracle.enum_keys().flatten() {
            if !name.to_ascii_uppercase().starts_with("KEY_") {
                continue;
            }
            let Ok(key) = oracle.open_subkey_with_flags(&name, KEY_READ) else {
                continue;
            };
            let home_name: String = key
                .get_value("ORACLE_HOME_NAME")
                .unwrap_or_else(|_| name.trim_start_matches("KEY_").to_string());
            if let Ok(dir) = key.get_value::<String, _>("TNS_ADMIN") {
                out.push((PathBuf::from(dir), home_name.clone(), "Windows registry"));
            }
            if let Ok(home) = key.get_value::<String, _>("ORACLE_HOME") {
                out.push((
                    PathBuf::from(home).join("network").join("admin"),
                    home_name,
                    "Oracle home",
                ));
            }
        }
    }
    out
}

#[cfg(not(windows))]
fn registry_homes() -> Vec<(PathBuf, String, &'static str)> {
    Vec::new()
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r#"
# Produção — comentário com acento
ERP, ERP.WORLD =
  (DESCRIPTION =
    (ADDRESS = (PROTOCOL = TCP)(HOST = erp-db.example)(PORT = 1521)) # inline comment
    (CONNECT_DATA = (SERVER = DEDICATED) (SERVICE_NAME = erppdb))
  )

legacy=(DESCRIPTION=(ADDRESS=(PROTOCOL=tcps)(HOST=old)(PORT=2484))(CONNECT_DATA=(SID=ORCL)))
EZ = db.example:1521/sales
"#;

    #[test]
    fn parses_aliases_descriptors_and_fields() {
        let entries: Vec<TnsEntry> = parse(SAMPLE)
            .into_iter()
            .flat_map(|(keys, value)| keys.into_iter().map(move |k| entry(k, &value)))
            .collect();
        let names: Vec<&str> = entries.iter().map(|e| e.alias.as_str()).collect();
        assert_eq!(names, ["ERP", "ERP.WORLD", "legacy", "EZ"]);
        let erp = &entries[0];
        assert_eq!(erp.host.as_deref(), Some("erp-db.example"));
        assert_eq!(erp.port, Some(1521));
        assert_eq!(erp.service_name.as_deref(), Some("erppdb"));
        assert!(!erp.descriptor.contains("inline comment"));
        assert!(erp.descriptor.starts_with("(DESCRIPTION = (ADDRESS"));
        assert_eq!(entries[2].sid.as_deref(), Some("ORCL"));
        assert_eq!(entries[2].protocol.as_deref(), Some("TCPS"));
        assert_eq!(entries[3].descriptor, "db.example:1521/sales");
        assert_eq!(entries[3].host, None);
    }

    #[test]
    fn decodes_latin1_files() {
        let bytes = b"# Produ\xe7\xe3o\nA=(DESCRIPTION=(ADDRESS=(HOST=h)(PORT=1)))\n";
        let text = decode(bytes);
        assert!(text.contains("Produção"));
        assert_eq!(parse(&text).len(), 1);
    }

    #[test]
    fn reads_folders_with_ifile_and_default_domain() {
        let dir = std::env::temp_dir().join(format!("lq-tns-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join(TNSNAMES),
            "IFILE = extra.ora\nSALES.WORLD=(DESCRIPTION=(ADDRESS=(HOST=a)(PORT=1521))(CONNECT_DATA=(SERVICE_NAME=s)))\n",
        )
        .unwrap();
        std::fs::write(
            dir.join("extra.ora"),
            "HR = (DESCRIPTION=(ADDRESS=(HOST=b)(PORT=1522)))\n",
        )
        .unwrap();
        std::fs::write(dir.join("sqlnet.ora"), "NAMES.DEFAULT_DOMAIN = WORLD\n").unwrap();

        let source = read_folder(dir.to_str().unwrap()).unwrap();
        let names: Vec<&str> = source.entries.iter().map(|e| e.alias.as_str()).collect();
        assert_eq!(names, ["HR", "SALES.WORLD"]);

        let (descriptor, found) = resolve("sales", Some(dir.to_str().unwrap())).unwrap();
        assert!(descriptor.contains("SERVICE_NAME=s"));
        assert_eq!(path_key(&found), path_key(&dir));
        let (hr, _) = resolve("hr", Some(dir.to_str().unwrap())).unwrap();
        assert!(hr.contains("HOST=b"));
        assert!(resolve("missing", Some(dir.to_str().unwrap())).is_err());

        // The Oracle home / Instant Client folder works too.
        let home = dir.join("home");
        std::fs::create_dir_all(home.join("network").join("admin")).unwrap();
        std::fs::write(
            home.join("network").join("admin").join(TNSNAMES),
            "X=(DESCRIPTION=(ADDRESS=(HOST=x)))",
        )
        .unwrap();
        assert_eq!(
            read_folder(home.to_str().unwrap()).unwrap().entries[0].alias,
            "X"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn finds_client_folders_from_the_environment() {
        let root = std::env::temp_dir().join(format!("lq-tns-env-{}", std::process::id()));
        let instant = root.join("instantclient_23_5");
        std::fs::create_dir_all(&instant).unwrap();
        let lib = if cfg!(windows) {
            "oci.dll"
        } else {
            "libclntsh.so.23.1"
        };
        std::fs::write(instant.join(lib), b"").unwrap();
        let admin = root.join("admin");
        let path = std::env::join_paths([instant.clone()])
            .unwrap()
            .into_string()
            .unwrap();

        let env = |name: &str| match name {
            "TNS_ADMIN" => Some(admin.display().to_string()),
            "ORACLE_HOME" => Some(root.join("product").display().to_string()),
            "PATH" => Some(path.clone()),
            _ => None,
        };
        let found = candidates(&env);
        assert_eq!(found[0].origin, "TNS_ADMIN");
        assert_eq!(found[0].dir, admin);
        assert_eq!(found[1].origin, "ORACLE_HOME");
        assert!(found.iter().any(|c| c.origin == "Instant Client on PATH"
            && c.dir == instant.join("network").join("admin")));

        let _ = std::fs::remove_dir_all(&root);
    }
}
