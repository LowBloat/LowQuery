//! The tools agents call: definitions for `tools/list` and their handlers.

use std::path::Path;

use serde_json::{json, Map, Value};

use super::catalog;
use super::format::{self, cut_value, plural, TEXT_CAP};
use super::guard;
use super::server::{CallInfo, Cancel, Kind, Reply, Server};
use crate::error::AppError;
use crate::model::{
    AiAccess, ConnectionProfile, Engine, ExecRequest, ExecResult, OracleConnectType,
};

const READ_ONLY: &str = "This connection is read-only for AI agents. Ask the user to allow changes in LowQuery (connection → AI access).";
const DEFAULT_ROWS: u64 = 100;
const MAX_ROWS: u64 = 1_000;
const DEFAULT_MATCHES: u64 = 50;
const MAX_MATCHES: u64 = 200;

const NAMES: &[&str] = &[
    "list_connections",
    "list_schemas",
    "list_tables",
    "describe_table",
    "search_objects",
    "query",
    "explain",
    "execute",
];

pub fn exists(name: &str) -> bool {
    NAMES.contains(&name)
}

// ---------------------------------------------------------------------------
// Definitions

fn connection_prop() -> Value {
    json!({ "type": "string", "description": "Connection id or name, from list_connections." })
}

fn object(properties: Value, required: &[&str]) -> Value {
    json!({ "type": "object", "properties": properties, "required": required, "additionalProperties": false })
}

fn result_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "columns": {
                "type": "array",
                "items": {
                    "type": "object",
                    "properties": { "name": { "type": "string" }, "type": { "type": "string" } },
                    "required": ["name", "type"]
                }
            },
            "rows": {
                "type": "array",
                "description": "Values as text, null for NULL; long values are cut.",
                "items": { "type": "array", "items": { "type": ["string", "null"] } }
            },
            "rowCount": { "type": "integer", "description": "Rows in this result." },
            "truncated": { "type": "boolean", "description": "More rows exist than are shown." },
            "elapsedMs": { "type": "integer" },
            "rowsAffected": { "type": ["integer", "null"] },
            "notices": { "type": "array", "items": { "type": "string" } }
        },
        "required": ["columns", "rows", "rowCount", "truncated", "elapsedMs"]
    })
}

fn tool(name: &str, title: &str, description: &str, input: Value, output: Value) -> Value {
    let write = name == "execute";
    json!({
        "name": name,
        "title": title,
        "description": description,
        "inputSchema": input,
        "outputSchema": output,
        "annotations": {
            "title": title,
            "readOnlyHint": !write,
            "destructiveHint": write,
            "idempotentHint": !write,
            "openWorldHint": false
        }
    })
}

