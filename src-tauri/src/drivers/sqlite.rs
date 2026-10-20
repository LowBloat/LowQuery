//! SQLite session on an embedded database file, through `rusqlite` with the
//! bundled SQLite library.
//!
//! The connection is synchronous and not `Sync`: it lives behind a mutex and
//! every call runs in `spawn_blocking`. The row limit stops stepping the
//! statement at `max_rows + 1`. Cancel uses `sqlite3_interrupt`, which is safe
//! to call from another thread while a statement runs.
//!
//! Grid editing: the column origins come from `sqlite3_column_table_name`
//! and friends, which resolve views and subqueries to their base table. An
//! `EXPLAIN QUERY PLAN` then checks that the rows come from one table
//! instance (no joins, self-joins or compound selects, whose origins SQLite
//! reports from one arm only). Keys come from `pragma_table_xinfo`, cached per
//! session.

use std::collections::HashMap;
use std::fmt::Write as _;
use std::panic::AssertUnwindSafe;
use std::path::Path;
use std::sync::Arc;
use std::time::Instant;

use parking_lot::Mutex;
use rusqlite::types::{Type, Value, ValueRef};
use rusqlite::{ffi, Connection, InterruptHandle, OpenFlags, Statement};

use super::{first_keywords, TxState};
use crate::error::{AppError, AppResult, ErrorKind};
use crate::model::{
    ApplyRequest, ApplyResult, ColumnInfo, ConnectionProfile, DbColumn, DbObject, EditColumn,
    EditTarget, ExecRequest, ExecResult, RowChange, ValueKind,
};

const BINARY_PREVIEW_BYTES: usize = 32 * 1024;
const BUSY_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

pub struct LiteSession {
    conn: Arc<Mutex<Connection>>,
    interrupt: InterruptHandle,
    state: Arc<Mutex<TxState>>,
    catalog: Arc<Mutex<Catalog>>,
    pub server_version: String,
    pub current_user: Option<String>,
    pub current_schema: Option<String>,
}

pub async fn connect(profile: &ConnectionProfile) -> AppResult<LiteSession> {
    let path = profile.database.trim().to_string();
    let create = profile.create_file;
    blocking(move || {
        if path.is_empty() {
            return Err(AppError::invalid("Choose the database file."));
        }
        let special = path == ":memory:" || path.starts_with("file:");
        if !special && !Path::new(&path).exists() && !create {
            return Err(AppError::connection(format!(
                "Database file not found: {path}"
            )));
        }
        let mut flags = OpenFlags::SQLITE_OPEN_READ_WRITE
            | OpenFlags::SQLITE_OPEN_NO_MUTEX
            | OpenFlags::SQLITE_OPEN_URI;
        if create {
            flags |= OpenFlags::SQLITE_OPEN_CREATE;
        }
        let conn = Connection::open_with_flags(&path, flags).map_err(connection_error)?;
        conn.busy_timeout(BUSY_TIMEOUT).map_err(connection_error)?;
        conn.execute_batch("PRAGMA foreign_keys = ON")
            .map_err(connection_error)?;
        // Opening is lazy: reading the schema catches files that are not
        // databases now instead of on the first statement.
        conn.query_row("SELECT count(*) FROM sqlite_schema", [], |r| {
            r.get::<_, i64>(0)
        })
        .map_err(connection_error)?;

        Ok(LiteSession {
            interrupt: conn.get_interrupt_handle(),
            conn: Arc::new(Mutex::new(conn)),
            state: Arc::new(Mutex::new(TxState {
                autocommit: true,
                in_transaction: false,
            })),
            catalog: Arc::default(),
            server_version: format!("SQLite {}", rusqlite::version()),
            current_user: None,
            current_schema: Some("main".into()),
        })
    })
    .await
}

async fn blocking<T, F>(f: F) -> AppResult<T>
where
    T: Send + 'static,
    F: FnOnce() -> AppResult<T> + Send + 'static,
{
    tauri::async_runtime::spawn_blocking(f)
        .await
        .map_err(|e| AppError::internal(e.to_string()))?
}

impl LiteSession {
    pub async fn apply_changes(&self, req: &ApplyRequest) -> AppResult<ApplyResult> {
        // Every value is converted before anything runs.
        let steps = plan_changes(&req.target, &req.changes)?;
        let conn = self.conn.clone();
        let state = self.state.clone();
        blocking(move || {
            let conn = conn.lock();
            let autocommit = state.lock().autocommit;
            let started = Instant::now();
            let outcome = apply_steps(&conn, &steps, autocommit);
            let in_transaction = !conn.is_autocommit();
            state.lock().in_transaction = in_transaction;
            let mut result = outcome?;
            result.elapsed_ms = started.elapsed().as_millis() as u64;
            result.in_transaction = in_transaction;
            Ok(result)
        })
        .await
    }

    pub fn tx_state(&self) -> TxState {
        *self.state.lock()
    }

    pub async fn execute(&self, req: &ExecRequest) -> AppResult<ExecResult> {
        let conn = self.conn.clone();
        let state = self.state.clone();
        let catalog = self.catalog.clone();
        let sql = req.sql.clone();
        let max_rows = req.max_rows;
        blocking(move || {
            let conn = conn.lock();
            let autocommit = state.lock().autocommit;
            let started = Instant::now();
            let outcome = run(&conn, &catalog, &sql, max_rows, autocommit);
            // SQLite knows whether a transaction is open, including one
            // started or ended by the user's own BEGIN/COMMIT/ROLLBACK.
            let in_transaction = !conn.is_autocommit();
            state.lock().in_transaction = in_transaction;
            let mut result = outcome?;
            result.elapsed_ms = started.elapsed().as_millis() as u64;
            result.in_transaction = in_transaction;
            Ok(result)
        })
        .await
    }

    pub async fn cancel(&self) -> AppResult<()> {
        self.interrupt.interrupt();
        Ok(())
    }

    pub async fn set_autocommit(&self, on: bool) -> AppResult<TxState> {
        let conn = self.conn.clone();
        let state = self.state.clone();
        blocking(move || {
            let conn = conn.lock();
            // Switching back to autocommit commits pending work, as JDBC
            // clients do.
            let outcome = if on && !conn.is_autocommit() {
                conn.execute_batch("COMMIT").map_err(map_error)
            } else {
                Ok(())
            };
            let mut st = state.lock();
            st.in_transaction = !conn.is_autocommit();
            outcome?;
            st.autocommit = on;
            Ok(*st)
        })
        .await
    }

    pub async fn end_transaction(&self, commit: bool) -> AppResult<TxState> {
        let conn = self.conn.clone();
        let state = self.state.clone();
        blocking(move || {
            let conn = conn.lock();
            let outcome = if conn.is_autocommit() {
                Ok(())
            } else {
                conn.execute_batch(if commit { "COMMIT" } else { "ROLLBACK" })
                    .map_err(map_error)
            };
            let mut st = state.lock();
            st.in_transaction = !conn.is_autocommit();
            outcome?;
            Ok(*st)
        })
        .await
    }

    pub async fn list_schemas(&self) -> AppResult<Vec<String>> {
        let rows = self
            .query_rows(
                "SELECT name FROM pragma_database_list
                 ORDER BY CASE name WHEN 'main' THEN 0 WHEN 'temp' THEN 2 ELSE 1 END, name"
                    .into(),
                Vec::new(),
            )
            .await?;
        Ok(rows.into_iter().map(|mut r| r.swap_remove(0)).collect())
    }

    pub async fn list_objects(&self, schema: &str) -> AppResult<Vec<DbObject>> {
        let sql = format!(
            "SELECT name, type FROM {}.sqlite_schema
             WHERE type IN ('table', 'view') AND name NOT LIKE 'sqlite\\_%' ESCAPE '\\'
             ORDER BY type, name",
            quote_ident(schema)
        );
        let rows = self.query_rows(sql, Vec::new()).await?;
        Ok(rows
            .into_iter()
            .map(|mut r| DbObject {
                kind: r.pop().unwrap_or_default(),
                name: r.pop().unwrap_or_default(),
            })
            .collect())
    }

    pub async fn list_columns(&self, schema: &str, table: &str) -> AppResult<Vec<DbColumn>> {
        // table_xinfo also lists generated columns; hidden = 1 are the
        // hidden columns of virtual tables.
        let rows = self
            .query_rows(
                "SELECT name, type, \"notnull\", pk FROM pragma_table_xinfo(?1, ?2)
                 WHERE hidden <> 1 ORDER BY cid"
                    .into(),
                vec![table.to_string(), schema.to_string()],
            )
            .await?;
        Ok(rows
            .into_iter()
            .map(|r| DbColumn {
                name: r[0].clone(),
                type_name: r[1].clone(),
                nullable: r[2] == "0",
                primary_key: r[3] != "0",
            })
            .collect())
    }

    async fn query_rows(&self, sql: String, params: Vec<String>) -> AppResult<Vec<Vec<String>>> {
        let conn = self.conn.clone();
        blocking(move || {
            let conn = conn.lock();
            let mut stmt = conn.prepare(&sql).map_err(map_error)?;
            let n = stmt.column_count();
            let mut rows = stmt
                .query(rusqlite::params_from_iter(params.iter()))
                .map_err(map_error)?;
            let mut out = Vec::new();
            while let Some(row) = rows.next().map_err(map_error)? {
                let mut cells = Vec::with_capacity(n);
                for i in 0..n {
                    cells.push(cell(row.get_ref(i).map_err(map_error)?).unwrap_or_default());
                }
                out.push(cells);
            }
            Ok(out)
        })
        .await
    }
}

/// Runs one statement. In manual-commit mode a transaction is opened first
/// and stays open until the user ends it.
fn run(
    conn: &Connection,
    catalog: &Mutex<Catalog>,
    sql: &str,
    max_rows: usize,
    autocommit: bool,
) -> AppResult<ExecResult> {
    if is_blank(sql) {
        return Ok(ExecResult::default());
    }
    let kw = first_keywords(sql, 1);
    let first = kw.first().map(String::as_str).unwrap_or("");
    // Transaction control is the user's own; VACUUM, ATTACH and DETACH
    // cannot run inside a transaction, and several PRAGMAs are ignored or
    // fail there.
    let skip_begin = matches!(
        first,
        "BEGIN"
            | "COMMIT"
            | "END"
            | "ROLLBACK"
            | "SAVEPOINT"
            | "RELEASE"
            | "VACUUM"
            | "ATTACH"
            | "DETACH"
            | "PRAGMA"
    );
    let opened = !autocommit && conn.is_autocommit() && !skip_begin;
    if opened {
        conn.execute_batch("BEGIN").map_err(map_error)?;
    }
    let outcome = statement(conn, catalog, sql, max_rows, first);
    if outcome.is_err() && opened && !conn.is_autocommit() {
        // The failed statement was the only one in the transaction opened
        // for it: do not leave an empty transaction behind.
        let _ = conn.execute_batch("ROLLBACK");
    }
    outcome
}

