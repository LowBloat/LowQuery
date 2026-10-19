//! Oracle session over the official `oracledb` driver (thin mode, pure Rust):
//! no Oracle Client/Instant Client needs to be installed.
//!
//! The driver is synchronous; each call runs in `spawn_blocking`. The row
//! limit is applied by tuning `prefetch_rows`/`fetch_array_size` and
//! stopping the cursor read, without fetching the rest from the server.

use std::fmt::Write as _;
use std::sync::Arc;
use std::time::Instant;

use oracledb::{
    Connection, JsonValue, Metadata, OracleNumber, OracleTimestamp, Row, Vector, VectorData,
};
use parking_lot::Mutex;

use super::{first_keywords, TxState};
use crate::error::{AppError, AppResult, ErrorKind};
use crate::model::{
    ColumnInfo, ConnectionProfile, DbColumn, DbObject, ExecRequest, ExecResult, OracleConnectType,
    OraclePrivilege, ValueKind,
};

const BINARY_PREVIEW_BYTES: usize = 32 * 1024;
const MAX_OUTPUT_LINES: usize = 5000;

pub struct OraSession {
    conn: Arc<Connection>,
    busy: tokio::sync::Mutex<()>,
    state: Arc<Mutex<TxState>>,
    pub server_version: String,
    pub current_user: Option<String>,
    pub current_schema: Option<String>,
}

/// Builds the address. Service and SID become full descriptors so they do
/// not depend on name resolution.
pub fn connect_string(profile: &ConnectionProfile) -> String {
    let host = profile.host.trim();
    let port = if profile.port == 0 {
        1521
    } else {
        profile.port
    };
    let target = profile.database.trim();
    match profile.oracle_connect_type {
        OracleConnectType::Service => format!(
            "(DESCRIPTION=(ADDRESS=(PROTOCOL=TCP)(HOST={host})(PORT={port}))(CONNECT_DATA=(SERVICE_NAME={target})))"
        ),
        OracleConnectType::Sid => format!(
            "(DESCRIPTION=(ADDRESS=(PROTOCOL=TCP)(HOST={host})(PORT={port}))(CONNECT_DATA=(SID={target})))"
        ),
        OracleConnectType::Custom => profile.connect_string.trim().to_string(),
    }
}