/// `tools/list` result for the negotiated revision: `title` and
/// `outputSchema` came in 2025-06-18, annotations in 2025-03-26.
pub fn list(version: &str) -> Value {
    let mut tools = vec![
        tool(
            "list_connections",
            "List connections",
            "Lists the database connections the user opened to AI agents: id, name, engine, where it points (never passwords), access (read or write) and notes on the SQL dialect. Call this first.",
            json!({ "type": "object", "additionalProperties": false }),
            json!({
                "type": "object",
                "properties": { "connections": { "type": "array", "items": {
                    "type": "object",
                    "properties": {
                        "id": { "type": "string" }, "name": { "type": "string" },
                        "engine": { "type": "string" }, "where": { "type": "string" },
                        "access": { "type": "string", "enum": ["read", "write"] },
                        "dialect": { "type": "string" }
                    },
                    "required": ["id", "name", "engine", "where", "access", "dialect"]
                } } },
                "required": ["connections"]
            }),
        ),
        tool(
            "list_schemas",
            "List schemas",
            "Lists the schemas (databases on MySQL, attached databases on SQLite, db0... on Redis) of a connection, and the current one.",
            object(json!({ "connection": connection_prop() }), &["connection"]),
            json!({
                "type": "object",
                "properties": {
                    "schemas": { "type": "array", "items": { "type": "string" } },
                    "current": { "type": ["string", "null"] }
                },
                "required": ["schemas"]
            }),
        ),
        tool(
            "list_tables",
            "List tables",
            "Lists the tables, views and other objects of a schema (default: the current schema) with their kind. For large schemas prefer search_objects.",
            object(
                json!({
                    "connection": connection_prop(),
                    "schema": { "type": "string", "description": "Schema to list; default: the current schema." }
                }),
                &["connection"],
            ),
            json!({
                "type": "object",
                "properties": {
                    "schema": { "type": "string" },
                    "tables": { "type": "array", "items": {
                        "type": "object",
                        "properties": { "name": { "type": "string" }, "kind": { "type": "string" } },
                        "required": ["name", "kind"]
                    } },
                    "truncated": { "type": "boolean" }
                },
                "required": ["schema", "tables"]
            }),
        ),
        tool(
            "describe_table",
            "Describe table",
            "Columns of a table or view (name, type, nullable, primary key) and its foreign keys, both outgoing and those referencing it. Read this before writing SQL against a table.",
            object(
                json!({
                    "connection": connection_prop(),
                    "table": { "type": "string", "description": "Table or view name; schema.table also works." },
                    "schema": { "type": "string", "description": "Default: the current schema." }
                }),
                &["connection", "table"],
            ),
            json!({
                "type": "object",
                "properties": {
                    "schema": { "type": "string" },
                    "table": { "type": "string" },
                    "columns": { "type": "array", "items": {
                        "type": "object",
                        "properties": {
                            "name": { "type": "string" }, "type": { "type": "string" },
                            "nullable": { "type": "boolean" }, "primaryKey": { "type": "boolean" }
                        },
                        "required": ["name", "type", "nullable", "primaryKey"]
                    } },
                    "foreignKeys": { "type": "array", "items": { "type": "object" } },
                    "referencedBy": { "type": "array", "items": { "type": "object" } },
                    "foreignKeysError": { "type": "string" }
                },
                "required": ["schema", "table", "columns"]
            }),
        ),
        tool(
            "search_objects",
            "Search objects",
            "Finds tables, views and columns whose name contains the pattern (case-insensitive) across the schemas of a connection; exact and prefix matches first. On Redis, finds keys.",
            object(
                json!({
                    "connection": connection_prop(),
                    "pattern": { "type": "string", "description": "Part of a table or column name." },
                    "limit": { "type": "integer", "minimum": 1, "maximum": MAX_MATCHES, "default": DEFAULT_MATCHES }
                }),
                &["connection", "pattern"],
            ),
            json!({
                "type": "object",
                "properties": { "matches": { "type": "array", "items": {
                    "type": "object",
                    "properties": {
                        "schema": { "type": ["string", "null"] }, "table": { "type": "string" },
                        "kind": { "type": "string" }, "column": { "type": ["string", "null"] },
                        "columnType": { "type": ["string", "null"] }
                    },
                    "required": ["table", "kind"]
                } } },
                "required": ["matches"]
            }),
        ),
        tool(
            "query",
            "Run a query",
            "Runs ONE read statement (SELECT, WITH, SHOW, DESCRIBE, EXPLAIN without ANALYZE, VALUES, TABLE, reading PRAGMAs; on Redis one read command) in the connection's dialect and returns the rows as a table. Rows are capped at max_rows; select only the columns you need and filter in SQL.",
            object(
                json!({
                    "connection": connection_prop(),
                    "sql": { "type": "string", "description": "One read statement; a trailing ; is fine." },
                    "max_rows": { "type": "integer", "minimum": 1, "maximum": MAX_ROWS, "default": DEFAULT_ROWS }
                }),
                &["connection", "sql"],
            ),
            result_schema(),
        ),
        tool(
            "explain",
            "Explain a statement",
            "Shows the engine's execution plan for one statement without running it. Pass the statement itself, not EXPLAIN ...",
            object(
                json!({
                    "connection": connection_prop(),
                    "sql": { "type": "string", "description": "The statement to explain." }
                }),
                &["connection", "sql"],
            ),
            json!({ "type": "object", "properties": { "plan": { "type": "string" } }, "required": ["plan"] }),
        ),
        tool(
            "execute",
            "Execute a change",
            "Runs ONE statement of any kind (INSERT, UPDATE, DELETE, DDL, a procedure call; on Redis one command) on a connection with write access, and commits it. Use only when the user asked for the change. Returns rows affected, notices and any rows. Transaction control (BEGIN, COMMIT...) is refused: each call commits on its own.",
            object(
                json!({
                    "connection": connection_prop(),
                    "sql": { "type": "string", "description": "One statement." },
                    "max_rows": { "type": "integer", "minimum": 1, "maximum": MAX_ROWS, "default": DEFAULT_ROWS }
                }),
                &["connection", "sql"],
            ),
            result_schema(),
        ),
    ];
    for t in &mut tools {
        if let Value::Object(t) = t {
            if version < "2025-06-18" {
                t.remove("title");
                t.remove("outputSchema");
            }
            if version < "2025-03-26" {
                t.remove("annotations");
            }
        }
    }
    json!({ "tools": tools })
}

