//! The MCP server against real servers, one test per engine, through the
//! in-memory client. They run only with the variables of the driver
//! integration tests and skip quietly without them:
//!
//! - PostgreSQL: `LQ_PG_HOST`, `LQ_PG_PORT`, `LQ_PG_USER`, `LQ_PG_PASSWORD`, `LQ_PG_DB`
//! - Oracle: `LQ_ORA_HOST`, `LQ_ORA_PORT`, `LQ_ORA_SERVICE`, `LQ_ORA_USER`, `LQ_ORA_PASSWORD`
//! - MySQL and MariaDB: `LQ_MYSQL_HOST`, `LQ_MYSQL_PORT`, `LQ_MYSQL_USER`,
//!   `LQ_MYSQL_PASSWORD`, `LQ_MYSQL_DB`, `LQ_MYSQL_ENGINE`
//! - SQL Server: `LQ_MSSQL_HOST`, `LQ_MSSQL_PORT`, `LQ_MSSQL_USER`, `LQ_MSSQL_PASSWORD`, `LQ_MSSQL_DB`
//! - Redis: `LQ_REDIS_HOST`, `LQ_REDIS_PORT`, `LQ_REDIS_PASSWORD`
//!
//! `cargo test --lib mcp_postgres_server -- --ignored --test-threads=1`
//!
//! Each test writes a `connections.json` with a read, an off and a write
//! profile for the same server, creates `lq_mcp_e2e_*` objects through the
//! write connection and drops them at the end, also when a check fails.

use std::future::Future;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use serde_json::{json, Value};

use super::audit::{self, AuditEntry};
use super::server::tests::Client;
use super::server::{set_test_password, Cancel, Kind, Server};
use crate::drivers::Session;
use crate::error::AppResult;
use crate::model::{
    AiAccess, ConnectionProfile, Engine, ExecRequest, ExecResult, OracleConnectType, SslMode,
};

/// Time limit of the main client.
const NORMAL: Duration = Duration::from_secs(30);
/// Time limit of the second client, for the slow calls.
const SHORT: Duration = Duration::from_secs(3);
const READ_ONLY: &str = "read-only for AI agents";
const PARENT: &str = "lq_mcp_e2e_parent";
/// The child table's name, mixed case with a space.
const CHILD: &str = "lq_mcp_e2e Child";
const UPDATE: &str = "UPDATE lq_mcp_e2e_parent SET name = 'uno' WHERE id = 1";

// ---------------------------------------------------------------------------
// Profiles from the environment

fn env(name: &str) -> Option<String> {
    std::env::var(name).ok().filter(|v| !v.is_empty())
}

fn port(name: &str, default: u16) -> u16 {
    env(name).and_then(|p| p.parse().ok()).unwrap_or(default)
}

type Target = (ConnectionProfile, Option<String>);

fn pg_target() -> Option<Target> {
    Some((
        ConnectionProfile {
            engine: Engine::Postgres,
            host: env("LQ_PG_HOST")?,
            port: port("LQ_PG_PORT", 5432),
            user: env("LQ_PG_USER").unwrap_or_else(|| "postgres".into()),
            database: env("LQ_PG_DB").unwrap_or_else(|| "postgres".into()),
            ssl_mode: SslMode::Prefer,
            ..Default::default()
        },
        env("LQ_PG_PASSWORD"),
    ))
}

fn ora_target() -> Option<Target> {
    Some((
        ConnectionProfile {
            engine: Engine::Oracle,
            host: env("LQ_ORA_HOST")?,
            port: port("LQ_ORA_PORT", 1521),
            user: env("LQ_ORA_USER")?,
            database: env("LQ_ORA_SERVICE").unwrap_or_else(|| "FREEPDB1".into()),
            oracle_connect_type: OracleConnectType::Service,
            ..Default::default()
        },
        env("LQ_ORA_PASSWORD"),
    ))
}

fn mysql_target() -> Option<Target> {
    let engine = if env("LQ_MYSQL_ENGINE").as_deref() == Some("mariadb") {
        Engine::Mariadb
    } else {
        Engine::Mysql
    };
    Some((
        ConnectionProfile {
            engine,
            host: env("LQ_MYSQL_HOST")?,
            port: port("LQ_MYSQL_PORT", 3306),
            user: env("LQ_MYSQL_USER").unwrap_or_else(|| "root".into()),
            database: env("LQ_MYSQL_DB").unwrap_or_else(|| "lowquery".into()),
            ssl_mode: SslMode::Prefer,
            ..Default::default()
        },
        env("LQ_MYSQL_PASSWORD"),
    ))
}

fn mssql_target() -> Option<Target> {
    Some((
        ConnectionProfile {
            engine: Engine::Sqlserver,
            host: env("LQ_MSSQL_HOST")?,
            port: port("LQ_MSSQL_PORT", 1433),
            user: env("LQ_MSSQL_USER").unwrap_or_else(|| "sa".into()),
            database: env("LQ_MSSQL_DB").unwrap_or_else(|| "master".into()),
            ssl_mode: SslMode::Prefer,
            ..Default::default()
        },
        Some(env("LQ_MSSQL_PASSWORD").unwrap_or_default()),
    ))
}

fn redis_target() -> Option<Target> {
    Some((
        ConnectionProfile {
            engine: Engine::Redis,
            host: env("LQ_REDIS_HOST")?,
            port: port("LQ_REDIS_PORT", 6379),
            database: "0".into(),
            ssl_mode: SslMode::Disable,
            ..Default::default()
        },
        env("LQ_REDIS_PASSWORD"),
    ))
}

// ---------------------------------------------------------------------------
// Harness

fn text(result: &Value) -> &str {
    result["content"][0]["text"].as_str().unwrap_or_default()
}

fn is_error(result: &Value) -> bool {
    result["isError"] == json!(true)
}

/// Rows of a result as text, `None` for NULL.
fn rows_of(result: &Value) -> Vec<Vec<Option<String>>> {
    result["structuredContent"]["rows"]
        .as_array()
        .map(|rows| {
            rows.iter()
                .map(|r| {
                    r.as_array()
                        .into_iter()
                        .flatten()
                        .map(|v| v.as_str().map(str::to_string))
                        .collect()
                })
                .collect()
        })
        .unwrap_or_default()
}

fn row(values: &[Option<&str>]) -> Vec<Option<String>> {
    values.iter().map(|v| v.map(str::to_string)).collect()
}

fn eq(a: &Value, b: &str) -> bool {
    a.as_str().is_some_and(|a| a.eq_ignore_ascii_case(b))
}