fn statement(
    conn: &Connection,
    catalog: &Mutex<Catalog>,
    sql: &str,
    max_rows: usize,
    first: &str,
) -> AppResult<ExecResult> {
    let mut stmt = conn.prepare(sql).map_err(map_error)?;
    let readonly = stmt.readonly();
    if !readonly || matches!(first, "ATTACH" | "DETACH") {
        // DDL may change keys: reload them on the next query.
        catalog.lock().tables.clear();
    }
    if stmt.column_count() == 0 {
        stmt.raw_execute().map_err(map_error)?;
        // changes() keeps the count of the last DML, so it only means
        // something right after one. WITH without result columns is DML.
        let dml = matches!(first, "INSERT" | "UPDATE" | "DELETE" | "REPLACE" | "WITH");
        return Ok(ExecResult {
            rows_affected: dml.then(|| conn.changes()),
            ..Default::default()
        });
    }

    let declared: Vec<(String, Option<String>)> = stmt
        .columns()
        .iter()
        .map(|c| {
            (
                c.name().to_string(),
                c.decl_type()
                    .map(|t| t.trim().to_string())
                    .filter(|t| !t.is_empty()),
            )
        })
        .collect();
    let n = declared.len();
    let editable = readonly && matches!(first, "SELECT" | "WITH");
    let origins = if editable {
        column_origins(&stmt)
    } else {
        None
    };
    let decls: Vec<Option<String>> = declared.iter().map(|(_, d)| d.clone()).collect();
    // Storage class of the first non-null value, for columns without a
    // declared type (expressions).
    let mut classes: Vec<Option<Type>> = vec![None; n];
    let mut out = Vec::new();
    let mut truncated = false;
    let mut rows = stmt.raw_query();
    while let Some(row) = rows.next().map_err(map_error)? {
        if max_rows > 0 && out.len() == max_rows {
            truncated = true;
            break;
        }
        let mut cells = Vec::with_capacity(n);
        for (i, class) in classes.iter_mut().enumerate() {
            let value = row.get_ref(i).map_err(map_error)?;
            if class.is_none() && value.data_type() != Type::Null {
                *class = Some(value.data_type());
            }
            cells.push(cell(value));
        }
        out.push(cells);
    }
    drop(rows);

    let columns = declared
        .into_iter()
        .zip(classes)
        .map(|((name, decl), class)| match decl {
            Some(t) => ColumnInfo {
                name,
                kind: declared_kind(&t),
                type_name: t,
            },
            None => ColumnInfo {
                name,
                type_name: class_name(class).into(),
                kind: class_kind(class),
            },
        })
        .collect();
    let mut result = ExecResult {
        columns: Some(columns),
        rows: out,
        truncated,
        ..Default::default()
    };
    if editable {
        let target = match origins {
            Some(origins) => edit_target(conn, catalog, sql, &origins, &decls),
            None => Err(UNKNOWN_TABLE.into()),
        };
        match target {
            Ok(target) => result.editable = Some(target),
            Err(reason) => result.read_only_reason = Some(reason),
        }
    }
    Ok(result)
}

/// Only whitespace, comments and semicolons: SQLite prepares nothing.
fn is_blank(sql: &str) -> bool {
    let bytes = sql.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        let c = bytes[i];
        if c.is_ascii_whitespace() || c == b';' {
            i += 1;
        } else if c == b'-' && bytes.get(i + 1) == Some(&b'-') {
            while i < bytes.len() && bytes[i] != b'\n' {
                i += 1;
            }
        } else if c == b'/' && bytes.get(i + 1) == Some(&b'*') {
            i += 2;
            while i + 1 < bytes.len() && !(bytes[i] == b'*' && bytes[i + 1] == b'/') {
                i += 1;
            }
            i += 2;
        } else {
            return false;
        }
    }
    true
}

fn quote_ident(name: &str) -> String {
    format!("\"{}\"", name.replace('"', "\"\""))
}

/// Kind from the declared type, following SQLite's affinity rules plus the
/// common names for booleans, dates and JSON.
fn declared_kind(decl: &str) -> ValueKind {
    let t = decl.to_ascii_uppercase();
    let has = |s: &str| t.contains(s);
    if has("BOOL") {
        ValueKind::Bool
    } else if has("JSON") {
        ValueKind::Json
    } else if has("DATE") || has("TIME") {
        ValueKind::Temporal
    } else if has("INT") {
        ValueKind::Number
    } else if has("CHAR") || has("CLOB") || has("TEXT") || has("STRING") || has("UUID") {
        ValueKind::Text
    } else if has("BLOB") || has("BINARY") {
        ValueKind::Binary
    } else if has("REAL") || has("FLOA") || has("DOUB") || has("NUM") || has("DEC") || has("MONEY")
    {
        ValueKind::Number
    } else {
        ValueKind::Other
    }
}

fn class_name(class: Option<Type>) -> &'static str {
    match class {
        Some(Type::Integer) => "INTEGER",
        Some(Type::Real) => "REAL",
        Some(Type::Text) => "TEXT",
        Some(Type::Blob) => "BLOB",
        Some(Type::Null) | None => "NULL",
    }
}

fn class_kind(class: Option<Type>) -> ValueKind {
    match class {
        Some(Type::Integer | Type::Real) => ValueKind::Number,
        Some(Type::Text) => ValueKind::Text,
        Some(Type::Blob) => ValueKind::Binary,
        Some(Type::Null) | None => ValueKind::Other,
    }
}

fn cell(value: ValueRef<'_>) -> Option<String> {
    match value {
        ValueRef::Null => None,
        ValueRef::Integer(i) => Some(i.to_string()),
        ValueRef::Real(f) => Some(real(f)),
        ValueRef::Text(t) => Some(String::from_utf8_lossy(t).into_owned()),
        ValueRef::Blob(b) => Some(blob(b)),
    }
}

/// Shortest text that reads back as the same double. Always shows it is a
/// REAL (`3.0`, `1.0e+300`), like the sqlite3 shell.
fn real(v: f64) -> String {
    if v.is_nan() {
        return "NaN".into();
    }
    if v.is_infinite() {
        return if v > 0.0 { "Inf" } else { "-Inf" }.into();
    }
    let abs = v.abs();
    if abs != 0.0 && !(1e-5..1e16).contains(&abs) {
        let s = format!("{v:e}");
        let (mantissa, exp) = s.split_once('e').unwrap_or((&s, "0"));
        let mantissa = if mantissa.contains('.') {
            mantissa.to_string()
        } else {
            format!("{mantissa}.0")
        };
        let exp = if exp.starts_with('-') {
            exp.to_string()
        } else {
            format!("+{exp}")
        };
        return format!("{mantissa}e{exp}");
    }
    let s = v.to_string();
    if s.contains('.') {
        s
    } else {
        s + ".0"
    }
}

/// `X'00FF'` like a blob literal; large blobs show the first 32 KiB.
fn blob(bytes: &[u8]) -> String {
    let shown = &bytes[..bytes.len().min(BINARY_PREVIEW_BYTES)];
    let mut s = String::with_capacity(shown.len() * 2 + 3);
    s.push_str("X'");
    for b in shown {
        let _ = write!(s, "{b:02X}");
    }
    s.push('\'');
    if bytes.len() > shown.len() {
        let _ = write!(s, "… ({} bytes)", bytes.len());
    }
    s
}

const UNKNOWN_TABLE: &str = "Read-only: the table behind this result could not be identified.";

/// Keys and columns of the tables behind editable results, per session.
#[derive(Default)]
struct Catalog {
    tables: HashMap<(String, String), Arc<TableMeta>>,
}

#[derive(Debug, Default)]
struct TableMeta {
    /// Column names, with whether each one can be written (generated and
    /// hidden columns cannot).
    columns: Vec<(String, bool)>,
    /// Primary key columns in key order.
    pk: Vec<String>,
    /// UNIQUE keys whose columns are all NOT NULL.
    unique: Vec<Vec<String>>,
}

/// (database, table, column) behind each result column, `None` for
/// expressions. `None` overall if SQLite returns a name that is not UTF-8.
fn column_origins(stmt: &Statement<'_>) -> Option<Vec<Option<(String, String, String)>>> {
    // rusqlite panics on names that are not UTF-8.
    std::panic::catch_unwind(AssertUnwindSafe(|| {
        stmt.columns_with_metadata()
            .iter()
            .map(|m| {
                Some((
                    m.database_name()?.to_string(),
                    m.table_name()?.to_string(),
                    m.origin_name()?.to_string(),
                ))
            })
            .collect()
    }))
    .ok()
}

/// The table an editable result comes from, or why it is read-only.
fn edit_target(
    conn: &Connection,
    catalog: &Mutex<Catalog>,
    sql: &str,
    origins: &[Option<(String, String, String)>],
    decls: &[Option<String>],
) -> Result<EditTarget, String> {
    let mut tables = origins.iter().flatten().map(|(db, t, _)| (db, t));
    let Some((db, table)) = tables.next() else {
        return Err("Read-only: only rows from a table can be edited.".into());
    };
    if tables.any(|(d, t)| d != db || t != table) {
        return Err("Read-only: the result combines columns from several tables.".into());
    }
    let shape = query_plan(conn, sql).map_err(|_| UNKNOWN_TABLE.to_string())?;
    if shape.compound {
        return Err("Read-only: the result combines rows from several queries.".into());
    }
    if shape.sources != 1 {
        return Err("Read-only: the result combines rows from several tables.".into());
    }
    let meta = table_meta(conn, catalog, db, table).map_err(|_| UNKNOWN_TABLE.to_string())?;
    build_target(db, table, &meta, origins, decls)
}

fn build_target(
    db: &str,
    table: &str,
    meta: &TableMeta,
    origins: &[Option<(String, String, String)>],
    decls: &[Option<String>],
) -> Result<EditTarget, String> {
    let has_rowid = meta.pk.is_empty()
        && !meta
            .columns
            .iter()
            .any(|(n, _)| n.eq_ignore_ascii_case("rowid"));
    let columns: Vec<Option<EditColumn>> = origins
        .iter()
        .enumerate()
        .map(|(i, origin)| {
            let (_, _, origin) = origin.as_ref()?;
            if let Some((name, writable)) = meta
                .columns
                .iter()
                .find(|(n, _)| n.eq_ignore_ascii_case(origin))
            {
                return writable.then(|| EditColumn {
                    name: name.clone(),
                    type_name: decls.get(i).cloned().flatten().unwrap_or_default(),
                });
            }
            // rowid, oid and _rowid_ all report "rowid".
            (has_rowid && origin.eq_ignore_ascii_case("rowid")).then(|| EditColumn {
                name: "rowid".into(),
                type_name: "INTEGER".into(),
            })
        })
        .collect();
    let find = |name: &str| {
        columns.iter().position(|c| {
            c.as_ref()
                .is_some_and(|c| c.name.eq_ignore_ascii_case(name))
        })
    };
    let key: Option<Vec<usize>> = if !meta.pk.is_empty() {
        let key: Option<Vec<usize>> = meta.pk.iter().map(|k| find(k)).collect();
        if key.is_none() {
            return Err(format!(
                "Read-only: add the primary key columns ({}) to the query to edit rows.",
                meta.pk.join(", ")
            ));
        }
        key
    } else {
        meta.unique
            .iter()
            .find_map(|u| u.iter().map(|k| find(k)).collect())
            .or_else(|| has_rowid.then(|| find("rowid").map(|i| vec![i])).flatten())
    };
    match key {
        Some(key) => Ok(EditTarget {
            schema: Some(db.to_string()),
            table: table.to_string(),
            columns,
            key,
        }),
        None if has_rowid => Err(format!(
            "Read-only: table {table} has no primary key; add rowid to the query to edit rows."
        )),
        None => Err(format!("Read-only: table {table} has no primary key.")),
    }
}

fn table_meta(
    conn: &Connection,
    catalog: &Mutex<Catalog>,
    db: &str,
    table: &str,
) -> rusqlite::Result<Arc<TableMeta>> {
    let key = (db.to_string(), table.to_string());
    if let Some(meta) = catalog.lock().tables.get(&key) {
        return Ok(meta.clone());
    }
    let meta = Arc::new(load_table_meta(conn, db, table)?);
    catalog.lock().tables.insert(key, meta.clone());
    Ok(meta)
}

fn load_table_meta(conn: &Connection, db: &str, table: &str) -> rusqlite::Result<TableMeta> {
    let mut meta = TableMeta::default();
    let mut pk = Vec::new();
    let mut not_null = Vec::new();
    let mut stmt = conn.prepare_cached(
        "SELECT name, \"notnull\", pk, hidden FROM pragma_table_xinfo(?1, ?2) ORDER BY cid",
    )?;
    let mut rows = stmt.query([table, db])?;
    while let Some(row) = rows.next()? {
        let name: String = row.get(0)?;
        if row.get::<_, i64>(1)? != 0 {
            not_null.push(name.to_ascii_lowercase());
        }
        let position: i64 = row.get(2)?;
        if position > 0 {
            pk.push((position, name.clone()));
        }
        // hidden: 1 virtual table hidden column, 2 and 3 generated.
        meta.columns.push((name, row.get::<_, i64>(3)? == 0));
    }
    pk.sort();
    meta.pk = pk.into_iter().map(|(_, n)| n).collect();

    let mut stmt = conn.prepare_cached(
        "SELECT il.name, ii.name FROM pragma_index_list(?1, ?2) AS il, \
         pragma_index_info(il.name, ?2) AS ii \
         WHERE il.\"unique\" AND NOT il.partial AND il.origin <> 'pk' \
         ORDER BY il.seq, ii.seqno",
    )?;
    let mut rows = stmt.query([table, db])?;
    // (index, columns, usable): expression columns have no name.
    let mut indexes: Vec<(String, Vec<String>, bool)> = Vec::new();
    while let Some(row) = rows.next()? {
        let index: String = row.get(0)?;
        let column: Option<String> = row.get(1)?;
        if indexes.last().is_none_or(|(i, _, _)| *i != index) {
            indexes.push((index, Vec::new(), true));
        }
        let entry = indexes.last_mut().expect("pushed above");
        match column {
            Some(c) if not_null.contains(&c.to_ascii_lowercase()) => entry.1.push(c),
            _ => entry.2 = false,
        }
    }
    meta.unique = indexes
        .into_iter()
        .filter(|(_, cols, usable)| *usable && !cols.is_empty())
        .map(|(_, cols, _)| cols)
        .collect();
    Ok(meta)
}

