//! Oracle session over the official `oracledb` driver (thin mode, pure Rust):
//! no Oracle Client/Instant Client needs to be installed.
//!
//! The driver is synchronous; each call runs in `spawn_blocking`. The row
//! limit is applied by tuning `prefetch_rows`/`fetch_array_size` and
//! stopping the cursor read, without fetching the rest from the server.

use std::collections::HashMap;
use std::fmt::Write as _;
use std::sync::Arc;
use std::time::Instant;

use oracledb::{
    Connection, JsonValue, Metadata, OracleNumber, OracleTimestamp, Row, Vector, VectorData,
};
use parking_lot::Mutex;

use super::{first_keywords, tns, TxState};
use crate::error::{AppError, AppResult, ErrorKind};
use crate::model::{
    ApplyRequest, ApplyResult, CellValue, ColumnInfo, ConnectionProfile, DbColumn, DbObject,
    EditColumn, EditTarget, ExecRequest, ExecResult, OracleConnectType, OraclePrivilege, RowChange,
    ValueKind,
};

const BINARY_PREVIEW_BYTES: usize = 32 * 1024;
const MAX_OUTPUT_LINES: usize = 5000;

pub struct OraSession {
    conn: Mutex<Arc<Connection>>,
    /// Opens a new connection with the same settings (see [`OraSession::reopen`]).
    dial: Arc<dyn Fn() -> AppResult<Connection> + Send + Sync>,
    busy: tokio::sync::Mutex<()>,
    state: Arc<Mutex<TxState>>,
    /// Catalog lookups for editable results, per table as written.
    tables: TableCache,
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
        OracleConnectType::Tns => profile.tns_alias.trim().to_string(),
        OracleConnectType::Custom => profile.connect_string.trim().to_string(),
    }
}

/// Connect string and network configuration folder for the driver. TNS
/// aliases are resolved to their descriptor here (see [`super::tns`]); the
/// folder still goes to the driver for `sqlnet.ora` settings.
fn network_target(profile: &ConnectionProfile) -> AppResult<(String, Option<String>)> {
    let own_dir = profile
        .config_dir
        .as_deref()
        .filter(|d| !d.trim().is_empty());
    match profile.oracle_connect_type {
        OracleConnectType::Tns => {
            let (descriptor, dir) = tns::resolve(&profile.tns_alias, own_dir)?;
            Ok((descriptor, Some(dir.display().to_string())))
        }
        OracleConnectType::Custom => {
            let target = connect_string(profile);
            let dir = own_dir
                .map(str::to_string)
                .or_else(|| tns::default_dir().map(|d| d.display().to_string()));
            // A bare name is an alias: resolve it here so Latin-1 files work.
            let is_alias = !target.is_empty() && !target.contains(['(', '/', ':', '@', ' ']);
            if is_alias {
                if let Ok((descriptor, found)) = tns::resolve(&target, own_dir) {
                    return Ok((descriptor, Some(found.display().to_string())));
                }
            }
            Ok((target, dir))
        }
        OracleConnectType::Service | OracleConnectType::Sid => {
            Ok((connect_string(profile), own_dir.map(str::to_string)))
        }
    }
}

/// Opens a connection and prepares the session (module name, DBMS_OUTPUT).
fn dial(profile: &ConnectionProfile, password: &str) -> AppResult<Connection> {
    let (target, config_dir) = network_target(profile)?;
    let mut cfg = oracledb::Config::default();
    if let Some(dir) = &config_dir {
        cfg = cfg.set_config_dir(dir);
    }
    cfg = cfg
        .set_credentials(profile.user.trim(), password)
        .set_program("LowQuery")
        .map_err(connection_error)?
        .set_connect_string(&target)
        .map_err(connection_error)?;
    if profile.oracle_privilege != OraclePrivilege::None {
        cfg = cfg.set_auth_mode(profile.oracle_privilege.auth_mode());
    }
    let conn = oracledb::connect(cfg).map_err(connection_error)?;
    let _ = conn.set_module("LowQuery");
    // Enables DBMS_OUTPUT with no buffer limit.
    conn.execute("begin dbms_output.enable(null); end;", &[])
        .map_err(map_error)?;
    Ok(conn)
}

