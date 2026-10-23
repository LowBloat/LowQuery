//! Exports a table, a view or a whole schema: a SQL script (structure, data
//! or both) that runs back on the same database, or the rows as CSV or JSON.
//! Rows are streamed from the server to the file, never held in memory.

use std::fs::File;
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};

use serde::{Deserialize, Serialize};
use tokio::sync::mpsc;

use super::catalog;
use super::sql::{self, InsertTarget};
use super::Progress;
use crate::drivers::{Session, StreamEvent};
use crate::error::{AppError, AppResult, ErrorKind};
use crate::model::{ColumnInfo, DbObject, Engine, ValueKind};

#[derive(Debug, Clone, Copy, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum ExportFormat {
    Sql,
    Csv,
    /// CSV that Excel opens with accents and columns right: `;`, BOM, CRLF.
    CsvExcel,
    Json,
}

#[derive(Debug, Clone, Copy, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum ExportContent {
    All,
    Structure,
    Data,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ExportRequest {
    pub job_id: String,
    pub session_id: String,
    pub schema: String,
    /// The objects to export; `None` exports the whole schema.
    pub objects: Option<Vec<DbObject>>,
    pub format: ExportFormat,
    pub content: ExportContent,
    /// A file, or a folder for CSV/JSON of several tables.
    pub path: String,
    /// Header comment: the app version and the source.
    pub source: String,
}

#[derive(Debug, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ExportSummary {
    pub objects: usize,
    pub rows: u64,
    pub files: Vec<String>,
    /// Objects that could not be scripted, with the reason.
    pub warnings: Vec<String>,
}

/// Kinds a schema export scripts, in the order they are created.
const STRUCTURE_ORDER: [&str; 10] = [
    "sequence",
    "type",
    "table",
    "foreign",
    "view",
    "matview",
    "function",
    "procedure",
    "package",
    "synonym",
];

const DATA_KINDS: [&str; 4] = ["table", "view", "matview", "foreign"];

pub async fn export(
    s: &Session,
    req: &ExportRequest,
    cancel: &AtomicBool,
    progress: &(dyn Fn(Progress) + Sync),
) -> AppResult<ExportSummary> {
    let whole = req.objects.is_none();
    let mut objects = match &req.objects {
        Some(list) => list.clone(),
        None => s.list_objects(&req.schema).await?,
    };
    objects.retain(|o| STRUCTURE_ORDER.contains(&o.kind.as_str()));
    // A schema export dumps the rows of tables; picking a view exports its rows.
    let data: Vec<String> = objects
        .iter()
        .filter(|o| {
            if whole {
                o.kind == "table"
            } else {
                DATA_KINDS.contains(&o.kind.as_str())
            }
        })
        .map(|o| o.name.clone())
        .collect();
    let data = catalog::dependency_order(s, &req.schema, data).await;
    if req.format == ExportFormat::Sql {
        export_sql(s, req, &objects, &data, cancel, progress).await
    } else {
        export_rows(s, req, &data, cancel, progress).await
    }
}

/// Writes to `<path>.part` and renames it at the end, so a cancelled or
/// failed export leaves no half file behind.
struct Output {
    path: PathBuf,
    part: PathBuf,
    writer: BufWriter<File>,
}

impl Output {
    fn create(path: &Path) -> AppResult<Self> {
        let mut part = path.as_os_str().to_owned();
        part.push(".part");
        let part = PathBuf::from(part);
        let writer = BufWriter::with_capacity(1 << 20, File::create(&part)?);
        Ok(Output {
            path: path.to_path_buf(),
            part,
            writer,
        })
    }

    fn finish(mut self) -> AppResult<String> {
        self.writer.flush()?;
        drop(self.writer);
        std::fs::rename(&self.part, &self.path)?;
        Ok(self.path.to_string_lossy().into_owned())
    }

    fn abandon(self) {
        drop(self.writer);
        let _ = std::fs::remove_file(&self.part);
    }
}

fn comment(text: &str) -> String {
    text.lines()
        .map(|l| format!("-- {l}\n"))
        .collect::<String>()
}

/// The statement's ending in a script, after DDL that may end in `/` or `GO`.
fn needs_terminator(ddl: &str) -> bool {
    let last = ddl.trim_end().lines().last().unwrap_or_default().trim();
    !(last.ends_with(';')
        || last == "/"
        || last.eq_ignore_ascii_case("GO")
        || last.eq_ignore_ascii_case("DELIMITER ;"))
}

