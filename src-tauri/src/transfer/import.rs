//! Imports rows from a CSV or JSON file into a table. The file's encoding,
//! delimiter and header are detected; columns are matched by name; values
//! written the way spreadsheets in many locales write them (`1.234,56`,
//! `31/12/2024`, `sim`) are converted for the column's type. Everything runs
//! in one transaction: an error leaves the table as it was. Cassandra has no
//! transactions: its rows are inserted one statement at a time, and an error
//! leaves the rows written so far.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};

use serde::{Deserialize, Serialize};

use super::catalog;
use super::sql::{self, InsertTarget};
use super::Progress;
use crate::drivers::Session;
use crate::error::{AppError, AppResult};
use crate::model::{Engine, ExecRequest, ValueKind};

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum FileFormat {
    Csv,
    Json,
}

/// What the user can change after the preview.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ReadOptions {
    /// `None` detects it.
    pub delimiter: Option<char>,
    /// `None` detects it.
    pub header: Option<bool>,
}

/// The inferred type of a file column, for a new table.
#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Inferred {
    Integer,
    Decimal,
    Boolean,
    Date,
    Timestamp,
    Text,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ImportPreview {
    pub format: FileFormat,
    pub encoding: String,
    pub delimiter: char,
    pub header: bool,
    pub columns: Vec<String>,
    /// The first rows, as they will be read.
    pub rows: Vec<Vec<Option<String>>>,
    pub total_rows: usize,
    pub inferred: Vec<Inferred>,
    /// Longest value per column, for text lengths.
    pub max_lengths: Vec<usize>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ImportRequest {
    pub job_id: String,
    pub session_id: String,
    pub path: String,
    pub options: ReadOptions,
    pub schema: String,
    pub table: String,
    /// Delete the table's rows first (in the same transaction).
    pub replace: bool,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ImportSummary {
    pub rows: u64,
    /// File columns with no table column of the same name.
    pub skipped_columns: Vec<String>,
}

/// The file decoded to text: UTF-8 (with or without BOM), UTF-16 with BOM,
/// or Windows-1252 when the bytes are not UTF-8.
pub fn decode(bytes: &[u8]) -> (String, &'static str) {
    if let Some(rest) = bytes.strip_prefix(&[0xEF, 0xBB, 0xBF]) {
        return (String::from_utf8_lossy(rest).into_owned(), "UTF-8");
    }
    let utf16 = |rest: &[u8], le: bool| {
        let units: Vec<u16> = rest
            .as_chunks::<2>()
            .0
            .iter()
            .map(|c| {
                if le {
                    u16::from_le_bytes([c[0], c[1]])
                } else {
                    u16::from_be_bytes([c[0], c[1]])
                }
            })
            .collect();
        String::from_utf16_lossy(&units)
    };
    if let Some(rest) = bytes.strip_prefix(&[0xFF, 0xFE]) {
        return (utf16(rest, true), "UTF-16");
    }
    if let Some(rest) = bytes.strip_prefix(&[0xFE, 0xFF]) {
        return (utf16(rest, false), "UTF-16");
    }
    match std::str::from_utf8(bytes) {
        Ok(s) => (s.to_string(), "UTF-8"),
        Err(_) => (bytes.iter().map(|&b| cp1252(b)).collect(), "Windows-1252"),
    }
}

fn cp1252(b: u8) -> char {
    const HIGH: [char; 32] = [
        '€', '\u{81}', '‚', 'ƒ', '„', '…', '†', '‡', 'ˆ', '‰', 'Š', '‹', 'Œ', '\u{8d}', 'Ž',
        '\u{8f}', '\u{90}', '‘', '’', '“', '”', '•', '–', '—', '˜', '™', 'š', '›', 'œ', '\u{9d}',
        'ž', 'Ÿ',
    ];
    if (0x80..0xA0).contains(&b) {
        HIGH[(b - 0x80) as usize]
    } else {
        b as char
    }
}

/// The delimiter used most outside quotes in the first lines.
fn detect_delimiter(text: &str) -> char {
    let sample: String = text.lines().take(5).collect::<Vec<_>>().join("\n");
    let mut best = (',', 0);
    for d in [',', ';', '\t', '|'] {
        let mut quoted = false;
        let n = sample
            .chars()
            .filter(|&c| {
                if c == '"' {
                    quoted = !quoted;
                }
                !quoted && c == d
            })
            .count();
        if n > best.1 {
            best = (d, n);
        }
    }
    best.0
}

/// RFC 4180 records: quoted fields may hold delimiters, quotes (doubled)
/// and line breaks.
fn parse_csv(text: &str, delimiter: char) -> Vec<Vec<String>> {
    let mut records = Vec::new();
    let mut record = Vec::new();
    let mut field = String::new();
    let mut quoted = false;
    let mut chars = text.chars().peekable();
    let mut any = false;
    while let Some(c) = chars.next() {
        any = true;
        if quoted {
            if c == '"' {
                if chars.peek() == Some(&'"') {
                    field.push('"');
                    chars.next();
                } else {
                    quoted = false;
                }
            } else {
                field.push(c);
            }
        } else if c == '"' && field.is_empty() {
            quoted = true;
        } else if c == delimiter {
            record.push(std::mem::take(&mut field));
        } else if c == '\n' || c == '\r' {
            if c == '\r' && chars.peek() == Some(&'\n') {
                chars.next();
            }
            record.push(std::mem::take(&mut field));
            if !(record.len() == 1 && record[0].is_empty()) {
                records.push(std::mem::take(&mut record));
            } else {
                record.clear();
            }
            any = false;
        } else {
            field.push(c);
        }
    }
    if any || !field.is_empty() || !record.is_empty() {
        record.push(field);
        if !(record.len() == 1 && record[0].is_empty()) {
            records.push(record);
        }
    }
    records
}

/// Columns and rows of a JSON array of objects, or of one object per line.
/// Column names and rows, every value as text.
type Table = (Vec<String>, Vec<Vec<Option<String>>>);

/// One JSON record with its values kept as the text they were written in:
/// parsing numbers into `f64` would cut `NUMERIC`, `BIGINT` and varint values
/// to 17 digits.
struct Record(Vec<(String, Option<String>)>);

impl<'de> serde::Deserialize<'de> for Record {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        struct Fields;
        impl<'de> serde::de::Visitor<'de> for Fields {
            type Value = Record;

            fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str("an object with one key per column")
            }

            fn visit_map<A: serde::de::MapAccess<'de>>(
                self,
                mut map: A,
            ) -> Result<Record, A::Error> {
                let mut fields: Vec<(String, Option<String>)> = Vec::new();
                while let Some((key, raw)) =
                    map.next_entry::<String, Box<serde_json::value::RawValue>>()?
                {
                    let value = raw_text(raw.get()).map_err(serde::de::Error::custom)?;
                    // As in a JSON object, the last of repeated keys wins.
                    match fields.iter_mut().find(|(k, _)| *k == key) {
                        Some(slot) => slot.1 = value,
                        None => fields.push((key, value)),
                    }
                }
                Ok(Record(fields))
            }
        }
        d.deserialize_map(Fields)
    }
}

