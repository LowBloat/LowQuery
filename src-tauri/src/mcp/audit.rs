//! Activity log of AI agents: one JSON object per line in
//! `ai-activity.jsonl` next to `connections.json`. The MCP server appends,
//! the app reads the tail to show what agents did.

use std::io::{BufRead, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::error::AppResult;

/// The log is cut to its newest half when it grows past this size.
const MAX_BYTES: u64 = 8 * 1024 * 1024;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct AuditEntry {
    /// RFC 3339 time in UTC.
    pub at: String,
    /// MCP client name and version from `initialize` ("claude-ai 0.1.0").
    pub client: String,
    /// Tool name ("query", "execute", "describe_table"...).
    pub tool: String,
    pub connection_id: Option<String>,
    pub connection_name: Option<String>,
    /// SQL text, or the object/pattern for catalog tools.
    pub detail: Option<String>,
    pub ok: bool,
    /// Rows returned or affected.
    pub rows: Option<u64>,
    pub error: Option<String>,
    pub elapsed_ms: u64,
}

pub fn log_path(config_dir: &Path) -> PathBuf {
    config_dir.join("ai-activity.jsonl")
}

/// Appends one entry. Failures to write the log never fail the tool call.
pub fn append(config_dir: &Path, entry: &AuditEntry) {
    let path = log_path(config_dir);
    let _ = std::fs::create_dir_all(config_dir);
    if std::fs::metadata(&path)
        .map(|m| m.len() > MAX_BYTES)
        .unwrap_or(false)
    {
        let _ = trim(&path);
    }
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
    {
        if let Ok(line) = serde_json::to_string(entry) {
            let _ = writeln!(f, "{line}");
        }
    }
}

/// Keeps the newest half of the log.
fn trim(path: &Path) -> std::io::Result<()> {
    let mut f = std::fs::File::open(path)?;
    let len = f.metadata()?.len();
    f.seek(SeekFrom::Start(len / 2))?;
    let mut reader = std::io::BufReader::new(f);
    let mut skip = String::new();
    reader.read_line(&mut skip)?; // partial line
    let mut rest = Vec::new();
    std::io::Read::read_to_end(&mut reader, &mut rest)?;
    let tmp = path.with_extension("jsonl.tmp");
    std::fs::write(&tmp, rest)?;
    std::fs::rename(tmp, path)
}

/// The newest `limit` entries, newest first.
pub fn read_recent(config_dir: &Path, limit: usize) -> AppResult<Vec<AuditEntry>> {
    let path = log_path(config_dir);
    let text = match std::fs::read_to_string(&path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e.into()),
    };
    Ok(text
        .lines()
        .rev()
        .filter_map(|l| serde_json::from_str(l).ok())
        .take(limit)
        .collect())
}

pub fn clear(config_dir: &Path) -> AppResult<()> {
    match std::fs::remove_file(log_path(config_dir)) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e.into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn appends_reads_and_clears() {
        let dir = std::env::temp_dir().join(format!("lq-audit-{}", uuid::Uuid::new_v4()));
        let entry = |i: u64| AuditEntry {
            at: format!("2026-10-20T10:00:0{i}Z"),
            client: "test 1.0".into(),
            tool: "query".into(),
            connection_id: Some("c1".into()),
            connection_name: Some("Local".into()),
            detail: Some(format!("select {i}")),
            ok: true,
            rows: Some(i),
            error: None,
            elapsed_ms: 3,
        };
        for i in 0..3 {
            append(&dir, &entry(i));
        }
        let got = read_recent(&dir, 2).unwrap();
        assert_eq!(got, vec![entry(2), entry(1)]);
        clear(&dir).unwrap();
        assert!(read_recent(&dir, 10).unwrap().is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