// ---------------------------------------------------------------------------
// Arguments

fn str_arg<'a>(args: &'a Map<String, Value>, key: &str) -> Result<Option<&'a str>, String> {
    match args.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(s)) if s.trim().is_empty() => Ok(None),
        Some(Value::String(s)) => Ok(Some(s.as_str())),
        Some(_) => Err(format!("Argument {key} must be a string.")),
    }
}

fn required<'a>(args: &'a Map<String, Value>, key: &str) -> Result<&'a str, String> {
    str_arg(args, key)?.ok_or_else(|| format!("Missing argument: {key}."))
}

/// An integer argument (a numeric string also works), clamped to 1..=max.
fn int_arg(args: &Map<String, Value>, key: &str, default: u64, max: u64) -> Result<u64, String> {
    let n = match args.get(key) {
        None | Some(Value::Null) => return Ok(default),
        Some(Value::Number(n)) => n.as_u64().or_else(|| n.as_f64().map(|f| f.max(0.0) as u64)),
        Some(Value::String(s)) => s.trim().parse::<u64>().ok(),
        Some(_) => None,
    };
    n.map(|n| n.clamp(1, max))
        .ok_or_else(|| format!("Argument {key} must be a positive integer."))
}

// ---------------------------------------------------------------------------
// Connections

pub fn engine_label(engine: Engine) -> &'static str {
    match engine {
        Engine::Postgres => "PostgreSQL",
        Engine::Oracle => "Oracle",
        Engine::Mysql => "MySQL",
        Engine::Mariadb => "MariaDB",
        Engine::Sqlserver => "SQL Server",
        Engine::Sqlite => "SQLite",
        Engine::Redis => "Redis",
    }
}

fn dialect_notes(engine: Engine) -> &'static str {
    match engine {
        Engine::Postgres => "PostgreSQL: LIMIT n; double-quote mixed-case names; ILIKE for case-insensitive matches; schema.table names.",
        Engine::Oracle => "Oracle: use FETCH FIRST n ROWS ONLY (no LIMIT); unquoted names are stored uppercase; FROM dual for expressions; DATE 'YYYY-MM-DD' literals; no trailing ; needed.",
        Engine::Mysql => "MySQL: LIMIT n; backtick-quote names; databases are the schemas.",
        Engine::Mariadb => "MariaDB: LIMIT n; backtick-quote names; databases are the schemas.",
        Engine::Sqlserver => "SQL Server (T-SQL): SELECT TOP (n) or ORDER BY ... OFFSET 0 ROWS FETCH NEXT n ROWS ONLY; [bracket] names; schema.table names (dbo.x).",
        Engine::Sqlite => "SQLite: LIMIT n; flexible column types; schemas are attached databases (main).",
        Engine::Redis => "Redis: not SQL; pass one redis-cli command, such as GET key, HGETALL key or SCAN 0 MATCH user:* COUNT 100 (KEYS is refused).",
    }
}

fn with_port(host: &str, port: u16) -> String {
    if port == 0 {
        host.trim().to_string()
    } else {
        format!("{}:{port}", host.trim())
    }
}

/// Where the connection points, without secrets.
fn location(p: &ConnectionProfile) -> String {
    let user = p.user.trim();
    let at = |rest: String| {
        if user.is_empty() {
            rest
        } else {
            format!("{user}@{rest}")
        }
    };
    let db = p.database.trim();
    match p.engine {
        Engine::Sqlite => Path::new(db)
            .file_name()
            .map_or_else(|| db.to_string(), |f| f.to_string_lossy().into_owned()),
        Engine::Oracle => match p.oracle_connect_type {
            OracleConnectType::Tns => at(p.tns_alias.trim().to_string()),
            OracleConnectType::Custom => at("(custom connect descriptor)".into()),
            OracleConnectType::Service | OracleConnectType::Sid => {
                at(format!("{}/{db}", with_port(&p.host, p.port)))
            }
        },
        Engine::Sqlserver => {
            let mut host = p.host.trim().to_string();
            if !p.instance.trim().is_empty() {
                host = format!("{host}\\{}", p.instance.trim());
            } else if p.port != 0 {
                host = with_port(&host, p.port);
            }
            let target = format!("{host}/{db}");
            if p.windows_auth {
                format!("Windows account@{target}")
            } else {
                at(target)
            }
        }
        Engine::Redis => at(format!(
            "{}/db{}",
            with_port(&p.host, p.port),
            if db.is_empty() { "0" } else { db }
        )),
        _ => at(format!("{}/{db}", with_port(&p.host, p.port))),
    }
}