async fn export_sql(
    s: &Session,
    req: &ExportRequest,
    objects: &[DbObject],
    data: &[String],
    cancel: &AtomicBool,
    progress: &(dyn Fn(Progress) + Sync),
) -> AppResult<ExportSummary> {
    let e = s.engine();
    let mut out = Output::create(Path::new(&req.path))?;
    let outcome = async {
        let mut summary = ExportSummary::default();
        let w = &mut out.writer;
        write!(w, "{}", comment(&req.source))?;
        writeln!(w)?;
        let mysql = matches!(e, Engine::Mysql | Engine::Mariadb);
        if mysql {
            writeln!(w, "SET FOREIGN_KEY_CHECKS = 0;\n")?;
        }
        let structure = req.content != ExportContent::Data;
        let with_rows = req.content != ExportContent::Structure;
        // Foreign keys PostgreSQL and SQL Server script as ALTER TABLE go to
        // the end, after every table and its rows exist.
        let mut foreign_keys = Vec::new();
        let total =
            if structure { objects.len() } else { 0 } + if with_rows { data.len() } else { 0 };
        let mut step = 0;

        if structure {
            let mut ordered: Vec<&DbObject> = Vec::new();
            for kind in STRUCTURE_ORDER {
                if kind == "table" {
                    for name in &catalog::dependency_order(
                        s,
                        &req.schema,
                        objects
                            .iter()
                            .filter(|o| o.kind == "table")
                            .map(|o| o.name.clone())
                            .collect(),
                    )
                    .await
                    {
                        ordered.extend(
                            objects
                                .iter()
                                .filter(|o| o.kind == "table" && &o.name == name),
                        );
                    }
                } else {
                    ordered.extend(objects.iter().filter(|o| o.kind == kind));
                }
            }
            for o in ordered {
                if cancel.load(Ordering::SeqCst) {
                    return Err(crate::drivers::stream_stopped());
                }
                step += 1;
                progress(Progress::new(&o.name, step, total, 0));
                match s.object_ddl(&req.schema, &o.name, &o.kind).await {
                    Ok(ddl) => {
                        let mut body = String::new();
                        for line in ddl.lines() {
                            let t = line.trim_start();
                            if t.starts_with("ALTER TABLE") && t.contains(" FOREIGN KEY ") {
                                foreign_keys.push(line.to_string());
                            } else {
                                body.push_str(line);
                                body.push('\n');
                            }
                        }
                        write!(w, "{}", comment(&format!("{} {}", o.kind, o.name)))?;
                        let body = body.trim_end();
                        write!(w, "{body}")?;
                        if needs_terminator(body) {
                            write!(w, ";")?;
                        }
                        writeln!(w, "\n")?;
                    }
                    Err(err) => {
                        summary
                            .warnings
                            .push(format!("{} {}: {}", o.kind, o.name, err.message));
                        writeln!(
                            w,
                            "{}",
                            comment(&format!(
                                "{} {} was not scripted: {}",
                                o.kind, o.name, err.message
                            ))
                        )?;
                    }
                }
                summary.objects += 1;
            }
        }

        if with_rows {
            for table in data {
                if cancel.load(Ordering::SeqCst) {
                    return Err(crate::drivers::stream_stopped());
                }
                step += 1;
                let rows = write_table_rows(s, &req.schema, table, w, cancel, &|n| {
                    progress(Progress::new(table, step, total, n))
                })
                .await?;
                summary.rows += rows;
                if !structure {
                    summary.objects += 1;
                }
            }
        }

        if !foreign_keys.is_empty() {
            write!(w, "{}", comment("Foreign keys"))?;
            for fk in &foreign_keys {
                writeln!(w, "{}", fk.trim())?;
            }
            writeln!(w)?;
        }
        if mysql {
            writeln!(w, "SET FOREIGN_KEY_CHECKS = 1;")?;
        }
        Ok(summary)
    }
    .await;
    match outcome {
        Ok(mut summary) => {
            summary.files.push(out.finish()?);
            Ok(summary)
        }
        Err(err) => {
            out.abandon();
            Err(err)
        }
    }
}

