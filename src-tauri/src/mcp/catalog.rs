//! Catalog queries for agents, per engine: finding tables and columns by
//! name, foreign keys of a table and execution plans.
//!
//! Everything runs through [`Session::execute`], which has no bind
//! parameters, so every value that comes from the agent is embedded as a
//! literal built by [`literal`] for the engine's dialect. Nothing here
//! writes: the queries only read the catalogs, and `explain` asks for the
//! plan without running the statement. The server runs these functions on
//! its read-only sessions, so they also work inside a read-only transaction
//! (see [`explain`] for Oracle and SQL Server).

use serde::Serialize;

use crate::drivers::{first_keywords, oracle, Session};
use crate::error::{AppError, AppResult, ErrorKind};
use crate::model::{ColumnInfo, Engine, ExecRequest, ExecResult};

/// A table, view or column whose name matches a search.
#[derive(Debug, Clone, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct ObjectMatch {
    pub schema: Option<String>,
    pub table: String,
    /// "table", "view", "materialized view"... as the engine names it.
    pub kind: String,
    /// Set when the match is a column of `table`.
    pub column: Option<String>,
    pub column_type: Option<String>,
}

/// A foreign key from `table` to another table.
#[derive(Debug, Clone, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct ForeignKey {
    pub name: Option<String>,
    pub columns: Vec<String>,
    pub ref_schema: Option<String>,
    pub ref_table: String,
    pub ref_columns: Vec<String>,
}

/// Longest search pattern accepted.
const MAX_PATTERN: usize = 128;
/// LIKE escape character: not special in any dialect's string literals.
const ESC: char = '!';
/// Rows read for the foreign keys of one table.
const FK_ROWS: usize = 10_000;
/// Plan lines read.
const PLAN_ROWS: usize = 5_000;
/// Redis: keys examined per SCAN call and SCAN calls per search.
const SCAN_COUNT: usize = 1_000;
const SCAN_ROUNDS: usize = 200;

/// Owners of Oracle-supplied objects, for servers before 12c (no
/// `ALL_USERS.ORACLE_MAINTAINED`).
const ORACLE_SYSTEM_OWNERS: &[&str] = &[
    "ANONYMOUS",
    "APEX_PUBLIC_USER",
    "APPQOSSYS",
    "AUDSYS",
    "CTXSYS",
    "DBSNMP",
    "DIP",
    "DVSYS",
    "EXFSYS",
    "FLOWS_FILES",
    "GSMADMIN_INTERNAL",
    "LBACSYS",
    "MDDATA",
    "MDSYS",
    "MGMT_VIEW",
    "OJVMSYS",
    "OLAPSYS",
    "ORACLE_OCM",
    "ORDDATA",
    "ORDPLUGINS",
    "ORDSYS",
    "OUTLN",
    "OWBSYS",
    "SI_INFORMTN_SCHEMA",
    "SPATIAL_CSW_ADMIN_USR",
    "SPATIAL_WFS_ADMIN_USR",
    "SYS",
    "SYSMAN",
    "SYSTEM",
    "WMSYS",
    "XDB",
    "XS$NULL",
];

// ---------------------------------------------------------------------------
// Public API

/// Tables, views and columns whose name contains `pattern` (case
/// insensitive), across the schemas the user can see, at most `limit`.
/// Table matches come first, then column matches; within each, exact names,
/// then prefixes, then the current schema. System schemas are left out.
pub async fn search(session: &Session, pattern: &str, limit: usize) -> AppResult<Vec<ObjectMatch>> {
    let pattern = normalize_pattern(pattern)?;
    let limit = limit.max(1);
    let current = session.current_schema();
    let mut found = match session.engine() {
        Engine::Redis => return redis_search(session, &pattern, limit).await,
        Engine::Sqlite => sqlite_search(session, &pattern, limit).await?,
        Engine::Oracle => {
            let sql = |modern| oracle_search_sql(&pattern, current.as_deref(), modern);
            let (tables, columns) = sql(true)?;
            match query(session, &tables, limit).await {
                Ok(t) => {
                    let mut out = tables_of(&t);
                    out.extend(columns_of(&query(session, &columns, limit).await?));
                    out
                }
                // ORACLE_MAINTAINED is missing before 12c.
                Err(e) if e.code.as_deref() == Some("ORA-00904") => {
                    let (tables, columns) = sql(false)?;
                    let mut out = tables_of(&query(session, &tables, limit).await?);
                    out.extend(columns_of(&query(session, &columns, limit).await?));
                    out
                }
                Err(e) => return Err(e),
            }
        }
        engine => {
            let (tables, columns) = search_sql(dialect(engine), &pattern, current.as_deref())?;
            let mut out = tables_of(&query(session, &tables, limit).await?);
            out.extend(columns_of(&query(session, &columns, limit).await?));
            out
        }
    };
    rank(&mut found, &pattern, current.as_deref());
    found.truncate(limit);
    Ok(found)
}

/// Foreign keys declared on `table` (outgoing) and those referencing it
/// (incoming, with the schema and table that declares them), in that order.
/// `schema` defaults to the session's current schema; names are exact.
pub async fn foreign_keys(
    session: &Session,
    schema: Option<&str>,
    table: &str,
) -> AppResult<(Vec<ForeignKey>, Vec<(Option<String>, String, ForeignKey)>)> {
    let engine = session.engine();
    if engine == Engine::Redis {
        return Ok((Vec::new(), Vec::new()));
    }
    if table.is_empty() {
        return Err(AppError::invalid("Give the name of the table."));
    }
    let schema = match schema.filter(|s| !s.is_empty()) {
        Some(s) => s.to_string(),
        None => session
            .current_schema()
            .ok_or_else(|| AppError::invalid("Give the schema of the table."))?,
    };
    if engine == Engine::Sqlite {
        return sqlite_foreign_keys(session, &schema, table).await;
    }
    let sql = fk_sql(dialect(engine), &schema, table)?;
    Ok(group_foreign_keys(&query(session, &sql, FK_ROWS).await?))
}

/// The engine's execution plan for `sql`, as text, without running it.
/// Only queries and DML can be explained.
pub async fn explain(session: &Session, sql: &str) -> AppResult<String> {
    let engine = session.engine();
    if engine == Engine::Redis {
        return Err(AppError::invalid(
            "Redis has no execution plans: each command runs as it is.",
        ));
    }
    check_text(sql)?;
    let sql = if engine == Engine::Oracle {
        oracle::prepare_sql(sql)
    } else {
        strip_terminator(sql).to_string()
    };
    check_explainable(engine, &sql)?;
    match engine {
        Engine::Postgres => {
            let prefix = "EXPLAIN (FORMAT TEXT) ";
            let r = query(session, &format!("{prefix}{sql}"), PLAN_ROWS)
                .await
                .map_err(|e| shifted(e, prefix))?;
            Ok(lines(&r, 0))
        }
        Engine::Mysql | Engine::Mariadb => mysql_explain(session, &sql).await,
        Engine::Oracle => oracle_explain(session, &sql).await,
        Engine::Sqlserver => sqlserver_explain(session, &sql).await,
        Engine::Sqlite => {
            let prefix = "EXPLAIN QUERY PLAN ";
            let r = query(session, &format!("{prefix}{sql}"), PLAN_ROWS)
                .await
                .map_err(|e| shifted(e, prefix))?;
            let rows: Vec<(i64, i64, String)> = r
                .rows
                .iter()
                .map(|row| {
                    let num = |i| cell(row, i).parse().unwrap_or(0);
                    (num(0), num(1), cell(row, 3))
                })
                .collect();
            Ok(sqlite_plan_tree(&rows))
        }
        Engine::Redis => unreachable!(),
    }
}

// ---------------------------------------------------------------------------
// Literals

/// How string literals are written.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Dialect {
    Postgres,
    Oracle,
    Mysql,
    SqlServer,
    Sqlite,
}

fn dialect(engine: Engine) -> Dialect {
    match engine {
        Engine::Postgres => Dialect::Postgres,
        Engine::Oracle => Dialect::Oracle,
        Engine::Mysql | Engine::Mariadb => Dialect::Mysql,
        Engine::Sqlserver => Dialect::SqlServer,
        Engine::Sqlite | Engine::Redis => Dialect::Sqlite,
    }
}

fn check_text(s: &str) -> AppResult<()> {
    if s.contains('\0') {
        return Err(AppError::invalid("The text contains a NUL character."));
    }
    Ok(())
}

/// `s` as a string literal that means exactly `s` whatever the session
/// settings: PostgreSQL uses `E''` when there are backslashes (immune to
/// `standard_conforming_strings`), MySQL a hex literal (immune to
/// `NO_BACKSLASH_ESCAPES` and the connection charset), SQL Server `N''`.
fn literal(d: Dialect, s: &str) -> AppResult<String> {
    check_text(s)?;
    let doubled = s.replace('\'', "''");
    Ok(match d {
        Dialect::Postgres if s.contains('\\') => format!("E'{}'", doubled.replace('\\', "\\\\")),
        Dialect::Postgres | Dialect::Oracle | Dialect::Sqlite => format!("'{doubled}'"),
        Dialect::SqlServer => format!("N'{doubled}'"),
        Dialect::Mysql => {
            let hex: String = s.bytes().map(|b| format!("{b:02X}")).collect();
            format!("_utf8mb4 X'{hex}'")
        }
    })
}

/// `s` with the LIKE wildcards and the escape character escaped by [`ESC`].
fn like_escape(d: Dialect, s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 4);
    for c in s.chars() {
        if c == ESC || c == '%' || c == '_' || (d == Dialect::SqlServer && c == '[') {
            out.push(ESC);
        }
        out.push(c);
    }
    out
}