async fn exec(s: &Session, sql: &str) -> AppResult<ExecResult> {
    s.execute(&ExecRequest {
        sql: sql.into(),
        max_rows: 0,
    })
    .await
}

/// Runs `statements` on a session of its own, ignoring errors.
async fn run_all(target: &Target, statements: &[&str]) {
    let s = Session::open(&target.0, target.1.as_deref())
        .await
        .expect("connect for cleanup");
    for sql in statements {
        let _ = exec(&s, sql).await;
    }
}

/// Drops leftovers, runs `body` and drops what it created, also when it
/// panicked.
async fn guarded(
    target: &Target,
    cleanup: &[&str],
    body: impl Future<Output = ()> + Send + 'static,
) {
    let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
    run_all(target, cleanup).await;
    let outcome = tokio::spawn(body).await;
    run_all(target, cleanup).await;
    if let Err(e) = outcome {
        match e.try_into_panic() {
            Ok(panic) => std::panic::resume_unwind(panic),
            Err(e) => panic!("{e}"),
        }
    }
}

/// Two clients on one `connections.json`: `c` with the normal time limit,
/// `short` with [`SHORT`].
struct E2e {
    dir: PathBuf,
    c: Client,
    short: Client,
    server: Arc<Server>,
    ro: String,
    rw: String,
    ro_name: String,
    rw_name: String,
    off_name: String,
}

impl E2e {
    async fn start(target: &Target, tag: &str) -> Self {
        let run = uuid::Uuid::new_v4().simple().to_string()[..8].to_string();
        let dir = std::env::temp_dir().join(format!("lq-mcp-e2e-{tag}-{run}"));
        std::fs::create_dir_all(&dir).unwrap();
        let profile = |suffix: &str, ai_access| ConnectionProfile {
            id: format!("e2e-{tag}-{suffix}-{run}"),
            name: format!("E2E {tag} {suffix}"),
            ai_access,
            ..target.0.clone()
        };
        let ro = profile("read", AiAccess::Read);
        let off = profile("off", AiAccess::Off);
        let rw = profile("write", AiAccess::Write);
        for p in [&ro, &off, &rw] {
            set_test_password(&p.id, target.1.as_deref());
        }
        std::fs::write(
            dir.join("connections.json"),
            serde_json::to_string(&[&ro, &off, &rw]).unwrap(),
        )
        .unwrap();
        let (mut c, server) = Client::start(dir.clone(), NORMAL);
        let (mut short, _) = Client::start(dir.clone(), SHORT);
        c.initialize("2025-11-25").await;
        short.initialize("2025-11-25").await;
        Self {
            dir,
            c,
            short,
            server,
            ro: ro.id,
            rw: rw.id,
            ro_name: ro.name,
            rw_name: rw.name,
            off_name: off.name,
        }
    }

    async fn ok(&mut self, tool: &str, args: Value) -> Value {
        let r = self.c.call(tool, args.clone()).await;
        assert!(!is_error(&r), "{tool} {args}: {}", text(&r));
        r
    }

    /// Text of a refused call.
    async fn refused(&mut self, tool: &str, args: Value) -> String {
        let r = self.c.call(tool, args.clone()).await;
        assert!(is_error(&r), "{tool} {args} was not refused: {}", text(&r));
        text(&r).to_string()
    }

    async fn query(&mut self, connection: &str, sql: &str) -> Vec<Vec<Option<String>>> {
        rows_of(
            &self
                .ok("query", json!({ "connection": connection, "sql": sql }))
                .await,
        )
    }

    async fn read(&mut self, sql: &str) -> Vec<Vec<Option<String>>> {
        let ro = self.ro.clone();
        self.query(&ro, sql).await
    }

    /// `execute` on the write connection; the structured result.
    async fn write(&mut self, sql: &str) -> Value {
        let rw = self.rw.clone();
        self.ok("execute", json!({ "connection": rw, "sql": sql }))
            .await["structuredContent"]
            .clone()
    }

    /// One value read through the write connection.
    async fn value(&mut self, sql: &str) -> Option<String> {
        let rw = self.rw.clone();
        let rows = self.query(&rw, sql).await;
        rows.first().and_then(|r| r.first().cloned()).flatten()
    }

    /// Runs `op` on the agent's session of the read connection, as tools do.
    async fn on_read<T, F, Fut>(&self, kind: Kind, op: F) -> AppResult<T>
    where
        F: Fn(Arc<Session>) -> Fut,
        Fut: Future<Output = AppResult<T>>,
    {
        let profile = self
            .server
            .agent_profiles()
            .unwrap()
            .into_iter()
            .find(|p| p.id == self.ro)
            .unwrap();
        self.server
            .run(&profile, kind, &Cancel::default(), op)
            .await
    }

    /// Read and write are listed, off is not and cannot be used.
    async fn check_connections(&mut self) {
        let list = self.ok("list_connections", json!({})).await;
        let conns = list["structuredContent"]["connections"].as_array().unwrap();
        let names: Vec<_> = conns.iter().map(|c| c["name"].as_str().unwrap()).collect();
        assert_eq!(names, [self.ro_name.as_str(), self.rw_name.as_str()]);
        assert_eq!(conns[0]["access"], "read");
        assert_eq!(conns[1]["access"], "write");
        assert!(!text(&list).contains(&self.off_name));
        let off = self.off_name.clone();
        let e = self
            .refused("list_schemas", json!({ "connection": off }))
            .await;
        assert!(e.contains("Unknown connection"), "{e}");
    }

    /// A call on the short client that runs past its limit, then a quick one
    /// on the same connection.
    async fn check_time_limit(&mut self, connection: &str, tool: &str, slow: &str, quick: &str) {
        let started = Instant::now();
        let r = self
            .short
            .call(tool, json!({ "connection": connection, "sql": slow }))
            .await;
        assert!(
            is_error(&r) && text(&r).contains("longer than 3 s"),
            "{slow}: {}",
            text(&r)
        );
        assert!(started.elapsed() < Duration::from_secs(20), "{slow}");
        let r = self
            .short
            .call("query", json!({ "connection": connection, "sql": quick }))
            .await;
        assert!(!is_error(&r), "after the time limit: {}", text(&r));
    }

    fn log(&self) -> Vec<AuditEntry> {
        let log = audit::read_recent(&self.dir, 10_000).unwrap();
        assert!(log.iter().all(|e| e.client == "test-client 1.0"));
        log
    }