/// A raw JSON value as the text of the cell: strings unescaped, numbers as
/// written, `null` as no value, arrays, objects and booleans compact.
fn raw_text(raw: &str) -> Result<Option<String>, serde_json::Error> {
    let raw = raw.trim();
    match raw.as_bytes().first() {
        Some(b'"') => serde_json::from_str::<String>(raw).map(Some),
        Some(b'-' | b'0'..=b'9') => Ok(Some(raw.to_string())),
        _ => Ok(match serde_json::from_str::<serde_json::Value>(raw)? {
            serde_json::Value::Null => None,
            other => Some(other.to_string()),
        }),
    }
}

fn parse_json(text: &str) -> AppResult<Table> {
    const NOT_AN_OBJECT: &str = "Each JSON item must be an object with one key per column.";
    let not_object = |e: &serde_json::Error| e.to_string().contains("one key per column");
    let records: Vec<Record> = if text.trim_start().starts_with('[') {
        serde_json::from_str(text).map_err(|e| {
            if not_object(&e) {
                AppError::invalid(NOT_AN_OBJECT)
            } else {
                AppError::invalid(format!("The JSON file is not valid: {e}"))
            }
        })?
    } else {
        text.lines()
            .filter(|l| !l.trim().is_empty())
            .enumerate()
            .map(|(i, l)| {
                serde_json::from_str(l).map_err(|e| {
                    if not_object(&e) {
                        AppError::invalid(NOT_AN_OBJECT)
                    } else {
                        AppError::invalid(format!("Line {} is not valid JSON: {e}", i + 1))
                    }
                })
            })
            .collect::<AppResult<_>>()?
    };
    let mut columns: Vec<String> = Vec::new();
    for record in &records {
        for (k, _) in &record.0 {
            if !columns.contains(k) {
                columns.push(k.clone());
            }
        }
    }
    let rows = records
        .iter()
        .map(|r| {
            columns
                .iter()
                .map(|c| {
                    r.0.iter()
                        .find(|(k, _)| k == c)
                        .and_then(|(_, v)| v.clone())
                })
                .collect()
        })
        .collect();
    Ok((columns, rows))
}