fn access_label(p: &ConnectionProfile) -> &'static str {
    if p.ai_access == AiAccess::Write {
        "write"
    } else {
        "read"
    }
}

/// Finds a connection by id or by name (case-insensitive).
fn resolve(server: &Server, key: &str, info: &mut CallInfo) -> Result<ConnectionProfile, String> {
    let all = server.agent_profiles()?;
    let key = key.trim();
    let found = match all.iter().find(|p| p.id.eq_ignore_ascii_case(key)) {
        Some(p) => p.clone(),
        None => {
            let lower = key.to_lowercase();
            let named: Vec<_> = all
                .iter()
                .filter(|p| p.name.trim().to_lowercase() == lower)
                .collect();
            match named.as_slice() {
                [] => {
                    return Err(format!(
                        "Unknown connection: {key}. Use list_connections to see the connections open to agents."
                    ))
                }
                [p] => (*p).clone(),
                many => {
                    let ids: Vec<_> = many
                        .iter()
                        .map(|p| format!("{} ({}, {})", p.id, engine_label(p.engine), location(p)))
                        .collect();
                    return Err(format!(
                        "Several connections are named {key}; pass one of these ids instead: {}.",
                        ids.join("; ")
                    ));
                }
            }
        }
    };
    info.connection_id = Some(found.id.clone());
    info.connection_name = Some(found.name.clone());
    Ok(found)
}

fn db_error(e: &AppError, sql: Option<&str>) -> String {
    format::error_text(e, sql)
}

// ---------------------------------------------------------------------------
// Handlers

pub async fn run(
    server: &Server,
    name: &str,
    args: &Map<String, Value>,
    cancel: &Cancel,
    info: &mut CallInfo,
) -> Reply {
    let outcome = match name {
        "list_connections" => list_connections(server),
        "list_schemas" => list_schemas(server, args, cancel, info).await,
        "list_tables" => list_tables(server, args, cancel, info).await,
        "describe_table" => describe_table(server, args, cancel, info).await,
        "search_objects" => search_objects(server, args, cancel, info).await,
        "query" => query(server, args, cancel, info).await,
        "explain" => explain(server, args, cancel, info).await,
        "execute" => execute(server, args, cancel, info).await,
        _ => Err(format!("Unknown tool: {name}")),
    };
    outcome.unwrap_or_else(Reply::error)
}

fn list_connections(server: &Server) -> Result<Reply, String> {
    let all = server.agent_profiles()?;
    if all.is_empty() {
        return Ok(Reply::ok(
            "No connections are open to AI agents. Ask the user to turn on AI access for a connection in LowQuery (connection → AI access).".into(),
            json!({ "connections": [] }),
        ).rows(0));
    }
    let mut text = String::new();
    let mut items = Vec::new();
    for p in &all {
        let item = json!({
            "id": p.id,
            "name": p.name,
            "engine": engine_label(p.engine),
            "where": location(p),
            "access": access_label(p),
            "dialect": dialect_notes(p.engine),
        });
        text.push_str(&format!(
            "- {} (id {}): {}, {}, {} access. {}\n",
            p.name,
            p.id,
            engine_label(p.engine),
            location(p),
            access_label(p),
            dialect_notes(p.engine)
        ));
        items.push(item);
    }
    Ok(Reply::ok(text, json!({ "connections": items })).rows(all.len()))
}

async fn list_schemas(
    server: &Server,
    args: &Map<String, Value>,
    cancel: &Cancel,
    info: &mut CallInfo,
) -> Result<Reply, String> {
    let p = resolve(server, required(args, "connection")?, info)?;
    let (schemas, current) = server
        .run(&p, Kind::Read, cancel, |s| async move {
            Ok((s.list_schemas().await?, s.current_schema()))
        })
        .await
        .map_err(|e| db_error(&e, None))?;
    let mut text = format!("{} in {}:\n", plural(schemas.len(), "schema"), p.name);
    for s in &schemas {
        text.push_str(&format!("- {s}\n"));
    }
    if let Some(c) = &current {
        text.push_str(&format!("Current schema: {c}\n"));
    }
    let n = schemas.len();
    Ok(Reply::ok(text, json!({ "schemas": schemas, "current": current })).rows(n))
}