    /// Entries every test leaves in the activity log.
    fn check_log(&self, update: &str) {
        let log = self.log();
        let has = |what: &str, f: &dyn Fn(&AuditEntry) -> bool| {
            assert!(log.iter().any(f), "no log entry for {what}: {log:#?}");
        };
        let ro = |e: &AuditEntry| {
            e.connection_id.as_deref() == Some(self.ro.as_str())
                && e.connection_name.as_deref() == Some(self.ro_name.as_str())
        };
        let rw = |e: &AuditEntry| {
            e.connection_id.as_deref() == Some(self.rw.as_str())
                && e.connection_name.as_deref() == Some(self.rw_name.as_str())
        };
        has("list_connections", &|e| {
            e.tool == "list_connections" && e.ok && e.connection_id.is_none()
        });
        has("search_objects", &|e| {
            e.tool == "search_objects" && e.ok && ro(e)
        });
        has("a read", &|e| {
            e.tool == "query" && e.ok && ro(e) && e.rows.is_some()
        });
        has("the update", &|e| {
            e.tool == "execute"
                && e.ok
                && rw(e)
                && e.rows == Some(1)
                && e.detail.as_deref() == Some(update)
        });
        has("a refused change", &|e| {
            e.tool == "query"
                && !e.ok
                && ro(e)
                && e.error.as_deref().is_some_and(|m| m.contains(READ_ONLY))
        });
        has("execute on the read connection", &|e| {
            e.tool == "execute" && !e.ok && ro(e)
        });
        has("the time limit", &|e| {
            !e.ok
                && e.connection_id.is_some()
                && e.error
                    .as_deref()
                    .is_some_and(|m| m.contains("longer than"))
        });
        has("the off connection", &|e| {
            !e.ok && e.connection_id.is_none() && e.tool == "list_schemas"
        });
    }
}

// ---------------------------------------------------------------------------
// SQL engines

/// What differs between SQL engines in the shared checks.
struct Sql {
    tag: &'static str,
    /// The child table as written in SQL.
    child: &'static str,
    /// Creates the parent and child tables and fills them: parents 1 one,
    /// 2 two, 3 three; children 10→1 'a', 11→1 NULL, 12→2 'c'.
    setup: &'static [&'static str],
    /// Drops everything the test may create.
    cleanup: &'static [&'static str],
    /// Refused by the read classifier on this engine only.
    refused_reads: &'static [&'static str],
    /// Transaction control refused by execute.
    tx_control: &'static [&'static str],
    /// A read that returns one row.
    quick: &'static str,
    /// Part of the plan of `DELETE FROM lq_mcp_e2e_parent WHERE id = 3`.
    delete_plan: Option<&'static str>,
}

