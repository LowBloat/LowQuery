//! Setting up AI clients to start `lowquery mcp`: the command to run, the
//! configuration each client expects, and adding LowQuery to the JSON
//! configuration files of the clients that have one.

use std::path::{Path, PathBuf};

use serde::Serialize;
use serde_json::{json, Map, Value};

use crate::error::{AppError, AppResult};

/// Name of the server entry in client configurations.
const SERVER_NAME: &str = "lowquery";

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AiSetup {
    /// Executable that runs the MCP server (`<command> mcp`).
    pub command: String,
    pub args: Vec<String>,
    pub clients: Vec<AiClient>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AiClient {
    pub id: &'static str,
    pub name: &'static str,
    /// Configuration file LowQuery can update, when the client has one.
    pub config_path: Option<String>,
    /// The client seems installed (its configuration folder exists).
    pub installed: bool,
    /// The configuration already starts this LowQuery.
    pub configured: bool,
    /// What to paste or run, for clients configured by hand.
    pub snippet: String,
    /// "json", "toml" or "shell", for highlighting.
    pub snippet_kind: &'static str,
}

/// How a client's JSON file lists servers.
#[derive(Clone, Copy)]
enum JsonStyle {
    /// `{ "mcpServers": { name: { command, args } } }` (Claude Desktop,
    /// Cursor, Windsurf, Gemini CLI).
    McpServers,
    /// `{ "servers": { name: { type: "stdio", command, args } } }` (VS Code).
    VsCode,
}

struct JsonClient {
    id: &'static str,
    name: &'static str,
    style: JsonStyle,
    /// Candidate files in order; the first whose folder exists is used.
    paths: Vec<PathBuf>,
}

/// The executable to start. An AppImage runs from a temporary mount, so
/// its own path (`$APPIMAGE`) is the stable one.
pub fn server_command() -> AppResult<String> {
    if let Ok(appimage) = std::env::var("APPIMAGE") {
        if !appimage.is_empty() {
            return Ok(appimage);
        }
    }
    let exe = std::env::current_exe()?;
    Ok(exe.display().to_string())
}

fn home() -> PathBuf {
    directories::BaseDirs::new()
        .map(|d| d.home_dir().to_path_buf())
        .unwrap_or_default()
}

fn config_root() -> PathBuf {
    directories::BaseDirs::new()
        .map(|d| d.config_dir().to_path_buf())
        .unwrap_or_default()
}

fn json_clients() -> Vec<JsonClient> {
    let home = home();
    let config = config_root();
    let mut claude = Vec::new();
    // The Microsoft Store build keeps its files in a virtualized folder.
    if cfg!(windows) {
        if let Some(local) = directories::BaseDirs::new().map(|d| d.data_local_dir().to_path_buf())
        {
            if let Ok(entries) = std::fs::read_dir(local.join("Packages")) {
                for e in entries.flatten() {
                    if e.file_name().to_string_lossy().starts_with("Claude_") {
                        claude.push(
                            e.path()
                                .join("LocalCache/Roaming/Claude/claude_desktop_config.json"),
                        );
                    }
                }
            }
        }
    }
    claude.push(config.join("Claude").join("claude_desktop_config.json"));
    vec![
        JsonClient {
            id: "claude-desktop",
            name: "Claude Desktop",
            style: JsonStyle::McpServers,
            paths: claude,
        },
        JsonClient {
            id: "cursor",
            name: "Cursor",
            style: JsonStyle::McpServers,
            paths: vec![home.join(".cursor").join("mcp.json")],
        },
        JsonClient {
            id: "vscode",
            name: "VS Code",
            style: JsonStyle::VsCode,
            paths: vec![config.join("Code").join("User").join("mcp.json")],
        },
        JsonClient {
            id: "windsurf",
            name: "Windsurf",
            style: JsonStyle::McpServers,
            paths: vec![home
                .join(".codeium")
                .join("windsurf")
                .join("mcp_config.json")],
        },
        JsonClient {
            id: "gemini",
            name: "Gemini CLI",
            style: JsonStyle::McpServers,
            paths: vec![home.join(".gemini").join("settings.json")],
        },
    ]
}

fn pick_path(paths: &[PathBuf]) -> Option<&PathBuf> {
    paths
        .iter()
        .find(|p| p.parent().is_some_and(Path::exists))
        .or(paths.last())
}

fn entry(style: JsonStyle, command: &str) -> Value {
    match style {
        JsonStyle::McpServers => json!({ "command": command, "args": ["mcp"] }),
        JsonStyle::VsCode => json!({ "type": "stdio", "command": command, "args": ["mcp"] }),
    }
}

fn servers_key(style: JsonStyle) -> &'static str {
    match style {
        JsonStyle::McpServers => "mcpServers",
        JsonStyle::VsCode => "servers",
    }
}

fn snippet(style: JsonStyle, command: &str) -> String {
    let doc = json!({ servers_key(style): { SERVER_NAME: entry(style, command) } });
    serde_json::to_string_pretty(&doc).unwrap_or_default()
}

/// Whether the file's server entry already starts `command`.
fn is_configured(path: &Path, style: JsonStyle, command: &str) -> bool {
    let Ok(text) = std::fs::read_to_string(path) else {
        return false;
    };
    let Ok(doc) = serde_json::from_str::<Value>(&text) else {
        return false;
    };
    doc.get(servers_key(style))
        .and_then(|s| s.get(SERVER_NAME))
        .and_then(|e| e.get("command"))
        .and_then(Value::as_str)
        .is_some_and(|c| c == command)
}

fn quote_shell(s: &str) -> String {
    if s.chars()
        .all(|c| c.is_ascii_alphanumeric() || "/\\:._-".contains(c))
    {
        s.to_string()
    } else {
        format!("\"{}\"", s.replace('"', "\\\""))
    }
}