pub async fn connect(profile: &ConnectionProfile, password: Option<&str>) -> AppResult<OraSession> {
    let profile = profile.clone();
    let password = password.unwrap_or_default().to_string();
    blocking(move || {
        let conn = dial(&profile, &password)?;
        let row = conn
            .query_row(
                "select sys_context('USERENV', 'SESSION_USER'), sys_context('USERENV', 'CURRENT_SCHEMA') from dual",
                &[],
            )
            .map_err(map_error)?;

        let version = conn.version().map(|v| v.to_string()).unwrap_or_default();
        Ok(OraSession {
            server_version: format!("Oracle {version}"),
            current_user: row.get(0).map_err(map_error)?,
            current_schema: row.get(1).map_err(map_error)?,
            conn: Mutex::new(Arc::new(conn)),
            dial: Arc::new(move || dial(&profile, &password)),
            busy: tokio::sync::Mutex::new(()),
            state: Arc::new(Mutex::new(TxState { autocommit: true, in_transaction: false })),
            tables: TableCache::default(),
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
    /// Runs `f` on the connection in a blocking task. When the driver loses
    /// track of the server's reply, the connection is replaced.
    async fn with_conn<T, F>(&self, f: F) -> AppResult<T>
    where
        T: Send + 'static,
        F: FnOnce(&Connection) -> AppResult<T> + Send + 'static,
    {
        let conn = self.conn.lock().clone();
        match blocking(move || f(&conn)).await {
            Err(e) if e.kind == ErrorKind::Internal => Err(self.reopen(e).await),
            result => result,
        }
    }

    /// After a protocol error the rest of the reply is still unread on the
    /// socket and every later call would fail on it, so the session moves to
    /// a new connection. The open transaction is lost with the old one.
    async fn reopen(&self, cause: AppError) -> AppError {
        let was_in_transaction = std::mem::take(&mut self.state.lock().in_transaction);
        self.tables.lock().clear();
        let dial = self.dial.clone();
        let mut err = match blocking(move || dial()).await {
            Ok(conn) => {
                *self.conn.lock() = Arc::new(conn);
                let mut message = String::from(
                    "The Oracle driver could not read the server's reply, so LowQuery reopened the connection.",
                );
                if was_in_transaction {
                    message.push_str(" The open transaction was rolled back.");
                }
                AppError::new(ErrorKind::Database, message)
            }
            Err(e) => AppError::connection(format!(
                "The Oracle driver could not read the server's reply, and reconnecting failed: {}",
                e.message
            )),
        };
        err.detail = Some(cause.message);
        err
    }

    pub async fn apply_changes(&self, req: &ApplyRequest) -> AppResult<ApplyResult> {
        let _busy = self.busy.lock().await;
        let state = self.state.clone();
        let req = req.clone();
        self.with_conn(move |conn| apply_blocking(conn, &req, &state))
            .await
    }

    pub fn tx_state(&self) -> TxState {
        *self.state.lock()
    }

    pub async fn execute(&self, req: &ExecRequest) -> AppResult<ExecResult> {
        let _busy = self.busy.lock().await;
        let state = self.state.clone();
        let tables = self.tables.clone();
        let sql = prepare_sql(&req.sql);
        let max_rows = req.max_rows;
        self.with_conn(move |conn| execute_blocking(conn, &sql, max_rows, &state, &tables))
            .await
    }

    pub async fn cancel(&self) -> AppResult<()> {
        Err(AppError::invalid(
            "The Oracle driver cannot interrupt a running call yet.",
        ))
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
        self.with_conn(move |conn| {
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
        // DATA_DEFAULT is a LONG; the VARCHAR2 copy and IDENTITY_COLUMN
        // arrived in 12.2, so older servers list the columns without defaults.
        macro_rules! columns_sql {
            ($default:literal) => {
                concat!(
                    "select c.column_name,
                        case
                          when c.data_type in ('VARCHAR2','NVARCHAR2','CHAR','NCHAR','RAW')
                            then c.data_type || '(' || case when c.char_length > 0 then c.char_length else c.data_length end || ')'
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
                        then 'Y' else 'N' end, ",
                    $default,
                    " from all_tab_columns c
                 where c.owner = :1 and c.table_name = :2
                 order by c.column_id"
                )
            };
        }
        let params = vec![schema.to_string(), table.to_string()];
        let rows = match self
            .query_rows(
                columns_sql!(
                    "case when c.identity_column = 'YES' then null else trim(c.data_default_vc) end"
                ),
                params.clone(),
            )
            .await
        {
            Ok(rows) => rows,
            Err(e) if e.kind == ErrorKind::Database => {
                self.query_rows(columns_sql!("null"), params).await?
            }
            Err(e) => return Err(e),
        };
        Ok(rows
            .into_iter()
            .map(|r| DbColumn {
                name: r[0].clone(),
                type_name: r[1].clone(),
                nullable: r[2] == "Y",
                primary_key: r[3] == "Y",
                default_value: Some(r[4].clone()).filter(|d| !d.is_empty()),
            })
            .collect())
    }

    /// DDL from `DBMS_METADATA`, with a table's indexes (other than those
    /// behind its keys), triggers and comments; PL/SQL units from their source.
    pub async fn object_ddl(&self, schema: &str, name: &str, kind: &str) -> AppResult<String> {
        let object_type = match kind {
            "table" => "TABLE",
            "view" => "VIEW",
            "matview" => "MATERIALIZED_VIEW",
            "sequence" => "SEQUENCE",
            "function" => "FUNCTION",
            "procedure" => "PROCEDURE",
            "package" => "PACKAGE",
            "type" => "TYPE",
            "synonym" => "SYNONYM",
            _ => {
                return Err(AppError::invalid(format!(
                    "LowQuery cannot script a {kind} on Oracle."
                )))
            }
        };
        let _busy = self.busy.lock().await;
        if matches!(kind, "function" | "procedure" | "package" | "type") {
            // PL/SQL units come from their stored source, as SQL Developer
            // shows them, spec and body together.
            let params = [
                schema.to_string(),
                name.to_string(),
                object_type.to_string(),
            ];
            return self
                .with_conn(move |conn| {
                    let mut out = String::new();
                    for unit in [params[2].clone(), format!("{} BODY", params[2])] {
                        let lines = first_texts(
                            conn,
                            "select text from all_source
                             where owner = :1 and name = :2 and type = :3
                             order by line",
                            &[params[0].clone(), params[1].clone(), unit],
                        )?;
                        if lines.is_empty() {
                            continue;
                        }
                        let source = lines.concat();
                        out.push_str(&format!("CREATE OR REPLACE {}\n/\n\n", source.trim_end()));
                    }
                    Ok(out)
                })
                .await;
        }
        let params = vec![
            object_type.to_string(),
            name.to_string(),
            schema.to_string(),
        ];
        let table = kind == "table";
        self.with_conn(move |conn| {
            conn.execute(
                "begin
                   dbms_metadata.set_transform_param(dbms_metadata.session_transform, 'SQLTERMINATOR', true);
                   dbms_metadata.set_transform_param(dbms_metadata.session_transform, 'PRETTY', true);
                   dbms_metadata.set_transform_param(dbms_metadata.session_transform, 'STORAGE', false);
                 end;",
                &[],
            )
            .map_err(map_error)?;
            let outcome = (|| {
                let mut parts = first_texts(
                    conn,
                    "select dbms_metadata.get_ddl(:1, :2, :3) from dual",
                    &params,
                )?;
                if table {
                    let owner_table = [params[2].clone(), params[1].clone()];
                    for sql in [
                        "select dbms_metadata.get_ddl('INDEX', i.index_name, i.owner)
                         from all_indexes i
                         where i.table_owner = :1 and i.table_name = :2 and i.index_type <> 'LOB'
                           and not exists (
                             select 1 from all_constraints k
                             where k.owner = i.table_owner and k.table_name = i.table_name
                               and k.index_name = i.index_name and k.constraint_type in ('P', 'U'))
                         order by i.index_name",
                        "select dbms_metadata.get_ddl('TRIGGER', t.trigger_name, t.owner)
                         from all_triggers t
                         where t.table_owner = :1 and t.table_name = :2
                         order by t.trigger_name",
                    ] {
                        parts.extend(first_texts(conn, sql, &owner_table)?);
                    }
                    // ORA-31608 when the table has no comments.
                    if let Ok(comments) = first_texts(
                        conn,
                        "select dbms_metadata.get_dependent_ddl('COMMENT', :1, :2) from dual",
                        &[params[1].clone(), params[2].clone()],
                    ) {
                        parts.extend(comments);
                    }
                }
                Ok(parts
                    .iter()
                    .map(|p| p.trim())
                    .filter(|p| !p.is_empty())
                    .collect::<Vec<_>>()
                    .join("\n\n")
                    + "\n")
            })();
            let _ = conn.execute(
                "begin dbms_metadata.set_transform_param(dbms_metadata.session_transform, 'DEFAULT'); end;",
                &[],
            );
            outcome
        })
        .await
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
        self.with_conn(move |conn| {
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

/// First column of every row, as text. The statement stays out of the
/// driver's cache: on 23ai, running a cached DBMS_METADATA query a third time
/// in a session fails with ORA-01002 (fetch out of sequence).
fn first_texts(conn: &Connection, sql: &str, params: &[String]) -> AppResult<Vec<String>> {
    let binds: Vec<&dyn oracledb::ToDbValue> = params
        .iter()
        .map(|p| p as &dyn oracledb::ToDbValue)
        .collect();
    let cursor = conn
        .statement(sql)
        .map_err(map_error)?
        .exclude_from_cache()
        .build()
        .map_err(map_error)?
        .query(&binds)
        .map_err(map_error)?;
    let mut out = Vec::new();
    for row in cursor {
        let row = row.map_err(map_error)?;
        out.push(
            row.get::<Option<String>>(0)
                .map_err(map_error)?
                .unwrap_or_default(),
        );
    }
    Ok(out)
}

fn execute_blocking(
    conn: &Connection,
    sql: &str,
    max_rows: usize,
    state: &Mutex<TxState>,
    tables: &TableCache,
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
        match edit_target(conn, sql, &cols, tables) {
            Ok(target) => result.editable = Some(target),
            Err(reason) => result.read_only_reason = Some(reason),
        }
    } else {
        let is_dml = stmt.is_dml();
        let is_plsql = stmt.is_plsql();
        let is_ddl = stmt.is_ddl();
        if !is_dml {
            // DDL, ALTER SESSION (current schema) or PL/SQL may change what a
            // table name resolves to.
            tables.lock().clear();
        }
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
    let local;
    let t = if with_tz {
        local = local_time(t);
        &local
    } else {
        t
    };
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

/// The driver returns TIMESTAMP WITH TIME ZONE values as UTC date and time
/// plus the offset; this moves them to the local time of that offset.
fn local_time(t: &OracleTimestamp) -> OracleTimestamp {
    use chrono::{Datelike, Timelike};
    let (h, m) = (t.tz_hour_offset(), t.tz_minute_offset());
    let minutes = i64::from(h) * 60 + i64::from(m);
    let shifted =
        chrono::NaiveDate::from_ymd_opt(t.year().into(), t.month().into(), t.day().into())
            .and_then(|d| d.and_hms_opt(t.hour().into(), t.minute().into(), t.second().into()))
            .and_then(|dt| dt.checked_add_signed(chrono::Duration::minutes(minutes)));
    match shifted {
        Some(dt) => OracleTimestamp::new_timestamp_tz(
            dt.year() as i16,
            dt.month() as u8,
            dt.day() as u8,
            dt.hour() as u8,
            dt.minute() as u8,
            dt.second() as u8,
            t.nanoseconds(),
            h,
            m,
        ),
        None => *t,
    }
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

// ---------------------------------------------------------------------------
// Editable results
//
// The thin protocol does not say which table a result column comes from, so
// the statement text is checked conservatively: one SELECT over exactly one
// table, with a select list of `*`, `alias.*` and plain columns. The table is
// resolved and its key read from the data dictionary in one query, cached
// per table name as written until DDL, ALTER SESSION or PL/SQL runs.
// ---------------------------------------------------------------------------

const RO_TABLE: &str = "Read-only: only rows from a table can be edited.";
const RO_JOIN: &str = "Read-only: the result combines columns from several tables.";
const RO_GROUPED: &str = "Read-only: grouped or distinct rows cannot be edited.";
const RO_SET: &str = "Read-only: the result combines several queries.";
const RO_COMPLEX: &str = "Read-only: the query is too complex to map its rows to a table.";
const RO_UNKNOWN: &str = "Read-only: the table behind this result could not be identified.";
const RO_REMOTE: &str = "Read-only: tables over database links cannot be edited.";

type TableCache = Arc<Mutex<HashMap<(Option<String>, String), Arc<Lookup>>>>;

enum Lookup {
    Table(TableInfo),
    ReadOnly(String),
}

struct TableInfo {
    owner: String,
    name: String,
    /// Visible columns in `select *` order.
    columns: Vec<TableColumn>,
    key: Vec<String>,
    primary: bool,
}

struct TableColumn {
    name: String,
    type_name: String,
    nullable: bool,
    generated: bool,
}

#[derive(Debug, Clone, PartialEq)]
enum Tok {
    /// Unquoted word, uppercased.
    Word(String),
    /// Quoted identifier, as written.
    Quoted(String),
    Str,
    Num,
    Punct(char),
}

/// Tokens of a statement, without comments. `None` when a literal, quoted
/// identifier or comment is not closed.
fn lex(sql: &str) -> Option<Vec<Tok>> {
    let c: Vec<char> = sql.chars().collect();
    let n = c.len();
    let mut out = Vec::new();
    let mut i = 0;
    while i < n {
        let ch = c[i];
        let next = c.get(i + 1).copied();
        if ch.is_whitespace() {
            i += 1;
        } else if ch == '-' && next == Some('-') {
            while i < n && c[i] != '\n' {
                i += 1;
            }
        } else if ch == '/' && next == Some('*') {
            i += 2;
            while i + 1 < n && !(c[i] == '*' && c[i + 1] == '/') {
                i += 1;
            }
            if i + 1 >= n {
                return None;
            }
            i += 2;
        } else if ch == '\'' {
            i = skip_string(&c, i)?;
            out.push(Tok::Str);
        } else if matches!(ch, 'q' | 'Q') && next == Some('\'') {
            i = skip_q_string(&c, i + 1)?;
            out.push(Tok::Str);
        } else if matches!(ch, 'n' | 'N') && next == Some('\'') {
            i = skip_string(&c, i + 1)?;
            out.push(Tok::Str);
        } else if matches!(ch, 'n' | 'N')
            && matches!(next, Some('q' | 'Q'))
            && c.get(i + 2) == Some(&'\'')
        {
            i = skip_q_string(&c, i + 2)?;
            out.push(Tok::Str);
        } else if ch == '"' {
            let start = i + 1;
            let end = start + c[start..].iter().position(|&x| x == '"')?;
            if end == start {
                return None;
            }
            out.push(Tok::Quoted(c[start..end].iter().collect()));
            i = end + 1;
        } else if ch.is_ascii_digit() || (ch == '.' && next.is_some_and(|d| d.is_ascii_digit())) {
            while i < n && (c[i].is_ascii_digit() || c[i] == '.') {
                i += 1;
            }
            if i < n && matches!(c[i], 'e' | 'E') {
                let sign = usize::from(matches!(c.get(i + 1), Some('+' | '-')));
                if c.get(i + 1 + sign).is_some_and(char::is_ascii_digit) {
                    i += 1 + sign;
                    while i < n && c[i].is_ascii_digit() {
                        i += 1;
                    }
                }
            }
            if i < n && matches!(c[i], 'f' | 'F' | 'd' | 'D') {
                i += 1;
            }
            out.push(Tok::Num);
        } else if ch.is_alphabetic() {
            let start = i;
            while i < n && (c[i].is_alphanumeric() || matches!(c[i], '_' | '$' | '#')) {
                i += 1;
            }
            let word: String = c[start..i].iter().collect();
            out.push(Tok::Word(word.to_uppercase()));
        } else {
            out.push(Tok::Punct(ch));
            i += 1;
        }
    }
    Some(out)
}

/// End of a `'...'` literal that starts at `quote`.
fn skip_string(c: &[char], quote: usize) -> Option<usize> {
    let mut i = quote + 1;
    loop {
        match c.get(i)? {
            '\'' if c.get(i + 1) == Some(&'\'') => i += 2,
            '\'' => return Some(i + 1),
            _ => i += 1,
        }
    }
}

/// End of a `q'[...]'` literal whose quote is at `quote`.
fn skip_q_string(c: &[char], quote: usize) -> Option<usize> {
    let open = *c.get(quote + 1)?;
    let close = match open {
        '[' => ']',
        '(' => ')',
        '{' => '}',
        '<' => '>',
        x => x,
    };
    let mut i = quote + 2;
    loop {
        if *c.get(i)? == close && c.get(i + 1) == Some(&'\'') {
            return Some(i + 2);
        }
        i += 1;
    }
}

#[derive(Debug, PartialEq)]
enum SelItem {
    /// `*` (no qualifier) or `q.*`.
    Star(Vec<String>),
    Column {
        qual: Vec<String>,
        name: String,
        alias: Option<String>,
    },
    Expr,
}

#[derive(Debug, PartialEq)]
struct SelectShape {
    schema: Option<String>,
    table: String,
    alias: Option<String>,
    items: Vec<SelItem>,
}

impl SelectShape {
    fn qualifier_matches(&self, qual: &[String]) -> bool {
        match (qual, &self.alias, &self.schema) {
            ([], _, _) => true,
            ([q], Some(alias), _) => q == alias,
            ([q], None, _) => *q == self.table,
            ([s, q], None, Some(schema)) => s == schema && *q == self.table,
            _ => false,
        }
    }
}

fn name_tok(t: Option<&Tok>) -> Option<String> {
    match t? {
        Tok::Word(w) | Tok::Quoted(w) => Some(w.clone()),
        _ => None,
    }
}

fn word_at(toks: &[Tok], i: usize) -> Option<&str> {
    match toks.get(i)? {
        Tok::Word(w) => Some(w),
        _ => None,
    }
}

/// Words that end the table reference instead of naming its alias.
const NOT_ALIAS: &[&str] = &[
    "WHERE",
    "ORDER",
    "FETCH",
    "OFFSET",
    "FOR",
    "GROUP",
    "HAVING",
    "CONNECT",
    "START",
    "UNION",
    "INTERSECT",
    "MINUS",
    "EXCEPT",
    "JOIN",
    "INNER",
    "LEFT",
    "RIGHT",
    "FULL",
    "CROSS",
    "NATURAL",
    "OUTER",
    "PIVOT",
    "UNPIVOT",
    "MODEL",
    "SAMPLE",
    "PARTITION",
    "SUBPARTITION",
    "AS",
    "VERSIONS",
    "MATCH_RECOGNIZE",
    "WINDOW",
    "LIMIT",
    "QUALIFY",
    "APPLY",
    "LATERAL",
    "WITH",
];

/// Checks that the statement reads rows of one table and nothing else.
fn parse_select(sql: &str) -> Result<SelectShape, &'static str> {
    let toks = lex(sql).ok_or(RO_COMPLEX)?;
    match word_at(&toks, 0) {
        Some("SELECT") => {}
        _ => return Err(RO_TABLE),
    }
    let mut i = 1;
    match word_at(&toks, i) {
        Some("DISTINCT" | "UNIQUE") => return Err(RO_GROUPED),
        Some("ALL") => i += 1,
        _ => {}
    }

    // Select list, up to FROM.
    let mut items = Vec::new();
    let mut depth = 0usize;
    let mut start = i;
    loop {
        match toks.get(i).ok_or(RO_TABLE)? {
            Tok::Punct('(') => depth += 1,
            Tok::Punct(')') => depth = depth.checked_sub(1).ok_or(RO_COMPLEX)?,
            Tok::Punct(',') if depth == 0 => {
                items.push(parse_item(&toks[start..i]));
                start = i + 1;
            }
            Tok::Word(w) if depth == 0 && w == "FROM" => {
                items.push(parse_item(&toks[start..i]));
                break;
            }
            _ => {}
        }
        i += 1;
    }

    // FROM [schema.]table [alias]
    i += 1;
    if toks.get(i) == Some(&Tok::Punct('(')) {
        return Err(RO_TABLE);
    }
    let mut parts = vec![name_tok(toks.get(i)).ok_or(RO_COMPLEX)?];
    i += 1;
    while toks.get(i) == Some(&Tok::Punct('.')) {
        parts.push(name_tok(toks.get(i + 1)).ok_or(RO_COMPLEX)?);
        i += 2;
    }
    if parts.len() > 2 {
        return Err(RO_COMPLEX);
    }
    match toks.get(i) {
        // Table functions, `only (...)`, `lateral (...)`.
        Some(Tok::Punct('(')) => return Err(RO_TABLE),
        Some(Tok::Punct('@')) => return Err(RO_REMOTE),
        _ => {}
    }
    let mut alias = None;
    match toks.get(i) {
        Some(Tok::Quoted(a)) => {
            alias = Some(a.clone());
            i += 1;
        }
        Some(Tok::Word(w)) if !NOT_ALIAS.contains(&w.as_str()) => {
            alias = Some(w.clone());
            i += 1;
        }
        _ => {}
    }
    match toks.get(i) {
        None => {}
        Some(Tok::Word(w)) => match w.as_str() {
            "WHERE" | "ORDER" | "FETCH" | "OFFSET" | "FOR" => {}
            "JOIN" | "INNER" | "LEFT" | "RIGHT" | "FULL" | "CROSS" | "NATURAL" | "OUTER"
            | "APPLY" => return Err(RO_JOIN),
            "GROUP" | "HAVING" => return Err(RO_GROUPED),
            "UNION" | "INTERSECT" | "MINUS" | "EXCEPT" => return Err(RO_SET),
            _ => return Err(RO_COMPLEX),
        },
        Some(Tok::Punct(',')) => return Err(RO_JOIN),
        Some(_) => return Err(RO_COMPLEX),
    }

    // The remaining clauses must not group, combine or walk the rows.
    let rest = &toks[i..];
    let mut depth = 0usize;
    for (j, t) in rest.iter().enumerate() {
        match t {
            Tok::Punct('(') => depth += 1,
            Tok::Punct(')') => depth = depth.saturating_sub(1),
            Tok::Word(w) if depth == 0 => match (w.as_str(), word_at(rest, j + 1)) {
                ("GROUP", Some("BY")) | ("HAVING", _) => return Err(RO_GROUPED),
                ("UNION" | "INTERSECT" | "MINUS" | "EXCEPT", _) => return Err(RO_SET),
                ("CONNECT", Some("BY"))
                | ("START", Some("WITH"))
                | ("DIMENSION", Some("BY"))
                | ("ORDER", Some("SIBLINGS")) => return Err(RO_COMPLEX),
                _ => {}
            },
            _ => {}
        }
    }

    let table = parts.pop().unwrap_or_default();
    Ok(SelectShape {
        schema: parts.pop(),
        table,
        alias,
        items,
    })
}

/// One select-list item: `*`, `q.*`, a plain column with an optional alias,
/// or anything else.
fn parse_item(t: &[Tok]) -> SelItem {
    if t == [Tok::Punct('*')] {
        return SelItem::Star(Vec::new());
    }
    let mut parts = Vec::new();
    let mut i = 0;
    loop {
        match t.get(i) {
            Some(Tok::Word(w) | Tok::Quoted(w)) => parts.push(w.clone()),
            Some(Tok::Punct('*')) if !parts.is_empty() && i + 1 == t.len() => {
                return SelItem::Star(parts)
            }
            _ => return SelItem::Expr,
        }
        i += 1;
        if t.get(i) == Some(&Tok::Punct('.')) {
            i += 1;
        } else {
            break;
        }
    }
    if parts.len() > 3 {
        return SelItem::Expr;
    }
    let alias = match &t[i..] {
        [] => None,
        [Tok::Word(as_kw), a] if as_kw == "AS" => match name_tok(Some(a)) {
            Some(a) => Some(a),
            None => return SelItem::Expr,
        },
        [a] => match name_tok(Some(a)) {
            Some(a) => Some(a),
            None => return SelItem::Expr,
        },
        _ => return SelItem::Expr,
    };
    let name = parts.pop().unwrap_or_default();
    SelItem::Column {
        qual: parts,
        name,
        alias,
    }
}

/// Works out whether the rows of a query can be edited, and how.
fn edit_target(
    conn: &Connection,
    sql: &str,
    cols: &[Metadata],
    tables: &TableCache,
) -> Result<EditTarget, String> {
    let shape = parse_select(sql).map_err(str::to_string)?;
    if shape.table == "DUAL" && shape.schema.as_deref().is_none_or(|s| s == "SYS") {
        return Err(RO_TABLE.into());
    }
    let cache_key = (shape.schema.clone(), shape.table.clone());
    let cached = tables.lock().get(&cache_key).cloned();
    let lookup = match cached {
        Some(l) => l,
        None => {
            let l = Arc::new(
                lookup_table(conn, shape.schema.as_deref(), &shape.table)
                    .map_err(|_| RO_UNKNOWN.to_string())?,
            );
            tables.lock().insert(cache_key, l.clone());
            l
        }
    };
    let info = match &*lookup {
        Lookup::Table(info) => info,
        Lookup::ReadOnly(reason) => return Err(reason.clone()),
    };
    let names: Vec<&str> = cols.iter().map(Metadata::name).collect();
    map_columns(&shape, info, &names)
}

/// Maps result columns (by their names in the result) to table columns and
/// finds the key in the result.
fn map_columns(
    shape: &SelectShape,
    info: &TableInfo,
    result_names: &[&str],
) -> Result<EditTarget, String> {
    let mismatch = || {
        format!(
            "Read-only: the result columns could not be matched to table {}.",
            info.name
        )
    };
    // Table column and the name the result should give it.
    let mut mapped: Vec<Option<(&TableColumn, String)>> = Vec::with_capacity(result_names.len());
    for item in &shape.items {
        match item {
            SelItem::Star(qual) => {
                if !shape.qualifier_matches(qual) {
                    return Err(mismatch());
                }
                mapped.extend(info.columns.iter().map(|c| Some((c, c.name.clone()))));
            }
            SelItem::Column { qual, name, alias } => {
                let col = shape
                    .qualifier_matches(qual)
                    .then(|| info.columns.iter().find(|c| c.name == *name))
                    .flatten();
                mapped.push(col.map(|c| (c, alias.clone().unwrap_or_else(|| c.name.clone()))));
            }
            SelItem::Expr => mapped.push(None),
        }
    }
    if mapped.len() != result_names.len() {
        return Err(mismatch());
    }
    let mut columns = Vec::with_capacity(mapped.len());
    for (m, result_name) in mapped.iter().zip(result_names) {
        columns.push(match m {
            Some((c, expected)) => {
                if result_name != expected {
                    return Err(mismatch());
                }
                (!c.generated && col_type(&c.type_name).is_some()).then(|| EditColumn {
                    name: c.name.clone(),
                    type_name: c.type_name.clone(),
                })
            }
            None => None,
        });
    }

    let mut key = Vec::with_capacity(info.key.len());
    for k in &info.key {
        let found = columns
            .iter()
            .position(|c| c.as_ref().is_some_and(|c| c.name == *k));
        match found {
            Some(i) => key.push(i),
            None if mapped.iter().flatten().any(|(c, _)| c.name == *k) => {
                return Err(format!(
                    "Read-only: key column {k} has a type that cannot be edited here."
                ))
            }
            None => {
                let kind = if info.primary { "primary key" } else { "key" };
                return Err(format!(
                    "Read-only: add the {kind} columns ({}) to the query to edit rows.",
                    info.key.join(", ")
                ));
            }
        }
    }
    Ok(EditTarget {
        schema: Some(info.owner.clone()),
        table: info.name.clone(),
        columns,
        key,
    })
}

/// Resolves a table name the way Oracle does (current schema, then public
/// synonyms; one level of synonym) and reads its columns and keys, in one
/// round trip.
const LOOKUP_SQL: &str = "
with named as (
  select o.owner, o.object_name, o.object_type
    from all_objects o
   where o.owner = nvl(:1, sys_context('USERENV', 'CURRENT_SCHEMA'))
     and o.object_name = :2
     and o.subobject_name is null
     and o.object_type in ('TABLE', 'VIEW', 'SYNONYM', 'MATERIALIZED VIEW')
  union all
  select s.owner, s.synonym_name, 'SYNONYM'
    from all_synonyms s
   where :3 is null and s.owner = 'PUBLIC' and s.synonym_name = :4
     and not exists (
       select 1 from all_objects o
        where o.owner = sys_context('USERENV', 'CURRENT_SCHEMA')
          and o.object_name = :5
          and o.subobject_name is null
          and o.object_type in ('TABLE', 'VIEW', 'SYNONYM', 'MATERIALIZED VIEW'))
),
resolved as (
  select n.object_type kind, n.owner, n.object_name name
    from named n
   where n.object_type <> 'SYNONYM'
  union all
  select case when s.db_link is not null then 'REMOTE'
              else nvl(o.object_type, 'MISSING') end,
         s.table_owner, s.table_name
    from named n
    join all_synonyms s on s.owner = n.owner and s.synonym_name = n.object_name
    left join all_objects o on o.owner = s.table_owner and o.object_name = s.table_name
         and o.subobject_name is null
         and o.object_type in ('TABLE', 'VIEW', 'SYNONYM', 'MATERIALIZED VIEW')
   where n.object_type = 'SYNONYM'
)
select 'O', r.kind, r.owner, r.name, cast(null as varchar2(3)), cast(null as varchar2(40))
  from resolved r
union all
select 'C', c.column_name,
       case
         when c.data_type in ('VARCHAR2', 'NVARCHAR2', 'CHAR', 'NCHAR', 'RAW')
           then c.data_type || '(' ||
                case when c.char_length > 0 then c.char_length else c.data_length end || ')'
         when c.data_type = 'NUMBER' and c.data_precision is not null
           then 'NUMBER(' || c.data_precision ||
                case when c.data_scale <> 0 then ',' || c.data_scale end || ')'
         when c.data_type = 'FLOAT' and c.data_precision is not null
           then 'FLOAT(' || c.data_precision || ')'
         else c.data_type end,
       c.nullable, c.virtual_column, to_char(c.column_id)
  from resolved r
  join all_tab_cols c on c.owner = r.owner and c.table_name = r.name
 where r.kind = 'TABLE' and c.hidden_column = 'NO' and c.column_id is not null
union all
select 'K', k.constraint_type, k.constraint_name, kc.column_name,
       cast(null as varchar2(3)), to_char(kc.position)
  from resolved r
  join all_constraints k on k.owner = r.owner and k.table_name = r.name
  join all_cons_columns kc on kc.owner = k.owner
       and kc.constraint_name = k.constraint_name and kc.table_name = k.table_name
 where r.kind = 'TABLE' and k.constraint_type in ('P', 'U') and k.status = 'ENABLED'";

fn lookup_table(
    conn: &Connection,
    schema: Option<&str>,
    name: &str,
) -> Result<Lookup, oracledb::Error> {
    let schema = schema.map(str::to_string);
    let name = name.to_string();
    let schema_bind: &dyn oracledb::ToDbValue = match &schema {
        Some(s) => s,
        None => &oracledb::DB_TYPE_VARCHAR,
    };
    let cursor = conn.query(LOOKUP_SQL, &[schema_bind, &name, schema_bind, &name, &name])?;
    let mut rows = Vec::new();
    for row in cursor {
        let row = row?;
        let mut cells = Vec::with_capacity(6);
        for i in 0..6 {
            cells.push(row.get::<Option<String>>(i)?.unwrap_or_default());
        }
        rows.push(cells);
    }
    Ok(table_info(&name, rows))
}

/// Builds the table description from the rows of [`LOOKUP_SQL`].
fn table_info(written: &str, rows: Vec<Vec<String>>) -> Lookup {
    let mut kinds = Vec::new();
    let mut target = None;
    let mut columns: Vec<(u32, TableColumn)> = Vec::new();
    // (type, name) -> [(position, column)], ordered by constraint name.
    let mut constraints: std::collections::BTreeMap<(String, String), Vec<(u32, String)>> =
        Default::default();
    for r in rows {
        match r[0].as_str() {
            "O" => {
                if r[1] == "TABLE" {
                    target = Some((r[2].clone(), r[3].clone()));
                }
                kinds.push(r[1].clone());
            }
            "C" => columns.push((
                r[5].parse().unwrap_or(u32::MAX),
                TableColumn {
                    name: r[1].clone(),
                    type_name: r[2].clone(),
                    nullable: r[3] != "N",
                    generated: r[4] == "YES",
                },
            )),
            "K" => constraints
                .entry((r[1].clone(), r[2].clone()))
                .or_default()
                .push((r[5].parse().unwrap_or(u32::MAX), r[3].clone())),
            _ => {}
        }
    }
    let has = |k: &str| kinds.iter().any(|x| x == k);
    if has("MATERIALIZED VIEW") {
        return Lookup::ReadOnly(format!("Read-only: {written} is a materialized view."));
    }
    if has("REMOTE") {
        return Lookup::ReadOnly(RO_REMOTE.into());
    }
    if has("VIEW") {
        return Lookup::ReadOnly(format!(
            "Read-only: {written} is a view; only tables can be edited."
        ));
    }
    let Some((owner, name)) = target else {
        return Lookup::ReadOnly(RO_TABLE.into());
    };
    columns.sort_by_key(|(id, _)| *id);
    let columns: Vec<TableColumn> = columns.into_iter().map(|(_, c)| c).collect();

    // The primary key, else the narrowest unique key on NOT NULL columns.
    let mut keys: Vec<(bool, Vec<String>)> = constraints
        .into_iter()
        .map(|((kind, _), mut cols)| {
            cols.sort_by_key(|(pos, _)| *pos);
            (kind == "P", cols.into_iter().map(|(_, c)| c).collect())
        })
        .filter(|(primary, cols): &(bool, Vec<String>)| {
            *primary
                || cols
                    .iter()
                    .all(|k| columns.iter().any(|c| c.name == *k && !c.nullable))
        })
        .collect();
    keys.sort_by_key(|(primary, cols)| (!*primary, cols.len()));
    let Some((primary, key)) = keys.into_iter().next() else {
        return Lookup::ReadOnly(format!("Read-only: table {name} has no primary key."));
    };
    Lookup::Table(TableInfo {
        owner,
        name,
        columns,
        key,
        primary,
    })
}

// ---------------------------------------------------------------------------
// Applying grid changes
// ---------------------------------------------------------------------------

/// Largest string or RAW value bound as one value (the PL/SQL limit).
const MAX_BIND_BYTES: usize = 32767;
/// Digits an Oracle NUMBER mantissa can hold.
const NUMBER_DIGITS: usize = 40;

/// How a column's text is converted and bound.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ColType {
    Number,
    BinaryFloat,
    BinaryDouble,
    Date,
    Timestamp,
    TimestampTz,
    Text,
    Binary,
    Bool,
}

/// Types the grid can write. TIMESTAMP WITH LOCAL TIME ZONE, intervals,
/// LONG, JSON, objects and the like stay read-only.
fn col_type(type_name: &str) -> Option<ColType> {
    let t = type_name.trim().to_ascii_uppercase();
    if t.starts_with("TIMESTAMP") {
        return if t.contains("LOCAL") {
            None
        } else if t.contains("TIME ZONE") {
            Some(ColType::TimestampTz)
        } else {
            Some(ColType::Timestamp)
        };
    }
    let base = t.split(['(', ' ']).next().unwrap_or_default();
    Some(match base {
        "NUMBER" | "FLOAT" | "INTEGER" | "DECIMAL" => ColType::Number,
        "BINARY_FLOAT" => ColType::BinaryFloat,
        "BINARY_DOUBLE" => ColType::BinaryDouble,
        "DATE" => ColType::Date,
        "VARCHAR2" | "NVARCHAR2" | "VARCHAR" | "CHAR" | "NCHAR" | "CLOB" | "NCLOB" => ColType::Text,
        "RAW" | "BLOB" => ColType::Binary,
        "BOOLEAN" => ColType::Bool,
        _ => return None,
    })
}

enum Bind {
    Text(String),
    Number(OracleNumber),
    Float(f32),
    Double(f64),
    Time(OracleTimestamp),
    Raw(Vec<u8>),
    Bool(bool),
}

impl Bind {
    fn value(&self) -> &dyn oracledb::ToDbValue {
        match self {
            Bind::Text(v) => v,
            Bind::Number(v) => v,
            Bind::Float(v) => v,
            Bind::Double(v) => v,
            Bind::Time(v) => v,
            Bind::Raw(v) => v,
            Bind::Bool(v) => v,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Op {
    Update,
    Delete,
    Insert,
}

struct Planned {
    /// 1-based index of the change.
    row: usize,
    op: Op,
    sql: String,
    binds: Vec<Bind>,
}

/// A number from its text: exact digits for an `OracleNumber` bind, or
/// `<digits>E<exponent>` for `to_number`, which has no decimal or group
/// character and so does not depend on NLS settings.
enum NumberText {
    Exact(OracleNumber),
    Scientific(String),
}

/// Parses `[-+]digits[.digits]`. Integers ending in zeros go through
/// `to_number`: the driver encodes their trailing zero digits instead of
/// dropping them, which is not Oracle's canonical NUMBER form.
fn parse_number(text: &str) -> Option<NumberText> {
    let s = text.trim();
    let (negative, body) = match s.strip_prefix('-') {
        Some(b) => (true, b),
        None => (false, s.strip_prefix('+').unwrap_or(s)),
    };
    let (int, frac) = body.split_once('.').unwrap_or((body, ""));
    if (int.is_empty() && frac.is_empty())
        || !int.bytes().all(|b| b.is_ascii_digit())
        || !frac.bytes().all(|b| b.is_ascii_digit())
    {
        return None;
    }
    let int = int.trim_start_matches('0');
    let frac = frac.trim_end_matches('0');
    let sign = if negative { "-" } else { "" };
    if frac.is_empty() && int.is_empty() {
        return "0".parse().ok().map(NumberText::Exact);
    }
    if frac.is_empty() && int.ends_with('0') {
        let mantissa = int.trim_end_matches('0');
        if mantissa.len() > NUMBER_DIGITS {
            return None;
        }
        let exponent = int.len() - mantissa.len();
        return Some(NumberText::Scientific(format!(
            "{sign}{mantissa}E{exponent}"
        )));
    }
    if int.len() + frac.len() > NUMBER_DIGITS {
        // Leading zeros of the fraction do not count as digits.
        let digits = if int.is_empty() {
            frac.trim_start_matches('0').len()
        } else {
            int.len() + frac.len()
        };
        if digits > NUMBER_DIGITS {
            return None;
        }
    }
    let int = if int.is_empty() { "0" } else { int };
    let text = if frac.is_empty() {
        format!("{sign}{int}")
    } else {
        format!("{sign}{int}.{frac}")
    };
    text.parse().ok().map(NumberText::Exact)
}

/// Parses the format [`timestamp`] prints:
/// `YYYY-MM-DD[ HH:MI:SS[.fffffffff]][ +HH:MM]` (`T` may separate date and
/// time). Returns the offset as `+HH:MM`.
fn parse_timestamp(text: &str) -> Option<(OracleTimestamp, Option<String>)> {
    fn num(s: &str) -> Option<u32> {
        if s.is_empty() || !s.bytes().all(|b| b.is_ascii_digit()) {
            return None;
        }
        s.parse().ok()
    }
    let s = text.trim();
    let date = s.get(..10)?;
    if date.as_bytes()[4] != b'-' || date.as_bytes()[7] != b'-' {
        return None;
    }
    let year = num(&date[..4])?;
    let month = num(&date[5..7])?;
    let day = num(&date[8..10])?;
    if year == 0 {
        return None;
    }
    chrono::NaiveDate::from_ymd_opt(year as i32, month, day)?;

    let mut rest = &s[10..];
    let (mut hour, mut minute, mut second, mut nanos) = (0, 0, 0, 0);
    let b = rest.as_bytes();
    if b.len() >= 9 && matches!(b[0], b' ' | b'T') && b[3] == b':' && b[6] == b':' {
        hour = num(rest.get(1..3)?)?;
        minute = num(rest.get(4..6)?)?;
        second = num(rest.get(7..9)?)?;
        if hour > 23 || minute > 59 || second > 59 {
            return None;
        }
        rest = rest.get(9..)?;
        if let Some(f) = rest.strip_prefix('.') {
            let digits = f.bytes().take_while(u8::is_ascii_digit).count();
            if digits == 0 || digits > 9 {
                return None;
            }
            nanos = num(&f[..digits])? * 10u32.pow((9 - digits) as u32);
            rest = &f[digits..];
        }
    }
    let rest = rest.trim_start();
    let tz = if rest.is_empty() {
        None
    } else {
        let b = rest.as_bytes();
        if b.len() != 6 || !matches!(b[0], b'+' | b'-') || b[3] != b':' {
            return None;
        }
        let hh = num(rest.get(1..3)?)?;
        let mm = num(rest.get(4..6)?)?;
        if hh > 14 || mm > 59 {
            return None;
        }
        Some(rest.to_string())
    };
    let t = OracleTimestamp::new_timestamp(
        year as i16,
        month as u8,
        day as u8,
        hour as u8,
        minute as u8,
        second as u8,
        nanos,
    );
    Some((t, tz))
}

fn parse_hex(text: &str) -> Option<Vec<u8>> {
    let s = text.trim();
    if !s.len().is_multiple_of(2) {
        return None;
    }
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(s.get(i..i + 2)?, 16).ok())
        .collect()
}

fn parse_bool(text: &str) -> Option<bool> {
    match text.trim().to_ascii_uppercase().as_str() {
        "TRUE" | "1" => Some(true),
        "FALSE" | "0" => Some(false),
        _ => None,
    }
}

/// Quoted identifier. Oracle names cannot contain double quotes.
fn quote_ident(name: &str) -> AppResult<String> {
    if name.is_empty() || name.contains(['"', '\0']) {
        return Err(AppError::invalid(format!("Invalid name: {name}")));
    }
    Ok(format!("\"{name}\""))
}

fn too_long(name: &str) -> AppError {
    AppError::invalid(format!(
        "Column {name}: values longer than {MAX_BIND_BYTES} bytes cannot be saved from the grid."
    ))
}

/// Binds of one statement, numbered in order of appearance.
#[derive(Default)]
struct SqlBuilder {
    binds: Vec<Bind>,
}

impl SqlBuilder {
    fn bind(&mut self, b: Bind) -> String {
        self.binds.push(b);
        format!(":{}", self.binds.len())
    }

    /// SQL expression for a value of `col`: a typed bind, wrapped where the
    /// bind type alone does not say enough.
    fn value(&mut self, col: &EditColumn, text: &str) -> AppResult<String> {
        let name = &col.name;
        let ty = col_type(&col.type_name)
            .ok_or_else(|| AppError::invalid(format!("Column {name} cannot be edited here.")))?;
        let bad = |what: &str| {
            AppError::invalid(format!("Column {name}: '{text}' is not a valid {what}."))
        };
        Ok(match ty {
            ColType::Number => match parse_number(text).ok_or_else(|| bad("number"))? {
                NumberText::Exact(n) => self.bind(Bind::Number(n)),
                NumberText::Scientific(s) => format!("to_number({})", self.bind(Bind::Text(s))),
            },
            ColType::BinaryFloat => {
                let v = text.trim().parse::<f32>().map_err(|_| bad("number"))?;
                self.bind(Bind::Float(v))
            }
            ColType::BinaryDouble => {
                let v = text.trim().parse::<f64>().map_err(|_| bad("number"))?;
                self.bind(Bind::Double(v))
            }
            ColType::Date => match parse_timestamp(text) {
                Some((t, None)) if t.nanoseconds() == 0 => {
                    format!("cast({} as date)", self.bind(Bind::Time(t)))
                }
                _ => return Err(bad("date (YYYY-MM-DD HH:MI:SS)")),
            },
            ColType::Timestamp => match parse_timestamp(text) {
                Some((t, None)) => self.bind(Bind::Time(t)),
                _ => return Err(bad("timestamp (YYYY-MM-DD HH:MI:SS.FF)")),
            },
            ColType::TimestampTz => match parse_timestamp(text) {
                Some((t, Some(tz))) => {
                    let ts = self.bind(Bind::Time(t));
                    let tz = self.bind(Bind::Text(tz));
                    format!("from_tz({ts}, {tz})")
                }
                _ => {
                    return Err(bad(
                        "timestamp with time zone (YYYY-MM-DD HH:MI:SS.FF +HH:MM)",
                    ))
                }
            },
            ColType::Text => {
                if text.len() > MAX_BIND_BYTES {
                    return Err(too_long(name));
                }
                self.bind(Bind::Text(text.to_string()))
            }
            ColType::Binary => {
                if text.contains("… (") {
                    return Err(AppError::invalid(
                        "This value is only a preview and cannot be saved.",
                    ));
                }
                let bytes = parse_hex(text).ok_or_else(|| bad("hexadecimal value"))?;
                if bytes.len() > MAX_BIND_BYTES {
                    return Err(too_long(name));
                }
                self.bind(Bind::Raw(bytes))
            }
            ColType::Bool => {
                let v = parse_bool(text).ok_or_else(|| bad("boolean (TRUE or FALSE)"))?;
                self.bind(Bind::Bool(v))
            }
        })
    }

    fn value_or_null(&mut self, col: &EditColumn, value: &Option<String>) -> AppResult<String> {
        match value {
            Some(text) => self.value(col, text),
            None => Ok("null".into()),
        }
    }

    /// `k1 = :n and k2 is null ...`
    fn key_filter(
        &mut self,
        key_cols: &[&EditColumn],
        key: &[Option<String>],
    ) -> AppResult<String> {
        if key.len() != key_cols.len() {
            return Err(AppError::invalid("The row key does not match the result."));
        }
        let mut parts = Vec::with_capacity(key.len());
        for (col, value) in key_cols.iter().zip(key) {
            let ident = quote_ident(&col.name)?;
            parts.push(match value {
                Some(text) => format!("{ident} = {}", self.value(col, text)?),
                None => format!("{ident} is null"),
            });
        }
        Ok(parts.join(" and "))
    }
}

fn edit_column(target: &EditTarget, index: usize) -> AppResult<&EditColumn> {
    target
        .columns
        .get(index)
        .and_then(Option::as_ref)
        .ok_or_else(|| AppError::invalid(format!("Result column {} cannot be edited.", index + 1)))
}

/// Values by table column; a column given twice keeps the last value.
fn change_values<'a>(
    target: &'a EditTarget,
    values: &'a [CellValue],
) -> AppResult<Vec<(&'a EditColumn, &'a Option<String>)>> {
    let mut out: Vec<(&EditColumn, &Option<String>)> = Vec::with_capacity(values.len());
    for v in values {
        let col = edit_column(target, v.column)?;
        match out.iter_mut().find(|(c, _)| c.name == col.name) {
            Some(slot) => slot.1 = &v.value,
            None => out.push((col, &v.value)),
        }
    }
    Ok(out)
}

/// Builds every statement before anything runs, so bad input changes nothing.
fn plan_changes(req: &ApplyRequest) -> AppResult<Vec<Planned>> {
    let target = &req.target;
    let table = match &target.schema {
        Some(schema) => format!("{}.{}", quote_ident(schema)?, quote_ident(&target.table)?),
        None => quote_ident(&target.table)?,
    };
    let key_cols = target
        .key
        .iter()
        .map(|&i| edit_column(target, i))
        .collect::<AppResult<Vec<_>>>()?;
    let mut planned = Vec::with_capacity(req.changes.len());
    for (n, change) in req.changes.iter().enumerate() {
        let mut b = SqlBuilder::default();
        let (op, sql) = match change {
            RowChange::Update { key, values } => {
                let values = change_values(target, values)?;
                if values.is_empty() {
                    continue;
                }
                let mut sets = Vec::with_capacity(values.len());
                for (col, value) in values {
                    let expr = b.value_or_null(col, value)?;
                    sets.push(format!("{} = {expr}", quote_ident(&col.name)?));
                }
                let filter = b.key_filter(&key_cols, key)?;
                let sql = format!("update {table} set {} where {filter}", sets.join(", "));
                (Op::Update, sql)
            }
            RowChange::Delete { key } => {
                let filter = b.key_filter(&key_cols, key)?;
                (Op::Delete, format!("delete from {table} where {filter}"))
            }
            RowChange::Insert { values } => {
                let values = change_values(target, values)?;
                let sql = if values.is_empty() {
                    // Oracle has no DEFAULT VALUES: name one column as DEFAULT.
                    let col =
                        target.columns.iter().flatten().next().ok_or_else(|| {
                            AppError::invalid("This result has no editable columns.")
                        })?;
                    let ident = quote_ident(&col.name)?;
                    format!("insert into {table} ({ident}) values (default)")
                } else {
                    let mut names = Vec::with_capacity(values.len());
                    let mut exprs = Vec::with_capacity(values.len());
                    for (col, value) in values {
                        names.push(quote_ident(&col.name)?);
                        exprs.push(b.value_or_null(col, value)?);
                    }
                    format!(
                        "insert into {table} ({}) values ({})",
                        names.join(", "),
                        exprs.join(", ")
                    )
                };
                (Op::Insert, sql)
            }
        };
        planned.push(Planned {
            row: n + 1,
            op,
            sql,
            binds: b.binds,
        });
    }
    Ok(planned)
}

fn not_saved(row: usize, matched: u64) -> AppError {
    let message = if matched == 0 {
        format!(
            "Row {row} was not saved: 0 rows matched its key (changed or removed by someone else)."
        )
    } else {
        format!("Row {row} was not saved: {matched} rows matched its key.")
    };
    AppError::new(ErrorKind::Database, message)
}

fn run_changes(conn: &Connection, planned: &[Planned], out: &mut ApplyResult) -> AppResult<()> {
    for p in planned {
        let binds: Vec<&dyn oracledb::ToDbValue> = p.binds.iter().map(Bind::value).collect();
        let affected = conn
            .execute(&p.sql, &binds)
            .map_err(map_error)?
            .rows_affected();
        if affected != 1 {
            return Err(not_saved(p.row, affected));
        }
        match p.op {
            Op::Update => out.updated += 1,
            Op::Delete => out.deleted += 1,
            Op::Insert => out.inserted += 1,
        }
    }
    Ok(())
}

fn apply_blocking(
    conn: &Connection,
    req: &ApplyRequest,
    state: &Mutex<TxState>,
) -> AppResult<ApplyResult> {
    let started = Instant::now();
    let planned = plan_changes(req)?;
    let autocommit = state.lock().autocommit;
    if !autocommit {
        // On failure only these changes are undone; earlier work of the open
        // transaction stays.
        conn.execute("savepoint lq_grid_apply", &[])
            .map_err(map_error)?;
    }
    let mut out = ApplyResult::default();
    if let Err(e) = run_changes(conn, &planned, &mut out) {
        if autocommit {
            let _ = conn.rollback();
        } else {
            let _ = conn.execute("rollback to savepoint lq_grid_apply", &[]);
        }
        return Err(e);
    }
    if autocommit {
        if let Err(e) = conn.commit() {
            let _ = conn.rollback();
            return Err(map_error(e));
        }
        state.lock().in_transaction = false;
    } else if !planned.is_empty() {
        state.lock().in_transaction = true;
    }
    out.elapsed_ms = started.elapsed().as_millis() as u64;
    out.in_transaction = state.lock().in_transaction;
    Ok(out)
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
        // The driver lost its place in the server's reply; the session
        // replaces the connection (see `OraSession::reopen`).
        K::UnknownTtcMessageType(..)
        | K::UnknownServerSidePiggyback(_)
        | K::OutOfData
        | K::UnexpectedResult
        | K::UnexpectedError
        | K::UnableToRecover => AppError::new(ErrorKind::Internal, e.to_string()),
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

    // --- Editable results: pure helpers ---

    fn col(name: &str, type_name: &str) -> TableColumn {
        TableColumn {
            name: name.into(),
            type_name: type_name.into(),
            nullable: true,
            generated: false,
        }
    }

    fn edit_col(name: &str, type_name: &str) -> Option<EditColumn> {
        Some(EditColumn {
            name: name.into(),
            type_name: type_name.into(),
        })
    }

    #[test]
    fn parses_single_table_selects() {
        let s = parse_select(
            "select /*+ first_rows */ id, t.name, qty * 2 as dbl, q'[it's, from]' lit -- x\n from lq t where x = 'a, from' order by 1",
        )
        .unwrap();
        assert_eq!(s.schema, None);
        assert_eq!(s.table, "LQ");
        assert_eq!(s.alias.as_deref(), Some("T"));
        assert_eq!(
            s.items,
            [
                SelItem::Column {
                    qual: vec![],
                    name: "ID".into(),
                    alias: None
                },
                SelItem::Column {
                    qual: vec!["T".into()],
                    name: "NAME".into(),
                    alias: None
                },
                SelItem::Expr,
                SelItem::Expr,
            ]
        );

        let s = parse_select(r#"select m.*, "Some Col" as "x" from app."My Tab" m for update"#)
            .unwrap();
        assert_eq!(s.schema.as_deref(), Some("APP"));
        assert_eq!(s.table, "My Tab");
        assert_eq!(
            s.items,
            [
                SelItem::Star(vec!["M".into()]),
                SelItem::Column {
                    qual: vec![],
                    name: "Some Col".into(),
                    alias: Some("x".into())
                },
            ]
        );

        // A subquery in WHERE does not change where the rows come from.
        assert!(parse_select(
            "select * from t where id in (select id from u group by id) fetch first 5 rows only"
        )
        .is_ok());
    }

    #[test]
    fn rejects_other_query_shapes() {
        let cases = [
            ("select a.id from a join b on a.id = b.id", RO_JOIN),
            ("select * from a, b", RO_JOIN),
            ("select * from a left outer join b using (id)", RO_JOIN),
            ("select * from (select 1 x from dual)", RO_TABLE),
            ("select * from table(f())", RO_TABLE),
            ("with q as (select 1 from dual) select * from q", RO_TABLE),
            ("select distinct x from t", RO_GROUPED),
            ("select x, count(*) from t group by x", RO_GROUPED),
            ("select x from t having count(*) > 1", RO_GROUPED),
            ("select x from t union all select x from u", RO_SET),
            ("select x from t where a = 1 minus select x from u", RO_SET),
            ("select level from t connect by level < 3", RO_COMPLEX),
            ("select * from t as of scn 1", RO_COMPLEX),
            ("select * from t sample (10)", RO_COMPLEX),
            ("select * from t pivot (sum(v) for k in (1))", RO_COMPLEX),
            ("select * from t@remote", RO_REMOTE),
            ("update t set x = 1", RO_TABLE),
            ("select 'unclosed from t", RO_COMPLEX),
        ];
        for (sql, reason) in cases {
            assert_eq!(parse_select(sql), Err(reason), "{sql}");
        }
    }

    fn sample_table() -> TableInfo {
        let mut gen = col("TOTAL", "NUMBER");
        gen.generated = true;
        TableInfo {
            owner: "APP".into(),
            name: "ORDERS".into(),
            columns: vec![
                col("ID", "NUMBER(10)"),
                col("NAME", "VARCHAR2(50)"),
                gen,
                col("SEEN", "TIMESTAMP(6) WITH LOCAL TIME ZONE"),
            ],
            key: vec!["ID".into()],
            primary: true,
        }
    }

    #[test]
    fn maps_result_columns_to_the_table() {
        let info = sample_table();
        let shape = parse_select("select name, o.id, id + 1, total from orders o").unwrap();
        let t = map_columns(&shape, &info, &["NAME", "ID", "ID+1", "TOTAL"]).unwrap();
        assert_eq!(t.schema.as_deref(), Some("APP"));
        assert_eq!(t.table, "ORDERS");
        assert_eq!(t.key, [1]);
        assert_eq!(t.columns[0].as_ref().unwrap().name, "NAME");
        assert_eq!(t.columns[1].as_ref().unwrap().type_name, "NUMBER(10)");
        // Expression, virtual column.
        assert!(t.columns[2].is_none());
        assert!(t.columns[3].is_none());

        let shape = parse_select("select * from orders").unwrap();
        let t = map_columns(&shape, &info, &["ID", "NAME", "TOTAL", "SEEN"]).unwrap();
        assert_eq!(t.key, [0]);
        // TIMESTAMP WITH LOCAL TIME ZONE stays read-only.
        assert!(t.columns[3].is_none());

        // The result must line up with what the text says.
        assert!(map_columns(&shape, &info, &["ID", "NAME"]).is_err());
        let shape = parse_select("select id x from orders").unwrap();
        assert!(map_columns(&shape, &info, &["ID"]).is_err());
        assert!(map_columns(&shape, &info, &["X"]).is_ok());

        let shape = parse_select("select name from orders").unwrap();
        let reason = map_columns(&shape, &info, &["NAME"]).unwrap_err();
        assert_eq!(
            reason,
            "Read-only: add the primary key columns (ID) to the query to edit rows."
        );

        // A qualifier that is not the table is not one of its columns.
        let shape = parse_select("select s.id, id from orders").unwrap();
        let t = map_columns(&shape, &info, &["ID", "ID"]).unwrap();
        assert!(t.columns[0].is_none());
        assert_eq!(t.key, [1]);
    }

    fn row(cells: &[&str]) -> Vec<String> {
        cells.iter().map(|c| c.to_string()).collect()
    }

    #[test]
    fn chooses_the_table_key() {
        let rows = vec![
            row(&["O", "TABLE", "APP", "T", "", ""]),
            row(&["C", "B", "VARCHAR2(10)", "N", "NO", "2"]),
            row(&["C", "A", "NUMBER", "Y", "NO", "1"]),
            row(&["C", "C", "NUMBER", "N", "NO", "3"]),
            row(&["K", "U", "T_A", "A", "", "1"]),
            row(&["K", "U", "T_BC", "C", "", "2"]),
            row(&["K", "U", "T_BC", "B", "", "1"]),
        ];
        let Lookup::Table(info) = table_info("T", rows.clone()) else {
            panic!("not a table")
        };
        assert_eq!(
            info.columns
                .iter()
                .map(|c| c.name.as_str())
                .collect::<Vec<_>>(),
            ["A", "B", "C"]
        );
        // T_A is on a nullable column.
        assert_eq!(info.key, ["B", "C"]);
        assert!(!info.primary);

        let mut with_pk = rows.clone();
        with_pk.push(row(&["K", "P", "T_PK", "A", "", "1"]));
        let Lookup::Table(info) = table_info("T", with_pk) else {
            panic!("not a table")
        };
        assert_eq!(info.key, ["A"]);
        assert!(info.primary);

        let Lookup::ReadOnly(reason) = table_info("T", rows[..4].to_vec()) else {
            panic!("editable without a key")
        };
        assert_eq!(reason, "Read-only: table T has no primary key.");

        let Lookup::ReadOnly(reason) =
            table_info("V", vec![row(&["O", "VIEW", "APP", "V", "", ""])])
        else {
            panic!("view")
        };
        assert!(reason.contains("view"), "{reason}");
        assert!(matches!(table_info("X", vec![]), Lookup::ReadOnly(_)));
    }

    fn number(text: &str) -> String {
        match parse_number(text).unwrap_or_else(|| panic!("{text}")) {
            NumberText::Exact(n) => n.to_string(),
            NumberText::Scientific(s) => format!("sci {s}"),
        }
    }

    #[test]
    fn converts_numbers_without_nls() {
        assert_eq!(number("123.4500"), "123.45");
        assert_eq!(number("-0.001"), "-0.001");
        assert_eq!(number(".5"), "0.5");
        assert_eq!(number("+7"), "7");
        assert_eq!(number("0"), "0");
        assert_eq!(number("-0.0"), "0");
        assert_eq!(number("15"), "15");
        assert_eq!(
            number("12345678901234567890.123456789"),
            "12345678901234567890.123456789"
        );
        assert_eq!(number("100"), "sci 1E2");
        assert_eq!(number("-1000"), "sci -1E3");
        assert_eq!(number(&format!("1{}", "0".repeat(125))), "sci 1E125");
        assert_eq!(
            number(&format!("0.{}1", "0".repeat(120))),
            format!("0.{}1", "0".repeat(120))
        );
        for bad in ["", "-", "abc", "1e5", "1,5", "1.2.3", " ", &"9".repeat(41)] {
            assert!(parse_number(bad).is_none(), "{bad}");
        }
    }

    #[test]
    fn parses_displayed_timestamps() {
        let (t, tz) = parse_timestamp("2024-05-01 13:45:07").unwrap();
        assert_eq!(
            (
                t.year(),
                t.month(),
                t.day(),
                t.hour(),
                t.minute(),
                t.second()
            ),
            (2024, 5, 1, 13, 45, 7)
        );
        assert_eq!((t.nanoseconds(), tz), (0, None));
        let (t, _) = parse_timestamp("2024-05-01 13:45:07.123").unwrap();
        assert_eq!(t.nanoseconds(), 123_000_000);
        let (t, tz) = parse_timestamp("2024-05-01 13:45:07.123456789 -06:00").unwrap();
        assert_eq!(t.nanoseconds(), 123_456_789);
        assert_eq!(tz.as_deref(), Some("-06:00"));
        let (t, _) = parse_timestamp("2024-05-01").unwrap();
        assert_eq!((t.hour(), t.second()), (0, 0));
        assert!(parse_timestamp("2024-05-01T13:45:07").is_some());
        for bad in [
            "2024-02-30 00:00:00",
            "2024-05-01 24:00:00",
            "2024-05-01 13:45",
            "2024-05-01 13:45:07.",
            "2024-05-01 13:45:07 +3",
            "01/05/2024",
            "",
        ] {
            assert!(parse_timestamp(bad).is_none(), "{bad}");
        }

        // What `timestamp` prints parses back to the same value.
        let t = OracleTimestamp::new_timestamp(1999, 12, 31, 23, 59, 58, 120_000_000);
        let shown = timestamp(&t, 6, false);
        assert_eq!(shown, "1999-12-31 23:59:58.120000");
        assert_eq!(parse_timestamp(&shown).unwrap().0, t);
    }

    #[test]
    fn shows_time_zone_values_in_local_time() {
        // The driver hands over UTC plus the offset.
        let utc = OracleTimestamp::new_timestamp_tz(2025, 10, 31, 17, 52, 15, 456_000_000, -6, 0);
        assert_eq!(timestamp(&utc, 3, true), "2025-10-31 11:52:15.456 -06:00");
        let utc = OracleTimestamp::new_timestamp_tz(2024, 1, 1, 2, 0, 0, 0, -3, -30);
        assert_eq!(timestamp(&utc, 0, true), "2023-12-31 22:30:00 -03:30");
        let utc = OracleTimestamp::new_timestamp_tz(2024, 2, 28, 20, 0, 0, 0, 5, 30);
        assert_eq!(timestamp(&utc, 0, true), "2024-02-29 01:30:00 +05:30");
    }

    #[test]
    fn parses_binary_and_bool() {
        assert_eq!(parse_hex("DEADbeef"), Some(vec![0xDE, 0xAD, 0xBE, 0xEF]));
        assert_eq!(parse_hex(""), Some(vec![]));
        assert_eq!(parse_hex("ABC"), None);
        assert_eq!(parse_hex("ZZ"), None);
        assert_eq!(parse_bool("true"), Some(true));
        assert_eq!(parse_bool("FALSE"), Some(false));
        assert_eq!(parse_bool("yes"), None);
    }

    fn sample_target() -> EditTarget {
        EditTarget {
            schema: Some("APP".into()),
            table: "My Tab".into(),
            columns: vec![
                edit_col("ID", "NUMBER(10)"),
                edit_col("Some Value", "VARCHAR2(20)"),
                None,
                edit_col("D", "DATE"),
                edit_col("TZ", "TIMESTAMP(6) WITH TIME ZONE"),
                edit_col("BIN", "BLOB"),
                edit_col("F", "BINARY_DOUBLE"),
            ],
            key: vec![0],
        }
    }

    fn cv(column: usize, value: Option<&str>) -> CellValue {
        CellValue {
            column,
            value: value.map(str::to_string),
        }
    }

    fn plan(changes: Vec<RowChange>) -> AppResult<Vec<Planned>> {
        plan_changes(&ApplyRequest {
            target: sample_target(),
            changes,
        })
    }

    #[test]
    fn builds_statements_with_binds() {
        let p = plan(vec![
            RowChange::Update {
                key: vec![Some("10".into())],
                values: vec![cv(1, Some("x")), cv(3, None), cv(1, Some("it's"))],
            },
            RowChange::Delete { key: vec![None] },
            RowChange::Insert { values: vec![] },
            RowChange::Insert {
                values: vec![
                    cv(3, Some("2024-02-29 13:00:00")),
                    cv(4, Some("2024-02-29 13:00:00.5 +03:00")),
                    cv(6, Some("Inf")),
                ],
            },
            RowChange::Update {
                key: vec![Some("7".into())],
                values: vec![],
            },
        ])
        .unwrap();
        assert_eq!(p.len(), 4);
        assert_eq!(
            p[0].sql,
            r#"update "APP"."My Tab" set "Some Value" = :1, "D" = null where "ID" = to_number(:2)"#
        );
        assert!(matches!(&p[0].binds[0], Bind::Text(s) if s == "it's"));
        assert!(matches!(&p[0].binds[1], Bind::Text(s) if s == "1E1"));
        assert_eq!(p[1].sql, r#"delete from "APP"."My Tab" where "ID" is null"#);
        assert_eq!(p[1].op, Op::Delete);
        assert_eq!(
            p[2].sql,
            r#"insert into "APP"."My Tab" ("ID") values (default)"#
        );
        assert_eq!(
            p[3].sql,
            r#"insert into "APP"."My Tab" ("D", "TZ", "F") values (cast(:1 as date), from_tz(:2, :3), :4)"#
        );
        assert!(matches!(&p[3].binds[2], Bind::Text(s) if s == "+03:00"));
        assert!(matches!(&p[3].binds[3], Bind::Double(f) if f.is_infinite()));
        assert_eq!(p[3].row, 4);
    }

    #[test]
    fn rejects_bad_changes() {
        let invalid = |changes: Vec<RowChange>| {
            let err = plan(changes).err().expect("should fail");
            assert_eq!(err.kind, ErrorKind::Invalid, "{err}");
            err.message
        };
        // Expression column.
        invalid(vec![RowChange::Insert {
            values: vec![cv(2, Some("1"))],
        }]);
        invalid(vec![RowChange::Delete {
            key: vec![Some("abc".into())],
        }]);
        invalid(vec![RowChange::Delete { key: vec![] }]);
        invalid(vec![RowChange::Insert {
            values: vec![cv(3, Some("2024-01-01 10:00:00.5"))],
        }]);
        invalid(vec![RowChange::Insert {
            values: vec![cv(4, Some("2024-01-01 10:00:00"))],
        }]);
        let msg = invalid(vec![RowChange::Insert {
            values: vec![cv(5, Some("DEADBEEF… (40000 bytes)"))],
        }]);
        assert_eq!(msg, "This value is only a preview and cannot be saved.");
        let mut target = sample_target();
        target.table = "bad\"name".into();
        assert!(plan_changes(&ApplyRequest {
            target,
            changes: vec![RowChange::Delete {
                key: vec![Some("1".into())]
            }],
        })
        .is_err());
    }

    // --- Editable results against a real server (see integration_tests.rs) ---

    fn ora_profile() -> Option<(ConnectionProfile, Option<String>)> {
        let env = |n: &str| std::env::var(n).ok().filter(|v| !v.is_empty());
        Some((
            ConnectionProfile {
                name: "test".into(),
                engine: crate::model::Engine::Oracle,
                host: env("LQ_ORA_HOST")?,
                port: env("LQ_ORA_PORT")
                    .and_then(|p| p.parse().ok())
                    .unwrap_or(1521),
                user: env("LQ_ORA_USER")?,
                database: env("LQ_ORA_SERVICE").unwrap_or_else(|| "FREEPDB1".into()),
                oracle_connect_type: OracleConnectType::Service,
                ..Default::default()
            },
            env("LQ_ORA_PASSWORD"),
        ))
    }

    async fn open() -> Option<OraSession> {
        let (profile, pw) = ora_profile()?;
        Some(connect(&profile, pw.as_deref()).await.expect("connection"))
    }

    async fn run(s: &OraSession, sql: &str) -> ExecResult {
        s.execute(&ExecRequest {
            sql: sql.into(),
            max_rows: 100,
        })
        .await
        .unwrap_or_else(|e| panic!("{sql}: {e}"))
    }

    async fn try_run(s: &OraSession, sql: &str) -> bool {
        s.execute(&ExecRequest {
            sql: sql.into(),
            max_rows: 0,
        })
        .await
        .is_ok()
    }

    fn target_of(r: &ExecResult) -> EditTarget {
        r.editable
            .clone()
            .unwrap_or_else(|| panic!("read-only: {:?}", r.read_only_reason))
    }

    fn reason_of(r: &ExecResult) -> String {
        assert!(r.editable.is_none(), "editable: {:?}", r.editable);
        r.read_only_reason.clone().expect("a reason")
    }

    async fn apply(
        s: &OraSession,
        target: &EditTarget,
        changes: Vec<RowChange>,
    ) -> AppResult<ApplyResult> {
        s.apply_changes(&ApplyRequest {
            target: target.clone(),
            changes,
        })
        .await
    }

    #[tokio::test]
    #[ignore]
    async fn oracle_edit_detection() {
        let Some(s) = open().await else { return };
        for t in ["lq_edit_t", "lq_edit_nokey", "lq_edit_uk"] {
            try_run(&s, &format!("drop table {t} purge")).await;
        }
        run(&s, "create table lq_edit_t (id number(10) primary key, name varchar2(50), qty number(10,2))").await;
        run(&s, "create table lq_edit_nokey (a number, b varchar2(10))").await;
        run(&s, "create table lq_edit_uk (code varchar2(10) not null, v number, constraint lq_edit_uk_u unique (code))").await;

        let r = run(&s, "select id, name, qty * 2 as dbl from lq_edit_t").await;
        let t = target_of(&r);
        assert_eq!(t.table, "LQ_EDIT_T");
        assert_eq!(t.schema, s.current_schema);
        assert_eq!(t.key, [0]);
        assert_eq!(t.columns[1].as_ref().unwrap().name, "NAME");
        assert!(t.columns[2].is_none());

        let r = run(
            &s,
            "select name, e.id from lq_edit_t e where id > 0 order by id",
        )
        .await;
        assert_eq!(target_of(&r).key, [1]);
        let r = run(&s, "select * from lq_edit_t").await;
        let t = target_of(&r);
        assert_eq!(t.key, [0]);
        assert_eq!(t.columns[2].as_ref().unwrap().type_name, "NUMBER(10,2)");

        let r = run(
            &s,
            "select a.id, b.name from lq_edit_t a join lq_edit_t b on a.id = b.id",
        )
        .await;
        assert!(reason_of(&r).contains("several tables"));
        let r = run(&s, "select * from lq_edit_nokey").await;
        assert!(
            reason_of(&r).contains("no primary key"),
            "{:?}",
            r.read_only_reason
        );
        let r = run(&s, "select name from lq_edit_t").await;
        assert!(reason_of(&r).contains("(ID)"), "{:?}", r.read_only_reason);
        let r = run(&s, "select v, code from lq_edit_uk").await;
        assert_eq!(target_of(&r).key, [1]);
        let r = run(&s, "select name, count(*) from lq_edit_t group by name").await;
        reason_of(&r);
        let r = run(&s, "select sysdate from dual").await;
        reason_of(&r);
        let r = run(&s, "insert into lq_edit_t (id) values (1)").await;
        assert!(r.editable.is_none() && r.read_only_reason.is_none());

        // Synonyms and views, when the test user may create them.
        if try_run(&s, "create or replace synonym lq_edit_syn for lq_edit_t").await {
            let r = run(&s, "select * from lq_edit_syn").await;
            assert_eq!(target_of(&r).table, "LQ_EDIT_T");
            try_run(&s, "drop synonym lq_edit_syn").await;
        }
        if try_run(
            &s,
            "create or replace view lq_edit_v as select * from lq_edit_t",
        )
        .await
        {
            let r = run(&s, "select * from lq_edit_v").await;
            assert!(reason_of(&r).contains("view"));
            try_run(&s, "drop view lq_edit_v").await;
        }

        for t in ["lq_edit_t", "lq_edit_nokey", "lq_edit_uk"] {
            run(&s, &format!("drop table {t} purge")).await;
        }
    }

    #[tokio::test]
    #[ignore]
    async fn oracle_edit_round_trip_types() {
        let Some(s) = open().await else { return };
        try_run(&s, "drop table lq_edit_types purge").await;
        run(
            &s,
            "create table lq_edit_types (
               id number(10) primary key, n number(20,5), big number, i number(10),
               f binary_float, d binary_double, dt date, ts timestamp(6), ts9 timestamp(9),
               tz timestamp(3) with time zone, c varchar2(100), ch char(5), nc nvarchar2(20),
               cl clob, r raw(16), bl blob)",
        )
        .await;
        run(
            &s,
            "insert into lq_edit_types (id, n, big, i, f, d, dt, ts, ts9, tz, c, ch, nc, cl, r, bl)
             values (10, 12345.678, 1e30, 1000, 1.5, 0.1, timestamp '2024-02-29 13:45:10',
                     timestamp '2024-02-29 13:45:10.123456', timestamp '2024-02-29 13:45:10.123456789',
                     timestamp '2024-02-29 13:45:10.250 -03:00', 'it''s' || chr(10) || 'two \"lines\"',
                     'ab', 'olá', to_clob('long text' || chr(10) || 'x'), hextoraw('DEADBEEF'),
                     hextoraw('00FF10'))",
        )
        .await;
        run(&s, "insert into lq_edit_types (id) values (20)").await;
        run(
            &s,
            "insert into lq_edit_types (id, n, f) values (7, -0.5, -2.25)",
        )
        .await;

        let sql = "select * from lq_edit_types order by id";
        let before = run(&s, sql).await;
        let t = target_of(&before);
        assert_eq!(t.key, [0]);
        assert!(t.columns.iter().all(Option::is_some), "{:?}", t.columns);
        let row10 = &before.rows[1];
        assert_eq!(row10[0].as_deref(), Some("10"));
        assert_eq!(
            row10[2].as_deref(),
            Some(format!("1{}", "0".repeat(30)).as_str())
        );
        assert_eq!(row10[6].as_deref(), Some("2024-02-29 13:45:10"));
        assert_eq!(row10[9].as_deref(), Some("2024-02-29 13:45:10.250 -03:00"));
        assert_eq!(row10[11].as_deref(), Some("ab   "));

        // Every column written back with its own displayed text.
        let changes = before
            .rows
            .iter()
            .map(|row| RowChange::Update {
                key: vec![row[0].clone()],
                values: (1..row.len()).map(|i| cv(i, row[i].as_deref())).collect(),
            })
            .collect();
        let res = apply(&s, &t, changes).await.unwrap();
        assert_eq!(res.updated, 3);
        assert!(!res.in_transaction);
        let after = run(&s, sql).await;
        assert_eq!(after.rows, before.rows);

        // New values in the display format.
        let res = apply(
            &s,
            &t,
            vec![RowChange::Update {
                key: vec![Some("10".into())],
                values: vec![
                    cv(1, Some("-0.00001")),
                    cv(4, Some("Inf")),
                    cv(5, Some("-1.25")),
                    cv(6, Some("2023-01-02 03:04:05")),
                    cv(7, Some("2023-01-02 03:04:05.000001")),
                    cv(9, Some("2023-01-02 03:04:05.5 +05:30")),
                    cv(10, Some("x'y\"z")),
                    cv(12, None),
                    cv(14, Some("0a0b")),
                ],
            }],
        )
        .await
        .unwrap();
        assert_eq!(res.updated, 1);
        let row = run(&s, "select * from lq_edit_types where id = 10")
            .await
            .rows
            .remove(0);
        assert_eq!(row[1].as_deref(), Some("-0.00001"));
        assert_eq!(row[4].as_deref(), Some("Inf"));
        assert_eq!(row[5].as_deref(), Some("-1.25"));
        assert_eq!(row[6].as_deref(), Some("2023-01-02 03:04:05"));
        assert_eq!(row[7].as_deref(), Some("2023-01-02 03:04:05.000001"));
        assert_eq!(row[9].as_deref(), Some("2023-01-02 03:04:05.500 +05:30"));
        assert_eq!(row[10].as_deref(), Some("x'y\"z"));
        assert_eq!(row[12], None);
        assert_eq!(row[14].as_deref(), Some("0A0B"));

        // Insert a copy of a row under a new key, then delete it.
        let source = &after.rows[1];
        let mut values: Vec<CellValue> = (1..source.len())
            .map(|i| cv(i, source[i].as_deref()))
            .collect();
        values.push(cv(0, Some("30")));
        let res = apply(&s, &t, vec![RowChange::Insert { values }])
            .await
            .unwrap();
        assert_eq!(res.inserted, 1);
        let copy = run(&s, "select * from lq_edit_types where id = 30")
            .await
            .rows
            .remove(0);
        assert_eq!(copy[1..], source[1..]);
        let res = apply(
            &s,
            &t,
            vec![RowChange::Delete {
                key: vec![Some("30".into())],
            }],
        )
        .await
        .unwrap();
        assert_eq!(res.deleted, 1);
        assert!(run(&s, "select * from lq_edit_types where id = 30")
            .await
            .rows
            .is_empty());

        run(&s, "drop table lq_edit_types purge").await;
    }

    #[tokio::test]
    #[ignore]
    async fn oracle_edit_date_keys_and_booleans() {
        let Some(s) = open().await else { return };
        try_run(&s, "drop table lq_edit_dk purge").await;
        run(&s, "create table lq_edit_dk (d date, ts timestamp(6), v varchar2(10), primary key (d, ts))").await;
        run(&s, "insert into lq_edit_dk values (timestamp '2024-01-02 03:04:05', timestamp '2024-01-02 03:04:05.000007', 'a')").await;
        let r = run(&s, "select v, d, ts from lq_edit_dk").await;
        let t = target_of(&r);
        assert_eq!(t.key, [1, 2]);
        let row = &r.rows[0];
        let res = apply(
            &s,
            &t,
            vec![RowChange::Update {
                key: vec![row[1].clone(), row[2].clone()],
                values: vec![cv(0, Some("b"))],
            }],
        )
        .await
        .unwrap();
        assert_eq!(res.updated, 1);
        run(&s, "drop table lq_edit_dk purge").await;

        // BOOLEAN columns exist from 23ai on.
        try_run(&s, "drop table lq_edit_bool purge").await;
        if try_run(
            &s,
            "create table lq_edit_bool (id number primary key, b boolean)",
        )
        .await
        {
            run(&s, "insert into lq_edit_bool values (1, true)").await;
            let r = run(&s, "select * from lq_edit_bool").await;
            let t = target_of(&r);
            assert_eq!(r.rows[0][1].as_deref(), Some("TRUE"));
            apply(
                &s,
                &t,
                vec![RowChange::Update {
                    key: vec![Some("1".into())],
                    values: vec![cv(1, Some("FALSE"))],
                }],
            )
            .await
            .unwrap();
            let r = run(&s, "select b from lq_edit_bool").await;
            assert_eq!(r.rows[0][0].as_deref(), Some("FALSE"));
            run(&s, "drop table lq_edit_bool purge").await;
        }
    }

    #[tokio::test]
    #[ignore]
    async fn oracle_edit_transactions() {
        let Some(s) = open().await else { return };
        let other = open().await.unwrap();
        try_run(&s, "drop table lq_edit_tx purge").await;
        run(
            &s,
            "create table lq_edit_tx (id number primary key, v varchar2(10))",
        )
        .await;
        run(&s, "insert into lq_edit_tx values (1, 'a')").await;
        run(&s, "insert into lq_edit_tx values (2, 'b')").await;
        let t = target_of(&run(&s, "select id, v from lq_edit_tx").await);
        let set = |id: &str, v: &str| RowChange::Update {
            key: vec![Some(id.into())],
            values: vec![cv(1, Some(v))],
        };
        let values = |rows: Vec<Vec<Option<String>>>| -> Vec<String> {
            rows.into_iter()
                .map(|r| r[0].clone().unwrap_or_default())
                .collect()
        };
        let sql = "select v from lq_edit_tx order by id";

        // A key that matches nothing undoes the whole batch.
        let err = apply(&s, &t, vec![set("1", "x"), set("99", "y")])
            .await
            .unwrap_err();
        assert_eq!(err.kind, ErrorKind::Database);
        assert!(
            err.message.starts_with("Row 2 was not saved: 0 rows"),
            "{err}"
        );
        assert_eq!(values(run(&s, sql).await.rows), ["a", "b"]);
        assert!(!s.tx_state().in_transaction);

        // Autocommit: other sessions see the change at once.
        let res = apply(&s, &t, vec![set("1", "x")]).await.unwrap();
        assert!(!res.in_transaction);
        assert_eq!(values(run(&other, sql).await.rows), ["x", "b"]);

        // Manual mode: the change joins the open transaction.
        s.set_autocommit(false).await.unwrap();
        let res = apply(&s, &t, vec![set("2", "z")]).await.unwrap();
        assert!(res.in_transaction);
        assert!(s.tx_state().in_transaction);
        assert_eq!(values(run(&s, sql).await.rows), ["x", "z"]);
        assert_eq!(values(run(&other, sql).await.rows), ["x", "b"]);
        s.end_transaction(false).await.unwrap();
        assert_eq!(values(run(&s, sql).await.rows), ["x", "b"]);

        // A failure in manual mode keeps the earlier work of the transaction.
        run(&s, "update lq_edit_tx set v = 'm' where id = 1").await;
        let err = apply(
            &s,
            &t,
            vec![
                set("2", "q"),
                RowChange::Delete {
                    key: vec![Some("99".into())],
                },
            ],
        )
        .await
        .unwrap_err();
        assert!(err.message.starts_with("Row 2"), "{err}");
        assert!(s.tx_state().in_transaction);
        assert_eq!(values(run(&s, sql).await.rows), ["m", "b"]);
        s.end_transaction(false).await.unwrap();
        s.set_autocommit(true).await.unwrap();

        // Bad input is refused before anything runs.
        let err = apply(
            &s,
            &t,
            vec![RowChange::Delete {
                key: vec![Some("abc".into())],
            }],
        )
        .await
        .unwrap_err();
        assert_eq!(err.kind, ErrorKind::Invalid);
        assert_eq!(values(run(&s, sql).await.rows), ["x", "b"]);

        run(&s, "drop table lq_edit_tx purge").await;
    }

    #[tokio::test]
    #[ignore]
    async fn oracle_edit_quoted_names() {
        let Some(s) = open().await else { return };
        try_run(&s, r#"drop table "lq_edit Mixed" purge"#).await;
        run(&s, r#"create table "lq_edit Mixed" ("Id Key" number primary key, "Some Value" varchar2(20), "lower" date)"#).await;
        run(
            &s,
            r#"insert into "lq_edit Mixed" values (1, 'one', date '2024-01-01')"#,
        )
        .await;

        let r = run(
            &s,
            r#"select m."Some Value" sv, m."Id Key", "lower" from "lq_edit Mixed" m"#,
        )
        .await;
        let t = target_of(&r);
        assert_eq!(t.table, "lq_edit Mixed");
        assert_eq!(t.key, [1]);
        assert_eq!(t.columns[0].as_ref().unwrap().name, "Some Value");

        let res = apply(
            &s,
            &t,
            vec![
                RowChange::Update {
                    key: vec![Some("1".into())],
                    values: vec![cv(0, Some("uno")), cv(2, Some("2024-12-31 23:59:59"))],
                },
                RowChange::Insert {
                    values: vec![cv(1, Some("2")), cv(0, Some("two"))],
                },
            ],
        )
        .await
        .unwrap();
        assert_eq!((res.updated, res.inserted), (1, 1));
        let r = run(
            &s,
            r#"select "Id Key", "Some Value", "lower" from "lq_edit Mixed" order by 1"#,
        )
        .await;
        assert_eq!(r.rows[0][1].as_deref(), Some("uno"));
        assert_eq!(r.rows[0][2].as_deref(), Some("2024-12-31 23:59:59"));
        assert_eq!(r.rows[1][1].as_deref(), Some("two"));
        let t2 = target_of(&r);
        let res = apply(
            &s,
            &t2,
            vec![RowChange::Delete {
                key: vec![Some("2".into())],
            }],
        )
        .await
        .unwrap();
        assert_eq!(res.deleted, 1);

        run(&s, r#"drop table "lq_edit Mixed" purge"#).await;
    }
}