/// From the connection list to the classifier refusals, on fresh tables.
async fn sql_common(t: &mut E2e, sql: &Sql) {
    let ro = t.ro.clone();
    let rw = t.rw.clone();
    t.check_connections().await;

    // Setup through the write connection.
    for stmt in sql.setup {
        let r = t.write(stmt).await;
        if stmt.trim_start().to_ascii_lowercase().starts_with("insert") {
            assert_eq!(r["rowsAffected"], 3, "{stmt}");
        }
    }

    // Catalog.
    let d = t
        .ok(
            "describe_table",
            json!({ "connection": ro, "table": CHILD }),
        )
        .await;
    let s = &d["structuredContent"];
    assert_eq!(s["table"], CHILD);
    let cols = s["columns"].as_array().unwrap();
    let names: Vec<_> = cols
        .iter()
        .map(|c| c["name"].as_str().unwrap().to_ascii_lowercase())
        .collect();
    assert_eq!(names, ["id", "parent_id", "note"], "{s}");
    assert_eq!(cols[0]["primaryKey"], true, "{s}");
    assert_eq!(cols[1]["primaryKey"], false, "{s}");
    assert_eq!(cols[1]["nullable"], false, "{s}");
    assert_eq!(cols[2]["nullable"], true, "{s}");
    let fks = s["foreignKeys"].as_array().expect("foreign keys");
    assert_eq!(fks.len(), 1, "{s}");
    assert!(eq(&fks[0]["name"], "lq_mcp_e2e_fk"), "{s}");
    assert!(eq(&fks[0]["refTable"], PARENT), "{s}");
    assert!(eq(&fks[0]["columns"][0], "parent_id"), "{s}");
    assert!(eq(&fks[0]["refColumns"][0], "id"), "{s}");
    assert!(text(&d).contains("PK"));

    // Parent by its unquoted name in another case: the catalog finds it.
    let d = t
        .ok(
            "describe_table",
            json!({ "connection": ro, "table": PARENT.to_ascii_uppercase() }),
        )
        .await;
    let s = &d["structuredContent"];
    assert!(eq(&s["table"], PARENT), "{s}");
    let by = s["referencedBy"].as_array().unwrap();
    assert!(by.iter().any(|f| f["table"] == CHILD), "{s}");
    let e = t
        .refused(
            "describe_table",
            json!({ "connection": ro, "table": "lq_mcp_e2e_missing" }),
        )
        .await;
    assert!(e.contains("not found"), "{e}");

    let found = t
        .ok(
            "search_objects",
            json!({ "connection": ro, "pattern": "lq_mcp_e2e" }),
        )
        .await;
    let matches = found["structuredContent"]["matches"].as_array().unwrap();
    for table in [PARENT, CHILD] {
        assert!(
            matches
                .iter()
                .any(|m| eq(&m["table"], table) && m["column"].is_null()),
            "{table}: {matches:?}"
        );
    }
    let found = t
        .ok(
            "search_objects",
            json!({ "connection": ro, "pattern": "parent_id", "limit": 200 }),
        )
        .await;
    let matches = found["structuredContent"]["matches"].as_array().unwrap();
    assert!(
        matches
            .iter()
            .any(|m| m["table"] == CHILD && eq(&m["column"], "parent_id")),
        "{matches:?}"
    );

    let schemas = t.ok("list_schemas", json!({ "connection": ro })).await;
    let current = schemas["structuredContent"]["current"]
        .as_str()
        .expect("current schema")
        .to_string();
    assert!(
        schemas["structuredContent"]["schemas"]
            .as_array()
            .unwrap()
            .iter()
            .any(|s| eq(s, &current)),
        "{current}: {}",
        text(&schemas)
    );
    let tables = t.ok("list_tables", json!({ "connection": ro })).await;
    let s = &tables["structuredContent"];
    assert_eq!(s["schema"], current.as_str());
    for table in [PARENT, CHILD] {
        assert!(
            s["tables"]
                .as_array()
                .unwrap()
                .iter()
                .any(|o| eq(&o["name"], table) && o["kind"] == "table"),
            "{table}: {s}"
        );
    }
    let tables = t
        .ok(
            "list_tables",
            json!({ "connection": ro, "schema": current }),
        )
        .await;
    assert!(rows_count(&tables) >= 2);

    // Plans, without running the statement.
    let plan = t
        .ok(
            "explain",
            json!({ "connection": ro, "sql": format!("SELECT * FROM {} WHERE parent_id = 1", sql.child) }),
        )
        .await;
    assert!(!text(&plan).trim().is_empty());
    let plan = t
        .ok(
            "explain",
            json!({ "connection": ro, "sql": "DELETE FROM lq_mcp_e2e_parent WHERE id = 3" }),
        )
        .await;
    assert!(!text(&plan).trim().is_empty());
    if let Some(part) = sql.delete_plan {
        assert!(text(&plan).contains(part), "{}", text(&plan));
    }
    let e = t
        .refused(
            "explain",
            json!({ "connection": ro, "sql": "EXPLAIN SELECT 1" }),
        )
        .await;
    assert!(e.contains("Pass the statement itself"), "{e}");

    // Queries on the read connection.
    let parents = [
        row(&[Some("1"), Some("one")]),
        row(&[Some("2"), Some("two")]),
        row(&[Some("3"), Some("three")]),
    ];
    let q = t
        .ok(
            "query",
            json!({ "connection": ro, "sql": "SELECT id, name FROM lq_mcp_e2e_parent ORDER BY id;" }),
        )
        .await;
    assert_eq!(rows_of(&q), parents);
    assert_eq!(q["structuredContent"]["rowCount"], 3);
    assert_eq!(q["structuredContent"]["truncated"], false);
    assert!(text(&q).contains("| 2 | two |"), "{}", text(&q));
    let q = t
        .ok(
            "query",
            json!({ "connection": ro, "sql": "SELECT id, name FROM lq_mcp_e2e_parent ORDER BY id", "max_rows": 2 }),
        )
        .await;
    assert_eq!(rows_of(&q), parents[..2]);
    assert_eq!(q["structuredContent"]["rowCount"], 2);
    assert_eq!(q["structuredContent"]["truncated"], true);
    assert!(text(&q).contains("more rows exist"), "{}", text(&q));
    let joined = t
        .read(&format!(
            "SELECT c.id, p.name, c.note FROM {} c JOIN lq_mcp_e2e_parent p ON p.id = c.parent_id ORDER BY c.id",
            sql.child
        ))
        .await;
    assert_eq!(
        joined,
        [
            row(&[Some("10"), Some("one"), Some("a")]),
            row(&[Some("11"), Some("one"), None]),
            row(&[Some("12"), Some("two"), Some("c")]),
        ]
    );
    let e = t
        .refused(
            "query",
            json!({ "connection": ro, "sql": "SELECT nope FROM lq_mcp_e2e_parent" }),
        )
        .await;
    assert!(e.to_ascii_lowercase().contains("nope"), "{e}");

    // Read-only, classifier level: refused before reaching the server.
    let common = [
        "INSERT INTO lq_mcp_e2e_parent (id, name) VALUES (9, 'x')",
        "UPDATE lq_mcp_e2e_parent SET name = 'x'",
        "DELETE FROM lq_mcp_e2e_parent",
        "TRUNCATE TABLE lq_mcp_e2e_parent",
        "DROP TABLE lq_mcp_e2e_parent",
        "CREATE TABLE lq_mcp_e2e_x (id int)",
        "ALTER TABLE lq_mcp_e2e_parent ADD extra int",
        "WITH x AS (SELECT 1 AS n) DELETE FROM lq_mcp_e2e_parent",
    ];
    for stmt in common.iter().chain(sql.refused_reads) {
        let e = t
            .refused("query", json!({ "connection": ro, "sql": stmt }))
            .await;
        assert!(e.contains(READ_ONLY), "{stmt}: {e}");
    }
    let e = t
        .refused(
            "query",
            json!({ "connection": ro, "sql": "SELECT 1 FROM lq_mcp_e2e_parent; DELETE FROM lq_mcp_e2e_parent" }),
        )
        .await;
    assert!(e.contains("one statement per call"), "{e}");
    for stmt in [
        "DELETE FROM lq_mcp_e2e_parent",
        "DROP TABLE lq_mcp_e2e_parent",
        "SELECT 1 FROM lq_mcp_e2e_parent",
    ] {
        let e = t
            .refused("execute", json!({ "connection": ro, "sql": stmt }))
            .await;
        assert!(e.contains(READ_ONLY), "{stmt}: {e}");
    }
    // query on the write connection is still read-only.
    let e = t
        .refused(
            "query",
            json!({ "connection": rw, "sql": "DELETE FROM lq_mcp_e2e_parent" }),
        )
        .await;
    assert!(e.contains("use execute"), "{e}");
    check_unchanged(t, sql.child, &["one", "two", "three"]).await;
}

fn rows_count(result: &Value) -> usize {
    result["structuredContent"]["tables"]
        .as_array()
        .map_or(0, Vec::len)
}

/// The tables hold what setup wrote, with these parent names, as the write
/// connection sees them.
async fn check_unchanged(t: &mut E2e, child: &str, names: &[&str]) {
    let rw = t.rw.clone();
    let parents = t
        .query(&rw, "SELECT id, name FROM lq_mcp_e2e_parent ORDER BY id")
        .await;
    let expected: Vec<_> = names
        .iter()
        .enumerate()
        .map(|(i, n)| row(&[Some(&(i + 1).to_string()), Some(n)]))
        .collect();
    assert_eq!(parents, expected);
    let children = t.value(&format!("SELECT count(*) FROM {child}")).await;
    assert_eq!(children.as_deref(), Some("3"));
}

/// The update commits and the read connection sees it; transaction control
/// is refused.
async fn sql_writes(t: &mut E2e, sql: &Sql) {
    let rw = t.rw.clone();
    let r = t.write(UPDATE).await;
    assert_eq!(r["rowsAffected"], 1, "{r}");
    let seen = t
        .read("SELECT name FROM lq_mcp_e2e_parent WHERE id = 1")
        .await;
    assert_eq!(seen, [row(&[Some("uno")])]);

    for stmt in sql.tx_control {
        let e = t
            .refused("execute", json!({ "connection": rw, "sql": stmt }))
            .await;
        assert!(e.contains("Transaction control"), "{stmt}: {e}");
    }
    let e = t
        .refused(
            "execute",
            json!({ "connection": rw, "sql": "UPDATE lq_mcp_e2e_parent SET name = 'a' WHERE id = 2; UPDATE lq_mcp_e2e_parent SET name = 'b' WHERE id = 3" }),
        )
        .await;
    assert!(e.contains("one statement per call"), "{e}");
    check_unchanged(t, sql.child, &["uno", "two", "three"]).await;
}