/// Whether the first record names the columns: no value in it is empty, a
/// number or a date, and no name repeats.
fn looks_like_header(first: &[String]) -> bool {
    let typed = |v: &str| {
        matches!(
            infer_one(v),
            Some(Inferred::Integer | Inferred::Decimal | Inferred::Date | Inferred::Timestamp)
        )
    };
    if first.iter().any(|v| v.trim().is_empty() || typed(v)) {
        return false;
    }
    let mut seen = std::collections::HashSet::new();
    first.iter().all(|v| seen.insert(v.trim().to_lowercase()))
}

pub struct FileData {
    pub format: FileFormat,
    pub encoding: &'static str,
    pub delimiter: char,
    pub header: bool,
    pub columns: Vec<String>,
    pub rows: Vec<Vec<Option<String>>>,
}

pub fn read(path: &str, options: &ReadOptions) -> AppResult<FileData> {
    let bytes = std::fs::read(path)?;
    let (text, encoding) = decode(&bytes);
    drop(bytes);
    let json = path.to_lowercase().ends_with(".json")
        || path.to_lowercase().ends_with(".ndjson")
        || text.trim_start().starts_with(['[', '{']);
    if json {
        let (columns, rows) = parse_json(&text)?;
        return Ok(FileData {
            format: FileFormat::Json,
            encoding,
            delimiter: ',',
            header: true,
            columns,
            rows,
        });
    }
    let delimiter = options.delimiter.unwrap_or_else(|| detect_delimiter(&text));
    let mut records = parse_csv(&text, delimiter);
    drop(text);
    let header = options
        .header
        .unwrap_or_else(|| records.first().is_some_and(|f| looks_like_header(f)));
    let width = records.iter().map(Vec::len).max().unwrap_or(0);
    let columns: Vec<String> = if header && !records.is_empty() {
        let mut names: Vec<String> = records
            .remove(0)
            .into_iter()
            .map(|n| n.trim().to_string())
            .collect();
        names.resize(width, String::new());
        names
            .into_iter()
            .enumerate()
            .map(|(i, n)| {
                if n.is_empty() {
                    format!("column_{}", i + 1)
                } else {
                    n
                }
            })
            .collect()
    } else {
        (1..=width).map(|i| format!("column_{i}")).collect()
    };
    let rows = records
        .into_iter()
        .map(|r| {
            let mut row: Vec<Option<String>> = r.into_iter().map(Some).collect();
            row.resize(width, None);
            row
        })
        .collect();
    Ok(FileData {
        format: FileFormat::Csv,
        encoding,
        delimiter,
        header,
        columns,
        rows,
    })
}

