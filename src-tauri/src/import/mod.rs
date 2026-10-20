//! Imports connection profiles from other database tools.
//!
//! Each tool module finds its configuration files on this machine
//! (`default_files`) and turns one file into candidates (`parse`). Passwords
//! read from a tool's store stay in this process: the preview only says that
//! one exists, and `apply` writes it straight to the OS credential store.

mod datagrip;
mod dbeaver;
pub mod jdbc;
mod pgadmin;
mod sqldeveloper;

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::error::{AppError, AppResult};
use crate::model::ConnectionProfile;
use crate::profiles::ProfileStore;

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Hash)]
#[serde(rename_all = "kebab-case")]
pub enum ImportTool {
    Dbeaver,
    Datagrip,
    Pgadmin,
    SqlDeveloper,
}

/// A configuration file found on this machine.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ImportSource {
    pub tool: ImportTool,
    /// Where it comes from, such as the DBeaver project name.
    pub label: String,
    pub path: String,
    /// Connections LowQuery can import from it.
    pub count: usize,
    pub error: Option<String>,
}

/// One connection that can be imported.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ImportCandidate {
    /// Stable within the file (the tool's own id when it has one).
    pub key: String,
    /// The profile to create; `id` is empty.
    pub profile: ConnectionProfile,
    /// Folder or group in the tool, shown in the preview.
    pub folder: Option<String>,
    /// A password can be imported with the connection.
    pub has_password: bool,
    /// Settings that could not be carried over (SSH tunnels, drivers
    /// options...).
    pub warnings: Vec<String>,
}

/// A connection in the file that LowQuery cannot use.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SkippedConnection {
    pub name: String,
    pub reason: String,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ImportPreview {
    pub tool: ImportTool,
    pub path: String,
    pub candidates: Vec<ImportCandidate>,
    pub skipped: Vec<SkippedConnection>,
    /// Notes about the whole file (for example, passwords kept in a store
    /// LowQuery cannot read).
    pub notes: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ImportResult {
    pub created: usize,
    pub passwords: usize,
}

/// What a tool module returns for one file. `passwords` is keyed by
/// candidate key and never leaves the Rust side.
#[derive(Debug, Default)]
pub struct Parsed {
    pub candidates: Vec<ImportCandidate>,
    pub skipped: Vec<SkippedConnection>,
    pub notes: Vec<String>,
    pub passwords: HashMap<String, String>,
}

impl Parsed {
    pub fn skip(&mut self, name: impl Into<String>, reason: impl Into<String>) {
        self.skipped.push(SkippedConnection {
            name: name.into(),
            reason: reason.into(),
        });
    }
}

fn default_files(tool: ImportTool) -> Vec<(String, PathBuf)> {
    match tool {
        ImportTool::Dbeaver => dbeaver::default_files(),
        ImportTool::Datagrip => datagrip::default_files(),
        ImportTool::Pgadmin => pgadmin::default_files(),
        ImportTool::SqlDeveloper => sqldeveloper::default_files(),
    }
}

fn parse(tool: ImportTool, path: &Path) -> AppResult<Parsed> {
    let mut parsed = match tool {
        ImportTool::Dbeaver => dbeaver::parse(path),
        ImportTool::Datagrip => datagrip::parse(path),
        ImportTool::Pgadmin => pgadmin::parse(path),
        ImportTool::SqlDeveloper => sqldeveloper::parse(path),
    }?;
    for c in &mut parsed.candidates {
        c.profile.id.clear();
        if c.profile.name.trim().is_empty() {
            c.profile.name = c.profile.host.clone();
        }
        c.has_password = parsed.passwords.contains_key(&c.key);
    }
    Ok(parsed)
}

/// Configuration files of every supported tool found on this machine.
pub fn detect() -> Vec<ImportSource> {
    let tools = [
        ImportTool::Dbeaver,
        ImportTool::Datagrip,
        ImportTool::Pgadmin,
        ImportTool::SqlDeveloper,
    ];
    let mut out = Vec::new();
    for tool in tools {
        for (label, path) in default_files(tool) {
            let (count, error) = match parse(tool, &path) {
                Ok(p) => (p.candidates.len(), None),
                Err(e) => (0, Some(e.message)),
            };
            out.push(ImportSource {
                tool,
                label,
                path: path.display().to_string(),
                count,
                error,
            });
        }
    }
    out
}

pub fn preview(tool: ImportTool, path: &str) -> AppResult<ImportPreview> {
    let parsed = parse(tool, Path::new(path))?;
    Ok(ImportPreview {
        tool,
        path: path.to_string(),
        candidates: parsed.candidates,
        skipped: parsed.skipped,
        notes: parsed.notes,
    })
}

/// Creates the profiles of the chosen candidates. The file is read again so
/// passwords never travel through the interface.
pub fn apply(
    store: &ProfileStore,
    tool: ImportTool,
    path: &str,
    keys: &[String],
    with_passwords: bool,
) -> AppResult<ImportResult> {
    let mut parsed = parse(tool, Path::new(path))?;
    let mut result = ImportResult {
        created: 0,
        passwords: 0,
    };
    for key in keys {
        let Some(c) = parsed.candidates.iter().find(|c| &c.key == key) else {
            return Err(AppError::invalid(
                "The file changed since it was read. Open it again to import.",
            ));
        };
        let password = if with_passwords {
            parsed.passwords.remove(key)
        } else {
            None
        };
        let mut profile = c.profile.clone();
        profile.save_password = profile.save_password || password.is_some();
        if password.is_some() {
            result.passwords += 1;
        }
        store.save(profile, password)?;
        result.created += 1;
    }
    Ok(result)
}

/// The user's home directory.
pub fn home() -> Option<PathBuf> {
    directories::BaseDirs::new().map(|d| d.home_dir().to_path_buf())
}

/// Roaming application data on Windows, `~/Library/Application Support` on
/// macOS and `$XDG_CONFIG_HOME` (or `~/.config`) on Linux.
pub fn config_dir() -> Option<PathBuf> {
    directories::BaseDirs::new().map(|d| d.config_dir().to_path_buf())
}

/// `%APPDATA%` on Windows, `~/Library/Application Support` on macOS and
/// `$XDG_DATA_HOME` (or `~/.local/share`) on Linux.
pub fn data_dir() -> Option<PathBuf> {
    directories::BaseDirs::new().map(|d| d.data_dir().to_path_buf())
}