async fn list_tables(
    server: &Server,
    args: &Map<String, Value>,
    cancel: &Cancel,
    info: &mut CallInfo,
) -> Result<Reply, String> {
    let p = resolve(server, required(args, "connection")?, info)?;
    let schema_arg = str_arg(args, "schema")?;
    info.detail = schema_arg.map(str::to_string);
    let (schema, objects) = server
        .run(&p, Kind::Read, cancel, |s| async move {
            let schema = match schema_arg {
                Some(x) => x.trim().to_string(),
                None => s.current_schema().ok_or_else(|| {
                    AppError::invalid(
                        "This connection has no current schema; pass schema (see list_schemas).",
                    )
                })?,
            };
            let objects = s.list_objects(&schema).await?;
            Ok((schema, objects))
        })
        .await
        .map_err(|e| db_error(&e, None))?;
    let mut text = format!("{} in {schema}:\n", plural(objects.len(), "object"));
    let mut kept = Vec::new();
    for o in &objects {
        let line = format!("- {} ({})\n", o.name, o.kind);
        if text.len() + line.len() > TEXT_CAP - 200 {
            break;
        }
        text.push_str(&line);
        kept.push(json!({ "name": o.name, "kind": o.kind }));
    }
    let omitted = objects.len() - kept.len();
    if omitted > 0 {
        text.push_str(&format!(
            "{} not shown; use search_objects to find the ones you need.\n",
            plural(omitted, "more object")
        ));
    }
    let n = kept.len();
    Ok(Reply::ok(
        text,
        json!({ "schema": schema, "tables": kept, "truncated": omitted > 0 }),
    )
    .rows(n))
}

async fn describe_table(
    server: &Server,
    args: &Map<String, Value>,
    cancel: &Cancel,
    info: &mut CallInfo,
) -> Result<Reply, String> {
    let p = resolve(server, required(args, "connection")?, info)?;
    let mut table = required(args, "table")?.trim();
    let mut schema_arg = str_arg(args, "schema")?.map(str::trim);
    if schema_arg.is_none() {
        if let Some((s, t)) = table.split_once('.') {
            if !t.contains('.') && !s.is_empty() && !t.is_empty() {
                schema_arg = Some(s);
                table = t;
            }
        }
    }
    info.detail = Some(match schema_arg {
        Some(s) => format!("{s}.{table}"),
        None => table.to_string(),
    });
    if p.engine == Engine::Redis {
        return Err("Redis keys have no columns. Use query with TYPE key, then GET, HGETALL, LRANGE, SMEMBERS or ZRANGE to read it.".into());
    }
    let (schema, name, columns, fks) = server
        .run(&p, Kind::Read, cancel, |s| async move {
            let schema = match schema_arg {
                Some(x) => x.to_string(),
                None => s.current_schema().ok_or_else(|| {
                    AppError::invalid(
                        "This connection has no current schema; pass schema (see list_schemas).",
                    )
                })?,
            };
            let mut name = table.to_string();
            let mut columns = s.list_columns(&schema, &name).await?;
            if columns.is_empty() {
                // `employees` for EMPLOYEES on Oracle, and similar.
                if let Ok(objects) = s.list_objects(&schema).await {
                    if let Some(o) = objects.iter().find(|o| o.name.eq_ignore_ascii_case(table)) {
                        name = o.name.clone();
                        columns = s.list_columns(&schema, &name).await?;
                    }
                }
            }
            if columns.is_empty() {
                return Err(AppError::not_found(format!(
                    "Table {schema}.{table} was not found (or has no columns). Use list_tables or search_objects."
                )));
            }
            let fks = catalog::foreign_keys(&s, Some(&schema), &name).await;
            Ok((schema, name, columns, fks))
        })
        .await
        .map_err(|e| db_error(&e, None))?;

    let mut text = format!(
        "Table {schema}.{name} ({}):\n| column | type | nullable | key |\n| --- | --- | --- | --- |\n",
        plural(columns.len(), "column")
    );
    let mut cols = Vec::new();
    for c in &columns {
        text.push_str(&format!(
            "| {} | {} | {} | {} |\n",
            c.name,
            c.type_name,
            if c.nullable { "yes" } else { "no" },
            if c.primary_key { "PK" } else { "" }
        ));
        cols.push(json!({
            "name": c.name,
            "type": c.type_name,
            "nullable": c.nullable,
            "primaryKey": c.primary_key,
        }));
    }
    let mut structured = json!({ "schema": schema, "table": name, "columns": cols });
    match fks {
        Ok((outgoing, incoming)) => {
            if !outgoing.is_empty() {
                text.push_str("Foreign keys:\n");
            }
            for fk in &outgoing {
                let target = match &fk.ref_schema {
                    Some(s) => format!("{s}.{}", fk.ref_table),
                    None => fk.ref_table.clone(),
                };
                text.push_str(&format!(
                    "- {}({}) → {target}({})\n",
                    fk.name
                        .as_deref()
                        .map(|n| format!("{n}: "))
                        .unwrap_or_default(),
                    fk.columns.join(", "),
                    fk.ref_columns.join(", ")
                ));
            }
            if !incoming.is_empty() {
                text.push_str("Referenced by:\n");
            }
            let mut by = Vec::new();
            for (s, t, fk) in &incoming {
                let source = match s {
                    Some(s) => format!("{s}.{t}"),
                    None => t.clone(),
                };
                text.push_str(&format!(
                    "- {source}({}) → ({}){}\n",
                    fk.columns.join(", "),
                    fk.ref_columns.join(", "),
                    fk.name
                        .as_deref()
                        .map(|n| format!(" [{n}]"))
                        .unwrap_or_default()
                ));
                by.push(json!({
                    "schema": s, "table": t, "name": fk.name,
                    "columns": fk.columns, "refColumns": fk.ref_columns,
                }));
            }
            structured["foreignKeys"] = serde_json::to_value(&outgoing).unwrap_or_default();
            structured["referencedBy"] = Value::Array(by);
        }
        Err(e) => {
            text.push_str(&format!("Foreign keys unavailable: {}\n", e.message));
            structured["foreignKeysError"] = Value::String(e.message);
        }
    }
    let n = columns.len();
    Ok(Reply::ok(text, structured).rows(n))
}