/// INSERT statements for every row of a table, with what identity columns
/// need around them.
async fn write_table_rows(
    s: &Session,
    schema: &str,
    table: &str,
    w: &mut BufWriter<File>,
    cancel: &AtomicBool,
    progress: &(dyn Fn(u64) + Sync),
) -> AppResult<u64> {
    let e = s.engine();
    let generated = catalog::generated_columns(s, schema, table).await?;
    let columns: Vec<String> = s
        .list_columns(schema, table)
        .await?
        .into_iter()
        .map(|c| c.name)
        .filter(|n| !generated.contains(n))
        .collect();
    if columns.is_empty() {
        return Ok(0);
    }
    let target = sql::qualified(e, schema, table);
    let quoted: Vec<String> = columns.iter().map(|c| sql::ident(e, c)).collect();
    let ids = catalog::identity_columns(s, schema, table).await?;
    let (before, after) = catalog::identity_statements(e, &target, &ids, &columns);
    let select = format!("SELECT {} FROM {target}", quoted.join(", "));

    write!(w, "{}", comment(&format!("Data for {table}")))?;
    for stmt in &before {
        writeln!(w, "{stmt};")?;
    }
    let mut kinds: Vec<ValueKind> = Vec::new();
    let rows = stream_rows(
        s,
        &select,
        cancel,
        |cols, batch| {
            if kinds.is_empty() {
                kinds = cols.iter().map(|c| c.kind).collect();
            }
            let t = InsertTarget {
                engine: e,
                table: &target,
                columns: &quoted,
                kinds: &kinds,
            };
            sql::write_inserts(w, &t, batch)?;
            Ok(())
        },
        progress,
    )
    .await?;
    for stmt in &after {
        writeln!(w, "{stmt};")?;
    }
    writeln!(w)?;
    Ok(rows)
}

/// Streams the rows of `select` to `write`, batch by batch, until the end or
/// a cancel.
async fn stream_rows(
    s: &Session,
    select: &str,
    cancel: &AtomicBool,
    mut write: impl FnMut(&[ColumnInfo], &[Vec<Option<String>>]) -> AppResult<()>,
    progress: &(dyn Fn(u64) + Sync),
) -> AppResult<u64> {
    let (tx, mut rx) = mpsc::channel(4);
    let reader = s.stream(select, tx);
    let consumer = async {
        let mut columns = Vec::new();
        let mut rows = 0u64;
        while let Some(event) = rx.recv().await {
            if cancel.load(Ordering::SeqCst) {
                return Err(crate::drivers::stream_stopped());
            }
            match event {
                StreamEvent::Columns(c) => columns = c,
                StreamEvent::Rows(batch) => {
                    write(&columns, &batch)?;
                    rows += batch.len() as u64;
                    progress(rows);
                }
            }
        }
        Ok(rows)
    };
    // The reader ends when the consumer stops (its receiver is dropped).
    let (read, written) = tokio::join!(reader, consumer);
    let written = written?;
    read?;
    Ok(written)
}

fn csv_field(v: &str, delimiter: char) -> String {
    if v.contains(delimiter) || v.contains(['"', '\n', '\r']) {
        format!("\"{}\"", v.replace('"', "\"\""))
    } else {
        v.to_string()
    }
}

fn json_value(kind: ValueKind, v: Option<&str>) -> String {
    let Some(v) = v else {
        return "null".into();
    };
    match kind {
        ValueKind::Number if serde_json::from_str::<serde_json::Number>(v).is_ok() => v.into(),
        ValueKind::Bool => match sql::truth(v) {
            Some(b) => b.to_string(),
            None => serde_json::Value::String(v.into()).to_string(),
        },
        ValueKind::Json => match serde_json::from_str::<serde_json::Value>(v) {
            Ok(j) => j.to_string(),
            Err(_) => serde_json::Value::String(v.into()).to_string(),
        },
        _ => serde_json::Value::String(v.into()).to_string(),
    }
}

/// File name for a table's rows: the table name without characters that
/// file systems reject.
fn file_name(table: &str, ext: &str) -> String {
    let safe: String = table
        .chars()
        .map(|c| {
            if "<>:\"/\\|?*".contains(c) || c.is_control() {
                '_'
            } else {
                c
            }
        })
        .collect();
    format!("{safe}.{ext}")
}