/// Runs the checks of one SQL engine: `engine` holds the engine-level
/// read-only checks, `tail` the transaction and time-limit checks.
async fn sql_engine<E, EF, L, LF>(target: Target, sql: &'static Sql, engine: E, tail: L)
where
    E: FnOnce(E2e) -> EF + Send + 'static,
    EF: Future<Output = E2e> + Send,
    L: FnOnce(E2e) -> LF + Send + 'static,
    LF: Future<Output = E2e> + Send,
{
    let t2 = target.clone();
    guarded(&target, sql.cleanup, async move {
        let mut t = E2e::start(&t2, sql.tag).await;
        sql_common(&mut t, sql).await;
        let mut t = engine(t).await;
        check_unchanged(&mut t, sql.child, &["one", "two", "three"]).await;
        sql_writes(&mut t, sql).await;
        let mut t = tail(t).await;
        check_unchanged(&mut t, sql.child, &["uno", "two", "three"]).await;
        t.check_log(UPDATE);
        let _ = std::fs::remove_dir_all(&t.dir);
    })
    .await;
}

// ---------------------------------------------------------------------------
// PostgreSQL

static PG: Sql = Sql {
    tag: "pg",
    child: "\"lq_mcp_e2e Child\"",
    setup: &[
        "CREATE TABLE lq_mcp_e2e_parent (id int PRIMARY KEY, name text NOT NULL)",
        "CREATE TABLE \"lq_mcp_e2e Child\" (id int PRIMARY KEY, \
         parent_id int NOT NULL CONSTRAINT lq_mcp_e2e_fk REFERENCES lq_mcp_e2e_parent (id), note text)",
        "INSERT INTO lq_mcp_e2e_parent VALUES (1, 'one'), (2, 'two'), (3, 'three')",
        "INSERT INTO \"lq_mcp_e2e Child\" VALUES (10, 1, 'a'), (11, 1, NULL), (12, 2, 'c')",
        // Writes, but reads like a plain function call.
        "CREATE FUNCTION lq_mcp_e2e_touch() RETURNS int LANGUAGE plpgsql AS $$ \
         BEGIN UPDATE lq_mcp_e2e_parent SET name = 'touched' WHERE id = 1; RETURN 1; END $$",
    ],
    cleanup: &[
        "DROP FUNCTION IF EXISTS lq_mcp_e2e_touch()",
        "DROP TABLE IF EXISTS \"lq_mcp_e2e Child\"",
        "DROP TABLE IF EXISTS lq_mcp_e2e_parent",
    ],
    refused_reads: &[
        "SELECT setval('lq_mcp_e2e_seq', 5)",
        "SELECT nextval('lq_mcp_e2e_seq')",
        "SELECT set_config('default_transaction_read_only', 'off', false)",
        "EXPLAIN ANALYZE DELETE FROM lq_mcp_e2e_parent",
        "SELECT * INTO lq_mcp_e2e_copy FROM lq_mcp_e2e_parent",
        "COPY lq_mcp_e2e_parent TO STDOUT",
        "CALL lq_mcp_e2e_proc()",
    ],
    tx_control: &[
        "BEGIN",
        "START TRANSACTION",
        "COMMIT",
        "ROLLBACK",
        "SAVEPOINT x",
        "END",
    ],
    quick: "SELECT 1",
    delete_plan: Some("Delete on lq_mcp_e2e_parent"),
};

#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn mcp_postgres_server() {
    let Some(target) = pg_target() else {
        return;
    };
    sql_engine(
        target,
        &PG,
        |mut t| async move {
            let ro = t.ro.clone();
            let objects = "SELECT count(*) FROM pg_largeobject_metadata";
            let large_objects = t.value(objects).await;
            // The classifier lets a user function through; the read-only
            // transaction refuses the write inside it.
            let stmt = "SELECT lq_mcp_e2e_touch()";
            let e = t
                .refused("query", json!({ "connection": ro, "sql": stmt }))
                .await;
            assert!(e.contains("read-only transaction"), "{stmt}: {e}");
            // Large objects are refused before they reach the server.
            let e = t
                .refused(
                    "query",
                    json!({ "connection": ro, "sql": "SELECT lo_creat(-1)" }),
                )
                .await;
            assert!(e.contains("not allowed"), "{e}");
            assert_eq!(t.value(objects).await, large_objects);

            // The agent's session itself refuses writes.
            let e = t
                .on_read(Kind::Read, |s| async move {
                    exec(&s, "UPDATE lq_mcp_e2e_parent SET name = 'x' WHERE id = 1").await
                })
                .await
                .unwrap_err();
            assert!(e.message.contains("read-only transaction"), "{e}");
            // A call that turns its own transaction read-write (SET is not
            // a read, so agents cannot) still has its work rolled back.
            let r = t
                .on_read(Kind::Read, |s| async move {
                    exec(&s, "SET TRANSACTION READ WRITE").await?;
                    exec(&s, "UPDATE lq_mcp_e2e_parent SET name = 'x' WHERE id = 3").await
                })
                .await
                .unwrap();
            assert_eq!(r.rows_affected, Some(1));
            t
        },
        |mut t| async move {
            let ro = t.ro.clone();
            t.check_time_limit(&ro, "query", "SELECT pg_sleep(30)", PG.quick)
                .await;
            t
        },
    )
    .await;
}

// ---------------------------------------------------------------------------
// MySQL and MariaDB