fn infer_one(v: &str) -> Option<Inferred> {
    let v = v.trim();
    if v.is_empty() {
        return None;
    }
    // Leading zeros (codes, postal codes) stay text.
    let digits = v.strip_prefix('-').unwrap_or(v);
    if digits.len() > 1
        && digits.starts_with('0')
        && !digits.starts_with("0.")
        && !digits.starts_with("0,")
    {
        return Some(Inferred::Text);
    }
    if digits.bytes().all(|b| b.is_ascii_digit()) && !digits.is_empty() {
        return Some(if digits.len() <= 18 {
            Inferred::Integer
        } else {
            Inferred::Decimal
        });
    }
    if number(v).is_some() {
        return Some(Inferred::Decimal);
    }
    if let Some(t) = temporal(v) {
        return Some(if t.len() == 10 {
            Inferred::Date
        } else {
            Inferred::Timestamp
        });
    }
    if matches!(v.to_lowercase().as_str(), "true" | "false") {
        return Some(Inferred::Boolean);
    }
    Some(Inferred::Text)
}

/// The narrowest type that holds every value of a column.
fn infer(rows: &[Vec<Option<String>>], col: usize) -> Inferred {
    let mut kind: Option<Inferred> = None;
    for v in rows.iter().filter_map(|r| r.get(col).cloned().flatten()) {
        let Some(k) = infer_one(&v) else { continue };
        kind = Some(match (kind, k) {
            (None, k) => k,
            (Some(a), b) if a == b => a,
            (Some(Inferred::Integer), Inferred::Decimal)
            | (Some(Inferred::Decimal), Inferred::Integer) => Inferred::Decimal,
            (Some(Inferred::Date), Inferred::Timestamp)
            | (Some(Inferred::Timestamp), Inferred::Date) => Inferred::Timestamp,
            _ => Inferred::Text,
        });
        if kind == Some(Inferred::Text) {
            break;
        }
    }
    kind.unwrap_or(Inferred::Text)
}

pub fn preview(path: &str, options: &ReadOptions) -> AppResult<ImportPreview> {
    let data = read(path, options)?;
    let n = data.columns.len();
    Ok(ImportPreview {
        format: data.format,
        encoding: data.encoding.into(),
        delimiter: data.delimiter,
        header: data.header,
        inferred: (0..n).map(|c| infer(&data.rows, c)).collect(),
        max_lengths: (0..n)
            .map(|c| {
                data.rows
                    .iter()
                    .filter_map(|r| r.get(c).cloned().flatten())
                    .map(|v| v.chars().count())
                    .max()
                    .unwrap_or(0)
            })
            .collect(),
        total_rows: data.rows.len(),
        rows: data.rows.into_iter().take(20).collect(),
        columns: data.columns,
    })
}

/// `1.234,56`, `1,234.56`, `12,5` and ` 7 ` as `1234.56`, `1234.56`, `12.5`, `7`.
fn number(v: &str) -> Option<String> {
    let v: String = v.trim().chars().filter(|c| !c.is_whitespace()).collect();
    let (comma, dot) = (v.rfind(','), v.rfind('.'));
    let normal = match (comma, dot) {
        (Some(c), Some(d)) if c > d => v.replace('.', "").replace(',', "."),
        (Some(_), Some(_)) => v.replace(',', ""),
        (Some(_), None) if v.matches(',').count() == 1 => v.replace(',', "."),
        (Some(_), None) => return None,
        _ => v.clone(),
    };
    let body = normal.strip_prefix(['-', '+']).unwrap_or(&normal);
    let ok = !body.is_empty()
        && body.matches('.').count() <= 1
        && body.bytes().all(|b| b.is_ascii_digit() || b == b'.')
        && body.bytes().any(|b| b.is_ascii_digit());
    ok.then_some(normal)
}