pub async fn connect(profile: &ConnectionProfile, password: Option<&str>) -> AppResult<OraSession> {
    let profile = profile.clone();
    let password = password.unwrap_or_default().to_string();
    blocking(move || {
        let mut cfg = oracledb::Config::default();
        if let Some(dir) = profile.config_dir.as_deref().filter(|d| !d.trim().is_empty()) {
            cfg = cfg.set_config_dir(dir.trim());
        }
        cfg = cfg
            .set_credentials(profile.user.trim(), &password)
            .set_program("LowQuery")
            .map_err(connection_error)?
            .set_connect_string(&connect_string(&profile))
            .map_err(connection_error)?;
        if profile.oracle_privilege != OraclePrivilege::None {
            cfg = cfg.set_auth_mode(profile.oracle_privilege.auth_mode());
        }
        let conn = oracledb::connect(cfg).map_err(connection_error)?;
        let _ = conn.set_module("LowQuery");

        let row = conn
            .query_row(
                "select sys_context('USERENV', 'SESSION_USER'), sys_context('USERENV', 'CURRENT_SCHEMA') from dual",
                &[],
            )
            .map_err(map_error)?;
        // Enables DBMS_OUTPUT with no buffer limit.
        conn.execute("begin dbms_output.enable(null); end;", &[]).map_err(map_error)?;

        let version = conn.version().map(|v| v.to_string()).unwrap_or_default();
        Ok(OraSession {
            server_version: format!("Oracle {version}"),
            current_user: row.get(0).map_err(map_error)?,
            current_schema: row.get(1).map_err(map_error)?,
            conn: Arc::new(conn),
            busy: tokio::sync::Mutex::new(()),
            state: Arc::new(Mutex::new(TxState { autocommit: true, in_transaction: false })),
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

/// Prepares the text for Oracle: strips the trailing SQL*Plus slash and the
/// trailing `;` of SQL statements. PL/SQL blocks keep the `;` (it is part of
/// `END;`).
pub fn prepare_sql(sql: &str) -> String {
    let mut text = sql.trim_end().to_string();
    if let Some(stripped) = text.strip_suffix('/') {
        // Only strips the slash when it is alone on the last line.
        if stripped.ends_with('\n') || stripped.trim_end().is_empty() {
            text = stripped.trim_end().to_string();
        }
    }
    if !is_plsql(&text) {
        while let Some(stripped) = text.strip_suffix(';') {
            text = stripped.trim_end().to_string();
        }
    }
    text
}

/// Anonymous blocks and creation of PL/SQL units.
pub fn is_plsql(sql: &str) -> bool {
    let kw = first_keywords(sql, 6);
    let mut words = kw.iter().map(String::as_str).peekable();
    match words.next() {
        Some("BEGIN" | "DECLARE") => true,
        Some("CREATE") => {
            if words.peek() == Some(&"OR") {
                words.next();
                words.next(); // REPLACE
            }
            if matches!(
                words.peek(),
                Some(&"EDITIONABLE" | &"NONEDITIONABLE" | &"EDITIONING")
            ) {
                words.next();
            }
            matches!(
                words.next(),
                Some(
                    "PROCEDURE" | "FUNCTION" | "PACKAGE" | "TRIGGER" | "TYPE" | "LIBRARY" | "JAVA"
                )
            )
        }
        _ => false,
    }
}

impl OraSession {
    pub fn tx_state(&self) -> TxState {
        *self.state.lock()
    }

    pub async fn execute(&self, req: &ExecRequest) -> AppResult<ExecResult> {
        let _busy = self.busy.lock().await;
        let conn = self.conn.clone();
        let state = self.state.clone();
        let sql = prepare_sql(&req.sql);
        let max_rows = req.max_rows;
        blocking(move || execute_blocking(&conn, &sql, max_rows, &state)).await
    }

    pub async fn set_autocommit(&self, on: bool) -> AppResult<TxState> {
        if on && self.state.lock().in_transaction {
            self.end_transaction(true).await?;
        }
        let mut st = self.state.lock();
        st.autocommit = on;
        Ok(*st)
    }

    pub async fn end_transaction(&self, commit: bool) -> AppResult<TxState> {
        let _busy = self.busy.lock().await;
        let conn = self.conn.clone();
        blocking(move || {
            if commit {
                conn.commit()
            } else {
                conn.rollback()
            }
            .map_err(map_error)
        })
        .await?;
        let mut st = self.state.lock();
        st.in_transaction = false;
        Ok(*st)
    }

    pub async fn list_schemas(&self) -> AppResult<Vec<String>> {
        // Schemas with objects: the user's first, Oracle-maintained ones last.
        // ORACLE_MAINTAINED only exists from 12c on.
        let modern = self
            .query_strings(
                "select username from all_users u
                 where u.username = sys_context('USERENV', 'CURRENT_SCHEMA')
                    or exists (select 1 from all_objects o where o.owner = u.username)
                 order by case when username = sys_context('USERENV', 'CURRENT_SCHEMA') then 0
                               when oracle_maintained = 'Y' then 2 else 1 end, username",
                Vec::new(),
            )
            .await;
        match modern {
            Ok(list) => Ok(list),
            Err(_) => {
                self.query_strings(
                    "select username from all_users u
                     where u.username = sys_context('USERENV', 'CURRENT_SCHEMA')
                        or exists (select 1 from all_objects o where o.owner = u.username)
                     order by case when username = sys_context('USERENV', 'CURRENT_SCHEMA')
                                   then 0 else 1 end, username",
                    Vec::new(),
                )
                .await
            }
        }
    }

    pub async fn list_objects(&self, schema: &str) -> AppResult<Vec<DbObject>> {
        let rows = self
            .query_rows(
                "select object_name,
                        case object_type when 'TABLE' then 'table' when 'VIEW' then 'view'
                             when 'MATERIALIZED VIEW' then 'matview' when 'SEQUENCE' then 'sequence'
                             when 'FUNCTION' then 'function' when 'PROCEDURE' then 'procedure'
                             when 'PACKAGE' then 'package' when 'TYPE' then 'type'
                             when 'SYNONYM' then 'synonym' end
                 from all_objects
                 where owner = :1 and object_type in ('TABLE','VIEW','MATERIALIZED VIEW','SEQUENCE',
                       'FUNCTION','PROCEDURE','PACKAGE','TYPE','SYNONYM')
                   and object_name not like 'BIN$%'
                   and not (object_type = 'TABLE' and exists (
                        select 1 from all_mviews m where m.owner = all_objects.owner
                        and m.mview_name = all_objects.object_name))
                 order by 2, 1",
                vec![schema.to_string()],
            )
            .await?;
        Ok(rows
            .into_iter()
            .map(|mut r| DbObject {
                kind: r.pop().unwrap_or_default(),
                name: r.pop().unwrap_or_default(),
            })
            .collect())
    }

    pub async fn list_columns(&self, schema: &str, table: &str) -> AppResult<Vec<DbColumn>> {
        let rows = self
            .query_rows(
                "select c.column_name,
                        case
                          when c.data_type in ('VARCHAR2','NVARCHAR2','CHAR','NCHAR','RAW')
                            then c.data_type || '(' || nvl(c.char_length, c.data_length) || ')'
                          when c.data_type = 'NUMBER' and c.data_precision is not null
                            then 'NUMBER(' || c.data_precision ||
                                 case when c.data_scale > 0 then ',' || c.data_scale end || ')'
                          else c.data_type end,
                        c.nullable,
                        case when exists (
                          select 1 from all_constraints k
                          join all_cons_columns kc on kc.owner = k.owner
                           and kc.constraint_name = k.constraint_name
                          where k.owner = c.owner and k.table_name = c.table_name
                            and k.constraint_type = 'P' and kc.column_name = c.column_name)
                        then 'Y' else 'N' end
                 from all_tab_columns c
                 where c.owner = :1 and c.table_name = :2
                 order by c.column_id",
                vec![schema.to_string(), table.to_string()],
            )
            .await?;
        Ok(rows
            .into_iter()
            .map(|r| DbColumn {
                name: r[0].clone(),
                type_name: r[1].clone(),
                nullable: r[2] == "Y",
                primary_key: r[3] == "Y",
            })
            .collect())
    }

    async fn query_strings(
        &self,
        sql: &'static str,
        params: Vec<String>,
    ) -> AppResult<Vec<String>> {
        Ok(self
            .query_rows(sql, params)
            .await?
            .into_iter()
            .filter_map(|mut r| r.drain(..).next())
            .collect())
    }

    async fn query_rows(
        &self,
        sql: &'static str,
        params: Vec<String>,
    ) -> AppResult<Vec<Vec<String>>> {
        let _busy = self.busy.lock().await;
        let conn = self.conn.clone();
        blocking(move || {
            let binds: Vec<&dyn oracledb::ToDbValue> = params
                .iter()
                .map(|p| p as &dyn oracledb::ToDbValue)
                .collect();
            let cursor = conn
                .statement(sql)
                .map_err(map_error)?
                .fetch_array_size(500)
                .prefetch_rows(500)
                .build()
                .map_err(map_error)?
                .query(&binds)
                .map_err(map_error)?;
            let mut out = Vec::new();
            for row in cursor {
                let row = row.map_err(map_error)?;
                let n = row.columns().len();
                let mut cells = Vec::with_capacity(n);
                for i in 0..n {
                    cells.push(
                        row.get::<Option<String>>(i)
                            .map_err(map_error)?
                            .unwrap_or_default(),
                    );
                }
                out.push(cells);
            }
            Ok(out)
        })
        .await
    }
}

fn execute_blocking(
    conn: &Connection,
    sql: &str,
    max_rows: usize,
    state: &Mutex<TxState>,
) -> AppResult<ExecResult> {
    let started = Instant::now();
    let array = if max_rows == 0 {
        1000
    } else {
        (max_rows + 1).min(1000)
    } as u32;
    let mut stmt = conn
        .statement(sql)
        .map_err(map_error)?
        .fetch_array_size(array)
        .prefetch_rows(array)
        .build()
        .map_err(map_error)?;

    let mut result = ExecResult::default();
    if stmt.is_query() {
        let cursor = stmt.query(&[]).map_err(map_error)?;
        let cols: Vec<Metadata> = cursor.columns().to_vec();
        result.columns = Some(
            cols.iter()
                .map(|c| ColumnInfo {
                    name: c.name().to_string(),
                    type_name: c.data_type(),
                    kind: value_kind(c),
                })
                .collect(),
        );
        for row in cursor {
            if max_rows > 0 && result.rows.len() == max_rows {
                result.truncated = true;
                break;
            }
            let row = row.map_err(map_error)?;
            let mut cells = Vec::with_capacity(cols.len());
            for (i, col) in cols.iter().enumerate() {
                cells.push(cell(&row, i, col).map_err(map_error)?);
            }
            result.rows.push(cells);
        }
    } else {
        let is_dml = stmt.is_dml();
        let is_plsql = stmt.is_plsql();
        let is_ddl = stmt.is_ddl();
        let exec = stmt.execute(&[]).map_err(map_error)?;
        if is_dml {
            result.rows_affected = Some(exec.rows_affected());
        }
        let kw = first_keywords(sql, 2);
        let first = kw.first().map(String::as_str).unwrap_or("");
        let mut st = state.lock();
        if (first == "COMMIT" || first == "ROLLBACK") && kw.get(1).map(String::as_str) != Some("TO")
        {
            st.in_transaction = false;
        } else if is_ddl {
            // DDL implicitly commits the transaction.
            st.in_transaction = false;
        } else if is_dml || is_plsql {
            if st.autocommit {
                conn.commit().map_err(map_error)?;
                st.in_transaction = false;
            } else {
                st.in_transaction = true;
            }
        }
        drop(st);
        result.notices = read_dbms_output(conn);
    }
    result.elapsed_ms = started.elapsed().as_millis() as u64;
    result.in_transaction = state.lock().in_transaction;
    Ok(result)
}

/// Reads the pending DBMS_OUTPUT lines.
fn read_dbms_output(conn: &Connection) -> Vec<String> {
    let mut lines = Vec::new();
    while lines.len() < MAX_OUTPUT_LINES {
        // Positional binds follow the order the placeholders appear in the
        // text, so :1 (line) must come before :2 (status).
        let exec = conn.execute(
            "declare l varchar2(32767); s integer; begin dbms_output.get_line(l, s); \
             :1 := substrb(l, 1, 3900); :2 := s; end;",
            &[&oracledb::DB_TYPE_VARCHAR, &oracledb::DB_TYPE_NUMBER],
        );
        let Ok(mut exec) = exec else { break };
        let row = exec.out_bind_data();
        let status: i64 = row.get::<Option<i64>>(1).ok().flatten().unwrap_or(1);
        if status != 0 {
            break;
        }
        lines.push(
            row.get::<Option<String>>(0)
                .ok()
                .flatten()
                .unwrap_or_default(),
        );
    }
    lines
}

fn value_kind(col: &Metadata) -> ValueKind {
    match col.db_type().name() {
        "DB_TYPE_NUMBER"
        | "DB_TYPE_BINARY_INTEGER"
        | "DB_TYPE_BINARY_DOUBLE"
        | "DB_TYPE_BINARY_FLOAT" => ValueKind::Number,
        "DB_TYPE_BOOLEAN" => ValueKind::Bool,
        "DB_TYPE_DATE"
        | "DB_TYPE_TIMESTAMP"
        | "DB_TYPE_TIMESTAMP_TZ"
        | "DB_TYPE_TIMESTAMP_LTZ"
        | "DB_TYPE_INTERVAL_DS"
        | "DB_TYPE_INTERVAL_YM" => ValueKind::Temporal,
        "DB_TYPE_RAW" | "DB_TYPE_LONG_RAW" | "DB_TYPE_BLOB" => ValueKind::Binary,
        "DB_TYPE_JSON" => ValueKind::Json,
        _ if col.db_type().is_string_type() => ValueKind::Text,
        _ => ValueKind::Other,
    }
}

fn cell(row: &Row, i: usize, col: &Metadata) -> Result<Option<String>, oracledb::Error> {
    let name = col.db_type().name();
    Ok(match name {
        "DB_TYPE_NUMBER" | "DB_TYPE_BINARY_INTEGER" => {
            row.get::<Option<OracleNumber>>(i)?.map(|n| n.to_string())
        }
        "DB_TYPE_BINARY_DOUBLE" => row
            .get::<Option<f64>>(i)?
            .map(|v| float(v.is_nan(), v.is_infinite(), v > 0.0, v.to_string())),
        "DB_TYPE_BINARY_FLOAT" => row
            .get::<Option<f32>>(i)?
            .map(|v| float(v.is_nan(), v.is_infinite(), v > 0.0, v.to_string())),
        "DB_TYPE_BOOLEAN" => row
            .get::<Option<bool>>(i)?
            .map(|b| if b { "TRUE" } else { "FALSE" }.to_string()),
        "DB_TYPE_DATE" => row
            .get::<Option<OracleTimestamp>>(i)?
            .map(|t| timestamp(&t, 0, false)),
        "DB_TYPE_TIMESTAMP" | "DB_TYPE_TIMESTAMP_LTZ" => row
            .get::<Option<OracleTimestamp>>(i)?
            .map(|t| timestamp(&t, col.scale(), false)),
        "DB_TYPE_TIMESTAMP_TZ" => row
            .get::<Option<OracleTimestamp>>(i)?
            .map(|t| timestamp(&t, col.scale(), true)),
        "DB_TYPE_INTERVAL_DS" => row
            .get::<Option<oracledb::OracleIntervalDS>>(i)?
            .map(|v| v.to_string()),
        "DB_TYPE_INTERVAL_YM" => row
            .get::<Option<oracledb::OracleIntervalYM>>(i)?
            .map(|v| v.to_string()),
        "DB_TYPE_RAW" | "DB_TYPE_LONG_RAW" | "DB_TYPE_BLOB" => {
            row.get::<Option<Vec<u8>>>(i)?.map(|b| hex(&b))
        }
        "DB_TYPE_JSON" => row.get::<Option<JsonValue>>(i)?.map(|j| {
            let mut s = String::new();
            json(&j, &mut s);
            s
        }),
        "DB_TYPE_VECTOR" => row.get::<Option<Vector>>(i)?.map(|v| vector(&v)),
        "DB_TYPE_CURSOR" => Some("(cursor)".into()),
        "DB_TYPE_BFILE" => Some("(bfile)".into()),
        _ => match row.get::<Option<String>>(i) {
            Ok(v) => v,
            Err(_) => match row.get::<Option<Vec<u8>>>(i) {
                Ok(v) => v.map(|b| hex(&b)),
                Err(_) => Some(format!("({})", col.data_type())),
            },
        },
    })
}

/// BINARY_FLOAT/DOUBLE with the special names Oracle uses.
fn float(nan: bool, infinite: bool, positive: bool, text: String) -> String {
    if nan {
        "Nan".into()
    } else if infinite {
        if positive {
            "Inf".into()
        } else {
            "-Inf".into()
        }
    } else {
        text
    }
}

/// `2024-05-01 13:45:00[.123][ +03:00]`, with the column's fractional digits.
fn timestamp(t: &OracleTimestamp, scale: i8, with_tz: bool) -> String {
    let mut s = format!(
        "{:04}-{:02}-{:02} {:02}:{:02}:{:02}",
        t.year(),
        t.month(),
        t.day(),
        t.hour(),
        t.minute(),
        t.second()
    );
    let digits = scale.clamp(0, 9) as usize;
    if digits > 0 {
        let frac = format!("{:09}", t.nanoseconds());
        s.push('.');
        s.push_str(&frac[..digits]);
    }
    if with_tz {
        let (h, m) = (t.tz_hour_offset(), t.tz_minute_offset());
        let sign = if h < 0 || m < 0 { '-' } else { '+' };
        let _ = write!(s, " {sign}{:02}:{:02}", h.unsigned_abs(), m.unsigned_abs());
    }
    s
}

/// Uppercase hexadecimal, the way SQL*Plus displays RAW.
fn hex(bytes: &[u8]) -> String {
    let shown = &bytes[..bytes.len().min(BINARY_PREVIEW_BYTES)];
    let mut s = String::with_capacity(shown.len() * 2);
    for b in shown {
        let _ = write!(s, "{b:02X}");
    }
    if bytes.len() > shown.len() {
        let _ = write!(s, "… ({} bytes)", bytes.len());
    }
    s
}

fn vector(v: &Vector) -> String {
    fn join<T: ToString>(items: &[T]) -> String {
        items
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
            .join(",")
    }
    fn data(d: &VectorData) -> String {
        match d {
            VectorData::Float32(x) => join(x),
            VectorData::Float64(x) => join(x),
            VectorData::Int8(x) => join(x),
            VectorData::Binary(x) => join(x),
        }
    }
    match v {
        Vector::Dense(d) => format!("[{}]", data(d)),
        // Oracle's text format for sparse vectors: [dimensions,[indices],[values]]
        Vector::Sparse(s) => format!(
            "[{},[{}],[{}]]",
            s.num_dimensions(),
            join(s.indices()),
            data(s.values())
        ),
    }
}

fn json(v: &JsonValue, out: &mut String) {
    match v {
        JsonValue::Null => out.push_str("null"),
        JsonValue::Boolean(b) => out.push_str(if *b { "true" } else { "false" }),
        JsonValue::Number(n) => out.push_str(&n.to_string()),
        JsonValue::BinaryDouble(f) => out.push_str(&f.to_string()),
        JsonValue::BinaryFloat(f) => out.push_str(&f.to_string()),
        JsonValue::String(s) => out.push_str(&serde_json::to_string(s).unwrap_or_default()),
        JsonValue::Timestamp(t) => {
            let _ = write!(out, "\"{}\"", timestamp(t, 6, false));
        }
        JsonValue::IntervalDS(i) => {
            let _ = write!(out, "\"{i}\"");
        }
        JsonValue::IntervalYM(i) => {
            let _ = write!(out, "\"{i}\"");
        }
        JsonValue::Raw(b) | JsonValue::JsonId(b) => {
            let _ = write!(out, "\"{}\"", hex(b));
        }
        JsonValue::Vector(v) => out.push_str(&vector(v)),
        JsonValue::JsonArray(items) => {
            out.push('[');
            for (i, item) in items.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                json(item, out);
            }
            out.push(']');
        }
        JsonValue::JsonObject(map) => {
            let mut keys: Vec<&String> = map.keys().collect();
            keys.sort();
            out.push('{');
            for (i, key) in keys.into_iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                out.push_str(&serde_json::to_string(key).unwrap_or_default());
                out.push(':');
                json(&map[key], out);
            }
            out.push('}');
        }
    }
}

fn connection_error(e: oracledb::Error) -> AppError {
    let mut err = map_error(e);
    err.kind = ErrorKind::Connection;
    err
}

pub fn map_error(e: oracledb::Error) -> AppError {
    use oracledb::ErrorKind as K;
    match e.kind() {
        K::DbError(db) => {
            let code = format!("ORA-{:05}", db.code());
            let message = db.message();
            let message = message
                .strip_prefix(&format!("{code}: "))
                .unwrap_or(message)
                .trim_end()
                .to_string();
            let mut err = AppError::new(ErrorKind::Database, message);
            // 1013: "user requested cancel of current operation".
            if db.code() == 1013 {
                err.kind = ErrorKind::Cancelled;
            }
            if db.offset() > 0 {
                err.position = Some(db.offset());
            }
            err.code = Some(code);
            err
        }
        K::DeadConnection | K::NotConnected => {
            AppError::connection("The connection to the server was closed.")
        }
        K::CallTimeoutExceeded => AppError::new(ErrorKind::Cancelled, "Call timeout exceeded."),
        _ => AppError::new(ErrorKind::Database, e.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strips_terminators_only_for_sql() {
        assert_eq!(prepare_sql("select 1 from dual;"), "select 1 from dual");
        assert_eq!(prepare_sql("select 1 from dual\n/"), "select 1 from dual");
        assert_eq!(prepare_sql("begin null; end;\n/"), "begin null; end;");
        assert_eq!(
            prepare_sql("create or replace procedure p as begin null; end;"),
            "create or replace procedure p as begin null; end;"
        );
        assert_eq!(
            prepare_sql("select 10/2 from dual"),
            "select 10/2 from dual"
        );
    }

    #[test]
    fn detects_plsql_units() {
        assert!(is_plsql("DECLARE x number; BEGIN null; END;"));
        assert!(is_plsql(
            "create or replace editionable package body pkg as end;"
        ));
        assert!(is_plsql(
            "CREATE TRIGGER t BEFORE INSERT ON x BEGIN NULL; END;"
        ));
        assert!(!is_plsql("create table t (id number)"));
        assert!(!is_plsql(
            "create or replace view v as select 1 x from dual"
        ));
    }
}