static MYSQL: Sql = Sql {
    tag: "mysql",
    child: "`lq_mcp_e2e Child`",
    setup: &[
        "CREATE TABLE lq_mcp_e2e_parent (id int PRIMARY KEY, name varchar(50) NOT NULL) ENGINE=InnoDB",
        "CREATE TABLE `lq_mcp_e2e Child` (id int PRIMARY KEY, parent_id int NOT NULL, note varchar(50), \
         CONSTRAINT lq_mcp_e2e_fk FOREIGN KEY (parent_id) REFERENCES lq_mcp_e2e_parent (id)) ENGINE=InnoDB",
        "INSERT INTO lq_mcp_e2e_parent VALUES (1, 'one'), (2, 'two'), (3, 'three')",
        "INSERT INTO `lq_mcp_e2e Child` VALUES (10, 1, 'a'), (11, 1, NULL), (12, 2, 'c')",
        // Writes, but reads like a plain function call.
        "CREATE FUNCTION lq_mcp_e2e_touch() RETURNS int DETERMINISTIC MODIFIES SQL DATA \
         BEGIN UPDATE lq_mcp_e2e_parent SET name = 'touched' WHERE id = 1; RETURN 1; END",
        // Leaves a transaction open when called.
        "CREATE PROCEDURE lq_mcp_e2e_open() \
         BEGIN START TRANSACTION; UPDATE lq_mcp_e2e_parent SET name = 'open' WHERE id = 2; END",
    ],
    cleanup: &[
        "DROP FUNCTION IF EXISTS lq_mcp_e2e_touch",
        "DROP PROCEDURE IF EXISTS lq_mcp_e2e_open",
        "DROP TABLE IF EXISTS `lq_mcp_e2e Child`, lq_mcp_e2e_parent",
    ],
    refused_reads: &[
        "REPLACE INTO lq_mcp_e2e_parent VALUES (9, 'x')",
        "SELECT * FROM lq_mcp_e2e_parent INTO OUTFILE '/tmp/lq_mcp_e2e'",
        "SELECT name INTO @x FROM lq_mcp_e2e_parent LIMIT 1",
        "SET @x = 1",
        "CALL lq_mcp_e2e_open()",
        "DO SLEEP(1)",
        "EXPLAIN ANALYZE SELECT * FROM lq_mcp_e2e_parent",
        "LOCK TABLES lq_mcp_e2e_parent READ",
    ],
    tx_control: &[
        "START TRANSACTION",
        "BEGIN",
        "BEGIN WORK",
        "COMMIT",
        "ROLLBACK",
        "SAVEPOINT x",
        "SET autocommit = 0",
        "SET @@session.autocommit = 0",
        "LOCK TABLES lq_mcp_e2e_parent WRITE",
        "XA START 'lq_mcp_e2e'",
    ],
    quick: "SELECT 1",
    // Older MySQL trees do not show single-table DELETE.
    delete_plan: None,
};

#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn mcp_mysql_server() {
    let Some(target) = mysql_target() else {
        return;
    };
    sql_engine(
        target,
        &MYSQL,
        |mut t| async move {
            let ro = t.ro.clone();
            // The classifier lets the call through; the READ ONLY
            // transaction refuses the UPDATE inside.
            let touch = json!({ "connection": ro, "sql": "SELECT lq_mcp_e2e_touch()" });
            let e = t.refused("query", touch.clone()).await;
            assert!(
                e.to_ascii_lowercase().contains("read only transaction"),
                "{e}"
            );

            // The agent's session itself refuses writes.
            let insert = "INSERT INTO lq_mcp_e2e_parent VALUES (99, 'x')";
            let e = t
                .on_read(Kind::Read, |s| async move { exec(&s, insert).await })
                .await
                .unwrap_err();
            assert!(
                e.message
                    .to_ascii_lowercase()
                    .contains("read only transaction"),
                "{e}"
            );
            // A read-write session default does not reach the transaction
            // each call starts.
            let r = t
                .on_read(Kind::Read, |s| async move {
                    exec(&s, "SET SESSION TRANSACTION READ WRITE").await?;
                    exec(&s, insert).await
                })
                .await;
            assert!(r.is_err(), "{r:?}");
            let e = t.refused("query", touch).await;
            assert!(
                e.to_ascii_lowercase().contains("read only transaction"),
                "{e}"
            );
            let _ = t
                .on_read(Kind::Read, |s| async move {
                    exec(&s, "SET SESSION TRANSACTION READ ONLY").await
                })
                .await;
            t
        },
        |mut t| async move {
            let (ro, rw) = (t.ro.clone(), t.rw.clone());
            // A procedure that opens a transaction: rolled back and refused.
            let e = t
                .refused(
                    "execute",
                    json!({ "connection": rw, "sql": "CALL lq_mcp_e2e_open()" }),
                )
                .await;
            assert!(e.contains("left a transaction open"), "{e}");
            t.check_time_limit(&ro, "query", "SELECT SLEEP(30)", MYSQL.quick)
                .await;
            t
        },
    )
    .await;
}

// ---------------------------------------------------------------------------
// SQL Server

static MSSQL: Sql = Sql {
    tag: "mssql",
    child: "[lq_mcp_e2e Child]",
    setup: &[
        "CREATE TABLE lq_mcp_e2e_parent (id int PRIMARY KEY, name nvarchar(50) NOT NULL)",
        "CREATE TABLE [lq_mcp_e2e Child] (id int PRIMARY KEY, parent_id int NOT NULL \
         CONSTRAINT lq_mcp_e2e_fk REFERENCES lq_mcp_e2e_parent (id), note nvarchar(50) NULL)",
        "INSERT INTO lq_mcp_e2e_parent VALUES (1, N'one'), (2, N'two'), (3, N'three')",
        "INSERT INTO [lq_mcp_e2e Child] VALUES (10, 1, N'a'), (11, 1, NULL), (12, 2, N'c')",
    ],
    cleanup: &[
        "DROP TABLE IF EXISTS [lq_mcp_e2e Child]",
        "DROP TABLE IF EXISTS lq_mcp_e2e_parent",
        "DROP TABLE IF EXISTS lq_mcp_e2e_tmp",
    ],
    refused_reads: &[
        "SELECT 1 DELETE FROM lq_mcp_e2e_parent",
        "SELECT 1 WAITFOR DELAY '00:00:01'",
        "EXEC sp_who",
        "SELECT * INTO lq_mcp_e2e_copy FROM lq_mcp_e2e_parent",
        "DECLARE @x int",
        "SET NOCOUNT ON",
        "SELECT * FROM OPENROWSET('SQLNCLI', 'x', 'SELECT 1')",
        "MERGE lq_mcp_e2e_parent AS t USING (SELECT 1 AS id) AS s ON t.id = s.id WHEN MATCHED THEN DELETE;",
    ],
    tx_control: &[
        "BEGIN TRAN",
        "BEGIN TRANSACTION",
        "COMMIT",
        "ROLLBACK",
        "SAVE TRAN x",
        "SET IMPLICIT_TRANSACTIONS ON",
    ],
    quick: "SELECT 1",
    delete_plan: Some("Delete"),
};