async fn search_objects(
    server: &Server,
    args: &Map<String, Value>,
    cancel: &Cancel,
    info: &mut CallInfo,
) -> Result<Reply, String> {
    let p = resolve(server, required(args, "connection")?, info)?;
    let pattern = required(args, "pattern")?;
    let limit = int_arg(args, "limit", DEFAULT_MATCHES, MAX_MATCHES)? as usize;
    info.detail = Some(pattern.to_string());
    let matches = server
        .run(&p, Kind::Read, cancel, |s| async move {
            catalog::search(&s, pattern, limit).await
        })
        .await
        .map_err(|e| db_error(&e, None))?;
    let mut text = format!("{} for \"{pattern}\":\n", plural(matches.len(), "match"));
    for m in &matches {
        let table = match &m.schema {
            Some(s) => format!("{s}.{}", m.table),
            None => m.table.clone(),
        };
        match &m.column {
            Some(c) => text.push_str(&format!(
                "- column {table}.{c}{} ({})\n",
                m.column_type
                    .as_deref()
                    .map(|t| format!(" {t}"))
                    .unwrap_or_default(),
                m.kind
            )),
            None => text.push_str(&format!("- {table} ({})\n", m.kind)),
        }
    }
    if matches.len() >= limit {
        text.push_str("More may exist; use a longer pattern or raise limit.\n");
    }
    let n = matches.len();
    Ok(Reply::ok(text, json!({ "matches": matches })).rows(n))
}

/// The text to send after the checks of `query` (read) or `execute`.
fn checked_sql(p: &ConnectionProfile, sql: &str, write: bool) -> Result<String, String> {
    let refusal = |why: String| {
        if write {
            why
        } else if p.ai_access == AiAccess::Read {
            format!("{why} {READ_ONLY}")
        } else {
            format!("{why} query runs read statements only; use execute for changes.")
        }
    };
    if p.engine == Engine::Redis {
        guard::check_redis(sql, write).map_err(refusal)?;
        return Ok(sql.trim().to_string());
    }
    let stmt = guard::one_statement(sql, p.engine)?;
    if write {
        guard::check_write(&stmt, p.engine)?;
    } else {
        guard::check_read(&stmt, p.engine).map_err(refusal)?;
    }
    Ok(stmt.sql)
}