/// What `EXPLAIN QUERY PLAN` says about where the rows come from.
#[derive(Debug, PartialEq, Eq)]
struct PlanShape {
    /// UNION, INTERSECT, EXCEPT or a recursive CTE produces rows.
    compound: bool,
    /// Table scans that produce rows (joins count each table).
    sources: usize,
}

fn query_plan(conn: &Connection, sql: &str) -> rusqlite::Result<PlanShape> {
    let mut stmt = conn.prepare(&format!("EXPLAIN QUERY PLAN {sql}"))?;
    // raw_query: parameters stay unbound (NULL), as in execute.
    let mut rows = stmt.raw_query();
    let mut lines = Vec::new();
    while let Some(row) = rows.next()? {
        lines.push((row.get(0)?, row.get(1)?, row.get(3)?));
    }
    Ok(plan_shape(&lines))
}

/// Reads plan rows (id, parent, detail). Subqueries in expressions (WHERE,
/// select list) do not add rows; scans of a co-routine or materialized
/// subquery read rows already counted inside it; an OR over several indexes
/// is one scan.
fn plan_shape(lines: &[(i64, i64, String)]) -> PlanShape {
    let ancestors = |i: usize| {
        let mut out = Vec::new();
        let mut parent = lines[i].1;
        while parent != 0 && out.len() < lines.len() {
            match lines.iter().position(|l| l.0 == parent) {
                Some(p) => {
                    out.push(p);
                    parent = lines[p].1;
                }
                None => break,
            }
        }
        out
    };
    let in_expression = |d: &str| {
        [
            "SCALAR SUBQUERY",
            "CORRELATED SCALAR SUBQUERY",
            "LIST SUBQUERY",
            "CORRELATED LIST SUBQUERY",
        ]
        .iter()
        .any(|p| d.starts_with(p))
    };
    let subquery_name = |d: &str| {
        d.strip_prefix("CO-ROUTINE ")
            .or_else(|| d.strip_prefix("MATERIALIZE "))
            .map(str::to_string)
    };
    let names = |rest: &str, name: &str| {
        rest.strip_prefix(name)
            .is_some_and(|r| r.is_empty() || r.starts_with(' '))
    };
    let mut shape = PlanShape {
        compound: false,
        sources: 0,
    };
    for (i, (_, _, detail)) in lines.iter().enumerate() {
        let up = ancestors(i);
        if up.iter().any(|&a| in_expression(&lines[a].2)) || in_expression(detail) {
            continue;
        }
        if [
            "COMPOUND",
            "MERGE (",
            "SETUP",
            "RECURSIVE STEP",
            "LEFT-MOST SUBQUERY",
        ]
        .iter()
        .any(|p| detail.starts_with(p))
        {
            shape.compound = true;
        }
        if detail.starts_with("MULTI-INDEX OR") {
            shape.sources += 1;
            continue;
        }
        if up.iter().any(|&a| lines[a].2.starts_with("MULTI-INDEX OR")) {
            continue;
        }
        let Some(rest) = detail
            .strip_prefix("SCAN ")
            .or_else(|| detail.strip_prefix("SEARCH "))
        else {
            continue;
        };
        // A constant row, or an EXISTS turned into a semi-join: no rows.
        if rest.starts_with("CONSTANT ROW") || rest.contains(" EXISTS") {
            continue;
        }
        let reads_subquery = lines
            .iter()
            .enumerate()
            .any(|(j, l)| subquery_name(&l.2).is_some_and(|n| names(rest, &n)) && !up.contains(&j));
        if !reads_subquery {
            shape.sources += 1;
        }
    }
    shape
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Op {
    Update,
    Delete,
    Insert,
}

/// One statement of an apply, with its bound values.
#[derive(Debug, PartialEq)]
struct Step {
    /// 1-based index of the change, for messages.
    row: usize,
    op: Op,
    sql: String,
    params: Vec<Value>,
}

fn plan_changes(target: &EditTarget, changes: &[RowChange]) -> AppResult<Vec<Step>> {
    let table = match &target.schema {
        Some(schema) => format!("{}.{}", quote_ident(schema), quote_ident(&target.table)),
        None => quote_ident(&target.table),
    };
    let mut steps = Vec::with_capacity(changes.len());
    for (i, change) in changes.iter().enumerate() {
        let mut params = Vec::new();
        let (op, sql) = match change {
            // Nothing to save for this row.
            RowChange::Update { values, .. } if values.is_empty() => continue,
            RowChange::Update { key, values } => {
                let mut sets = Vec::with_capacity(values.len());
                for v in values {
                    let column = target_column(target, v.column)?;
                    sets.push(format!("{} = ?", quote_ident(&column.name)));
                    params.push(to_value(v.value.as_deref(), &column.type_name)?);
                }
                let cond = key_condition(target, key, &mut params)?;
                let sql = format!("UPDATE {table} SET {} WHERE {cond}", sets.join(", "));
                (Op::Update, sql)
            }
            RowChange::Delete { key } => {
                let cond = key_condition(target, key, &mut params)?;
                (Op::Delete, format!("DELETE FROM {table} WHERE {cond}"))
            }
            RowChange::Insert { values } if values.is_empty() => {
                (Op::Insert, format!("INSERT INTO {table} DEFAULT VALUES"))
            }
            RowChange::Insert { values } => {
                let mut names = Vec::with_capacity(values.len());
                for v in values {
                    let column = target_column(target, v.column)?;
                    names.push(quote_ident(&column.name));
                    params.push(to_value(v.value.as_deref(), &column.type_name)?);
                }
                let marks = vec!["?"; names.len()].join(", ");
                let sql = format!(
                    "INSERT INTO {table} ({}) VALUES ({marks})",
                    names.join(", ")
                );
                (Op::Insert, sql)
            }
        };
        steps.push(Step {
            row: i + 1,
            op,
            sql,
            params,
        });
    }
    Ok(steps)
}

fn target_column(target: &EditTarget, index: usize) -> AppResult<&EditColumn> {
    target
        .columns
        .get(index)
        .and_then(Option::as_ref)
        .ok_or_else(|| {
            AppError::invalid(format!(
                "Result column {} is not a table column and cannot be edited.",
                index + 1
            ))
        })
}

fn key_condition(
    target: &EditTarget,
    key: &[Option<String>],
    params: &mut Vec<Value>,
) -> AppResult<String> {
    if key.len() != target.key.len() {
        return Err(AppError::invalid(
            "The row key does not match the key columns of the result.",
        ));
    }
    let mut parts = Vec::with_capacity(key.len());
    for (&index, value) in target.key.iter().zip(key) {
        let column = target_column(target, index)?;
        let name = quote_ident(&column.name);
        match value {
            None => parts.push(format!("{name} IS NULL")),
            Some(v) => {
                parts.push(format!("{name} = ?"));
                params.push(to_value(Some(v), &column.type_name)?);
            }
        }
    }
    Ok(parts.join(" AND "))
}

/// Runs the steps as one unit. Outside a transaction it opens one: with
/// autocommit it commits, in manual mode it stays open, as for user
/// statements. Inside an open transaction a savepoint undoes a failed batch
/// and keeps earlier work.
fn apply_steps(conn: &Connection, steps: &[Step], autocommit: bool) -> AppResult<ApplyResult> {
    let mut result = ApplyResult::default();
    if conn.is_autocommit() {
        conn.execute_batch(if autocommit {
            "BEGIN IMMEDIATE"
        } else {
            "BEGIN"
        })
        .map_err(map_error)?;
        let mut outcome = run_steps(conn, steps, &mut result);
        if outcome.is_ok() && autocommit {
            outcome = conn.execute_batch("COMMIT").map_err(map_error);
        }
        if outcome.is_err() && !conn.is_autocommit() {
            let _ = conn.execute_batch("ROLLBACK");
        }
        outcome.map(|()| result)
    } else {
        conn.execute_batch("SAVEPOINT lq_apply")
            .map_err(map_error)?;
        let outcome = run_steps(conn, steps, &mut result);
        if outcome.is_err() {
            let _ = conn.execute_batch("ROLLBACK TO lq_apply");
        }
        let released = conn.execute_batch("RELEASE lq_apply").map_err(map_error);
        outcome.and(released).map(|()| result)
    }
}

fn run_steps(conn: &Connection, steps: &[Step], result: &mut ApplyResult) -> AppResult<()> {
    for step in steps {
        let changed = conn
            .prepare_cached(&step.sql)
            .and_then(|mut s| s.execute(rusqlite::params_from_iter(&step.params)))
            .map_err(|e| {
                let mut err = map_error(e);
                // The position would point into SQL the user never saw.
                err.position = None;
                if err.kind == ErrorKind::Database {
                    err.message = format!("Row {} was not saved: {}", step.row, err.message);
                }
                err
            })?;
        match step.op {
            Op::Insert => result.inserted += changed as u64,
            Op::Update | Op::Delete if changed != 1 => {
                let why = if changed == 0 {
                    " (changed or removed by someone else)"
                } else {
                    ""
                };
                return Err(AppError::new(
                    ErrorKind::Database,
                    format!(
                        "Row {} was not saved: {changed} rows matched its key{why}.",
                        step.row
                    ),
                ));
            }
            Op::Update => result.updated += 1,
            Op::Delete => result.deleted += 1,
        }
    }
    Ok(())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Affinity {
    Integer,
    Text,
    Blob,
    Real,
    Numeric,
}

/// Column affinity from the declared type (SQLite "Datatypes", 3.1).
fn affinity(decl: &str) -> Affinity {
    let t = decl.to_ascii_uppercase();
    let has = |s: &str| t.contains(s);
    if has("INT") {
        Affinity::Integer
    } else if has("CHAR") || has("CLOB") || has("TEXT") {
        Affinity::Text
    } else if has("BLOB") || t.trim().is_empty() {
        Affinity::Blob
    } else if has("REAL") || has("FLOA") || has("DOUB") {
        Affinity::Real
    } else {
        Affinity::Numeric
    }
}

/// Converts grid text back to a value for a column with the declared type.
/// Numeric affinities bind numbers; columns without affinity (BLOB or no
/// type) keep their storage class through the display format: `X'..'` is a
/// blob, and text that the formatter would print for an integer or real is
/// that number.
fn to_value(text: Option<&str>, decl: &str) -> AppResult<Value> {
    let Some(text) = text else {
        return Ok(Value::Null);
    };
    if text.starts_with("X'") && text.contains("'… (") {
        return Err(AppError::invalid(
            "This value is only a preview and cannot be saved.",
        ));
    }
    let value = match affinity(decl) {
        Affinity::Text => None,
        Affinity::Integer | Affinity::Real | Affinity::Numeric => {
            let t = text.trim();
            let boolean = decl.to_ascii_uppercase().contains("BOOL");
            if boolean && t.eq_ignore_ascii_case("true") {
                Some(Value::Integer(1))
            } else if boolean && t.eq_ignore_ascii_case("false") {
                Some(Value::Integer(0))
            } else if let Ok(i) = t.parse::<i64>() {
                Some(Value::Integer(i))
            } else {
                parse_real(t).map(Value::Real)
            }
        }
        Affinity::Blob => hex_blob(text)
            .map(Value::Blob)
            .or_else(|| match text.parse::<i64>() {
                Ok(i) if i.to_string() == text => Some(Value::Integer(i)),
                _ => parse_real(text)
                    .filter(|&f| real(f) == text)
                    .map(Value::Real),
            }),
    };
    Ok(value.unwrap_or_else(|| Value::Text(text.to_string())))
}

/// A decimal or exponent literal, or the `Inf` the formatter prints.
fn parse_real(t: &str) -> Option<f64> {
    match t {
        "Inf" | "+Inf" => return Some(f64::INFINITY),
        "-Inf" => return Some(f64::NEG_INFINITY),
        _ => {}
    }
    let literal = t.bytes().any(|b| b.is_ascii_digit())
        && t.bytes()
            .all(|b| b.is_ascii_digit() || b"+-.eE".contains(&b));
    literal.then(|| t.parse().ok()).flatten()
}

/// Bytes of an `X'00FF'` literal.
fn hex_blob(t: &str) -> Option<Vec<u8>> {
    let hex = t
        .strip_prefix("X'")
        .or_else(|| t.strip_prefix("x'"))?
        .strip_suffix('\'')?;
    if hex.len() % 2 != 0 || !hex.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    (0..hex.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&hex[i..i + 2], 16).ok())
        .collect()
}

macro_rules! code_names {
    ($code:expr; $($name:ident),* $(,)?) => {
        match $code {
            $(ffi::$name => Some(stringify!($name)),)*
            _ => None,
        }
    };
}

/// Name of an (extended) result code, as in the SQLite documentation.
fn code_name(code: i32) -> String {
    fn lookup(code: i32) -> Option<&'static str> {
        code_names!(code;
            SQLITE_ERROR, SQLITE_INTERNAL, SQLITE_PERM, SQLITE_ABORT, SQLITE_BUSY,
            SQLITE_LOCKED, SQLITE_NOMEM, SQLITE_READONLY, SQLITE_INTERRUPT, SQLITE_IOERR,
            SQLITE_CORRUPT, SQLITE_NOTFOUND, SQLITE_FULL, SQLITE_CANTOPEN, SQLITE_PROTOCOL,
            SQLITE_EMPTY, SQLITE_SCHEMA, SQLITE_TOOBIG, SQLITE_CONSTRAINT, SQLITE_MISMATCH,
            SQLITE_MISUSE, SQLITE_NOLFS, SQLITE_AUTH, SQLITE_FORMAT, SQLITE_RANGE,
            SQLITE_NOTADB,
            SQLITE_ERROR_MISSING_COLLSEQ, SQLITE_ERROR_RETRY, SQLITE_ERROR_SNAPSHOT,
            SQLITE_IOERR_READ, SQLITE_IOERR_SHORT_READ, SQLITE_IOERR_WRITE, SQLITE_IOERR_FSYNC,
            SQLITE_IOERR_DIR_FSYNC, SQLITE_IOERR_TRUNCATE, SQLITE_IOERR_FSTAT,
            SQLITE_IOERR_UNLOCK, SQLITE_IOERR_RDLOCK, SQLITE_IOERR_DELETE, SQLITE_IOERR_BLOCKED,
            SQLITE_IOERR_NOMEM, SQLITE_IOERR_ACCESS, SQLITE_IOERR_CHECKRESERVEDLOCK,
            SQLITE_IOERR_LOCK, SQLITE_IOERR_CLOSE, SQLITE_IOERR_DIR_CLOSE, SQLITE_IOERR_SHMOPEN,
            SQLITE_IOERR_SHMSIZE, SQLITE_IOERR_SHMLOCK, SQLITE_IOERR_SHMMAP, SQLITE_IOERR_SEEK,
            SQLITE_IOERR_DELETE_NOENT, SQLITE_IOERR_MMAP, SQLITE_IOERR_GETTEMPPATH,
            SQLITE_IOERR_CONVPATH, SQLITE_IOERR_VNODE, SQLITE_IOERR_AUTH,
            SQLITE_IOERR_BEGIN_ATOMIC, SQLITE_IOERR_COMMIT_ATOMIC, SQLITE_IOERR_ROLLBACK_ATOMIC,
            SQLITE_IOERR_DATA, SQLITE_IOERR_CORRUPTFS,
            SQLITE_LOCKED_SHAREDCACHE, SQLITE_LOCKED_VTAB,
            SQLITE_BUSY_RECOVERY, SQLITE_BUSY_SNAPSHOT, SQLITE_BUSY_TIMEOUT,
            SQLITE_CANTOPEN_NOTEMPDIR, SQLITE_CANTOPEN_ISDIR, SQLITE_CANTOPEN_FULLPATH,
            SQLITE_CANTOPEN_CONVPATH, SQLITE_CANTOPEN_DIRTYWAL, SQLITE_CANTOPEN_SYMLINK,
            SQLITE_CORRUPT_VTAB, SQLITE_CORRUPT_SEQUENCE, SQLITE_CORRUPT_INDEX,
            SQLITE_READONLY_RECOVERY, SQLITE_READONLY_CANTLOCK, SQLITE_READONLY_ROLLBACK,
            SQLITE_READONLY_DBMOVED, SQLITE_READONLY_CANTINIT, SQLITE_READONLY_DIRECTORY,
            SQLITE_ABORT_ROLLBACK,
            SQLITE_CONSTRAINT_CHECK, SQLITE_CONSTRAINT_COMMITHOOK, SQLITE_CONSTRAINT_FOREIGNKEY,
            SQLITE_CONSTRAINT_FUNCTION, SQLITE_CONSTRAINT_NOTNULL, SQLITE_CONSTRAINT_PRIMARYKEY,
            SQLITE_CONSTRAINT_TRIGGER, SQLITE_CONSTRAINT_UNIQUE, SQLITE_CONSTRAINT_VTAB,
            SQLITE_CONSTRAINT_ROWID, SQLITE_CONSTRAINT_PINNED, SQLITE_CONSTRAINT_DATATYPE,
            SQLITE_AUTH_USER,
        )
    }
    lookup(code)
        .or_else(|| lookup(code & 0xff))
        .map(str::to_string)
        .unwrap_or_else(|| format!("SQLITE_{code}"))
}