/// `31/12/2024 [23:59[:59]]` and ISO dates with a `T` as `2024-12-31 23:59:59`.
fn temporal(v: &str) -> Option<String> {
    let v = v.trim();
    let (date, time) = match v.split_once([' ', 'T']) {
        Some((d, t)) => (d, Some(t.trim_end_matches('Z'))),
        None => (v, None),
    };
    let iso = if date.len() == 10 && date.as_bytes()[4] == b'-' && date.as_bytes()[7] == b'-' {
        date.to_string()
    } else {
        let parts: Vec<&str> = date.split('/').collect();
        if parts.len() != 3 || parts[2].len() != 4 || parts[0].len() > 2 || parts[1].len() > 2 {
            return None;
        }
        let (d, m, y) = (
            parts[0].parse::<u32>().ok()?,
            parts[1].parse::<u32>().ok()?,
            parts[2],
        );
        if !(1..=31).contains(&d) || !(1..=12).contains(&m) {
            return None;
        }
        format!("{y}-{m:02}-{d:02}")
    };
    if !iso.bytes().enumerate().all(|(i, b)| {
        if i == 4 || i == 7 {
            b == b'-'
        } else {
            b.is_ascii_digit()
        }
    }) {
        return None;
    }
    match time {
        None => Some(iso),
        Some(t) => {
            let t = if t.len() == 5 {
                format!("{t}:00")
            } else {
                t.to_string()
            };
            let ok = t.len() >= 8 && t.as_bytes()[2] == b':' && t.as_bytes()[5] == b':';
            ok.then(|| format!("{iso} {t}"))
        }
    }
}

/// A file value as the column expects it. Empty values are NULL.
pub fn convert(kind: ValueKind, v: Option<String>) -> Option<String> {
    let v = v?;
    if v.trim().is_empty() {
        return None;
    }
    Some(match kind {
        ValueKind::Number => number(&v).unwrap_or(v),
        ValueKind::Temporal => temporal(&v).unwrap_or(v),
        _ => v,
    })
}