fn render(res: &ExecResult, max_rows: usize, write: bool) -> Reply {
    let mut text = String::new();
    let mut structured = Map::new();
    let mut count = res.rows_affected.unwrap_or(0) as usize;
    match &res.columns {
        Some(cols) => {
            let (table, kept) = format::table(cols, &res.rows, TEXT_CAP - 2_000);
            text.push_str(&table);
            let omitted = res.rows.len() - kept.len();
            let shown = kept.len();
            if omitted > 0 {
                text.push_str(&format!(
                    "Showing {shown} of {} fetched; {} omitted to keep the reply short. Narrow the query (fewer columns, a WHERE clause) to see the rest.\n",
                    plural(res.rows.len(), "row"),
                    plural(omitted, "row")
                ));
            } else if res.truncated {
                text.push_str(&format!(
                    "{} shown; more rows exist (max_rows {max_rows}). Narrow the query, aggregate, or raise max_rows (at most {MAX_ROWS}).\n",
                    plural(shown, "row")
                ));
            } else {
                text.push_str(&format!(
                    "{} ({} ms).\n",
                    plural(shown, "row"),
                    res.elapsed_ms
                ));
            }
            if !write || res.rows_affected.is_none() {
                count = shown;
            }
            structured.insert(
                "columns".into(),
                cols.iter()
                    .map(|c| json!({ "name": c.name, "type": c.type_name }))
                    .collect(),
            );
            structured.insert("rowCount".into(), json!(shown));
            structured.insert("truncated".into(), json!(res.truncated || omitted > 0));
            structured.insert("rows".into(), json!(kept));
        }
        None => {
            match res.rows_affected {
                Some(n) => text.push_str(&format!(
                    "{} affected ({} ms).\n",
                    plural(n as usize, "row"),
                    res.elapsed_ms
                )),
                None => text.push_str(&format!("Done ({} ms).\n", res.elapsed_ms)),
            }
            structured.insert("columns".into(), json!([]));
            structured.insert("rows".into(), json!([]));
            structured.insert("rowCount".into(), json!(0));
            structured.insert("truncated".into(), json!(false));
        }
    }
    if let (Some(n), true) = (res.rows_affected, res.columns.is_some()) {
        text.push_str(&format!("{} affected.\n", plural(n as usize, "row")));
    }
    if !res.notices.is_empty() {
        text.push_str("Notices:\n");
        for n in &res.notices {
            text.push_str(&format!("- {}\n", cut_value(n)));
        }
    }
    structured.insert("elapsedMs".into(), json!(res.elapsed_ms));
    if write {
        structured.insert("rowsAffected".into(), json!(res.rows_affected));
    }
    structured.insert(
        "notices".into(),
        json!(res.notices.iter().map(|n| cut_value(n)).collect::<Vec<_>>()),
    );
    Reply::ok(text, Value::Object(structured)).rows(count)
}

async fn query(
    server: &Server,
    args: &Map<String, Value>,
    cancel: &Cancel,
    info: &mut CallInfo,
) -> Result<Reply, String> {
    let sql = required(args, "sql")?;
    info.detail = Some(sql.to_string());
    let p = resolve(server, required(args, "connection")?, info)?;
    let max_rows = int_arg(args, "max_rows", DEFAULT_ROWS, MAX_ROWS)? as usize;
    let send = checked_sql(&p, sql, false)?;
    let req = ExecRequest {
        sql: send,
        max_rows,
    };
    let req = &req;
    let res = server
        .run(
            &p,
            Kind::Read,
            cancel,
            |s| async move { s.execute(req).await },
        )
        .await
        .map_err(|e| db_error(&e, Some(&req.sql)))?;
    Ok(render(&res, max_rows, false))
}

async fn explain(
    server: &Server,
    args: &Map<String, Value>,
    cancel: &Cancel,
    info: &mut CallInfo,
) -> Result<Reply, String> {
    let sql = required(args, "sql")?;
    info.detail = Some(sql.to_string());
    let p = resolve(server, required(args, "connection")?, info)?;
    let send = if p.engine == Engine::Redis {
        sql.trim().to_string()
    } else {
        let stmt = guard::one_statement(sql, p.engine)?;
        guard::check_explain(&stmt)?;
        stmt.sql
    };
    let send = &send;
    let plan = server
        .run(&p, Kind::Explain, cancel, |s| async move {
            catalog::explain(&s, send).await
        })
        .await
        .map_err(|e| {
            let mut text = db_error(&e, Some(send));
            if p.engine == Engine::Oracle && send.contains(':') {
                text.push_str(
                    "\nIf the statement has :name placeholders, write literal values in their place for explain.",
                );
            }
            text
        })?;
    let mut text = plan.clone();
    if text.len() > TEXT_CAP {
        let cut: String = text.chars().take(TEXT_CAP).collect();
        text = format!("{cut}\n… (plan cut; {} chars in all)", plan.chars().count());
    }
    let lines = plan.lines().count();
    Ok(Reply::ok(text.clone(), json!({ "plan": text })).rows(lines))
}