#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn mcp_sqlserver_server() {
    let Some(target) = mssql_target() else {
        return;
    };
    sql_engine(
        target,
        &MSSQL,
        |mut t| async move {
            // No read the classifier accepts writes here, so check the
            // session: every call of a read connection runs in a
            // transaction that is rolled back, DDL included.
            let in_tx = t
                .on_read(Kind::Read, |s| async move {
                    exec(&s, "INSERT INTO lq_mcp_e2e_parent VALUES (99, N'x')").await?;
                    exec(&s, "CREATE TABLE lq_mcp_e2e_tmp (id int)").await?;
                    Ok(s.tx_state().in_transaction)
                })
                .await
                .unwrap();
            assert!(in_tx);
            let n = t
                .value("SELECT count(*) FROM lq_mcp_e2e_parent WHERE id = 99")
                .await;
            assert_eq!(n.as_deref(), Some("0"));
            let n = t
                .value("SELECT count(*) FROM sys.tables WHERE name = 'lq_mcp_e2e_tmp'")
                .await;
            assert_eq!(n.as_deref(), Some("0"));
            // The read connection keeps working after that.
            let rows = t.read("SELECT count(*) FROM lq_mcp_e2e_parent").await;
            assert_eq!(rows, [row(&[Some("3")])]);
            t
        },
        |mut t| async move {
            let rw = t.rw.clone();
            // A batch that opens a transaction without BEGIN TRAN first:
            // rolled back and refused.
            let e = t
                .refused(
                    "execute",
                    json!({ "connection": rw, "sql": "BEGIN TRY BEGIN TRANSACTION UPDATE lq_mcp_e2e_parent SET name = N'open' WHERE id = 2 END TRY BEGIN CATCH END CATCH" }),
                )
                .await;
            assert!(e.contains("left a transaction open"), "{e}");
            // WAITFOR is not a read: the write connection waits.
            t.check_time_limit(&rw, "execute", "WAITFOR DELAY '00:00:20'", MSSQL.quick)
                .await;
            t
        },
    )
    .await;
}

// ---------------------------------------------------------------------------
// Oracle

static ORACLE: Sql = Sql {
    tag: "ora",
    child: "\"lq_mcp_e2e Child\"",
    setup: &[
        "create table lq_mcp_e2e_parent (id number primary key, name varchar2(50) not null)",
        "create table \"lq_mcp_e2e Child\" (id number primary key, parent_id number not null \
         constraint lq_mcp_e2e_fk references lq_mcp_e2e_parent (id), note varchar2(50))",
        "insert into lq_mcp_e2e_parent select 1, 'one' from dual union all \
         select 2, 'two' from dual union all select 3, 'three' from dual",
        "insert into \"lq_mcp_e2e Child\" select 10, 1, 'a' from dual union all \
         select 11, 1, null from dual union all select 12, 2, 'c' from dual",
    ],
    cleanup: &[
        "drop table \"lq_mcp_e2e Child\" purge",
        "drop table lq_mcp_e2e_parent purge",
    ],
    refused_reads: &[
        "BEGIN NULL; END;",
        "MERGE INTO lq_mcp_e2e_parent t USING dual ON (t.id = 1) WHEN MATCHED THEN UPDATE SET t.name = 'x'",
        "SELECT lq_mcp_e2e_seq.nextval FROM dual",
        "LOCK TABLE lq_mcp_e2e_parent IN EXCLUSIVE MODE",
        "CALL dbms_session.sleep(1)",
        "ALTER SESSION SET current_schema = sys",
    ],
    tx_control: &["COMMIT", "ROLLBACK", "SAVEPOINT x", "ROLLBACK TO SAVEPOINT x"],
    quick: "SELECT 1 FROM dual",
    delete_plan: Some("DELETE"),
};

#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn mcp_oracle_server() {
    let Some(target) = ora_target() else {
        return;
    };
    sql_engine(
        target,
        &ORACLE,
        |mut t| async move {
            let ro = t.ro.clone();
            // FOR UPDATE passes the classifier (it only locks); a READ ONLY
            // transaction refuses it.
            let e = t
                .refused(
                    "query",
                    json!({ "connection": ro, "sql": "SELECT id FROM lq_mcp_e2e_parent WHERE id = 1 FOR UPDATE" }),
                )
                .await;
            assert!(e.contains("ORA-"), "{e}");

            // The agent's session itself refuses writes.
            let insert = "INSERT INTO lq_mcp_e2e_parent VALUES (99, 'x')";
            let e = t
                .on_read(Kind::Read, |s| async move { exec(&s, insert).await })
                .await
                .unwrap_err();
            assert_eq!(e.code.as_deref(), Some("ORA-01456"), "{e}");
            // explain skips SET TRANSACTION READ ONLY (EXPLAIN PLAN writes
            // PLAN_TABLE): what such a call writes is rolled back.
            let in_tx = t
                .on_read(Kind::Explain, |s| async move {
                    exec(&s, insert).await?;
                    Ok(s.tx_state().in_transaction)
                })
                .await
                .unwrap();
            assert!(in_tx);
            let n = t
                .value("SELECT count(*) FROM lq_mcp_e2e_parent WHERE id = 99")
                .await;
            assert_eq!(n.as_deref(), Some("0"));
            t
        },
        |mut t| async move {
            let rw = t.rw.clone();
            // No sleep can be called from a read, and the driver cannot
            // interrupt a call: the session is dropped and the next call
            // opens another.
            t.check_time_limit(
                &rw,
                "execute",
                "BEGIN DBMS_SESSION.SLEEP(8); END;",
                ORACLE.quick,
            )
            .await;
            t
        },
    )
    .await;
}

// ---------------------------------------------------------------------------
// Redis

const KEYS: &str = "lq_mcp_e2e:str lq_mcp_e2e:hash lq_mcp_e2e:list lq_mcp_e2e:none";

/// Values of a Redis reply, row by row.
fn cells(result: &Value) -> Vec<String> {
    rows_of(result).into_iter().flatten().flatten().collect()
}