pub async fn import(
    s: &Session,
    req: &ImportRequest,
    cancel: &AtomicBool,
    progress: &(dyn Fn(Progress) + Sync),
) -> AppResult<ImportSummary> {
    let e = s.engine();
    let path = req.path.clone();
    let options = req.options.clone();
    let data = tauri::async_runtime::spawn_blocking(move || read(&path, &options))
        .await
        .map_err(|e| AppError::internal(e.to_string()))??;

    let table_columns = s.list_columns(&req.schema, &req.table).await?;
    if table_columns.is_empty() {
        return Err(AppError::not_found(format!(
            "{}.{} was not found.",
            req.schema, req.table
        )));
    }
    let generated = catalog::generated_columns(s, &req.schema, &req.table).await?;
    // File column → table column, matched by name ignoring case, spaces and underscores.
    let key = |n: &str| n.trim().to_lowercase().replace([' ', '_', '-'], "");
    let by_name: HashMap<String, &crate::model::DbColumn> = table_columns
        .iter()
        .filter(|c| !generated.contains(&c.name))
        .map(|c| (key(&c.name), c))
        .collect();
    let mut used = Vec::new();
    let mut skipped = Vec::new();
    for (i, name) in data.columns.iter().enumerate() {
        match by_name.get(&key(name)) {
            Some(c)
                if !used
                    .iter()
                    .any(|(_, u): &(usize, &crate::model::DbColumn)| u.name == c.name) =>
            {
                used.push((i, *c))
            }
            _ => skipped.push(name.clone()),
        }
    }
    if used.is_empty() {
        return Err(AppError::invalid(format!(
            "No column of the file matches a column of {}. The file has: {}.",
            req.table,
            data.columns.join(", ")
        )));
    }

    let target = sql::qualified(e, &req.schema, &req.table);
    let names: Vec<String> = used.iter().map(|(_, c)| c.name.clone()).collect();
    let quoted: Vec<String> = names.iter().map(|n| sql::ident(e, n)).collect();
    let kinds: Vec<ValueKind> = used
        .iter()
        .map(|(_, c)| sql::column_kind(e, &c.type_name))
        .collect();
    let types: Vec<String> = used.iter().map(|(_, c)| c.type_name.clone()).collect();
    let keys: Vec<bool> = used.iter().map(|(_, c)| c.primary_key).collect();
    let ids = catalog::identity_columns(s, &req.schema, &req.table).await?;
    let (before, after) = catalog::identity_statements(e, &target, &ids, &names);

    let run = |sql: String| async move {
        s.execute(&ExecRequest { sql, max_rows: 0 })
            .await
            .map(|_| ())
    };
    // CQL has no transactions (set_autocommit(false) is an error there), and
    // the INSERTs are not wrapped in BEGIN BATCH either: a batch spanning
    // partitions is an anti-pattern. Each row is its own statement; the
    // chunks only group the progress reports.
    let transactional = e != Engine::Cassandra;
    let was_autocommit = transactional && s.tx_state().autocommit;
    if was_autocommit {
        s.set_autocommit(false).await?;
    }
    let outcome = async {
        // Oracle commits the identity changes (DDL) on its own; everything
        // else is undone together on error.
        for stmt in &before {
            run(stmt.clone()).await?;
        }
        if req.replace {
            // CQL cannot DELETE without a key.
            let clear = if e == Engine::Cassandra {
                "TRUNCATE"
            } else {
                "DELETE FROM"
            };
            run(format!("{clear} {target}")).await?;
        }
        let t = InsertTarget {
            engine: e,
            table: &target,
            columns: &quoted,
            kinds: &kinds,
            types: &types,
            keys: &keys,
        };
        let mut done = 0usize;
        for chunk in data.rows.chunks(sql::ROWS_PER_INSERT) {
            if cancel.load(Ordering::SeqCst) {
                return Err(crate::drivers::stream_stopped());
            }
            let rows: Vec<Vec<Option<String>>> = chunk
                .iter()
                .map(|r| {
                    used.iter()
                        .zip(&kinds)
                        .map(|((i, _), k)| convert(*k, r.get(*i).cloned().flatten()))
                        .collect()
                })
                .collect();
            for stmt in sql::insert_statements(&t, &rows) {
                run(stmt).await.map_err(|mut err| {
                    let first = done + 1 + if data.header { 1 } else { 0 };
                    err.message = format!(
                        "Rows {first}–{} of the file: {}",
                        first + rows.len() - 1,
                        err.message
                    );
                    err
                })?;
            }
            done += rows.len();
            progress(Progress::new(&req.table, 1, 1, done as u64));
        }
        for stmt in &after {
            run(stmt.clone()).await?;
        }
        Ok(done as u64)
    }
    .await;
    let finish = if transactional {
        s.end_transaction(outcome.is_ok()).await
    } else {
        Ok(s.tx_state())
    };
    if was_autocommit {
        let _ = s.set_autocommit(true).await;
    }
    let rows = outcome?;
    finish?;
    Ok(ImportSummary {
        rows,
        skipped_columns: skipped,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn csv_with_quotes_and_line_breaks() {
        let text = "id;name;note\r\n1;\"Silva; Ana\";\"line 1\nline \"\"2\"\"\"\r\n2;Bruno;\n";
        assert_eq!(detect_delimiter(text), ';');
        let rows = parse_csv(text, ';');
        assert_eq!(rows.len(), 3);
        assert_eq!(rows[1], ["1", "Silva; Ana", "line 1\nline \"2\""]);
        assert_eq!(rows[2], ["2", "Bruno", ""]);
    }

    #[test]
    fn decodes_windows_1252_and_bom() {
        assert_eq!(decode(&[0xEF, 0xBB, 0xBF, b'a']).0, "a");
        let (text, enc) = decode(&[b'S', 0xE3, b'o', b' ', 0x80]);
        assert_eq!(text, "São €");
        assert_eq!(enc, "Windows-1252");
    }

    #[test]
    fn numbers_and_dates_from_spreadsheets() {
        assert_eq!(number("1.234,56").as_deref(), Some("1234.56"));
        assert_eq!(number("1,234.56").as_deref(), Some("1234.56"));
        assert_eq!(number("12,5").as_deref(), Some("12.5"));
        assert_eq!(number("abc"), None);
        assert_eq!(temporal("31/12/2024").as_deref(), Some("2024-12-31"));
        assert_eq!(
            temporal("1/2/2024 08:30").as_deref(),
            Some("2024-02-01 08:30:00")
        );
        assert_eq!(
            temporal("2024-02-29T13:45:10Z").as_deref(),
            Some("2024-02-29 13:45:10")
        );
        assert_eq!(temporal("12/2024"), None);
        assert_eq!(convert(ValueKind::Number, Some("  ".into())), None);
    }

    #[test]
    fn infers_column_types() {
        let rows = |vals: &[&str]| {
            vals.iter()
                .map(|v| vec![Some(v.to_string())])
                .collect::<Vec<_>>()
        };
        assert_eq!(infer(&rows(&["1", "22", ""]), 0), Inferred::Integer);
        assert_eq!(infer(&rows(&["1", "2,5"]), 0), Inferred::Decimal);
        assert_eq!(infer(&rows(&["01310", "22"]), 0), Inferred::Text);
        assert_eq!(
            infer(&rows(&["31/12/2024", "2024-01-01 10:00"]), 0),
            Inferred::Timestamp
        );
        assert_eq!(infer(&rows(&["true", "false"]), 0), Inferred::Boolean);
        assert_eq!(infer(&rows(&["x", "1"]), 0), Inferred::Text);
    }

    #[test]
    fn json_arrays_and_lines() {
        let (cols, rows) =
            parse_json("[{\"a\": 1, \"b\": \"x\"}, {\"b\": null, \"c\": [1]}]").unwrap();
        assert_eq!(cols, ["a", "b", "c"]);
        assert_eq!(rows[1], [None, None, Some("[1]".into())]);
        let (cols, rows) = parse_json("{\"a\": true}\n\n{\"a\": false}\n").unwrap();
        assert_eq!(cols, ["a"]);
        assert_eq!(rows.len(), 2);
    }

    #[test]
    fn json_numbers_keep_every_digit() {
        let (_, rows) = parse_json(
            "[{\"a\": 123456789012345678901234567890, \"b\": 1.50, \"c\": -1e5, \
             \"d\": \"x\\ny\", \"e\": {\"k\": [1, true]}, \"f\": false}]",
        )
        .unwrap();
        assert_eq!(
            rows[0],
            [
                Some("123456789012345678901234567890".into()),
                Some("1.50".into()),
                Some("-1e5".into()),
                Some("x\ny".into()),
                Some("{\"k\":[1,true]}".into()),
                Some("false".into()),
            ]
        );
        assert!(parse_json("[1]").is_err());
        assert!(parse_json("{\"a\": 1}\n[2]").is_err());
        // The last of repeated keys wins.
        let (cols, rows) = parse_json("[{\"a\": 1, \"a\": 2}]").unwrap();
        assert_eq!((cols.len(), rows[0][0].as_deref()), (1, Some("2")));
    }

    #[test]
    fn header_detection() {
        let r = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        assert!(looks_like_header(&r(&["id", "name"])));
        assert!(!looks_like_header(&r(&["1", "Ana"])));
        assert!(!looks_like_header(&r(&["a", "a"])));
    }
}