async fn export_rows(
    s: &Session,
    req: &ExportRequest,
    tables: &[String],
    cancel: &AtomicBool,
    progress: &(dyn Fn(Progress) + Sync),
) -> AppResult<ExportSummary> {
    let e = s.engine();
    let ext = if req.format == ExportFormat::Json {
        "json"
    } else {
        "csv"
    };
    let single = tables.len() == 1 && req.objects.is_some();
    if !single {
        std::fs::create_dir_all(&req.path)?;
    }
    let mut summary = ExportSummary::default();
    for (i, table) in tables.iter().enumerate() {
        let path = if single {
            PathBuf::from(&req.path)
        } else {
            Path::new(&req.path).join(file_name(table, ext))
        };
        let mut out = Output::create(&path)?;
        let select = format!("SELECT * FROM {}", sql::qualified(e, &req.schema, table));
        let report = |n| progress(Progress::new(table, i + 1, tables.len(), n));
        let format = req.format;
        let outcome = async {
            let w = &mut out.writer;
            let (delimiter, newline) = if format == ExportFormat::CsvExcel {
                (';', "\r\n")
            } else {
                (',', "\n")
            };
            if format == ExportFormat::CsvExcel {
                w.write_all("\u{feff}".as_bytes())?;
            }
            if format == ExportFormat::Json {
                write!(w, "[")?;
            }
            let mut first = true;
            let rows = stream_rows(
                s,
                &select,
                cancel,
                |cols, batch| {
                    if format == ExportFormat::Json {
                        let keys: Vec<String> = cols
                            .iter()
                            .map(|c| serde_json::Value::String(c.name.clone()).to_string())
                            .collect();
                        for row in batch {
                            let fields: Vec<String> = keys
                                .iter()
                                .zip(cols)
                                .zip(row)
                                .map(|((k, c), v)| {
                                    format!("{k}: {}", json_value(c.kind, v.as_deref()))
                                })
                                .collect();
                            write!(
                                w,
                                "{}\n  {{{}}}",
                                if first { "" } else { "," },
                                fields.join(", ")
                            )?;
                            first = false;
                        }
                    } else {
                        if first {
                            let header: Vec<String> =
                                cols.iter().map(|c| csv_field(&c.name, delimiter)).collect();
                            write!(w, "{}{newline}", header.join(&delimiter.to_string()))?;
                            first = false;
                        }
                        for row in batch {
                            let fields: Vec<String> = row
                                .iter()
                                .map(|v| {
                                    v.as_deref()
                                        .map(|v| csv_field(v, delimiter))
                                        .unwrap_or_default()
                                })
                                .collect();
                            write!(w, "{}{newline}", fields.join(&delimiter.to_string()))?;
                        }
                    }
                    Ok(())
                },
                &report,
            )
            .await?;
            if format == ExportFormat::Json {
                writeln!(w, "{}]", if first { "" } else { "\n" })?;
            }
            Ok::<u64, AppError>(rows)
        }
        .await;
        match outcome {
            Ok(rows) => {
                summary.rows += rows;
                summary.objects += 1;
                summary.files.push(out.finish()?);
            }
            Err(err) => {
                out.abandon();
                return Err(err);
            }
        }
    }
    if summary.objects == 0 {
        return Err(AppError::new(
            ErrorKind::Invalid,
            "There are no tables to export.",
        ));
    }
    Ok(summary)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn csv_quotes_only_when_needed() {
        assert_eq!(csv_field("plain", ','), "plain");
        assert_eq!(csv_field("a,b", ','), "\"a,b\"");
        assert_eq!(csv_field("a,b", ';'), "a,b");
        assert_eq!(csv_field("say \"hi\"\nbye", ';'), "\"say \"\"hi\"\"\nbye\"");
    }

    #[test]
    fn json_keeps_types() {
        assert_eq!(json_value(ValueKind::Number, Some("12.50")), "12.50");
        assert_eq!(json_value(ValueKind::Number, Some("NaN")), "\"NaN\"");
        assert_eq!(json_value(ValueKind::Bool, Some("true")), "true");
        assert_eq!(
            json_value(ValueKind::Json, Some("{\"a\": [1, 2]}")),
            "{\"a\":[1,2]}"
        );
        assert_eq!(json_value(ValueKind::Text, None), "null");
    }

    #[test]
    fn terminators_follow_the_ddl() {
        assert!(!needs_terminator("CREATE TABLE t (a int);\n"));
        assert!(!needs_terminator("CREATE PROCEDURE p AS SELECT 1\nGO\n"));
        assert!(!needs_terminator("END;\n/\n"));
        assert!(needs_terminator("CREATE VIEW v AS SELECT 1"));
        assert_eq!(file_name("a/b:c", "csv"), "a_b_c.csv");
    }
}