fn toml_string(s: &str) -> String {
    format!("\"{}\"", s.replace('\\', "\\\\").replace('"', "\\\""))
}

pub fn setup() -> AppResult<AiSetup> {
    let command = server_command()?;
    let mut clients: Vec<AiClient> = json_clients()
        .into_iter()
        .map(|c| {
            let path = pick_path(&c.paths).cloned();
            let installed = path
                .as_ref()
                .and_then(|p| p.parent())
                .is_some_and(Path::exists);
            AiClient {
                id: c.id,
                name: c.name,
                configured: path
                    .as_ref()
                    .is_some_and(|p| is_configured(p, c.style, &command)),
                config_path: path.map(|p| p.display().to_string()),
                installed,
                snippet: snippet(c.style, &command),
                snippet_kind: "json",
            }
        })
        .collect();
    let shell_cmd = quote_shell(&command);
    clients.insert(
        1,
        AiClient {
            id: "claude-code",
            name: "Claude Code",
            config_path: None,
            installed: home().join(".claude").exists(),
            configured: false,
            snippet: format!("claude mcp add --scope user {SERVER_NAME} -- {shell_cmd} mcp"),
            snippet_kind: "shell",
        },
    );
    clients.push(AiClient {
        id: "codex",
        name: "Codex",
        config_path: None,
        installed: home().join(".codex").exists(),
        configured: false,
        snippet: format!(
            "[mcp_servers.{SERVER_NAME}]\ncommand = {}\nargs = [\"mcp\"]",
            toml_string(&command)
        ),
        snippet_kind: "toml",
    });
    Ok(AiSetup {
        command,
        args: vec!["mcp".into()],
        clients,
    })
}

/// Adds (or updates) the LowQuery entry in a client's JSON configuration,
/// keeping everything else. The previous file is kept as
/// `<file>.lowquery-backup`.
pub fn connect_client(id: &str) -> AppResult<String> {
    let command = server_command()?;
    let client = json_clients()
        .into_iter()
        .find(|c| c.id == id)
        .ok_or_else(|| AppError::invalid("This client is configured by hand."))?;
    let path = pick_path(&client.paths)
        .cloned()
        .ok_or_else(|| AppError::invalid("Could not find the client configuration folder."))?;
    write_entry(&path, client.style, &command)?;
    Ok(path.display().to_string())
}

fn write_entry(path: &Path, style: JsonStyle, command: &str) -> AppResult<()> {
    let mut doc = match std::fs::read_to_string(path) {
        Ok(text) if text.trim().is_empty() => Value::Object(Map::new()),
        Ok(text) => {
            let text = text.trim_start_matches('\u{feff}');
            serde_json::from_str::<Value>(text).map_err(|_| {
                AppError::invalid(
                    "The configuration file is not plain JSON (it may have comments). Add the snippet by hand.",
                )
            })?
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Value::Object(Map::new()),
        Err(e) => return Err(e.into()),
    };
    let root = doc
        .as_object_mut()
        .ok_or_else(|| AppError::invalid("The configuration file does not hold a JSON object."))?;
    let servers = root
        .entry(servers_key(style))
        .or_insert_with(|| Value::Object(Map::new()));
    let servers = servers.as_object_mut().ok_or_else(|| {
        AppError::invalid("The servers section of the configuration is not an object.")
    })?;
    servers.insert(SERVER_NAME.into(), entry(style, command));
    if path.exists() {
        let mut backup = path.as_os_str().to_owned();
        backup.push(".lowquery-backup");
        std::fs::copy(path, PathBuf::from(backup))?;
    } else if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let tmp = path.with_extension("lowquery-tmp");
    std::fs::write(&tmp, serde_json::to_string_pretty(&doc)? + "\n")?;
    std::fs::rename(&tmp, path)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn adds_entry_and_keeps_the_rest() {
        let dir = std::env::temp_dir().join(format!("lq-ai-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("claude_desktop_config.json");
        std::fs::write(
            &path,
            r#"{"theme":"dark","mcpServers":{"other":{"command":"x"}},"zeta":1}"#,
        )
        .unwrap();
        write_entry(
            &path,
            JsonStyle::McpServers,
            r"C:\Apps\LowQuery\lowquery.exe",
        )
        .unwrap();
        let doc: Value = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        let keys: Vec<&String> = doc.as_object().unwrap().keys().collect();
        assert_eq!(keys, ["theme", "mcpServers", "zeta"]);
        assert_eq!(doc["mcpServers"]["other"]["command"], "x");
        assert_eq!(doc["mcpServers"]["lowquery"]["args"][0], "mcp");
        assert!(is_configured(
            &path,
            JsonStyle::McpServers,
            r"C:\Apps\LowQuery\lowquery.exe"
        ));
        assert!(dir
            .join("claude_desktop_config.json.lowquery-backup")
            .exists());

        let vs = dir.join("mcp.json");
        write_entry(&vs, JsonStyle::VsCode, "/usr/bin/lowquery").unwrap();
        let doc: Value = serde_json::from_str(&std::fs::read_to_string(&vs).unwrap()).unwrap();
        assert_eq!(doc["servers"]["lowquery"]["type"], "stdio");

        std::fs::write(&vs, "{ // comment\n}").unwrap();
        assert!(write_entry(&vs, JsonStyle::VsCode, "x").is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn snippets_quote_paths() {
        assert_eq!(quote_shell("/usr/bin/lowquery"), "/usr/bin/lowquery");
        assert_eq!(
            quote_shell(r"C:\Program Files\LowQuery\lowquery.exe"),
            r#""C:\Program Files\LowQuery\lowquery.exe""#
        );
        assert_eq!(toml_string(r"C:\a"), r#""C:\\a""#);
    }
}