/// Case-insensitive name tests for one pattern: the filter (contains) and a
/// rank (0 exact, 1 prefix, 2 contains).
struct NameTest {
    exact: String,
    prefix: String,
    contains: String,
}

impl NameTest {
    fn new(d: Dialect, pattern: &str) -> AppResult<Self> {
        let esc = like_escape(d, pattern);
        Ok(Self {
            exact: literal(d, pattern)?,
            prefix: literal(d, &format!("{esc}%"))?,
            contains: literal(d, &format!("%{esc}%"))?,
        })
    }

    fn filter(&self, expr: &str) -> String {
        format!("LOWER({expr}) LIKE LOWER({}) ESCAPE '{ESC}'", self.contains)
    }

    fn rank(&self, expr: &str) -> String {
        format!(
            "CASE WHEN LOWER({expr}) = LOWER({}) THEN 0 \
             WHEN LOWER({expr}) LIKE LOWER({}) ESCAPE '{ESC}' THEN 1 ELSE 2 END",
            self.exact, self.prefix
        )
    }
}

/// Sorts the current schema first, when there is one.
fn schema_rank(d: Dialect, expr: &str, current: Option<&str>) -> AppResult<String> {
    Ok(match current {
        Some(c) => format!("CASE WHEN {expr} = {} THEN 0 ELSE 1 END", literal(d, c)?),
        None => "1".into(),
    })
}

/// Trims the pattern and the `%`/`*` an agent may wrap it in ("contains" is
/// implied).
fn normalize_pattern(pattern: &str) -> AppResult<String> {
    check_text(pattern)?;
    let p = pattern.trim().trim_matches(|c| c == '%' || c == '*').trim();
    if p.is_empty() {
        return Err(AppError::invalid(
            "Give a name or part of one to search for.",
        ));
    }
    if p.chars().count() > MAX_PATTERN {
        return Err(AppError::invalid(format!(
            "The search pattern is too long (at most {MAX_PATTERN} characters)."
        )));
    }
    Ok(p.to_string())
}

// ---------------------------------------------------------------------------
// Search SQL. Table queries return (schema, table, kind); column queries
// (schema, table, kind, column, type).

/// Table and column queries for PostgreSQL, MySQL/MariaDB and SQL Server.
fn search_sql(d: Dialect, pattern: &str, current: Option<&str>) -> AppResult<(String, String)> {
    let t = NameTest::new(d, pattern)?;
    Ok(match d {
        Dialect::Postgres => {
            let from = "FROM pg_class c JOIN pg_namespace n ON n.oid = c.relnamespace";
            let kind = "CASE c.relkind WHEN 'v' THEN 'view' WHEN 'm' THEN 'materialized view' \
                        WHEN 'f' THEN 'foreign table' ELSE 'table' END";
            let schemas = format!(
                "c.relkind IN ('r', 'p', 'v', 'm', 'f') AND NOT c.relispartition \
                 AND n.nspname <> 'information_schema' AND n.nspname NOT LIKE 'pg{ESC}_%' ESCAPE '{ESC}'"
            );
            let in_schema = schema_rank(d, "n.nspname::text", current)?;
            (
                format!(
                    "SELECT n.nspname::text, c.relname::text, {kind} {from} \
                     WHERE {schemas} AND {} ORDER BY {}, {in_schema}, 1, 2",
                    t.filter("c.relname::text"),
                    t.rank("c.relname::text"),
                ),
                format!(
                    "SELECT n.nspname::text, c.relname::text, {kind}, a.attname::text, \
                     format_type(a.atttypid, a.atttypmod) \
                     FROM pg_attribute a JOIN pg_class c ON c.oid = a.attrelid \
                     JOIN pg_namespace n ON n.oid = c.relnamespace \
                     WHERE a.attnum > 0 AND NOT a.attisdropped AND {schemas} AND {} \
                     ORDER BY {}, {in_schema}, 1, 2, a.attnum",
                    t.filter("a.attname::text"),
                    t.rank("a.attname::text"),
                ),
            )
        }
        Dialect::Mysql => {
            let kind = |col: &str| {
                format!(
                    "CASE {col} WHEN 'VIEW' THEN 'view' WHEN 'SYSTEM VIEW' THEN 'view' \
                     WHEN 'SEQUENCE' THEN 'sequence' ELSE 'table' END"
                )
            };
            let visible = |col: &str| -> AppResult<String> {
                let system = "('mysql', 'information_schema', 'performance_schema', 'sys')";
                Ok(match current {
                    Some(c) => format!("({col} = {} OR {col} NOT IN {system})", literal(d, c)?),
                    None => format!("{col} NOT IN {system}"),
                })
            };
            (
                format!(
                    "SELECT TABLE_SCHEMA, TABLE_NAME, {} FROM information_schema.TABLES \
                     WHERE {} AND {} ORDER BY {}, {}, TABLE_SCHEMA, TABLE_NAME",
                    kind("TABLE_TYPE"),
                    visible("TABLE_SCHEMA")?,
                    t.filter("TABLE_NAME"),
                    t.rank("TABLE_NAME"),
                    schema_rank(d, "TABLE_SCHEMA", current)?,
                ),
                format!(
                    "SELECT c.TABLE_SCHEMA, c.TABLE_NAME, {}, c.COLUMN_NAME, c.COLUMN_TYPE \
                     FROM information_schema.COLUMNS c JOIN information_schema.TABLES t \
                     ON t.TABLE_SCHEMA = c.TABLE_SCHEMA AND t.TABLE_NAME = c.TABLE_NAME \
                     WHERE {} AND {} \
                     ORDER BY {}, {}, c.TABLE_SCHEMA, c.TABLE_NAME, c.ORDINAL_POSITION",
                    kind("t.TABLE_TYPE"),
                    visible("c.TABLE_SCHEMA")?,
                    t.filter("c.COLUMN_NAME"),
                    t.rank("c.COLUMN_NAME"),
                    schema_rank(d, "c.TABLE_SCHEMA", current)?,
                ),
            )
        }
        Dialect::SqlServer => {
            let base = "FROM sys.objects o JOIN sys.schemas s ON s.schema_id = o.schema_id";
            let kind = "CASE o.type WHEN 'V' THEN 'view' ELSE 'table' END";
            let user = "o.type IN ('U', 'V') AND o.is_ms_shipped = 0";
            let in_schema = schema_rank(d, "s.name", current)?;
            (
                format!(
                    "SELECT s.name, o.name, {kind} {base} WHERE {user} AND {} \
                     ORDER BY {}, {in_schema}, s.name, o.name",
                    t.filter("o.name"),
                    t.rank("o.name"),
                ),
                format!(
                    "SELECT s.name, o.name, {kind}, c.name, {} \
                     {base} JOIN sys.columns c ON c.object_id = o.object_id \
                     JOIN sys.types t ON t.user_type_id = c.user_type_id \
                     WHERE {user} AND {} ORDER BY {}, {in_schema}, s.name, o.name, c.column_id",
                    SQLSERVER_TYPE,
                    t.filter("c.name"),
                    t.rank("c.name"),
                ),
            )
        }
        Dialect::Oracle | Dialect::Sqlite => {
            return Err(AppError::internal("Use the engine's own search."));
        }
    })
}

/// Column type as in DDL (`nvarchar(40)`, `decimal(10,2)`).
const SQLSERVER_TYPE: &str = "CASE WHEN t.is_user_defined = 1 THEN t.name \
     WHEN t.name IN ('varchar', 'char', 'varbinary', 'binary') \
       THEN t.name + '(' + CASE WHEN c.max_length = -1 THEN 'max' \
            ELSE CAST(c.max_length AS varchar(10)) END + ')' \
     WHEN t.name IN ('nvarchar', 'nchar') \
       THEN t.name + '(' + CASE WHEN c.max_length = -1 THEN 'max' \
            ELSE CAST(c.max_length / 2 AS varchar(10)) END + ')' \
     WHEN t.name IN ('decimal', 'numeric') \
       THEN t.name + '(' + CAST(c.precision AS varchar(10)) + ',' \
            + CAST(c.scale AS varchar(10)) + ')' \
     WHEN t.name IN ('datetime2', 'time', 'datetimeoffset') \
       THEN t.name + '(' + CAST(c.scale AS varchar(10)) + ')' \
     ELSE t.name END";

/// Oracle table and column queries. `modern` filters Oracle-maintained
/// owners through `ALL_USERS.ORACLE_MAINTAINED` (12c on); otherwise a fixed
/// list is used.
fn oracle_search_sql(
    pattern: &str,
    current: Option<&str>,
    modern: bool,
) -> AppResult<(String, String)> {
    let d = Dialect::Oracle;
    let t = NameTest::new(d, pattern)?;
    let objects = "with objs as (\
           select t.owner, t.table_name name, 'table' kind from all_tables t \
           where t.nested = 'NO' and t.secondary = 'N' and t.dropped = 'NO' \
             and (t.iot_type is null or t.iot_type = 'IOT') \
             and not exists (select 1 from all_mviews m \
                             where m.owner = t.owner and m.mview_name = t.table_name) \
           union all select v.owner, v.view_name, 'view' from all_views v \
           union all select m.owner, m.mview_name, 'materialized view' from all_mviews m)";
    let system = if modern {
        "o.owner not in (select username from all_users where oracle_maintained = 'Y')".to_string()
    } else {
        let list: Vec<String> = ORACLE_SYSTEM_OWNERS
            .iter()
            .map(|o| format!("'{o}'"))
            .collect();
        format!(
            "o.owner not in ({}) and o.owner not like 'APEX{ESC}_%' escape '{ESC}'",
            list.join(", ")
        )
    };
    let visible = match current {
        Some(c) => format!("(o.owner = {} or {system})", literal(d, c)?),
        None => system,
    };
    let in_schema = schema_rank(d, "o.owner", current)?;
    let ty = "case \
          when c.data_type in ('VARCHAR2','NVARCHAR2','CHAR','NCHAR','RAW') \
            then c.data_type || '(' || case when c.char_length > 0 then c.char_length else c.data_length end || ')' \
          when c.data_type = 'NUMBER' and c.data_precision is not null \
            then 'NUMBER(' || c.data_precision || \
                 case when c.data_scale > 0 then ',' || c.data_scale end || ')' \
          else c.data_type end";
    Ok((
        format!(
            "{objects} select o.owner, o.name, o.kind from objs o \
             where {visible} and {} and o.name not like 'BIN$%' \
             order by {}, {in_schema}, o.owner, o.name",
            t.filter("o.name"),
            t.rank("o.name"),
        ),
        format!(
            "{objects} select o.owner, o.name, o.kind, c.column_name, {ty} \
             from all_tab_columns c join objs o on o.owner = c.owner and o.name = c.table_name \
             where {visible} and {} and o.name not like 'BIN$%' \
             order by {}, {in_schema}, o.owner, o.name, c.column_id",
            t.filter("c.column_name"),
            t.rank("c.column_name"),
        ),
    ))
}