fn sqlite_error(error: ffi::Error, msg: Option<String>) -> AppError {
    let message = msg.unwrap_or_else(|| ffi::code_to_str(error.extended_code).to_string());
    let mut err = AppError::new(ErrorKind::Database, message);
    if error.code == ffi::ErrorCode::OperationInterrupted {
        err.kind = ErrorKind::Cancelled;
        err.message = "The statement was cancelled.".into();
    }
    err.code = Some(code_name(error.extended_code));
    err
}

fn connection_error(e: rusqlite::Error) -> AppError {
    let mut err = map_error(e);
    err.kind = ErrorKind::Connection;
    err
}

pub fn map_error(e: rusqlite::Error) -> AppError {
    match e {
        rusqlite::Error::SqliteFailure(error, msg) => sqlite_error(error, msg),
        rusqlite::Error::SqlInputError {
            error,
            msg,
            sql,
            offset,
        } => {
            let mut err = sqlite_error(error, Some(msg));
            // SQLite reports a byte offset; the UI wants characters.
            err.position = usize::try_from(offset)
                .ok()
                .and_then(|o| sql.get(..o))
                .map(|s| s.chars().count());
            err
        }
        rusqlite::Error::MultipleStatement => {
            AppError::invalid("Only one statement can run at a time.")
        }
        other => AppError::new(ErrorKind::Database, other.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::Engine;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicUsize, Ordering};

    static COUNTER: AtomicUsize = AtomicUsize::new(0);

    fn temp_path(name: &str) -> PathBuf {
        let n = COUNTER.fetch_add(1, Ordering::SeqCst);
        let path = std::env::temp_dir().join(format!(
            "lowquery-test-{name}-{}-{n}.db",
            std::process::id()
        ));
        let _ = std::fs::remove_file(&path);
        path
    }

    fn profile(path: &Path, create: bool) -> ConnectionProfile {
        ConnectionProfile {
            engine: Engine::Sqlite,
            database: path.display().to_string(),
            create_file: create,
            ..Default::default()
        }
    }

    struct TempDb {
        path: PathBuf,
        session: Option<LiteSession>,
    }

    impl TempDb {
        async fn new(name: &str) -> Self {
            let path = temp_path(name);
            let session = connect(&profile(&path, true)).await.unwrap();
            Self {
                path,
                session: Some(session),
            }
        }

        fn s(&self) -> &LiteSession {
            self.session.as_ref().unwrap()
        }

        async fn run(&self, sql: &str) -> ExecResult {
            self.run_limit(sql, 0).await
        }

        async fn run_limit(&self, sql: &str, max_rows: usize) -> ExecResult {
            self.try_run(sql, max_rows)
                .await
                .unwrap_or_else(|e| panic!("{sql}: {e}"))
        }

        async fn try_run(&self, sql: &str, max_rows: usize) -> AppResult<ExecResult> {
            self.s()
                .execute(&ExecRequest {
                    sql: sql.into(),
                    max_rows,
                })
                .await
        }

        async fn scalar(&self, sql: &str) -> Option<String> {
            self.run(sql).await.rows[0][0].clone()
        }
    }

    impl Drop for TempDb {
        fn drop(&mut self) {
            self.session.take();
            for suffix in ["", "-journal", "-wal", "-shm"] {
                let _ = std::fs::remove_file(format!("{}{suffix}", self.path.display()));
            }
        }
    }

    #[test]
    fn formats_reals_shortest() {
        assert_eq!(real(3.0), "3.0");
        assert_eq!(real(-0.5), "-0.5");
        assert_eq!(real(0.1 + 0.2), "0.30000000000000004");
        assert_eq!(real(1e300), "1.0e+300");
        assert_eq!(real(-1.5e-7), "-1.5e-7");
        assert_eq!(real(123456789.125), "123456789.125");
        assert_eq!(real(f64::INFINITY), "Inf");
        assert_eq!(real(0.0), "0.0");
    }

    #[test]
    fn kinds_follow_affinity() {
        assert_eq!(declared_kind("INTEGER"), ValueKind::Number);
        assert_eq!(declared_kind("bigint"), ValueKind::Number);
        assert_eq!(declared_kind("NUMERIC(10,2)"), ValueKind::Number);
        assert_eq!(declared_kind("DOUBLE PRECISION"), ValueKind::Number);
        assert_eq!(declared_kind("VARCHAR(20)"), ValueKind::Text);
        assert_eq!(declared_kind("DATETIME"), ValueKind::Temporal);
        assert_eq!(declared_kind("BOOLEAN"), ValueKind::Bool);
        assert_eq!(declared_kind("JSON"), ValueKind::Json);
        assert_eq!(declared_kind("BLOB"), ValueKind::Binary);
        assert_eq!(declared_kind("GEOMETRY"), ValueKind::Other);
    }

    #[test]
    fn names_result_codes() {
        assert_eq!(code_name(ffi::SQLITE_ERROR), "SQLITE_ERROR");
        assert_eq!(
            code_name(ffi::SQLITE_CONSTRAINT_UNIQUE),
            "SQLITE_CONSTRAINT_UNIQUE"
        );
        assert_eq!(code_name(ffi::SQLITE_IOERR | (99 << 8)), "SQLITE_IOERR");
    }

    #[test]
    fn blank_statements() {
        assert!(is_blank(" ; -- x\n /* y */ ;"));
        assert!(!is_blank("-- x\nselect 1"));
    }

    #[tokio::test]
    async fn formats_every_storage_class() {
        let db = TempDb::new("values").await;
        assert!(db.s().server_version.starts_with("SQLite 3."));
        assert_eq!(db.s().current_schema.as_deref(), Some("main"));
        db.run("CREATE TABLE v (i INTEGER, r REAL, t TEXT, b BLOB, n NUMERIC, d DATETIME, j JSON, f BOOLEAN)")
            .await;
        db.run(
            "INSERT INTO v VALUES (-42, 2.5, 'olá', x'00ff10', 12.50, '2024-05-01 13:45:00', '{\"a\":1}', 1)",
        )
        .await;
        db.run("INSERT INTO v VALUES (NULL, 3, NULL, NULL, NULL, NULL, NULL, NULL)")
            .await;
        let r = db.run("SELECT * FROM v").await;
        let cols = r.columns.unwrap();
        let types: Vec<_> = cols.iter().map(|c| c.type_name.as_str()).collect();
        assert_eq!(
            types,
            ["INTEGER", "REAL", "TEXT", "BLOB", "NUMERIC", "DATETIME", "JSON", "BOOLEAN"]
        );
        let kinds: Vec<_> = cols.iter().map(|c| c.kind).collect();
        assert_eq!(
            kinds,
            [
                ValueKind::Number,
                ValueKind::Number,
                ValueKind::Text,
                ValueKind::Binary,
                ValueKind::Number,
                ValueKind::Temporal,
                ValueKind::Json,
                ValueKind::Bool
            ]
        );
        let row =
            |i: usize| -> Vec<Option<&str>> { r.rows[i].iter().map(|c| c.as_deref()).collect() };
        assert_eq!(
            row(0),
            [
                Some("-42"),
                Some("2.5"),
                Some("olá"),
                Some("X'00FF10'"),
                Some("12.5"),
                Some("2024-05-01 13:45:00"),
                Some("{\"a\":1}"),
                Some("1")
            ]
        );
        // REAL affinity stores 3 as 3.0.
        assert_eq!(row(1)[1], Some("3.0"));
        assert_eq!(row(1)[0], None);

        let r = db
            .run("SELECT 1 + 1 AS a, 0.1 + 0.2, 'x', x'0A', NULL, 1e300, typeof(1)")
            .await;
        let cols = r.columns.unwrap();
        let types: Vec<_> = cols.iter().map(|c| c.type_name.as_str()).collect();
        assert_eq!(
            types,
            ["INTEGER", "REAL", "TEXT", "BLOB", "NULL", "REAL", "TEXT"]
        );
        assert_eq!(cols[0].name, "a");
        assert_eq!(cols[4].kind, ValueKind::Other);
        let cells: Vec<_> = r.rows[0].iter().map(|c| c.as_deref()).collect();
        assert_eq!(
            cells,
            [
                Some("2"),
                Some("0.30000000000000004"),
                Some("x"),
                Some("X'0A'"),
                None,
                Some("1.0e+300"),
                Some("integer")
            ]
        );

        let big = db.scalar("SELECT zeroblob(40000)").await.unwrap();
        assert!(big.starts_with("X'0000"));
        assert!(big.ends_with("'… (40000 bytes)"));
        assert_eq!(
            big.len(),
            2 + BINARY_PREVIEW_BYTES * 2 + "'… (40000 bytes)".len()
        );
    }

    #[tokio::test]
    async fn applies_row_limit() {
        let db = TempDb::new("limit").await;
        let sql = "WITH RECURSIVE c(x) AS (SELECT 1 UNION ALL SELECT x + 1 FROM c WHERE x < 10) SELECT x FROM c";
        let r = db.run_limit(sql, 5).await;
        assert_eq!(r.rows.len(), 5);
        assert!(r.truncated);
        assert_eq!(r.rows[4][0].as_deref(), Some("5"));
        let r = db.run_limit(sql, 10).await;
        assert_eq!(r.rows.len(), 10);
        assert!(!r.truncated);
        let r = db.run_limit(sql, 0).await;
        assert_eq!(r.rows.len(), 10);
        assert!(!r.truncated);
    }

    #[tokio::test]
    async fn reports_rows_affected() {
        let db = TempDb::new("affected").await;
        let r = db
            .run("CREATE TABLE t (id INTEGER PRIMARY KEY, v TEXT)")
            .await;
        assert_eq!(r.rows_affected, None);
        assert!(r.columns.is_none());
        let r = db
            .run("INSERT INTO t (v) VALUES ('a'), ('b'), ('c');")
            .await;
        assert_eq!(r.rows_affected, Some(3));
        let r = db.run("UPDATE t SET v = 'z' WHERE id > 1").await;
        assert_eq!(r.rows_affected, Some(2));
        // A DDL right after keeps no stale count.
        let r = db.run("CREATE INDEX t_v ON t (v)").await;
        assert_eq!(r.rows_affected, None);
        let r = db.run("DELETE FROM t").await;
        assert_eq!(r.rows_affected, Some(3));
        // INSERT ... RETURNING returns rows.
        let r = db
            .run("INSERT INTO t (v) VALUES ('r') RETURNING id, v")
            .await;
        assert_eq!(r.rows.len(), 1);
        // Comments only: nothing to do.
        let r = db.run("-- nothing\n;").await;
        assert!(r.columns.is_none());
        // Trigger bodies arrive as one statement.
        db.run("CREATE TABLE log (msg TEXT)").await;
        db.run(
            "CREATE TRIGGER t_ins AFTER INSERT ON t BEGIN\n  INSERT INTO log VALUES (new.v);\n  INSERT INTO log VALUES ('again');\nEND;",
        )
        .await;
        db.run("INSERT INTO t (v) VALUES ('x')").await;
        assert_eq!(
            db.scalar("SELECT count(*) FROM log").await.as_deref(),
            Some("2")
        );
    }

    #[tokio::test]
    async fn maps_errors() {
        let db = TempDb::new("errors").await;
        let e = db.try_run("SELEC 1", 0).await.unwrap_err();
        assert_eq!(e.kind, ErrorKind::Database);
        assert_eq!(e.code.as_deref(), Some("SQLITE_ERROR"));
        assert_eq!(e.position, Some(0));
        assert!(e.message.contains("syntax error"), "{}", e.message);

        // Character offset, not bytes: 'ção' has multibyte characters.
        let e = db.try_run("SELECT 'ção', FROM t", 0).await.unwrap_err();
        assert_eq!(e.position, Some(14));

        let e = db.try_run("SELECT * FROM missing", 0).await.unwrap_err();
        assert_eq!(e.code.as_deref(), Some("SQLITE_ERROR"));
        assert!(e.message.contains("no such table"), "{}", e.message);

        db.run("CREATE TABLE u (k TEXT UNIQUE)").await;
        db.run("INSERT INTO u VALUES ('a')").await;
        let e = db
            .try_run("INSERT INTO u VALUES ('a')", 0)
            .await
            .unwrap_err();
        assert_eq!(e.code.as_deref(), Some("SQLITE_CONSTRAINT_UNIQUE"));
        assert_eq!(e.kind, ErrorKind::Database);

        // Foreign keys are enforced.
        db.run("CREATE TABLE p (id INTEGER PRIMARY KEY)").await;
        db.run("CREATE TABLE c (p INTEGER REFERENCES p (id))").await;
        let e = db.try_run("INSERT INTO c VALUES (7)", 0).await.unwrap_err();
        assert_eq!(e.code.as_deref(), Some("SQLITE_CONSTRAINT_FOREIGNKEY"));

        let e = db.try_run("SELECT 1; SELECT 2", 0).await.unwrap_err();
        assert_eq!(e.kind, ErrorKind::Invalid);
    }

    #[tokio::test]
    async fn manual_transactions() {
        let db = TempDb::new("manual").await;
        db.run("CREATE TABLE t (v INTEGER)").await;
        let st = db.s().set_autocommit(false).await.unwrap();
        assert!(!st.autocommit && !st.in_transaction);

        let r = db.run("INSERT INTO t VALUES (1)").await;
        assert!(r.in_transaction);
        assert!(db.s().tx_state().in_transaction);
        let st = db.s().end_transaction(false).await.unwrap();
        assert!(!st.in_transaction);
        let r = db.run("SELECT count(*) FROM t").await;
        assert_eq!(r.rows[0][0].as_deref(), Some("0"));
        // Reads also open the transaction in manual mode.
        assert!(r.in_transaction);

        db.run("INSERT INTO t VALUES (2)").await;
        let st = db.s().end_transaction(true).await.unwrap();
        assert!(!st.in_transaction);
        assert_eq!(
            db.scalar("SELECT count(*) FROM t").await.as_deref(),
            Some("1")
        );
        db.s().end_transaction(true).await.unwrap();

        // A failing first statement does not leave an empty transaction.
        let e = db.try_run("SELEC 1", 0).await.unwrap_err();
        assert_eq!(e.code.as_deref(), Some("SQLITE_ERROR"));
        assert!(!db.s().tx_state().in_transaction);

        // A failing statement inside a transaction keeps earlier work.
        db.run("INSERT INTO t VALUES (3)").await;
        db.try_run("INSERT INTO nope VALUES (1)", 0)
            .await
            .unwrap_err();
        assert!(db.s().tx_state().in_transaction);

        // COMMIT typed by the user ends it.
        let r = db.run("COMMIT").await;
        assert!(!r.in_transaction);
        assert!(!db.s().tx_state().in_transaction);
        assert_eq!(
            db.scalar("SELECT count(*) FROM t").await.as_deref(),
            Some("2")
        );

        // Back to autocommit commits pending work.
        db.run("INSERT INTO t VALUES (4)").await;
        let st = db.s().set_autocommit(true).await.unwrap();
        assert!(st.autocommit && !st.in_transaction);
        assert_eq!(
            db.scalar("SELECT count(*) FROM t").await.as_deref(),
            Some("3")
        );
        assert!(!db.s().tx_state().in_transaction);
    }

    #[tokio::test]
    async fn user_typed_transaction_control() {
        let db = TempDb::new("typed").await;
        db.run("CREATE TABLE t (v INTEGER)").await;
        let r = db.run("BEGIN").await;
        assert!(r.in_transaction);
        db.run("INSERT INTO t VALUES (1)").await;
        db.run("SAVEPOINT sp").await;
        db.run("INSERT INTO t VALUES (2)").await;
        let r = db.run("ROLLBACK TO sp").await;
        assert!(r.in_transaction);
        let r = db.run("COMMIT").await;
        assert!(!r.in_transaction);
        assert_eq!(
            db.scalar("SELECT count(*) FROM t").await.as_deref(),
            Some("1")
        );

        db.run("BEGIN").await;
        db.run("DELETE FROM t").await;
        let r = db.run("ROLLBACK").await;
        assert!(!r.in_transaction);
        assert_eq!(
            db.scalar("SELECT count(*) FROM t").await.as_deref(),
            Some("1")
        );
        assert!(db.s().tx_state().autocommit);
    }

    #[tokio::test]
    async fn interrupts_long_query() {
        let mut db = TempDb::new("cancel").await;
        let session = Arc::new(db.session.take().unwrap());
        let runner = session.clone();
        let task = tokio::spawn(async move {
            runner
                .execute(&ExecRequest {
                    sql: "WITH RECURSIVE c(x) AS (SELECT 1 UNION ALL SELECT x + 1 FROM c WHERE x < 1000000000) SELECT count(*) FROM c".into(),
                    max_rows: 0,
                })
                .await
        });
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        let started = Instant::now();
        session.cancel().await.unwrap();
        let e = task.await.unwrap().unwrap_err();
        assert_eq!(e.kind, ErrorKind::Cancelled);
        assert_eq!(e.code.as_deref(), Some("SQLITE_INTERRUPT"));
        assert!(started.elapsed() < std::time::Duration::from_secs(5));
        // The session is usable afterwards.
        let r = session
            .execute(&ExecRequest {
                sql: "SELECT 1".into(),
                max_rows: 0,
            })
            .await
            .unwrap();
        assert_eq!(r.rows[0][0].as_deref(), Some("1"));
        drop(session);
    }

    #[tokio::test]
    async fn creates_file_only_when_asked() {
        let path = temp_path("create");
        let e = connect(&profile(&path, false)).await.err().unwrap();
        assert_eq!(e.kind, ErrorKind::Connection);
        assert_eq!(
            e.message,
            format!("Database file not found: {}", path.display())
        );
        assert!(!path.exists());

        let session = connect(&profile(&path, true)).await.unwrap();
        assert!(path.exists());
        drop(session);
        // Existing file opens without create_file.
        let session = connect(&profile(&path, false)).await.unwrap();
        drop(session);
        let _ = std::fs::remove_file(&path);

        // A file that is not a database fails at connect time.
        let junk = temp_path("junk");
        std::fs::write(&junk, vec![b'x'; 4096]).unwrap();
        let e = connect(&profile(&junk, false)).await.err().unwrap();
        assert_eq!(e.kind, ErrorKind::Connection);
        assert_eq!(e.code.as_deref(), Some("SQLITE_NOTADB"));
        let _ = std::fs::remove_file(&junk);
    }

    #[tokio::test]
    async fn lists_metadata() {
        let db = TempDb::new("meta").await;
        let other = temp_path("meta-other");
        db.run(
            "CREATE TABLE person (id INTEGER PRIMARY KEY AUTOINCREMENT, name TEXT NOT NULL, \
             score NUMERIC(10,2), full TEXT GENERATED ALWAYS AS (name || '!') VIRTUAL)",
        )
        .await;
        db.run("INSERT INTO person (name) VALUES ('a')").await;
        db.run("CREATE VIEW people AS SELECT name FROM person")
            .await;
        db.run("CREATE TABLE another (x)").await;
        db.run(&format!(
            "ATTACH DATABASE '{}' AS extra",
            other.display().to_string().replace('\'', "''")
        ))
        .await;
        db.run("CREATE TABLE extra.ext (k TEXT, v BLOB, PRIMARY KEY (k, v))")
            .await;
        db.run("CREATE TEMP TABLE scratch (z)").await;

        let schemas = db.s().list_schemas().await.unwrap();
        assert_eq!(schemas, ["main", "extra", "temp"]);

        let objects = db.s().list_objects("main").await.unwrap();
        let names: Vec<_> = objects
            .iter()
            .map(|o| (o.kind.as_str(), o.name.as_str()))
            .collect();
        // sqlite_sequence is internal and hidden.
        assert_eq!(
            names,
            [
                ("table", "another"),
                ("table", "person"),
                ("view", "people")
            ]
        );
        let ext = db.s().list_objects("extra").await.unwrap();
        assert_eq!(ext.len(), 1);
        assert_eq!(ext[0].name, "ext");
        let tmp = db.s().list_objects("temp").await.unwrap();
        assert_eq!(tmp[0].name, "scratch");

        let cols = db.s().list_columns("main", "person").await.unwrap();
        let shape: Vec<_> = cols
            .iter()
            .map(|c| {
                (
                    c.name.as_str(),
                    c.type_name.as_str(),
                    c.nullable,
                    c.primary_key,
                )
            })
            .collect();
        assert_eq!(
            shape,
            [
                ("id", "INTEGER", true, true),
                ("name", "TEXT", false, false),
                ("score", "NUMERIC(10,2)", true, false),
                ("full", "TEXT", true, false),
            ]
        );
        let cols = db.s().list_columns("extra", "ext").await.unwrap();
        assert!(cols.iter().all(|c| c.primary_key));
        let cols = db.s().list_columns("main", "ext").await.unwrap();
        assert!(cols.is_empty());

        db.run("DETACH DATABASE extra").await;
        let _ = std::fs::remove_file(&other);
    }
    // Grid editing.

    use crate::model::CellValue;

    fn set(column: usize, value: Option<&str>) -> CellValue {
        CellValue {
            column,
            value: value.map(str::to_string),
        }
    }

    fn key(values: &[Option<&str>]) -> Vec<Option<String>> {
        values.iter().map(|v| v.map(str::to_string)).collect()
    }

    fn ec(name: &str, type_name: &str) -> Option<EditColumn> {
        Some(EditColumn {
            name: name.into(),
            type_name: type_name.into(),
        })
    }

    fn col(name: &str, type_name: &str) -> Option<(String, String)> {
        Some((name.into(), type_name.into()))
    }

    fn cols(columns: &[Option<EditColumn>]) -> Vec<Option<(String, String)>> {
        columns
            .iter()
            .map(|c| c.as_ref().map(|c| (c.name.clone(), c.type_name.clone())))
            .collect()
    }

    impl TempDb {
        async fn target(&self, sql: &str) -> EditTarget {
            let r = self.run(sql).await;
            assert_eq!(r.read_only_reason, None, "{sql}");
            r.editable.unwrap_or_else(|| panic!("{sql}: not editable"))
        }

        async fn reason(&self, sql: &str) -> String {
            let r = self.run(sql).await;
            assert!(r.editable.is_none(), "{sql}: editable");
            r.read_only_reason
                .unwrap_or_else(|| panic!("{sql}: no reason"))
        }

        async fn apply(
            &self,
            target: &EditTarget,
            changes: Vec<RowChange>,
        ) -> AppResult<ApplyResult> {
            self.s()
                .apply_changes(&ApplyRequest {
                    target: target.clone(),
                    changes,
                })
                .await
        }

        async fn rows(&self, sql: &str) -> Vec<Vec<Option<String>>> {
            self.run(sql).await.rows
        }
    }

    fn texts(rows: &[&[Option<&str>]]) -> Vec<Vec<Option<String>>> {
        rows.iter()
            .map(|r| r.iter().map(|c| c.map(str::to_string)).collect())
            .collect()
    }

    #[test]
    fn affinity_rules() {
        assert_eq!(affinity("INTEGER"), Affinity::Integer);
        assert_eq!(affinity("BIGINT"), Affinity::Integer);
        // "POINT" contains INT, as in SQLite.
        assert_eq!(affinity("POINT"), Affinity::Integer);
        assert_eq!(affinity("VARCHAR(20)"), Affinity::Text);
        assert_eq!(affinity("CLOB"), Affinity::Text);
        assert_eq!(affinity("BLOB"), Affinity::Blob);
        assert_eq!(affinity(""), Affinity::Blob);
        assert_eq!(affinity("DOUBLE PRECISION"), Affinity::Real);
        assert_eq!(affinity("FLOAT"), Affinity::Real);
        assert_eq!(affinity("NUMERIC(10,2)"), Affinity::Numeric);
        assert_eq!(affinity("DECIMAL"), Affinity::Numeric);
        assert_eq!(affinity("BOOLEAN"), Affinity::Numeric);
        assert_eq!(affinity("DATETIME"), Affinity::Numeric);
    }

    #[test]
    fn converts_text_by_affinity() {
        let v = |t: &str, decl: &str| to_value(Some(t), decl).unwrap();
        assert_eq!(to_value(None, "INTEGER").unwrap(), Value::Null);
        assert_eq!(v("-42", "INTEGER"), Value::Integer(-42));
        assert_eq!(v(" 7 ", "INT"), Value::Integer(7));
        assert_eq!(v("2.5", "INTEGER"), Value::Real(2.5));
        assert_eq!(v("abc", "INTEGER"), Value::Text("abc".into()));
        assert_eq!(v("3.0", "REAL"), Value::Real(3.0));
        assert_eq!(v("1.0e+300", "REAL"), Value::Real(1e300));
        assert_eq!(v("-1.5e-7", "DOUBLE"), Value::Real(-1.5e-7));
        assert_eq!(v("Inf", "REAL"), Value::Real(f64::INFINITY));
        assert_eq!(v("-Inf", "REAL"), Value::Real(f64::NEG_INFINITY));
        assert_eq!(v("NaN", "REAL"), Value::Text("NaN".into()));
        assert_eq!(v("12.5", "NUMERIC(10,2)"), Value::Real(12.5));
        assert_eq!(v("12", "DECIMAL"), Value::Integer(12));
        assert_eq!(v("0x10", "NUMERIC"), Value::Text("0x10".into()));
        assert_eq!(v("true", "BOOLEAN"), Value::Integer(1));
        assert_eq!(v("FALSE", "BOOL"), Value::Integer(0));
        assert_eq!(v("true", "NUMERIC"), Value::Text("true".into()));
        assert_eq!(
            v("2024-05-01 13:45:00.123", "DATETIME"),
            Value::Text("2024-05-01 13:45:00.123".into())
        );
        // TEXT affinity keeps everything as typed.
        assert_eq!(v("42", "TEXT"), Value::Text("42".into()));
        assert_eq!(v("X'00'", "VARCHAR"), Value::Text("X'00'".into()));
        // No affinity: the display format tells the storage class.
        assert_eq!(v("X'00FF10'", "BLOB"), Value::Blob(vec![0, 0xff, 0x10]));
        assert_eq!(v("x''", ""), Value::Blob(vec![]));
        assert_eq!(v("X'0'", "BLOB"), Value::Text("X'0'".into()));
        assert_eq!(v("X'ZZ'", "BLOB"), Value::Text("X'ZZ'".into()));
        assert_eq!(v("42", ""), Value::Integer(42));
        assert_eq!(v("007", ""), Value::Text("007".into()));
        assert_eq!(v("3.0", "BLOB"), Value::Real(3.0));
        assert_eq!(v("3.50", ""), Value::Text("3.50".into()));
        assert_eq!(v("hello", ""), Value::Text("hello".into()));

        let e = to_value(Some("X'0000'… (40000 bytes)"), "BLOB").unwrap_err();
        assert_eq!(e.kind, ErrorKind::Invalid);
        assert_eq!(
            e.message,
            "This value is only a preview and cannot be saved."
        );
        // Ordinary text with an ellipsis is not a preview.
        assert_eq!(v("wait… (2)", "TEXT"), Value::Text("wait… (2)".into()));
    }

    #[test]
    fn builds_statements() {
        let target = EditTarget {
            schema: Some("main".into()),
            table: "lq_edit Odd \"T\"".into(),
            columns: vec![
                ec("My Id", "INTEGER"),
                ec("v", "TEXT"),
                None,
                ec("k2", "TEXT"),
            ],
            key: vec![0, 3],
        };
        let steps = plan_changes(
            &target,
            &[
                RowChange::Update {
                    key: key(&[Some("1"), None]),
                    values: vec![set(1, Some("it's")), set(0, Some("2"))],
                },
                RowChange::Update {
                    key: key(&[Some("9"), Some("x")]),
                    values: vec![],
                },
                RowChange::Delete {
                    key: key(&[Some("3"), Some("b")]),
                },
                RowChange::Insert {
                    values: vec![set(1, None), set(3, Some("k"))],
                },
                RowChange::Insert { values: vec![] },
            ],
        )
        .unwrap();
        let t = "\"main\".\"lq_edit Odd \"\"T\"\"\"";
        assert_eq!(
            steps,
            [
                Step {
                    row: 1,
                    op: Op::Update,
                    sql: format!("UPDATE {t} SET \"v\" = ?, \"My Id\" = ? WHERE \"My Id\" = ? AND \"k2\" IS NULL"),
                    params: vec![Value::Text("it's".into()), Value::Integer(2), Value::Integer(1)],
                },
                Step {
                    row: 3,
                    op: Op::Delete,
                    sql: format!("DELETE FROM {t} WHERE \"My Id\" = ? AND \"k2\" = ?"),
                    params: vec![Value::Integer(3), Value::Text("b".into())],
                },
                Step {
                    row: 4,
                    op: Op::Insert,
                    sql: format!("INSERT INTO {t} (\"v\", \"k2\") VALUES (?, ?)"),
                    params: vec![Value::Null, Value::Text("k".into())],
                },
                Step {
                    row: 5,
                    op: Op::Insert,
                    sql: format!("INSERT INTO {t} DEFAULT VALUES"),
                    params: vec![],
                },
            ]
        );

        let e = plan_changes(
            &target,
            &[RowChange::Insert {
                values: vec![set(2, Some("x"))],
            }],
        )
        .unwrap_err();
        assert_eq!(e.kind, ErrorKind::Invalid);
        let e = plan_changes(
            &target,
            &[RowChange::Delete {
                key: key(&[Some("1")]),
            }],
        )
        .unwrap_err();
        assert_eq!(e.kind, ErrorKind::Invalid);
        let e = plan_changes(
            &target,
            &[RowChange::Update {
                key: key(&[Some("1"), Some("a")]),
                values: vec![set(1, Some("X'00'… (9 bytes)"))],
            }],
        )
        .unwrap_err();
        assert_eq!(e.kind, ErrorKind::Invalid);
    }

    #[test]
    fn reads_query_plans() {
        let shape = |lines: &[(i64, i64, &str)]| {
            let lines: Vec<_> = lines
                .iter()
                .map(|&(a, b, d)| (a, b, d.to_string()))
                .collect();
            let s = plan_shape(&lines);
            (s.compound, s.sources)
        };
        assert_eq!(shape(&[(2, 0, "SCAN t")]), (false, 1));
        // OR over two indexes is one scan.
        assert_eq!(
            shape(&[
                (4, 0, "MULTI-INDEX OR"),
                (5, 4, "INDEX 1"),
                (11, 5, "SEARCH t USING INDEX t_name (name=?)"),
                (16, 4, "INDEX 2"),
                (22, 16, "SEARCH t USING INDEX t_n (n=?)"),
            ]),
            (false, 1)
        );
        // EXISTS as a semi-join, and subqueries in expressions.
        assert_eq!(
            shape(&[
                (4, 0, "SCAN t"),
                (6, 0, "SEARCH u EXISTS USING PRIMARY KEY (a=?)")
            ]),
            (false, 1)
        );
        assert_eq!(
            shape(&[
                (2, 0, "SEARCH t USING INTEGER PRIMARY KEY (rowid=?)"),
                (6, 0, "LIST SUBQUERY 2"),
                (8, 6, "MERGE (UNION)"),
                (10, 8, "LEFT"),
                (13, 10, "SCAN u"),
                (23, 0, "CORRELATED SCALAR SUBQUERY 1"),
                (29, 23, "SCAN u"),
            ]),
            (false, 1)
        );
        // Window functions read t through a co-routine.
        assert_eq!(
            shape(&[
                (3, 0, "CO-ROUTINE (subquery-2)"),
                (6, 3, "SCAN t USING COVERING INDEX t_name"),
                (18, 0, "SCAN (subquery-2)"),
            ]),
            (false, 1)
        );
        // A subquery aliased with the name of its table.
        assert_eq!(
            shape(&[(2, 0, "CO-ROUTINE t"), (5, 2, "SCAN t"), (9, 0, "SCAN t")]),
            (false, 1)
        );
        // Joins, self-joins and compounds.
        assert_eq!(
            shape(&[
                (3, 0, "SCAN a"),
                (5, 0, "SEARCH b USING INTEGER PRIMARY KEY (rowid=?)")
            ]),
            (false, 2)
        );
        assert_eq!(
            shape(&[
                (1, 0, "COMPOUND QUERY"),
                (2, 1, "LEFT-MOST SUBQUERY"),
                (4, 2, "SCAN t"),
                (10, 1, "UNION ALL"),
                (12, 10, "SCAN CONSTANT ROW"),
            ]),
            (true, 1)
        );
        assert!(
            shape(&[
                (2, 0, "CO-ROUTINE r"),
                (5, 2, "SETUP"),
                (7, 5, "SCAN t"),
                (20, 2, "RECURSIVE STEP"),
                (21, 20, "SCAN r"),
                (31, 0, "SCAN r"),
            ])
            .0
        );
        assert_eq!(shape(&[(2, 0, "SCAN CONSTANT ROW")]), (false, 0));
    }

    #[tokio::test]
    async fn detects_editable_results() {
        let db = TempDb::new("edit-detect").await;
        db.run(
            "CREATE TABLE lq_edit_t (id INTEGER PRIMARY KEY, name TEXT NOT NULL, n NUMERIC(10,2), \
             g TEXT GENERATED ALWAYS AS (name || '!') VIRTUAL)",
        )
        .await;
        db.run("CREATE INDEX lq_edit_t_name ON lq_edit_t (name)")
            .await;
        db.run("CREATE INDEX lq_edit_t_n ON lq_edit_t (n)").await;
        db.run("INSERT INTO lq_edit_t (name, n) VALUES ('a', 1), ('b', 2)")
            .await;
        db.run("CREATE TABLE lq_edit_c (b TEXT, a INT, v TEXT, PRIMARY KEY (b, a)) WITHOUT ROWID")
            .await;
        db.run("CREATE TABLE lq_edit_nk (x INT, y TEXT)").await;
        db.run("CREATE TABLE lq_edit_u (code TEXT NOT NULL UNIQUE, maybe TEXT UNIQUE, v TEXT)")
            .await;
        db.run("CREATE VIEW lq_edit_v AS SELECT id, name FROM lq_edit_t WHERE id > 0")
            .await;
        db.run("CREATE VIEW lq_edit_vu AS SELECT id, name FROM lq_edit_t UNION ALL SELECT x, y FROM lq_edit_nk")
            .await;

        let t = db
            .target("SELECT id, name, name || '?' AS e, n, g FROM lq_edit_t")
            .await;
        assert_eq!(t.schema.as_deref(), Some("main"));
        assert_eq!(t.table, "lq_edit_t");
        assert_eq!(t.key, [0]);
        assert_eq!(
            cols(&t.columns),
            [
                col("id", "INTEGER"),
                col("name", "TEXT"),
                None,
                col("n", "NUMERIC(10,2)"),
                // Generated columns cannot be written.
                None,
            ]
        );
        assert_eq!(
            db.target("SELECT name, id AS k FROM lq_edit_t").await.key,
            [1]
        );
        // rowid of a table with INTEGER PRIMARY KEY is the key column.
        assert_eq!(
            cols(&db.target("SELECT rowid, name FROM lq_edit_t").await.columns)[0],
            col("id", "INTEGER")
        );
        // Composite key in key order, whatever the result order.
        assert_eq!(db.target("SELECT v, a, b FROM lq_edit_c").await.key, [2, 1]);
        // Filters, sorting, OR over indexes, subqueries and window functions.
        for sql in [
            "SELECT * FROM lq_edit_t WHERE name = 'a' OR n = 2 ORDER BY name",
            "SELECT * FROM lq_edit_t WHERE id IN (SELECT a FROM lq_edit_c)",
            "SELECT * FROM lq_edit_t t WHERE EXISTS (SELECT 1 FROM lq_edit_c c WHERE c.b = t.name)",
            "SELECT id, (SELECT count(*) FROM lq_edit_c) AS c FROM lq_edit_t",
            "SELECT id, name, row_number() OVER (ORDER BY name) FROM lq_edit_t",
            "SELECT * FROM (SELECT id, name FROM lq_edit_t ORDER BY name LIMIT 5)",
            "WITH c AS (SELECT * FROM lq_edit_t) SELECT * FROM c",
            // SQLite resolves view columns to the base table.
            "SELECT * FROM lq_edit_v",
        ] {
            let t = db.target(sql).await;
            assert_eq!(t.table, "lq_edit_t", "{sql}");
            assert_eq!(t.key, [0], "{sql}");
        }
        assert_eq!(
            cols(
                &db.target("SELECT id, (SELECT 1) FROM lq_edit_t")
                    .await
                    .columns
            )[1],
            None
        );

        assert_eq!(
            db.reason("SELECT t.id, c.v FROM lq_edit_t t JOIN lq_edit_c c ON c.b = t.name")
                .await,
            "Read-only: the result combines columns from several tables."
        );
        assert_eq!(
            db.reason("SELECT a.id, b.name FROM lq_edit_t a JOIN lq_edit_t b ON b.id = a.id + 1")
                .await,
            "Read-only: the result combines rows from several tables."
        );
        assert_eq!(
            db.reason("SELECT id, name FROM lq_edit_t UNION ALL SELECT 9, 'z'")
                .await,
            "Read-only: the result combines rows from several queries."
        );
        assert_eq!(
            db.reason("SELECT * FROM lq_edit_vu").await,
            "Read-only: the result combines rows from several queries."
        );
        assert_eq!(
            db.reason("SELECT 1, 'x'").await,
            "Read-only: only rows from a table can be edited."
        );
        assert_eq!(
            db.reason("SELECT count(*) FROM lq_edit_t").await,
            "Read-only: only rows from a table can be edited."
        );
        assert_eq!(
            db.reason("SELECT name, n FROM lq_edit_t").await,
            "Read-only: add the primary key columns (id) to the query to edit rows."
        );
        assert_eq!(
            db.reason("SELECT v, a FROM lq_edit_c").await,
            "Read-only: add the primary key columns (b, a) to the query to edit rows."
        );
        assert_eq!(
            db.reason("SELECT * FROM lq_edit_nk").await,
            "Read-only: table lq_edit_nk has no primary key; add rowid to the query to edit rows."
        );
        // Without a primary key: rowid, oid or _rowid_ in the result.
        for sql in [
            "SELECT rowid, * FROM lq_edit_nk",
            "SELECT x, oid FROM lq_edit_nk",
            "SELECT _rowid_, y FROM lq_edit_nk",
        ] {
            let t = db.target(sql).await;
            let i = t.key[0];
            assert_eq!(cols(&t.columns)[i], col("rowid", "INTEGER"), "{sql}");
        }
        // A UNIQUE NOT NULL key; the nullable one does not count.
        assert_eq!(db.target("SELECT v, code FROM lq_edit_u").await.key, [1]);
        assert_eq!(
            db.reason("SELECT v, maybe FROM lq_edit_u").await,
            "Read-only: table lq_edit_u has no primary key; add rowid to the query to edit rows."
        );

        // Attached and temporary schemas.
        let other = temp_path("edit-detect-other");
        db.run(&format!(
            "ATTACH DATABASE '{}' AS extra",
            other.display().to_string().replace('\'', "''")
        ))
        .await;
        db.run("CREATE TABLE extra.lq_edit_t (k TEXT PRIMARY KEY, v)")
            .await;
        db.run("CREATE TEMP TABLE lq_edit_tmp (k INTEGER PRIMARY KEY, v)")
            .await;
        let t = db.target("SELECT v, k FROM extra.lq_edit_t").await;
        assert_eq!(
            (t.schema.as_deref(), t.key.as_slice()),
            (Some("extra"), &[1][..])
        );
        assert_eq!(cols(&t.columns)[0], col("v", ""));
        let t = db.target("SELECT * FROM lq_edit_tmp").await;
        assert_eq!(t.schema.as_deref(), Some("temp"));

        // Statements that are not queries say nothing.
        for sql in [
            "PRAGMA table_info(lq_edit_t)",
            "INSERT INTO lq_edit_nk VALUES (1, 'r') RETURNING *",
            "EXPLAIN QUERY PLAN SELECT * FROM lq_edit_t",
        ] {
            let r = db.run(sql).await;
            assert!(
                r.editable.is_none() && r.read_only_reason.is_none(),
                "{sql}"
            );
        }

        // Keys are reloaded after DDL.
        db.run("DROP TABLE lq_edit_nk").await;
        db.run("CREATE TABLE lq_edit_nk (x INT PRIMARY KEY, y TEXT)")
            .await;
        assert_eq!(db.target("SELECT y, x FROM lq_edit_nk").await.key, [1]);

        db.run("DETACH DATABASE extra").await;
        let _ = std::fs::remove_file(&other);
    }

    #[tokio::test]
    async fn applies_updates_deletes_and_inserts() {
        let db = TempDb::new("edit-apply").await;
        db.run("CREATE TABLE lq_edit_t (id INTEGER PRIMARY KEY, name TEXT NOT NULL DEFAULT 'none', n INT DEFAULT 5)")
            .await;
        db.run("INSERT INTO lq_edit_t (name, n) VALUES ('a', 1), ('b', 2), ('c', 3)")
            .await;
        let t = db.target("SELECT id, name, n, n * 2 FROM lq_edit_t").await;
        let r = db
            .apply(
                &t,
                vec![
                    RowChange::Update {
                        key: key(&[Some("1")]),
                        values: vec![set(1, Some("a'2\nline")), set(2, None)],
                    },
                    RowChange::Delete {
                        key: key(&[Some("2")]),
                    },
                    RowChange::Insert {
                        values: vec![set(1, Some("d"))],
                    },
                    RowChange::Insert { values: vec![] },
                ],
            )
            .await
            .unwrap();
        assert_eq!((r.updated, r.deleted, r.inserted), (1, 1, 2));
        assert!(!r.in_transaction);
        assert!(!db.s().tx_state().in_transaction);
        assert_eq!(
            db.rows("SELECT id, name, n FROM lq_edit_t ORDER BY id")
                .await,
            texts(&[
                &[Some("1"), Some("a'2\nline"), None],
                &[Some("3"), Some("c"), Some("3")],
                &[Some("4"), Some("d"), Some("5")],
                &[Some("5"), Some("none"), Some("5")],
            ])
        );
        // Changing the key itself.
        db.apply(
            &t,
            vec![RowChange::Update {
                key: key(&[Some("5")]),
                values: vec![set(0, Some("50"))],
            }],
        )
        .await
        .unwrap();
        assert_eq!(
            db.scalar("SELECT name FROM lq_edit_t WHERE id = 50")
                .await
                .as_deref(),
            Some("none")
        );
        // A column that is not in the table.
        let e = db
            .apply(
                &t,
                vec![RowChange::Update {
                    key: key(&[Some("1")]),
                    values: vec![set(3, Some("1"))],
                }],
            )
            .await
            .unwrap_err();
        assert_eq!(e.kind, ErrorKind::Invalid);

        // A table without a key, edited through rowid.
        db.run("CREATE TABLE lq_edit_nk (x INT, y TEXT)").await;
        db.run("INSERT INTO lq_edit_nk VALUES (1, 'same'), (2, 'same')")
            .await;
        let t = db.target("SELECT rowid, x, y FROM lq_edit_nk").await;
        db.apply(
            &t,
            vec![
                RowChange::Update {
                    key: key(&[Some("2")]),
                    values: vec![set(2, Some("other"))],
                },
                RowChange::Delete {
                    key: key(&[Some("1")]),
                },
            ],
        )
        .await
        .unwrap();
        assert_eq!(
            db.rows("SELECT rowid, x, y FROM lq_edit_nk").await,
            texts(&[&[Some("2"), Some("2"), Some("other")]])
        );
    }

    #[tokio::test]
    async fn round_trips_displayed_values() {
        let db = TempDb::new("edit-roundtrip").await;
        db.run(
            "CREATE TABLE lq_edit_types (id INTEGER PRIMARY KEY, i INTEGER, bi BIGINT, r REAL, \
             d DOUBLE, num NUMERIC(10,2), dec DECIMAL(20,6), b BOOLEAN, dt DATE, ts DATETIME, \
             tz TIMESTAMP WITH TIME ZONE, t TEXT, vc VARCHAR(40), bl BLOB, u, nu TEXT)",
        )
        .await;
        db.run(
            "INSERT INTO lq_edit_types VALUES (1, -42, 9223372036854775807, 2.5, 0.30000000000000004, \
             12.5, 1234567.125, 1, '2024-02-29', '2024-05-01 13:45:00.123456', \
             '2024-05-01T13:45:00.123+02:00', 'it''s \"q\"' || char(10) || 'line 2', 'olá', \
             x'00FF10', 7, NULL)",
        )
        .await;
        db.run(
            "INSERT INTO lq_edit_types VALUES (2, 0, -9223372036854775808, 1e300, -1.5e-7, \
             100, -0.5, 0, '1999-12-31', '2000-01-01 00:00:00', '2000-01-01T00:00:00Z', '', \
             '42', x'', 2.5, 'x')",
        )
        .await;
        db.run(
            "INSERT INTO lq_edit_types VALUES (3, NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL, \
             NULL, NULL, NULL, NULL, NULL, 'text in untyped', NULL)",
        )
        .await;
        let typed = "SELECT * FROM lq_edit_types ORDER BY id";
        let classes = "SELECT id, typeof(i), typeof(bi), typeof(r), typeof(d), typeof(num), \
                       typeof(dec), typeof(b), typeof(dt), typeof(ts), typeof(tz), typeof(t), \
                       typeof(vc), typeof(bl), typeof(u), typeof(nu) FROM lq_edit_types ORDER BY id";
        let before = db.run(typed).await;
        let classes_before = db.rows(classes).await;
        let target = before.editable.clone().unwrap();
        assert_eq!(target.key, [0]);

        // Write every displayed value back, keys included.
        let changes: Vec<RowChange> = before
            .rows
            .iter()
            .map(|row| RowChange::Update {
                key: vec![row[0].clone()],
                values: row
                    .iter()
                    .enumerate()
                    .map(|(i, v)| CellValue {
                        column: i,
                        value: v.clone(),
                    })
                    .collect(),
            })
            .collect();
        let r = db.apply(&target, changes).await.unwrap();
        assert_eq!(r.updated, 3);
        assert_eq!(db.rows(typed).await, before.rows);
        assert_eq!(db.rows(classes).await, classes_before);

        // Values typed in the grid.
        db.apply(
            &target,
            vec![RowChange::Update {
                key: key(&[Some("3")]),
                values: vec![
                    set(1, Some("12")),
                    set(3, Some("3")),
                    set(5, Some("9.99")),
                    set(7, Some("true")),
                    set(13, Some("X'CAFE'")),
                ],
            }],
        )
        .await
        .unwrap();
        assert_eq!(
            db.rows(
                "SELECT typeof(i), r, typeof(r), num, b, hex(bl) FROM lq_edit_types WHERE id = 3"
            )
            .await,
            texts(&[&[
                Some("integer"),
                Some("3.0"),
                Some("real"),
                Some("9.99"),
                Some("1"),
                Some("CAFE")
            ]])
        );

        // A preview of a large blob cannot be saved.
        db.run("UPDATE lq_edit_types SET bl = zeroblob(40000) WHERE id = 1")
            .await;
        let preview = db.scalar("SELECT bl FROM lq_edit_types WHERE id = 1").await;
        let e = db
            .apply(
                &target,
                vec![RowChange::Update {
                    key: key(&[Some("1")]),
                    values: vec![set(13, preview.as_deref())],
                }],
            )
            .await
            .unwrap_err();
        assert_eq!(e.kind, ErrorKind::Invalid);
        assert_eq!(
            e.message,
            "This value is only a preview and cannot be saved."
        );
    }

    #[tokio::test]
    async fn failed_change_rolls_back_the_batch() {
        let db = TempDb::new("edit-rollback").await;
        db.run("CREATE TABLE lq_edit_t (id INTEGER PRIMARY KEY, name TEXT UNIQUE)")
            .await;
        db.run("INSERT INTO lq_edit_t VALUES (1, 'a'), (2, 'b')")
            .await;
        let t = db.target("SELECT id, name FROM lq_edit_t").await;
        let before = db.rows("SELECT * FROM lq_edit_t ORDER BY id").await;

        let e = db
            .apply(
                &t,
                vec![
                    RowChange::Update {
                        key: key(&[Some("1")]),
                        values: vec![set(1, Some("changed"))],
                    },
                    RowChange::Insert {
                        values: vec![set(1, Some("new"))],
                    },
                    RowChange::Update {
                        key: key(&[Some("99")]),
                        values: vec![set(1, Some("gone"))],
                    },
                ],
            )
            .await
            .unwrap_err();
        assert_eq!(e.kind, ErrorKind::Database);
        assert_eq!(
            e.message,
            "Row 3 was not saved: 0 rows matched its key (changed or removed by someone else)."
        );
        assert_eq!(db.rows("SELECT * FROM lq_edit_t ORDER BY id").await, before);
        assert!(!db.s().tx_state().in_transaction);

        // Server errors keep their code and name the row.
        let e = db
            .apply(
                &t,
                vec![
                    RowChange::Delete {
                        key: key(&[Some("1")]),
                    },
                    RowChange::Update {
                        key: key(&[Some("2")]),
                        values: vec![set(1, Some("x"))],
                    },
                    RowChange::Insert {
                        values: vec![set(1, Some("x"))],
                    },
                ],
            )
            .await
            .unwrap_err();
        assert_eq!(e.code.as_deref(), Some("SQLITE_CONSTRAINT_UNIQUE"));
        assert!(
            e.message.starts_with("Row 3 was not saved: UNIQUE"),
            "{}",
            e.message
        );
        assert_eq!(e.position, None);
        assert_eq!(db.rows("SELECT * FROM lq_edit_t ORDER BY id").await, before);

        // The session is still usable.
        db.apply(
            &t,
            vec![RowChange::Delete {
                key: key(&[Some("2")]),
            }],
        )
        .await
        .unwrap();
        assert_eq!(
            db.scalar("SELECT count(*) FROM lq_edit_t").await.as_deref(),
            Some("1")
        );
    }

    #[tokio::test]
    async fn manual_commit_keeps_the_transaction() {
        let db = TempDb::new("edit-manual").await;
        db.run("CREATE TABLE lq_edit_t (id INTEGER PRIMARY KEY, v TEXT)")
            .await;
        db.run("INSERT INTO lq_edit_t VALUES (1, 'a')").await;
        let t = db.target("SELECT id, v FROM lq_edit_t").await;
        db.s().set_autocommit(false).await.unwrap();
        db.s().end_transaction(false).await.unwrap();

        let r = db
            .apply(
                &t,
                vec![RowChange::Insert {
                    values: vec![set(1, Some("b"))],
                }],
            )
            .await
            .unwrap();
        assert!(r.in_transaction);
        assert!(db.s().tx_state().in_transaction);
        db.s().end_transaction(false).await.unwrap();
        assert_eq!(
            db.scalar("SELECT count(*) FROM lq_edit_t").await.as_deref(),
            Some("1")
        );

        // Inside an open transaction a failed batch undoes only itself.
        db.run("INSERT INTO lq_edit_t VALUES (2, 'mine')").await;
        let e = db
            .apply(
                &t,
                vec![
                    RowChange::Update {
                        key: key(&[Some("1")]),
                        values: vec![set(1, Some("z"))],
                    },
                    RowChange::Delete {
                        key: key(&[Some("7")]),
                    },
                ],
            )
            .await
            .unwrap_err();
        assert!(
            e.message.starts_with("Row 2 was not saved"),
            "{}",
            e.message
        );
        assert!(db.s().tx_state().in_transaction);
        assert_eq!(
            db.rows("SELECT id, v FROM lq_edit_t ORDER BY id").await,
            texts(&[&[Some("1"), Some("a")], &[Some("2"), Some("mine")]])
        );
        let r = db
            .apply(
                &t,
                vec![RowChange::Update {
                    key: key(&[Some("1")]),
                    values: vec![set(1, Some("z"))],
                }],
            )
            .await
            .unwrap();
        assert!(r.in_transaction);
        db.s().end_transaction(true).await.unwrap();
        assert_eq!(
            db.rows("SELECT id, v FROM lq_edit_t ORDER BY id").await,
            texts(&[&[Some("1"), Some("z")], &[Some("2"), Some("mine")]])
        );

        // Autocommit with a transaction the user opened: it stays open.
        db.s().set_autocommit(true).await.unwrap();
        db.run("BEGIN").await;
        let r = db
            .apply(
                &t,
                vec![RowChange::Delete {
                    key: key(&[Some("2")]),
                }],
            )
            .await
            .unwrap();
        assert!(r.in_transaction);
        db.run("ROLLBACK").await;
        assert_eq!(
            db.scalar("SELECT count(*) FROM lq_edit_t").await.as_deref(),
            Some("2")
        );
    }

    #[tokio::test]
    async fn edits_names_that_need_quoting() {
        let db = TempDb::new("edit-quoting").await;
        db.run("CREATE TABLE \"lq_edit Mixed Case\" (\"My Id\" INTEGER PRIMARY KEY, \"Some \"\"Value\"\"\" TEXT)")
            .await;
        db.run("INSERT INTO \"lq_edit Mixed Case\" VALUES (1, 'a'), (2, 'b')")
            .await;
        let t = db
            .target("SELECT \"Some \"\"Value\"\"\" AS v, \"My Id\" FROM \"lq_edit Mixed Case\" AS \"q r\"")
            .await;
        assert_eq!(t.table, "lq_edit Mixed Case");
        assert_eq!(t.key, [1]);
        assert_eq!(cols(&t.columns)[0], col("Some \"Value\"", "TEXT"));
        let r = db
            .apply(
                &t,
                vec![
                    RowChange::Update {
                        key: key(&[Some("1")]),
                        values: vec![set(0, Some("A"))],
                    },
                    RowChange::Delete {
                        key: key(&[Some("2")]),
                    },
                    RowChange::Insert {
                        values: vec![set(0, Some("c")), set(1, Some("10"))],
                    },
                ],
            )
            .await
            .unwrap();
        assert_eq!((r.updated, r.deleted, r.inserted), (1, 1, 1));
        assert_eq!(
            db.rows("SELECT * FROM \"lq_edit Mixed Case\" ORDER BY 1")
                .await,
            texts(&[&[Some("1"), Some("A")], &[Some("10"), Some("c")]])
        );
    }
}