#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn mcp_redis_server() {
    let Some(target) = redis_target() else {
        return;
    };
    let t2 = target.clone();
    let cleanup = format!("DEL {KEYS}");
    guarded(&target, &[cleanup.as_str()], async move {
        let mut t = E2e::start(&t2, "redis").await;
        let (ro, rw) = (t.ro.clone(), t.rw.clone());
        t.check_connections().await;

        t.write("SET lq_mcp_e2e:str hello").await;
        t.write("HSET lq_mcp_e2e:hash f1 v1 f2 v2").await;
        t.write("RPUSH lq_mcp_e2e:list a b c").await;

        // Keys and databases.
        let found = t
            .ok(
                "search_objects",
                json!({ "connection": ro, "pattern": "lq_mcp_e2e:" }),
            )
            .await;
        let matches = found["structuredContent"]["matches"].as_array().unwrap();
        for key in ["lq_mcp_e2e:str", "lq_mcp_e2e:hash", "lq_mcp_e2e:list"] {
            assert!(
                matches
                    .iter()
                    .any(|m| m["table"] == key && m["kind"] == "key"),
                "{key}: {matches:?}"
            );
        }
        let schemas = t.ok("list_schemas", json!({ "connection": ro })).await;
        assert_eq!(schemas["structuredContent"]["current"], "db0");
        let tables = t.ok("list_tables", json!({ "connection": ro })).await;
        let objects = tables["structuredContent"]["tables"].as_array().unwrap();
        for (key, kind) in [
            ("lq_mcp_e2e:str", "string"),
            ("lq_mcp_e2e:hash", "hash"),
            ("lq_mcp_e2e:list", "list"),
        ] {
            assert!(
                objects
                    .iter()
                    .any(|o| o["name"] == key && o["kind"] == kind),
                "{key}: {objects:?}"
            );
        }
        let e = t
            .refused(
                "describe_table",
                json!({ "connection": ro, "table": "lq_mcp_e2e:str" }),
            )
            .await;
        assert!(e.contains("no columns"), "{e}");
        let e = t
            .refused(
                "explain",
                json!({ "connection": ro, "sql": "GET lq_mcp_e2e:str" }),
            )
            .await;
        assert!(e.contains("no execution plans"), "{e}");

        // Reads.
        let get = |key: &str| json!({ "connection": ro, "sql": format!("GET {key}") });
        let r = t.ok("query", get("lq_mcp_e2e:str")).await;
        assert_eq!(cells(&r), ["hello"]);
        let r = t
            .ok(
                "query",
                json!({ "connection": ro, "sql": "HGETALL lq_mcp_e2e:hash" }),
            )
            .await;
        let c = cells(&r);
        assert!(
            c.contains(&"f1".into()) && c.contains(&"v2".into()),
            "{c:?}"
        );
        let r = t
            .ok(
                "query",
                json!({ "connection": ro, "sql": "LRANGE lq_mcp_e2e:list 0 -1", "max_rows": 2 }),
            )
            .await;
        assert_eq!(cells(&r), ["a", "b"]);
        assert_eq!(r["structuredContent"]["truncated"], true);
        let r = t
            .ok(
                "query",
                json!({ "connection": ro, "sql": "SCAN 0 MATCH lq_mcp_e2e:* COUNT 1000" }),
            )
            .await;
        assert!(!cells(&r).is_empty());

        // Read-only: the read connection only takes read commands.
        for cmd in [
            "SET lq_mcp_e2e:str x",
            "DEL lq_mcp_e2e:str",
            "HSET lq_mcp_e2e:hash f1 x",
            "EXPIRE lq_mcp_e2e:str 1",
            "RPOP lq_mcp_e2e:list",
            "FLUSHDB",
            "EVAL \"return 1\" 0",
            "CONFIG SET maxmemory 1",
            "BLPOP lq_mcp_e2e:list 1",
            "MULTI",
        ] {
            let e = t
                .refused("query", json!({ "connection": ro, "sql": cmd }))
                .await;
            assert!(e.contains(READ_ONLY), "{cmd}: {e}");
        }
        let e = t
            .refused(
                "execute",
                json!({ "connection": ro, "sql": "SET lq_mcp_e2e:str x" }),
            )
            .await;
        assert!(e.contains(READ_ONLY), "{e}");
        let e = t
            .refused(
                "query",
                json!({ "connection": ro, "sql": "KEYS lq_mcp_e2e:*" }),
            )
            .await;
        assert!(e.contains("SCAN 0 MATCH"), "{e}");
        let r = t.ok("query", get("lq_mcp_e2e:str")).await;
        assert_eq!(cells(&r), ["hello"]);
        let r = t
            .ok(
                "query",
                json!({ "connection": ro, "sql": "TTL lq_mcp_e2e:str" }),
            )
            .await;
        assert_eq!(cells(&r), ["-1"]);
        let r = t
            .ok(
                "query",
                json!({ "connection": ro, "sql": "LLEN lq_mcp_e2e:list" }),
            )
            .await;
        assert_eq!(cells(&r), ["3"]);

        // Writes on the write connection; session commands stay refused.
        t.write("SET lq_mcp_e2e:str world").await;
        let r = t.ok("query", get("lq_mcp_e2e:str")).await;
        assert_eq!(cells(&r), ["world"]);
        for cmd in ["MULTI", "SELECT 1", "CLIENT KILL TYPE normal"] {
            let e = t
                .refused("execute", json!({ "connection": rw, "sql": cmd }))
                .await;
            assert!(e.contains("not available to agents"), "{cmd}: {e}");
        }
        let e = t
            .refused(
                "query",
                json!({ "connection": rw, "sql": "DEL lq_mcp_e2e:str" }),
            )
            .await;
        assert!(e.contains("use execute"), "{e}");

        // A blocking command past the time limit is unblocked.
        t.check_time_limit(
            &rw,
            "execute",
            "BLPOP lq_mcp_e2e:none 20",
            "GET lq_mcp_e2e:str",
        )
        .await;
        let r = t.ok("query", get("lq_mcp_e2e:str")).await;
        assert_eq!(cells(&r), ["world"]);

        // Activity log.
        let log = t.log();
        let on = |e: &AuditEntry, id: &str| e.connection_id.as_deref() == Some(id);
        assert!(log.iter().any(|e| e.tool == "list_connections" && e.ok));
        assert!(log
            .iter()
            .any(|e| e.tool == "search_objects" && e.ok && on(e, &ro) && e.rows >= Some(3)));
        assert!(log.iter().any(|e| e.tool == "execute"
            && e.ok
            && on(e, &rw)
            && e.detail.as_deref() == Some("SET lq_mcp_e2e:str world")));
        assert!(log.iter().any(|e| e.tool == "query"
            && !e.ok
            && on(e, &ro)
            && e.detail.as_deref() == Some("KEYS lq_mcp_e2e:*")
            && e.error.as_deref().is_some_and(|m| m.contains("SCAN"))));
        assert!(log.iter().any(|e| e.tool == "execute"
            && !e.ok
            && on(e, &rw)
            && e.error
                .as_deref()
                .is_some_and(|m| m.contains("longer than"))));
        assert!(log
            .iter()
            .any(|e| !e.ok && e.tool == "list_schemas" && e.connection_id.is_none()));
        let _ = std::fs::remove_dir_all(&t.dir);
    })
    .await;
}