/// SQLite double-quoted identifier.
fn sqlite_ident(name: &str) -> String {
    format!("\"{}\"", name.replace('"', "\"\""))
}

/// SQLite table and column queries for one attached schema.
fn sqlite_search_sql(schema: &str, pattern: &str) -> AppResult<(String, String)> {
    let d = Dialect::Sqlite;
    let t = NameTest::new(d, pattern)?;
    let s = literal(d, schema)?;
    let master = format!("{}.sqlite_schema", sqlite_ident(schema));
    let user =
        format!("m.type IN ('table', 'view') AND m.name NOT LIKE 'sqlite{ESC}_%' ESCAPE '{ESC}'");
    Ok((
        format!(
            "SELECT {s}, m.name, m.type FROM {master} m WHERE {user} AND {} ORDER BY {}, m.name",
            t.filter("m.name"),
            t.rank("m.name"),
        ),
        format!(
            "SELECT {s}, m.name, m.type, p.name, p.type FROM {master} m \
             JOIN pragma_table_info(m.name, {s}) p WHERE {user} AND {} \
             ORDER BY {}, m.name, p.cid",
            t.filter("p.name"),
            t.rank("p.name"),
        ),
    ))
}

async fn sqlite_search(
    session: &Session,
    pattern: &str,
    limit: usize,
) -> AppResult<Vec<ObjectMatch>> {
    let schemas = session.list_schemas().await?;
    let mut tables = Vec::new();
    let mut columns = Vec::new();
    for schema in &schemas {
        let (t, c) = sqlite_search_sql(schema, pattern)?;
        tables.extend(tables_of(&query(session, &t, limit).await?));
        columns.extend(columns_of(&query(session, &c, limit).await?));
    }
    tables.extend(columns);
    Ok(tables)
}

fn tables_of(r: &ExecResult) -> Vec<ObjectMatch> {
    r.rows
        .iter()
        .map(|row| ObjectMatch {
            schema: row.first().cloned().flatten(),
            table: cell(row, 1),
            kind: cell(row, 2),
            column: None,
            column_type: None,
        })
        .collect()
}

fn columns_of(r: &ExecResult) -> Vec<ObjectMatch> {
    r.rows
        .iter()
        .map(|row| ObjectMatch {
            schema: row.first().cloned().flatten(),
            table: cell(row, 1),
            kind: cell(row, 2),
            column: Some(cell(row, 3)),
            column_type: row.get(4).cloned().flatten(),
        })
        .collect()
}

/// Tables before columns; then exact names, prefixes, the rest; then the
/// current schema first. Stable, so the engine's order stays within ties.
fn rank(found: &mut [ObjectMatch], pattern: &str, current: Option<&str>) {
    let p = pattern.to_lowercase();
    found.sort_by_cached_key(|m| {
        let name = m.column.as_deref().unwrap_or(&m.table).to_lowercase();
        let quality = if name == p {
            0
        } else if name.starts_with(&p) {
            1
        } else {
            2
        };
        let elsewhere = current.is_some() && m.schema.as_deref() != current;
        (m.column.is_some(), quality, elsewhere)
    });
}

// ---------------------------------------------------------------------------
// Redis

/// Glob for keys containing `pattern`, ASCII letters in either case.
fn redis_glob(pattern: &str) -> String {
    let mut g = String::from("*");
    for c in pattern.chars() {
        match c {
            '*' | '?' | '[' | ']' | '\\' => {
                g.push('\\');
                g.push(c);
            }
            c if c.is_ascii_alphabetic() => {
                g.push('[');
                g.push(c.to_ascii_lowercase());
                g.push(c.to_ascii_uppercase());
                g.push(']');
            }
            c => g.push(c),
        }
    }
    g.push('*');
    g
}