async fn execute(
    server: &Server,
    args: &Map<String, Value>,
    cancel: &Cancel,
    info: &mut CallInfo,
) -> Result<Reply, String> {
    let sql = required(args, "sql")?;
    info.detail = Some(sql.to_string());
    let p = resolve(server, required(args, "connection")?, info)?;
    if p.ai_access != AiAccess::Write {
        return Err(format!("{READ_ONLY} Use query for read statements."));
    }
    let max_rows = int_arg(args, "max_rows", DEFAULT_ROWS, MAX_ROWS)? as usize;
    let send = checked_sql(&p, sql, true)?;
    let req = ExecRequest {
        sql: send,
        max_rows,
    };
    let req = &req;
    let res = server
        .run(
            &p,
            Kind::Write,
            cancel,
            |s| async move { s.execute(req).await },
        )
        .await
        .map_err(|e| db_error(&e, Some(&req.sql)))?;
    Ok(render(&res, max_rows, true))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{ColumnInfo, ValueKind};

    fn profile(engine: Engine) -> ConnectionProfile {
        ConnectionProfile {
            id: "x".into(),
            name: "X".into(),
            engine,
            host: "db.local".into(),
            port: 5432,
            database: "app".into(),
            user: "ann".into(),
            ..Default::default()
        }
    }

    #[test]
    fn locations_never_hold_secrets() {
        assert_eq!(
            location(&profile(Engine::Postgres)),
            "ann@db.local:5432/app"
        );
        let mut lite = profile(Engine::Sqlite);
        lite.database = r"C:\data\shop.db".into();
        if cfg!(windows) {
            assert_eq!(location(&lite), "shop.db");
        }
        let mut ora = profile(Engine::Oracle);
        ora.oracle_connect_type = OracleConnectType::Tns;
        ora.tns_alias = "PROD".into();
        assert_eq!(location(&ora), "ann@PROD");
        let mut ms = profile(Engine::Sqlserver);
        ms.instance = "SQLEXPRESS".into();
        ms.windows_auth = true;
        assert_eq!(location(&ms), "Windows account@db.local\\SQLEXPRESS/app");
    }

    #[test]
    fn integer_arguments_are_clamped() {
        let args: Map<String, Value> =
            serde_json::from_value(json!({ "a": 5000, "b": "20", "c": "x", "d": 0 })).unwrap();
        assert_eq!(int_arg(&args, "a", 100, 1000).unwrap(), 1000);
        assert_eq!(int_arg(&args, "b", 100, 1000).unwrap(), 20);
        assert!(int_arg(&args, "c", 100, 1000).is_err());
        assert_eq!(int_arg(&args, "d", 100, 1000).unwrap(), 1);
        assert_eq!(int_arg(&args, "none", 100, 1000).unwrap(), 100);
    }

    #[test]
    fn results_note_truncation() {
        let res = ExecResult {
            columns: Some(vec![ColumnInfo {
                name: "n".into(),
                type_name: "int".into(),
                kind: ValueKind::Number,
            }]),
            rows: vec![vec![Some("1".into())], vec![Some("2".into())]],
            truncated: true,
            ..Default::default()
        };
        let r = render(&res, 2, false);
        assert!(r
            .text
            .contains("2 rows shown; more rows exist (max_rows 2)"));
        let s = r.structured.unwrap();
        assert_eq!(s["truncated"], true);
        assert_eq!(s["rowCount"], 2);
        assert!(s.get("rowsAffected").is_none());

        // Wide results are cut to the text budget and say how many rows
        // were left out.
        let wide = ExecResult {
            columns: res.columns.clone(),
            rows: (0..1000).map(|_| vec![Some("x".repeat(900))]).collect(),
            ..Default::default()
        };
        let r = render(&wide, 1000, false);
        assert!(r.text.len() <= TEXT_CAP);
        assert!(r.text.contains("omitted to keep the reply short"));
        assert_eq!(r.structured.unwrap()["truncated"], true);

        let dml = ExecResult {
            rows_affected: Some(3),
            ..Default::default()
        };
        let r = render(&dml, 100, true);
        assert!(r.text.starts_with("3 rows affected"));
        assert_eq!(r.structured.unwrap()["rowsAffected"], 3);
        assert_eq!(r.rows, Some(3));
    }

    #[test]
    fn read_refusals_name_the_access() {
        let mut p = profile(Engine::Postgres);
        p.ai_access = AiAccess::Read;
        let e = checked_sql(&p, "delete from t", false).unwrap_err();
        assert!(e.ends_with(READ_ONLY));
        p.ai_access = AiAccess::Write;
        let e = checked_sql(&p, "delete from t", false).unwrap_err();
        assert!(e.contains("use execute"));
        assert_eq!(
            checked_sql(&p, "delete from t;", true).unwrap(),
            "delete from t"
        );
        let mut r = profile(Engine::Redis);
        r.ai_access = AiAccess::Read;
        assert!(checked_sql(&r, "DEL k", false)
            .unwrap_err()
            .ends_with(READ_ONLY));
    }
}