/// One argument for the Redis command line parser (redis-cli rules).
fn redis_quote(arg: &str) -> String {
    let mut out = String::from("\"");
    for c in arg.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            c if c.is_ascii_control() => out.push_str(&format!("\\x{:02x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// The cursor in the driver's "Next cursor: N" notice.
fn next_cursor(notices: &[String]) -> Option<String> {
    notices.iter().find_map(|n| {
        let rest = n.strip_prefix("Next cursor: ")?;
        let cursor: String = rest.chars().take_while(char::is_ascii_digit).collect();
        (!cursor.is_empty()).then_some(cursor)
    })
}

async fn redis_search(
    session: &Session,
    pattern: &str,
    limit: usize,
) -> AppResult<Vec<ObjectMatch>> {
    if session.tx_state().in_transaction {
        return Err(AppError::invalid(
            "Finish the transaction with EXEC or DISCARD first.",
        ));
    }
    let glob = redis_quote(&redis_glob(pattern));
    let mut keys: Vec<String> = Vec::new();
    let mut cursor = "0".to_string();
    for _ in 0..SCAN_ROUNDS {
        let r = query(
            session,
            &format!("SCAN {cursor} MATCH {glob} COUNT {SCAN_COUNT}"),
            0,
        )
        .await?;
        keys.extend(r.rows.iter().map(|row| cell(row, 1)));
        cursor =
            next_cursor(&r.notices).ok_or_else(|| AppError::internal("Unexpected SCAN reply."))?;
        if cursor == "0" || keys.len() >= limit {
            break;
        }
    }
    // The exact key first, even when the scan stopped before reaching it.
    if !keys.iter().any(|k| k == pattern) {
        let r = query(session, &format!("EXISTS {}", redis_quote(pattern)), 0).await?;
        if r.rows.first().map(|row| cell(row, 0)).as_deref() == Some("1") {
            keys.push(pattern.to_string());
        }
    }
    keys.sort();
    keys.dedup();
    let schema = session.current_schema();
    let mut found: Vec<ObjectMatch> = keys
        .into_iter()
        .map(|k| ObjectMatch {
            schema: schema.clone(),
            table: k,
            kind: "key".into(),
            column: None,
            column_type: None,
        })
        .collect();
    rank(&mut found, pattern, None);
    found.truncate(limit);
    Ok(found)
}

// ---------------------------------------------------------------------------
// Foreign keys. Rows: direction ('out'/'in'), constraint, schema, table,
// referenced schema, referenced table, position, column, referenced column;
// sorted so each constraint's rows are together, outgoing first.

fn fk_sql(d: Dialect, schema: &str, table: &str) -> AppResult<String> {
    let s = literal(d, schema)?;
    let t = literal(d, table)?;
    Ok(match d {
        Dialect::Postgres => format!(
            "WITH t AS (SELECT c.oid FROM pg_class c JOIN pg_namespace n ON n.oid = c.relnamespace \
                        WHERE n.nspname = {s} AND c.relname = {t}), \
             fk AS (SELECT 'out' AS dir, con.oid FROM pg_constraint con \
                    WHERE con.contype = 'f' AND con.conrelid IN (SELECT oid FROM t) \
                    UNION ALL \
                    SELECT 'in', con.oid FROM pg_constraint con \
                    WHERE con.contype = 'f' AND con.confrelid IN (SELECT oid FROM t)) \
             SELECT fk.dir, con.conname::text, sn.nspname::text, sc.relname::text, \
                    rn.nspname::text, rc.relname::text, k.ord, sa.attname::text, ra.attname::text \
             FROM fk JOIN pg_constraint con ON con.oid = fk.oid \
             JOIN pg_class sc ON sc.oid = con.conrelid \
             JOIN pg_namespace sn ON sn.oid = sc.relnamespace \
             JOIN pg_class rc ON rc.oid = con.confrelid \
             JOIN pg_namespace rn ON rn.oid = rc.relnamespace \
             CROSS JOIN LATERAL unnest(con.conkey, con.confkey) WITH ORDINALITY AS k(src, ref, ord) \
             JOIN pg_attribute sa ON sa.attrelid = con.conrelid AND sa.attnum = k.src \
             JOIN pg_attribute ra ON ra.attrelid = con.confrelid AND ra.attnum = k.ref \
             ORDER BY 1 DESC, 3, 4, 2, k.ord"
        ),
        Dialect::Oracle => {
            let part = |dir: &str, owner: &str, name: &str| {
                format!(
                    "select '{dir}' dir, c.constraint_name, c.owner, c.table_name, r.owner r_owner, \
                            r.table_name r_table, cc.position, cc.column_name, rc.column_name r_column \
                     from all_constraints c \
                     join all_constraints r on r.owner = c.r_owner \
                      and r.constraint_name = c.r_constraint_name \
                     join all_cons_columns cc on cc.owner = c.owner \
                      and cc.constraint_name = c.constraint_name \
                     join all_cons_columns rc on rc.owner = r.owner \
                      and rc.constraint_name = r.constraint_name and rc.position = cc.position \
                     where c.constraint_type = 'R' and {owner} = {s} and {name} = {t}"
                )
            };
            format!(
                "{} union all {} order by 1 desc, 3, 4, 2, 7",
                part("out", "c.owner", "c.table_name"),
                part("in", "r.owner", "r.table_name"),
            )
        }
        Dialect::Mysql => {
            let part = |dir: &str, owner: &str, name: &str| {
                format!(
                    "SELECT '{dir}', CONSTRAINT_NAME, TABLE_SCHEMA, TABLE_NAME, \
                            REFERENCED_TABLE_SCHEMA, REFERENCED_TABLE_NAME, ORDINAL_POSITION, \
                            COLUMN_NAME, REFERENCED_COLUMN_NAME \
                     FROM information_schema.KEY_COLUMN_USAGE \
                     WHERE REFERENCED_TABLE_NAME IS NOT NULL AND {owner} = {s} AND {name} = {t}"
                )
            };
            format!(
                "{} UNION ALL {} ORDER BY 1 DESC, 3, 4, 2, 7",
                part("out", "TABLE_SCHEMA", "TABLE_NAME"),
                part("in", "REFERENCED_TABLE_SCHEMA", "REFERENCED_TABLE_NAME"),
            )
        }
        Dialect::SqlServer => {
            let id = format!("OBJECT_ID(QUOTENAME({s}) + N'.' + QUOTENAME({t}))");
            format!(
                "SELECT x.dir, fk.name, ss.name, st.name, rs.name, rt.name, \
                        fkc.constraint_column_id, sc.name, rc.name \
                 FROM (SELECT 'out' AS dir, f.object_id FROM sys.foreign_keys f \
                       WHERE f.parent_object_id = {id} \
                       UNION ALL \
                       SELECT 'in', f.object_id FROM sys.foreign_keys f \
                       WHERE f.referenced_object_id = {id}) x \
                 JOIN sys.foreign_keys fk ON fk.object_id = x.object_id \
                 JOIN sys.objects st ON st.object_id = fk.parent_object_id \
                 JOIN sys.schemas ss ON ss.schema_id = st.schema_id \
                 JOIN sys.objects rt ON rt.object_id = fk.referenced_object_id \
                 JOIN sys.schemas rs ON rs.schema_id = rt.schema_id \
                 JOIN sys.foreign_key_columns fkc ON fkc.constraint_object_id = fk.object_id \
                 JOIN sys.columns sc ON sc.object_id = fkc.parent_object_id \
                  AND sc.column_id = fkc.parent_column_id \
                 JOIN sys.columns rc ON rc.object_id = fkc.referenced_object_id \
                  AND rc.column_id = fkc.referenced_column_id \
                 ORDER BY x.dir DESC, ss.name, st.name, fk.name, fkc.constraint_column_id"
            )
        }
        Dialect::Sqlite => return Err(AppError::internal("Use the SQLite pragmas.")),
    })
}

type Incoming = Vec<(Option<String>, String, ForeignKey)>;

/// Groups the rows of [`fk_sql`] into constraints.
fn group_foreign_keys(r: &ExecResult) -> (Vec<ForeignKey>, Incoming) {
    let mut outgoing: Vec<ForeignKey> = Vec::new();
    let mut incoming: Incoming = Vec::new();
    let mut last: Option<[Option<String>; 6]> = None;
    for row in &r.rows {
        let key: [Option<String>; 6] = std::array::from_fn(|i| row.get(i).cloned().flatten());
        let column = cell(row, 7);
        let ref_column = cell(row, 8);
        if last.as_ref() == Some(&key) {
            let fk = if cell(row, 0) == "out" {
                outgoing.last_mut()
            } else {
                incoming.last_mut().map(|(_, _, f)| f)
            };
            if let Some(fk) = fk {
                fk.columns.push(column);
                fk.ref_columns.push(ref_column);
                continue;
            }
        }
        let fk = ForeignKey {
            name: key[1].clone(),
            columns: vec![column],
            ref_schema: key[4].clone(),
            ref_table: key[5].clone().unwrap_or_default(),
            ref_columns: vec![ref_column],
        };
        if cell(row, 0) == "out" {
            outgoing.push(fk);
        } else {
            incoming.push((key[2].clone(), key[3].clone().unwrap_or_default(), fk));
        }
        last = Some(key);
    }
    (outgoing, incoming)
}

/// SQLite: `pragma_foreign_key_list` of the table, and of every table of the
/// schema for the incoming ones. A key without target columns references
/// the primary key.
async fn sqlite_foreign_keys(
    session: &Session,
    schema: &str,
    table: &str,
) -> AppResult<(Vec<ForeignKey>, Incoming)> {
    let d = Dialect::Sqlite;
    let (s, t) = (literal(d, schema)?, literal(d, table)?);
    let out = query(
        session,
        &format!(
            "SELECT {t}, id, seq, \"table\", \"from\", \"to\" \
             FROM pragma_foreign_key_list({t}, {s}) ORDER BY id, seq"
        ),
        FK_ROWS,
    )
    .await?;
    let inc = query(
        session,
        &format!(
            "SELECT m.name, f.id, f.seq, f.\"table\", f.\"from\", f.\"to\" \
             FROM {}.sqlite_schema m JOIN pragma_foreign_key_list(m.name, {s}) f \
             WHERE m.type = 'table' AND LOWER(f.\"table\") = LOWER({t}) \
             ORDER BY m.name, f.id, f.seq",
            sqlite_ident(schema)
        ),
        FK_ROWS,
    )
    .await?;

    // (declaring table, constraint id) → key.
    let group = |r: &ExecResult| -> Vec<(String, ForeignKey, bool)> {
        let mut keys: Vec<(String, String, ForeignKey, bool)> = Vec::new();
        for row in &r.rows {
            let (owner, id) = (cell(row, 0), cell(row, 1));
            let to = row.get(5).cloned().flatten();
            let implicit = to.is_none();
            match keys.last_mut() {
                Some((o, i, fk, imp)) if *o == owner && *i == id => {
                    fk.columns.push(cell(row, 4));
                    fk.ref_columns.extend(to);
                    *imp |= implicit;
                }
                _ => keys.push((
                    owner,
                    id,
                    ForeignKey {
                        name: None,
                        columns: vec![cell(row, 4)],
                        ref_schema: Some(schema.to_string()),
                        ref_table: cell(row, 3),
                        ref_columns: to.into_iter().collect(),
                    },
                    implicit,
                )),
            }
        }
        keys.into_iter()
            .map(|(owner, _, fk, implicit)| (owner, fk, implicit))
            .collect()
    };

    let mut outgoing = Vec::new();
    for (_, mut fk, implicit) in group(&out) {
        if implicit {
            fk.ref_columns = sqlite_primary_key(session, schema, &fk.ref_table).await?;
        }
        outgoing.push(fk);
    }
    let mut incoming = Vec::new();
    let mut own_key: Option<Vec<String>> = None;
    for (owner, mut fk, implicit) in group(&inc) {
        if implicit {
            if own_key.is_none() {
                own_key = Some(sqlite_primary_key(session, schema, table).await?);
            }
            fk.ref_columns = own_key.clone().unwrap_or_default();
        }
        incoming.push((Some(schema.to_string()), owner, fk));
    }
    Ok((outgoing, incoming))
}

async fn sqlite_primary_key(
    session: &Session,
    schema: &str,
    table: &str,
) -> AppResult<Vec<String>> {
    let d = Dialect::Sqlite;
    let r = query(
        session,
        &format!(
            "SELECT name FROM pragma_table_info({}, {}) WHERE pk > 0 ORDER BY pk",
            literal(d, table)?,
            literal(d, schema)?
        ),
        FK_ROWS,
    )
    .await?;
    Ok(r.rows.iter().map(|row| cell(row, 0)).collect())
}

// ---------------------------------------------------------------------------
// Explain

/// Drops trailing `;` and whitespace.
fn strip_terminator(sql: &str) -> &str {
    let mut text = sql.trim_end();
    while let Some(stripped) = text.strip_suffix(';') {
        text = stripped.trim_end();
    }
    text
}

/// The text after leading whitespace and comments (`--`, `#` for MySQL,
/// `/* */`).
fn after_comments(sql: &str, hash: bool) -> &str {
    let mut s = sql.trim_start();
    loop {
        if s.starts_with("--") || (hash && s.starts_with('#')) {
            s = s.find('\n').map_or("", |i| &s[i + 1..]).trim_start();
        } else if let Some(rest) = s.strip_prefix("/*") {
            s = rest.find("*/").map_or("", |i| &rest[i + 2..]).trim_start();
        } else {
            return s;
        }
    }
}

/// Only queries and DML can be explained: anything else is refused before
/// it reaches the server, so the wrapper cannot become something that runs
/// (`EXPLAIN ANALYZE`, MySQL `/*! */` code, a second statement).
fn check_explainable(engine: Engine, sql: &str) -> AppResult<()> {
    let mysql = matches!(engine, Engine::Mysql | Engine::Mariadb);
    let allowed: &[&str] = match engine {
        Engine::Postgres => &[
            "SELECT", "WITH", "INSERT", "UPDATE", "DELETE", "MERGE", "VALUES", "TABLE",
        ],
        Engine::Mysql | Engine::Mariadb => &[
            "SELECT", "WITH", "INSERT", "UPDATE", "DELETE", "REPLACE", "TABLE", "VALUES",
        ],
        Engine::Oracle | Engine::Sqlserver => {
            &["SELECT", "WITH", "INSERT", "UPDATE", "DELETE", "MERGE"]
        }
        Engine::Sqlite => &[
            "SELECT", "WITH", "INSERT", "UPDATE", "DELETE", "REPLACE", "VALUES",
        ],
        Engine::Redis => &[],
    };
    let rest = after_comments(sql, mysql);
    if rest.is_empty() {
        return Err(AppError::invalid("Nothing to explain."));
    }
    let ok = if rest.starts_with('(') {
        true
    } else {
        let first = first_keywords(rest, 1);
        first.first().is_some_and(|w| allowed.contains(&w.as_str()))
    };
    if !ok {
        return Err(AppError::invalid(format!(
            "Only queries and {} statements can be explained. Pass the statement itself, without EXPLAIN.",
            allowed[2..].join(", ")
        )));
    }
    if mysql {
        let upper = sql.to_ascii_uppercase();
        if upper.contains("/*!") || upper.contains("/*M!") {
            return Err(AppError::invalid(
                "Remove the /*! */ comments to explain the statement.",
            ));
        }
        if mysql_has_separator(sql, true) || mysql_has_separator(sql, false) {
            return Err(AppError::invalid("Explain one statement at a time."));
        }
    }
    Ok(())
}

/// Whether a MySQL text has a `;` outside literals, quoted identifiers and
/// comments, reading backslashes in strings as escapes or not (the server's
/// `NO_BACKSLASH_ESCAPES` decides; both readings are checked).
fn mysql_has_separator(sql: &str, backslash: bool) -> bool {
    let b = sql.as_bytes();
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            q @ (b'\'' | b'"' | b'`') => {
                i += 1;
                while i < b.len() {
                    if backslash && q != b'`' && b[i] == b'\\' {
                        i += 2;
                    } else if b[i] == q {
                        if b.get(i + 1) == Some(&q) {
                            i += 2;
                        } else {
                            break;
                        }
                    } else {
                        i += 1;
                    }
                }
                i += 1;
            }
            b'#' => {
                while i < b.len() && b[i] != b'\n' {
                    i += 1;
                }
            }
            b'-' if b.get(i + 1) == Some(&b'-')
                && b.get(i + 2)
                    .is_none_or(|c| c.is_ascii_whitespace() || c.is_ascii_control()) =>
            {
                while i < b.len() && b[i] != b'\n' {
                    i += 1;
                }
            }
            b'/' if b.get(i + 1) == Some(&b'*') => {
                i += 2;
                while i < b.len() && !(b[i] == b'*' && b.get(i + 1) == Some(&b'/')) {
                    i += 1;
                }
                i += 2;
            }
            b';' => return true,
            _ => i += 1,
        }
    }
    false
}

/// Error position relative to the agent's statement instead of the wrapper.
fn shifted(mut e: AppError, prefix: &str) -> AppError {
    let n = prefix.chars().count();
    e.position = e.position.and_then(|p| p.checked_sub(n));
    e
}

/// MySQL 8.0.16+ prints a tree; MariaDB and older MySQL the classic table.
async fn mysql_explain(session: &Session, sql: &str) -> AppResult<String> {
    if session.engine() == Engine::Mysql {
        let prefix = "EXPLAIN FORMAT=TREE ";
        match query(session, &format!("{prefix}{sql}"), PLAN_ROWS).await {
            Ok(r) => return Ok(lines(&r, 0)),
            Err(e) if e.kind == ErrorKind::Database => {}
            Err(e) => return Err(shifted(e, prefix)),
        }
    }
    let prefix = "EXPLAIN ";
    let r = query(session, &format!("{prefix}{sql}"), PLAN_ROWS)
        .await
        .map_err(|e| shifted(e, prefix))?;
    Ok(text_table(
        r.columns.as_deref().unwrap_or_default(),
        &r.rows,
    ))
}

/// `EXPLAIN PLAN` writes the plan to PLAN_TABLE (a session-private
/// temporary table), which a read-only transaction refuses (ORA-01456). In
/// that case the read-only transaction, which holds no changes, is rolled
/// back, the plan is made and read in a fresh transaction that is rolled
/// back too, and the read-only transaction is started again. Otherwise the
/// plan rows are deleted once read.
async fn oracle_explain(session: &Session, sql: &str) -> AppResult<String> {
    let id = format!(
        "LQ_MCP_{}",
        uuid::Uuid::new_v4().simple().to_string()[..16].to_ascii_uppercase()
    );
    let prefix = format!("EXPLAIN PLAN SET STATEMENT_ID = '{id}' FOR ");
    let plan = format!("{prefix}{sql}");
    let display =
        format!("SELECT plan_table_output FROM TABLE(DBMS_XPLAN.DISPLAY(NULL, '{id}', 'TYPICAL'))");
    match query(session, &plan, 0).await {
        Ok(_) => {
            let shown = query(session, &display, PLAN_ROWS).await;
            let _ = query(
                session,
                &format!("DELETE FROM plan_table WHERE statement_id = '{id}'"),
                0,
            )
            .await;
            Ok(lines(&shown?, 0))
        }
        Err(e) if e.code.as_deref() == Some("ORA-01456") => {
            session.end_transaction(false).await?;
            let outcome = match query(session, &plan, 0).await {
                Ok(_) => query(session, &display, PLAN_ROWS).await,
                Err(e) => Err(shifted(e, &prefix)),
            };
            let back = session.end_transaction(false).await;
            let read_only = query(session, "SET TRANSACTION READ ONLY", 0).await;
            let shown = outcome?;
            back?;
            read_only?;
            Ok(lines(&shown, 0))
        }
        Err(e) => Err(shifted(e, &prefix)),
    }
}

/// With SHOWPLAN_ALL on, SQL Server compiles the batch and returns its plan
/// without running anything. SHOWPLAN_TEXT would put the plan in a second
/// result set, which the driver does not return, so the StmtText column of
/// SHOWPLAN_ALL (the same text) is used, with the estimates.
///
/// The setting lasts for the session: if this call is interrupted between
/// ON and OFF, later statements are not executed, so the caller must drop
/// the session after an interrupted explain.
async fn sqlserver_explain(session: &Session, sql: &str) -> AppResult<String> {
    query(session, "SET SHOWPLAN_ALL ON", 0).await?;
    let outcome = query(session, sql, 0).await;
    let off = query(session, "SET SHOWPLAN_ALL OFF", 0).await;
    let r = outcome?;
    off?;
    let cols = r.columns.as_deref().unwrap_or_default();
    let at = |name: &str| cols.iter().position(|c| c.name.eq_ignore_ascii_case(name));
    let (text, rows, cost, kind) = (
        at("StmtText").unwrap_or(0),
        at("EstimateRows"),
        at("TotalSubtreeCost"),
        at("Type"),
    );
    let mut out = Vec::with_capacity(r.rows.len());
    for row in &r.rows {
        let mut line = cell(row, text).trim_end().to_string();
        let operator = kind.is_some_and(|k| cell(row, k) == "PLAN_ROW");
        if let (true, Some(n), Some(c)) = (operator, rows, cost) {
            line.push_str(&format!("  [rows {}, cost {}]", cell(row, n), cell(row, c)));
        }
        out.push(line);
    }
    Ok(out.join("\n"))
}

/// `EXPLAIN QUERY PLAN` rows (id, parent, detail) drawn like the sqlite3
/// shell does.
fn sqlite_plan_tree(rows: &[(i64, i64, String)]) -> String {
    fn walk(
        rows: &[(i64, i64, String)],
        parent: i64,
        prefix: &str,
        depth: usize,
        out: &mut String,
    ) {
        if depth > 64 {
            return;
        }
        let kids: Vec<_> = rows
            .iter()
            .filter(|r| r.1 == parent && r.0 != parent)
            .collect();
        for (k, (id, _, detail)) in kids.iter().enumerate() {
            let last = k + 1 == kids.len();
            out.push('\n');
            out.push_str(prefix);
            out.push_str(if last { "`--" } else { "|--" });
            out.push_str(detail);
            let deeper = format!("{prefix}{}", if last { "   " } else { "|  " });
            walk(rows, *id, &deeper, depth + 1, out);
        }
    }
    let mut out = String::from("QUERY PLAN");
    walk(rows, 0, "", 0, &mut out);
    out
}

/// Rows as an aligned text table with a header.
fn text_table(columns: &[ColumnInfo], rows: &[Vec<Option<String>>]) -> String {
    let show = |v: &Option<String>| v.clone().unwrap_or_else(|| "NULL".into());
    let mut widths: Vec<usize> = columns.iter().map(|c| c.name.chars().count()).collect();
    for row in rows {
        for (i, v) in row.iter().enumerate() {
            if let Some(w) = widths.get_mut(i) {
                *w = (*w).max(show(v).chars().count());
            }
        }
    }
    let line = |cells: Vec<String>| {
        cells
            .iter()
            .zip(&widths)
            .map(|(c, w)| format!("{c:<w$}"))
            .collect::<Vec<_>>()
            .join(" | ")
            .trim_end()
            .to_string()
    };
    let mut out = vec![line(columns.iter().map(|c| c.name.clone()).collect())];
    out.push(
        widths
            .iter()
            .map(|w| "-".repeat(*w))
            .collect::<Vec<_>>()
            .join("-+-"),
    );
    for row in rows {
        out.push(line(row.iter().map(show).collect()));
    }
    out.join("\n")
}

// ---------------------------------------------------------------------------
// Helpers

async fn query(session: &Session, sql: &str, max_rows: usize) -> AppResult<ExecResult> {
    session
        .execute(&ExecRequest {
            sql: sql.to_string(),
            max_rows,
        })
        .await
}

fn cell(row: &[Option<String>], i: usize) -> String {
    row.get(i).cloned().flatten().unwrap_or_default()
}

/// Column `i` of every row, one per line.
fn lines(r: &ExecResult, i: usize) -> String {
    r.rows
        .iter()
        .map(|row| cell(row, i))
        .collect::<Vec<_>>()
        .join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{ConnectionProfile, ValueKind};

    #[test]
    fn literals_per_dialect() {
        assert_eq!(literal(Dialect::Postgres, "o'k").unwrap(), "'o''k'");
        assert_eq!(literal(Dialect::Postgres, "a\\'b").unwrap(), "E'a\\\\''b'");
        assert_eq!(literal(Dialect::Oracle, "it's").unwrap(), "'it''s'");
        assert_eq!(literal(Dialect::Sqlite, "a\\b").unwrap(), "'a\\b'");
        assert_eq!(literal(Dialect::SqlServer, "o'k").unwrap(), "N'o''k'");
        assert_eq!(
            literal(Dialect::Mysql, "a'\\é").unwrap(),
            "_utf8mb4 X'61275CC3A9'"
        );
        assert_eq!(literal(Dialect::Mysql, "").unwrap(), "_utf8mb4 X''");
        for d in [
            Dialect::Postgres,
            Dialect::Oracle,
            Dialect::Mysql,
            Dialect::SqlServer,
            Dialect::Sqlite,
        ] {
            assert!(literal(d, "a\0b").is_err());
        }
    }

    #[test]
    fn like_escaping() {
        assert_eq!(like_escape(Dialect::Postgres, "a_b%c!d[e"), "a!_b!%c!!d[e");
        assert_eq!(like_escape(Dialect::SqlServer, "a[b]_"), "a![b]!_");
        let t = NameTest::new(Dialect::Oracle, "x_y").unwrap();
        assert_eq!(t.filter("n"), "LOWER(n) LIKE LOWER('%x!_y%') ESCAPE '!'");
        assert!(t.rank("n").contains("LOWER(n) = LOWER('x_y')"));
        assert!(t.rank("n").contains("LIKE LOWER('x!_y%') ESCAPE '!'"));
    }

    #[test]
    fn patterns_are_normalized() {
        assert_eq!(normalize_pattern("  %cust*  ").unwrap(), "cust");
        assert_eq!(normalize_pattern("a%b").unwrap(), "a%b");
        assert!(normalize_pattern(" %% ").is_err());
        assert!(normalize_pattern(&"x".repeat(MAX_PATTERN + 1)).is_err());
        assert!(normalize_pattern("a\0").is_err());
    }

    #[test]
    fn search_sql_embeds_only_literals() {
        let evil = "x'; drop table t; --";
        for d in [Dialect::Postgres, Dialect::Mysql, Dialect::SqlServer] {
            let (tables, columns) = search_sql(d, evil, Some("pub'lic")).unwrap();
            for sql in [&tables, &columns] {
                assert!(!sql.contains("x'; drop"), "{d:?}: {sql}");
            }
        }
        let (tables, _) = search_sql(Dialect::Postgres, "cust", Some("public")).unwrap();
        assert!(tables.contains("LIKE LOWER('%cust%') ESCAPE '!'"));
        assert!(tables.contains("CASE WHEN n.nspname::text = 'public' THEN 0"));
        let (_, columns) = search_sql(Dialect::Mysql, "id", None).unwrap();
        assert!(columns.contains("LOWER(_utf8mb4 X'25696425')"));
        let (tables, _) = search_sql(Dialect::SqlServer, "a[1]", None).unwrap();
        assert!(tables.contains("N'%a![1]%'"));

        let (modern, _) = oracle_search_sql("emp", Some("HR"), true).unwrap();
        assert!(modern.contains("oracle_maintained = 'Y'"));
        assert!(modern.contains("o.owner = 'HR' or"));
        let (_, legacy) = oracle_search_sql("emp", None, false).unwrap();
        assert!(legacy.contains("'SYSTEM'") && !legacy.contains("oracle_maintained"));

        let (tables, columns) = sqlite_search_sql("we\"ird", "a'b").unwrap();
        assert!(tables.contains("FROM \"we\"\"ird\".sqlite_schema m"));
        assert!(columns.contains("pragma_table_info(m.name, 'we\"ird')"));
        assert!(columns.contains("'%a''b%'"));
    }

    #[test]
    fn fk_sql_quotes_names() {
        let sql = fk_sql(Dialect::SqlServer, "dbo", "o'rders").unwrap();
        assert!(sql.contains("QUOTENAME(N'dbo') + N'.' + QUOTENAME(N'o''rders')"));
        let sql = fk_sql(Dialect::Mysql, "db", "t").unwrap();
        assert!(sql.contains("TABLE_SCHEMA = _utf8mb4 X'6462' AND TABLE_NAME = _utf8mb4 X'74'"));
        let sql = fk_sql(Dialect::Oracle, "HR", "EMP").unwrap();
        assert!(sql.contains("c.owner = 'HR' and c.table_name = 'EMP'"));
        assert!(sql.contains("r.owner = 'HR' and r.table_name = 'EMP'"));
        let sql = fk_sql(Dialect::Postgres, "public", "a\\b").unwrap();
        assert!(sql.contains("c.relname = E'a\\\\b'"));
    }

    fn result(rows: &[&[&str]]) -> ExecResult {
        ExecResult {
            rows: rows
                .iter()
                .map(|r| r.iter().map(|c| Some(c.to_string())).collect())
                .collect(),
            ..Default::default()
        }
    }

    #[test]
    fn groups_foreign_key_rows() {
        let r = result(&[
            &["out", "fk_a", "s", "t", "s", "p", "1", "a1", "id1"],
            &["out", "fk_a", "s", "t", "s", "p", "2", "a2", "id2"],
            &["out", "fk_b", "s", "t", "x", "q", "1", "b", "id"],
            &["in", "fk_c", "s", "child", "s", "t", "1", "t_id", "id"],
        ]);
        let (out, inc) = group_foreign_keys(&r);
        assert_eq!(out.len(), 2);
        assert_eq!(out[0].columns, ["a1", "a2"]);
        assert_eq!(out[0].ref_columns, ["id1", "id2"]);
        assert_eq!(out[1].ref_schema.as_deref(), Some("x"));
        assert_eq!(inc.len(), 1);
        assert_eq!(inc[0].0.as_deref(), Some("s"));
        assert_eq!(inc[0].1, "child");
        assert_eq!(inc[0].2.ref_table, "t");
    }

    #[test]
    fn ranks_tables_then_exact_then_prefix() {
        let m = |schema: &str, table: &str, column: Option<&str>| ObjectMatch {
            schema: Some(schema.into()),
            table: table.into(),
            kind: "table".into(),
            column: column.map(String::from),
            column_type: None,
        };
        let mut found = vec![
            m("public", "orders", Some("customer")),
            m("public", "old_customer", None),
            m("other", "Customer", None),
            m("public", "customers", None),
            m("public", "customer", None),
        ];
        rank(&mut found, "customer", Some("public"));
        let names: Vec<_> = found
            .iter()
            .map(|f| format!("{}.{}", f.schema.as_deref().unwrap(), f.table))
            .collect();
        assert_eq!(
            names,
            [
                "public.customer",
                "other.Customer",
                "public.customers",
                "public.old_customer",
                "public.orders"
            ]
        );
    }

    #[test]
    fn redis_patterns() {
        assert_eq!(redis_glob("a*1"), "*[aA]\\*1*");
        assert_eq!(redis_glob("[x]?"), "*\\[[xX]\\]\\?*");
        assert_eq!(redis_quote("a\"b\\c\n"), "\"a\\\"b\\\\c\\x0a\"");
        let args = crate::drivers::redis_db::split_args(&format!(
            "SCAN 0 MATCH {}",
            redis_quote("a\"\\x41 b")
        ))
        .unwrap();
        assert_eq!(args[3], b"a\"\\x41 b");
        assert_eq!(
            next_cursor(&["Next cursor: 1234".into()]).as_deref(),
            Some("1234")
        );
        assert_eq!(
            next_cursor(&["Next cursor: 0 (the scan is complete).".into()]).as_deref(),
            Some("0")
        );
    }

    #[test]
    fn explain_accepts_only_queries_and_dml() {
        assert!(check_explainable(Engine::Postgres, "-- c\n select 1").is_ok());
        assert!(check_explainable(Engine::Postgres, "(select 1) union select 2").is_ok());
        assert!(check_explainable(Engine::Postgres, "ANALYZE select 1").is_err());
        assert!(check_explainable(Engine::Postgres, "explain select 1").is_err());
        assert!(check_explainable(Engine::Oracle, "drop table t").is_err());
        assert!(check_explainable(Engine::Sqlite, "  ").is_err());
        assert!(check_explainable(Engine::Mysql, "# c\nselect 1").is_ok());
        assert!(check_explainable(Engine::Mysql, "/*!ANALYZE*/ select 1").is_err());
        assert!(check_explainable(Engine::Mariadb, "select /*M!100000 1 */ 2").is_err());
        assert!(check_explainable(Engine::Mysql, "select 1; drop table t").is_err());
        assert!(check_explainable(Engine::Mysql, "select ';' from t").is_ok());
        // Without backslash escapes the string ends at \' and `;` is outside.
        assert!(check_explainable(Engine::Mysql, r"select 'a\'; drop table t; -- '").is_err());
        assert!(check_explainable(Engine::Mysql, "select 1 -- ; x").is_ok());
        assert!(check_explainable(Engine::Mysql, "select 1 --; x").is_err());
    }

    #[test]
    fn positions_and_terminators() {
        let mut e = AppError::invalid("x");
        e.position = Some(25);
        assert_eq!(shifted(e, "EXPLAIN (FORMAT TEXT) ").position, Some(3));
        assert_eq!(strip_terminator("select 1 ;; \n"), "select 1");
    }

    #[test]
    fn draws_sqlite_plans_and_tables() {
        let rows = vec![
            (2, 0, "SCAN a".to_string()),
            (5, 0, "SEARCH b USING INDEX i (x=?)".to_string()),
            (7, 5, "LIST SUBQUERY 1".to_string()),
            (9, 0, "USE TEMP B-TREE FOR ORDER BY".to_string()),
        ];
        assert_eq!(
            sqlite_plan_tree(&rows),
            "QUERY PLAN\n|--SCAN a\n|--SEARCH b USING INDEX i (x=?)\n|  `--LIST SUBQUERY 1\n`--USE TEMP B-TREE FOR ORDER BY"
        );
        let col = |n: &str| ColumnInfo {
            name: n.into(),
            type_name: String::new(),
            kind: ValueKind::Text,
        };
        assert_eq!(
            text_table(&[col("id"), col("table")], &[vec![Some("1".into()), None]]),
            "id | table\n---+------\n1  | NULL"
        );
    }

    // ---- SQLite, end to end.

    async fn sqlite_session(name: &str) -> (Session, std::path::PathBuf) {
        let path =
            std::env::temp_dir().join(format!("lq-mcp-catalog-{name}-{}.db", std::process::id()));
        let _ = std::fs::remove_file(&path);
        let profile = ConnectionProfile {
            engine: Engine::Sqlite,
            database: path.display().to_string(),
            create_file: true,
            ..Default::default()
        };
        (Session::open(&profile, None).await.unwrap(), path)
    }

    async fn run(s: &Session, sql: &str) -> ExecResult {
        query(s, sql, 0)
            .await
            .unwrap_or_else(|e| panic!("{sql}: {e}"))
    }

    #[tokio::test]
    async fn sqlite_catalog() {
        let (s, path) = sqlite_session("e2e").await;
        run(
            &s,
            "CREATE TABLE lq_mcp_customer (id INTEGER PRIMARY KEY, name TEXT)",
        )
        .await;
        run(
            &s,
            "CREATE TABLE \"lq mcp order\" (id INTEGER PRIMARY KEY, \
             customer_id INTEGER REFERENCES lq_mcp_customer, \"it's\" TEXT)",
        )
        .await;
        run(
            &s,
            "CREATE TABLE lq_mcp_line (order_id INTEGER, n INTEGER, \
             FOREIGN KEY (order_id) REFERENCES \"lq mcp order\" (id))",
        )
        .await;
        run(
            &s,
            "CREATE VIEW lq_mcp_customer_v AS SELECT * FROM lq_mcp_customer",
        )
        .await;
        run(&s, "INSERT INTO lq_mcp_customer VALUES (1, 'a')").await;

        // Tables first (exact, then prefix), then columns.
        let found = search(&s, "LQ_MCP_CUSTOMER", 50).await.unwrap();
        assert_eq!(found[0].table, "lq_mcp_customer");
        assert_eq!(found[0].kind, "table");
        assert_eq!(found[0].schema.as_deref(), Some("main"));
        assert_eq!(found[1].table, "lq_mcp_customer_v");
        assert_eq!(found[1].kind, "view");
        let found = search(&s, "customer", 50).await.unwrap();
        assert!(found[0].column.is_none());
        let col = found
            .iter()
            .find(|m| m.column.as_deref() == Some("customer_id"))
            .expect("column match");
        assert_eq!(col.table, "lq mcp order");
        assert_eq!(col.column_type.as_deref(), Some("INTEGER"));
        assert_eq!(search(&s, "customer", 1).await.unwrap().len(), 1);
        // `_` is literal: "q m" in "lq mcp order" does not match "q_m".
        let found = search(&s, "q_m", 50).await.unwrap();
        assert!(found.iter().all(|m| m.table != "lq mcp order"));
        assert!(found.iter().any(|m| m.table == "lq_mcp_line"));
        // Quotes are data.
        let found = search(&s, "it's", 50).await.unwrap();
        assert_eq!(found[0].column.as_deref(), Some("it's"));
        assert!(search(&s, "%", 50).await.is_err());

        // Foreign keys: implicit target columns resolve to the primary key.
        let (out, inc) = foreign_keys(&s, None, "lq mcp order").await.unwrap();
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].columns, ["customer_id"]);
        assert_eq!(out[0].ref_table, "lq_mcp_customer");
        assert_eq!(out[0].ref_columns, ["id"]);
        assert_eq!(inc.len(), 1);
        assert_eq!(inc[0].1, "lq_mcp_line");
        assert_eq!(inc[0].2.columns, ["order_id"]);
        assert_eq!(inc[0].2.ref_columns, ["id"]);
        let (out, inc) = foreign_keys(&s, Some("main"), "lq_mcp_customer")
            .await
            .unwrap();
        assert!(out.is_empty());
        assert_eq!(inc[0].1, "lq mcp order");
        assert_eq!(inc[0].2.ref_columns, ["id"]);

        // Explain: a tree, nothing runs, only queries and DML.
        let plan = explain(&s, "SELECT * FROM lq_mcp_customer WHERE id = 1;")
            .await
            .unwrap();
        assert!(plan.starts_with("QUERY PLAN\n`--SEARCH"), "{plan}");
        explain(&s, "DELETE FROM lq_mcp_customer").await.unwrap();
        let r = run(&s, "SELECT count(*) FROM lq_mcp_customer").await;
        assert_eq!(r.rows[0][0].as_deref(), Some("1"));
        assert!(explain(&s, "DROP TABLE lq_mcp_customer").await.is_err());
        let e = explain(&s, "SELECT nope FROM lq_mcp_customer")
            .await
            .unwrap_err();
        assert!(e.message.contains("nope"), "{e}");

        // Read-only session, as the MCP server opens it.
        run(&s, "PRAGMA query_only = ON").await;
        assert!(search(&s, "order", 10).await.unwrap().len() >= 2);
        foreign_keys(&s, None, "lq_mcp_line").await.unwrap();
        let plan = explain(&s, "INSERT INTO lq_mcp_customer VALUES (2, 'b')")
            .await
            .unwrap();
        assert!(plan.starts_with("QUERY PLAN"), "{plan}");
        drop(s);
        let _ = std::fs::remove_file(&path);
    }

    // ---- Integration tests against real servers (`--ignored`), with the
    // variables of the driver tests.

    fn env(name: &str) -> Option<String> {
        std::env::var(name).ok().filter(|v| !v.is_empty())
    }

    fn port(name: &str, default: u16) -> u16 {
        env(name).and_then(|p| p.parse().ok()).unwrap_or(default)
    }

    async fn open(profile: Option<(ConnectionProfile, Option<String>)>) -> Option<Session> {
        let (profile, password) = profile?;
        Some(
            Session::open(&profile, password.as_deref())
                .await
                .expect("connection"),
        )
    }

    /// Shared checks: `parent` and `child` exist, `child.parent_id`
    /// references `parent.id`.
    async fn check_catalog(s: &Session, parent: &str, child: &str, query_sql: &str) {
        let found = search(s, "lq_mcp", 50).await.unwrap();
        assert!(
            found
                .iter()
                .any(|m| m.table == parent && m.column.is_none()),
            "{found:?}"
        );
        assert!(
            found.iter().any(|m| m.table == child && m.column.is_none()),
            "{found:?}"
        );
        let found = search(s, "PARENT_ID", 50).await.unwrap();
        let col = found
            .iter()
            .find(|m| m.table == child)
            .unwrap_or_else(|| panic!("{found:?}"));
        assert!(col
            .column
            .as_deref()
            .unwrap()
            .eq_ignore_ascii_case("parent_id"));
        assert!(col.column_type.is_some());
        assert_eq!(
            found[0].column.as_deref().map(str::to_lowercase).as_deref(),
            Some("parent_id")
        );
        assert!(search(s, "x'; --", 5).await.is_ok());

        let (out, inc) = foreign_keys(s, None, child).await.unwrap();
        assert_eq!(out.len(), 1, "{out:?}");
        assert_eq!(out[0].ref_table, parent);
        assert!(out[0].columns[0].eq_ignore_ascii_case("parent_id"));
        assert!(out[0].ref_columns[0].eq_ignore_ascii_case("id"));
        assert!(out[0].name.is_some());
        assert!(inc.is_empty());
        let (out, inc) = foreign_keys(s, None, parent).await.unwrap();
        assert!(out.is_empty());
        assert_eq!(inc.len(), 1);
        assert_eq!(inc[0].1, child);

        let plan = explain(s, query_sql).await.unwrap();
        assert!(!plan.trim().is_empty());
    }

    #[tokio::test]
    #[ignore]
    async fn mcp_postgres_catalog() {
        let profile = env("LQ_PG_HOST").map(|host| {
            (
                ConnectionProfile {
                    engine: Engine::Postgres,
                    host,
                    port: port("LQ_PG_PORT", 5432),
                    user: env("LQ_PG_USER").unwrap_or_else(|| "postgres".into()),
                    database: env("LQ_PG_DB").unwrap_or_else(|| "postgres".into()),
                    ..Default::default()
                },
                env("LQ_PG_PASSWORD"),
            )
        });
        let Some(s) = open(profile).await else {
            return;
        };
        run(&s, "DROP TABLE IF EXISTS \"lq_mcp Child\", lq_mcp_parent").await;
        run(&s, "CREATE TABLE lq_mcp_parent (id int PRIMARY KEY)").await;
        run(
            &s,
            "CREATE TABLE \"lq_mcp Child\" (id int PRIMARY KEY, \
             parent_id int CONSTRAINT lq_mcp_fk REFERENCES lq_mcp_parent (id))",
        )
        .await;
        run(&s, "INSERT INTO lq_mcp_parent VALUES (1)").await;
        check_catalog(
            &s,
            "lq_mcp_parent",
            "lq_mcp Child",
            "SELECT * FROM \"lq_mcp Child\"",
        )
        .await;

        run(&s, "SET SESSION CHARACTERISTICS AS TRANSACTION READ ONLY").await;
        let plan = explain(&s, "DELETE FROM lq_mcp_parent").await.unwrap();
        assert!(plan.contains("Delete on lq_mcp_parent"), "{plan}");
        search(&s, "lq_mcp", 5).await.unwrap();
        run(&s, "SET SESSION CHARACTERISTICS AS TRANSACTION READ WRITE").await;
        let r = run(&s, "SELECT count(*) FROM lq_mcp_parent").await;
        assert_eq!(r.rows[0][0].as_deref(), Some("1"));
        let e = explain(&s, "SELECT nope FROM lq_mcp_parent")
            .await
            .unwrap_err();
        assert_eq!(e.position, Some(7));
        run(&s, "DROP TABLE \"lq_mcp Child\", lq_mcp_parent").await;
    }

    #[tokio::test]
    #[ignore]
    async fn mcp_oracle_catalog() {
        let profile = env("LQ_ORA_HOST").and_then(|host| {
            Some((
                ConnectionProfile {
                    engine: Engine::Oracle,
                    host,
                    port: port("LQ_ORA_PORT", 1521),
                    user: env("LQ_ORA_USER")?,
                    database: env("LQ_ORA_SERVICE").unwrap_or_else(|| "FREEPDB1".into()),
                    ..Default::default()
                },
                env("LQ_ORA_PASSWORD"),
            ))
        });
        let Some(s) = open(profile).await else {
            return;
        };
        for t in ["\"lq_mcp Child\"", "lq_mcp_parent"] {
            let _ = query(&s, &format!("drop table {t} purge"), 0).await;
        }
        run(&s, "create table lq_mcp_parent (id number primary key)").await;
        run(
            &s,
            "create table \"lq_mcp Child\" (id number primary key, \
             parent_id number constraint lq_mcp_fk references lq_mcp_parent (id))",
        )
        .await;
        run(&s, "insert into lq_mcp_parent values (1)").await;
        run(&s, "commit").await;
        check_catalog(
            &s,
            "LQ_MCP_PARENT",
            "lq_mcp Child",
            "select * from \"lq_mcp Child\"",
        )
        .await;

        // As the MCP server runs reads: manual commit, read-only transaction.
        s.set_autocommit(false).await.unwrap();
        run(&s, "set transaction read only").await;
        let plan = explain(&s, "delete from lq_mcp_parent").await.unwrap();
        assert!(plan.contains("DELETE"), "{plan}");
        let e = query(&s, "insert into lq_mcp_parent values (2)", 0)
            .await
            .unwrap_err();
        assert_eq!(e.code.as_deref(), Some("ORA-01456"), "{e}");
        s.end_transaction(false).await.unwrap();
        s.set_autocommit(true).await.unwrap();
        let r = run(
            &s,
            "select count(*) from plan_table where statement_id like 'LQ_MCP_%'",
        )
        .await;
        assert_eq!(r.rows[0][0].as_deref(), Some("0"));
        let r = run(&s, "select count(*) from lq_mcp_parent").await;
        assert_eq!(r.rows[0][0].as_deref(), Some("1"));
        run(&s, "drop table \"lq_mcp Child\" purge").await;
        run(&s, "drop table lq_mcp_parent purge").await;
    }

    #[tokio::test]
    #[ignore]
    async fn mcp_mysql_catalog() {
        let profile = env("LQ_MYSQL_HOST").map(|host| {
            let engine = if env("LQ_MYSQL_ENGINE").as_deref() == Some("mariadb") {
                Engine::Mariadb
            } else {
                Engine::Mysql
            };
            (
                ConnectionProfile {
                    engine,
                    host,
                    port: port("LQ_MYSQL_PORT", 3306),
                    user: env("LQ_MYSQL_USER").unwrap_or_else(|| "root".into()),
                    database: env("LQ_MYSQL_DB").unwrap_or_else(|| "lowquery".into()),
                    ..Default::default()
                },
                env("LQ_MYSQL_PASSWORD"),
            )
        });
        let Some(s) = open(profile).await else {
            return;
        };
        run(&s, "DROP TABLE IF EXISTS `lq_mcp Child`, lq_mcp_parent").await;
        run(
            &s,
            "CREATE TABLE lq_mcp_parent (id int PRIMARY KEY) ENGINE=InnoDB",
        )
        .await;
        run(
            &s,
            "CREATE TABLE `lq_mcp Child` (id int PRIMARY KEY, parent_id int, \
             CONSTRAINT lq_mcp_fk FOREIGN KEY (parent_id) REFERENCES lq_mcp_parent (id)) ENGINE=InnoDB",
        )
        .await;
        run(&s, "INSERT INTO lq_mcp_parent VALUES (1)").await;
        check_catalog(
            &s,
            "lq_mcp_parent",
            "lq_mcp Child",
            "SELECT * FROM `lq_mcp Child`",
        )
        .await;

        run(&s, "SET SESSION TRANSACTION READ ONLY").await;
        explain(&s, "DELETE FROM lq_mcp_parent").await.unwrap();
        // Backslashes and quotes in the pattern stay data in both modes.
        search(&s, "a\\'b", 5).await.unwrap();
        run(
            &s,
            "SET SESSION sql_mode = CONCAT_WS(',', NULLIF(@@sql_mode, ''), 'NO_BACKSLASH_ESCAPES')",
        )
        .await;
        search(&s, "a\\'b", 5).await.unwrap();
        run(&s, "SET SESSION TRANSACTION READ WRITE").await;
        let r = run(&s, "SELECT count(*) FROM lq_mcp_parent").await;
        assert_eq!(r.rows[0][0].as_deref(), Some("1"));
        run(&s, "DROP TABLE `lq_mcp Child`, lq_mcp_parent").await;
    }

    #[tokio::test]
    #[ignore]
    async fn mcp_sqlserver_catalog() {
        let profile = env("LQ_MSSQL_HOST").map(|host| {
            (
                ConnectionProfile {
                    engine: Engine::Sqlserver,
                    host,
                    port: port("LQ_MSSQL_PORT", 1433),
                    user: env("LQ_MSSQL_USER").unwrap_or_else(|| "sa".into()),
                    database: env("LQ_MSSQL_DB").unwrap_or_else(|| "master".into()),
                    ..Default::default()
                },
                Some(env("LQ_MSSQL_PASSWORD").unwrap_or_default()),
            )
        });
        let Some(s) = open(profile).await else {
            return;
        };
        let _ = query(&s, "DROP TABLE IF EXISTS [lq_mcp Child], lq_mcp_parent", 0).await;
        run(&s, "CREATE TABLE lq_mcp_parent (id int PRIMARY KEY)").await;
        run(
            &s,
            "CREATE TABLE [lq_mcp Child] (id int PRIMARY KEY, \
             parent_id int CONSTRAINT lq_mcp_fk REFERENCES lq_mcp_parent (id))",
        )
        .await;
        run(&s, "INSERT INTO lq_mcp_parent VALUES (1)").await;
        check_catalog(
            &s,
            "lq_mcp_parent",
            "lq_mcp Child",
            "SELECT * FROM [lq_mcp Child]",
        )
        .await;

        // Nothing runs under SHOWPLAN, and the session is back to normal.
        let plan = explain(&s, "DELETE FROM lq_mcp_parent").await.unwrap();
        assert!(plan.contains("Delete"), "{plan}");
        s.set_autocommit(false).await.unwrap();
        explain(&s, "SELECT * FROM lq_mcp_parent").await.unwrap();
        s.end_transaction(false).await.unwrap();
        s.set_autocommit(true).await.unwrap();
        let r = run(&s, "SELECT count(*) FROM lq_mcp_parent").await;
        assert_eq!(r.rows[0][0].as_deref(), Some("1"));
        run(&s, "DROP TABLE [lq_mcp Child], lq_mcp_parent").await;
    }

    #[tokio::test]
    #[ignore]
    async fn mcp_redis_search() {
        let profile = env("LQ_REDIS_HOST").map(|host| {
            (
                ConnectionProfile {
                    engine: Engine::Redis,
                    host,
                    port: port("LQ_REDIS_PORT", 6379),
                    database: "0".into(),
                    ssl_mode: crate::model::SslMode::Disable,
                    ..Default::default()
                },
                env("LQ_REDIS_PASSWORD"),
            )
        });
        let Some(s) = open(profile).await else {
            return;
        };
        run(&s, "SET lq_mcp:User:1 a").await;
        run(&s, "SET \"lq_mcp:*star\" b").await;
        let found = search(&s, "LQ_MCP:user", 10).await.unwrap();
        assert!(
            found.iter().any(|m| m.table == "lq_mcp:User:1"),
            "{found:?}"
        );
        let found = search(&s, "mcp:*st", 10).await.unwrap();
        assert_eq!(found.len(), 1, "{found:?}");
        assert_eq!(found[0].kind, "key");
        assert!(foreign_keys(&s, None, "x").await.unwrap().0.is_empty());
        assert!(explain(&s, "GET x").await.is_err());
        run(&s, "DEL lq_mcp:User:1 \"lq_mcp:*star\"").await;
    }
}
