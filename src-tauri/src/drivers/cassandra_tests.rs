//! Tests of the Cassandra driver against a real server. They only run with
//! these environment variables (all but the host are optional):
//!
//! `LQ_CASSANDRA_HOST`, `LQ_CASSANDRA_PORT` (9042), `LQ_CASSANDRA_USER`,
//! `LQ_CASSANDRA_PASSWORD`, `LQ_CASSANDRA_DC`
//!
//! `cargo test --lib cassandra -- --ignored --test-threads=1`

use std::time::Duration;

use super::{RowSink, Session, StreamEvent};
use crate::error::ErrorKind;
use crate::model::{
    ApplyRequest, CellValue, ConnectionProfile, Engine, ExecRequest, ExecResult, RowChange,
    SslMode, ValueKind,
};

fn env(name: &str) -> Option<String> {
    std::env::var(name).ok().filter(|v| !v.is_empty())
}

async fn open() -> Option<Session> {
    let host = env("LQ_CASSANDRA_HOST")?;
    let profile = ConnectionProfile {
        name: "test".into(),
        engine: Engine::Cassandra,
        host,
        port: env("LQ_CASSANDRA_PORT")
            .and_then(|p| p.parse().ok())
            .unwrap_or(9042),
        user: env("LQ_CASSANDRA_USER").unwrap_or_default(),
        datacenter: env("LQ_CASSANDRA_DC").unwrap_or_default(),
        ssl_mode: SslMode::Disable,
        ..Default::default()
    };
    let password = env("LQ_CASSANDRA_PASSWORD");
    Some(
        Session::open(&profile, password.as_deref())
            .await
            .expect("connection"),
    )
}

async fn run(s: &Session, sql: &str, max_rows: usize) -> ExecResult {
    s.execute(&ExecRequest {
        sql: sql.into(),
        max_rows,
    })
    .await
    .unwrap_or_else(|e| panic!("{sql}: {e}"))
}

async fn keyspace(s: &Session, name: &str) {
    run(s, &format!("DROP KEYSPACE IF EXISTS {name}"), 0).await;
    run(
        s,
        &format!(
            "CREATE KEYSPACE {name} WITH replication = \
             {{'class': 'SimpleStrategy', 'replication_factor': 1}}"
        ),
        0,
    )
    .await;
}

fn row(r: &ExecResult) -> Vec<String> {
    r.rows[0]
        .iter()
        .map(|v| v.clone().unwrap_or_else(|| "<null>".into()))
        .collect()
}

fn value(column: usize, v: Option<&str>) -> CellValue {
    CellValue {
        column,
        value: v.map(str::to_string),
    }
}

#[tokio::test]
#[ignore]
async fn cassandra_types_limits_and_errors() {
    let Some(s) = open().await else {
        return;
    };
    assert!(
        s.server_version().starts_with("Apache Cassandra "),
        "{}",
        s.server_version()
    );
    assert!(s.tx_state().autocommit && !s.tx_state().in_transaction);
    assert!(s.set_autocommit(false).await.is_err());
    assert!(s.end_transaction(true).await.is_err());

    keyspace(&s, "lq_types").await;
    run(
        &s,
        "CREATE TYPE lq_types.addr (street text, zip int, tags set<text>)",
        0,
    )
    .await;
    run(
        &s,
        "CREATE TABLE lq_types.t (
            id int PRIMARY KEY, a ascii, b bigint, bl blob, bo boolean, d date, de decimal,
            du double, dur duration, f float, i inet, s smallint, t text, ti time,
            ts timestamp, tu timeuuid, ty tinyint, u uuid, v varint,
            l list<int>, st set<text>, m map<text, frozen<list<int>>>,
            tp tuple<int, text, boolean>, ad frozen<addr>, n int)",
        0,
    )
    .await;
    run(
        &s,
        "INSERT INTO lq_types.t (id, a, b, bl, bo, d, de, du, dur, f, i, s, t, ti, ts, tu, ty, u, v,
            l, st, m, tp, ad) VALUES (
            1, 'ascii', 9223372036854775807, 0x00ff10, true, '2024-05-06',
            12345678901234567890.123456789, 0.1, 1y2mo3d4h5m6s7ms8us9ns, 1.5, '::1', -32768,
            'it''s é', '07:08:09.123456789', '2024-05-06 07:08:09.123+0000',
            50554d6e-29bb-11e5-b345-feff819cdc9f, -128, 123e4567-e89b-12d3-a456-426614174000,
            -123456789012345678901234567890,
            [1, 2, 3], {'b', 'a''x'}, {'k': [1, 2]}, (7, 'x', false),
            {street: 'Main', zip: 12345, tags: {'p', 'q'}})",
        0,
    )
    .await;
    let r = run(&s, "SELECT * FROM lq_types.t", 100).await;
    let columns = r.columns.clone().unwrap();
    let by = |name: &str| columns.iter().position(|c| c.name == name).unwrap();
    let cells = row(&r);
    let cell = |name: &str| cells[by(name)].as_str();
    assert_eq!(cell("a"), "ascii");
    assert_eq!(cell("b"), "9223372036854775807");
    assert_eq!(cell("bl"), "0x00ff10");
    assert_eq!(cell("bo"), "true");
    assert_eq!(cell("d"), "2024-05-06");
    assert_eq!(cell("de"), "12345678901234567890.123456789");
    assert_eq!(cell("du"), "0.1");
    assert_eq!(cell("dur"), "1y2mo3d4h5m6s7ms8us9ns");
    assert_eq!(cell("f"), "1.5");
    assert_eq!(cell("i"), "::1");
    assert_eq!(cell("s"), "-32768");
    assert_eq!(cell("t"), "it's é");
    assert_eq!(cell("ti"), "07:08:09.123456789");
    assert_eq!(cell("ts"), "2024-05-06 07:08:09.123+0000");
    assert_eq!(cell("tu"), "50554d6e-29bb-11e5-b345-feff819cdc9f");
    assert_eq!(cell("ty"), "-128");
    assert_eq!(cell("u"), "123e4567-e89b-12d3-a456-426614174000");
    assert_eq!(cell("v"), "-123456789012345678901234567890");
    assert_eq!(cell("l"), "[1, 2, 3]");
    assert_eq!(cell("st"), "{'a''x', 'b'}");
    assert_eq!(cell("m"), "{'k': [1, 2]}");
    assert_eq!(cell("tp"), "(7, 'x', false)");
    assert_eq!(cell("ad"), "{street: 'Main', zip: 12345, tags: {'p', 'q'}}");
    assert_eq!(cell("n"), "<null>");
    let type_of = |name: &str| columns[by(name)].type_name.clone();
    assert_eq!(type_of("l"), "list<int>");
    // Result metadata does not say which collections and UDTs are frozen.
    assert_eq!(type_of("m"), "map<text, list<int>>");
    assert_eq!(type_of("tp"), "tuple<int, text, boolean>");
    assert_eq!(type_of("ad"), "addr");
    assert_eq!(type_of("v"), "varint");
    assert_eq!(columns[by("de")].kind, ValueKind::Number);
    assert_eq!(columns[by("bl")].kind, ValueKind::Binary);
    assert_eq!(columns[by("ts")].kind, ValueKind::Temporal);
    assert_eq!(columns[by("st")].kind, ValueKind::Other);

    // Every composite cell is a literal the server reads back.
    run(
        &s,
        &format!(
            "INSERT INTO lq_types.t (id, l, st, m, tp, ad) VALUES (2, {}, {}, {}, {}, {})",
            cell("l"),
            cell("st"),
            cell("m"),
            cell("tp"),
            cell("ad")
        ),
        0,
    )
    .await;
    let again = run(
        &s,
        "SELECT l, st, m, tp, ad FROM lq_types.t WHERE id = 2",
        10,
    )
    .await;
    assert_eq!(
        row(&again),
        [cell("l"), cell("st"), cell("m"), cell("tp"), cell("ad")]
    );

    // Counters are not editable.
    run(
        &s,
        "CREATE TABLE lq_types.c (id int PRIMARY KEY, n counter)",
        0,
    )
    .await;
    run(&s, "UPDATE lq_types.c SET n = n + 5 WHERE id = 1", 0).await;
    let c = run(&s, "SELECT id, n FROM lq_types.c", 10).await;
    assert_eq!(c.columns.as_ref().unwrap()[1].type_name, "counter");
    assert_eq!(row(&c), ["1", "5"]);
    assert!(c.read_only_reason.as_deref().unwrap().contains("Counter"));

    // The row limit is applied by paging.
    run(
        &s,
        "CREATE TABLE lq_types.many (id int PRIMARY KEY, v int)",
        0,
    )
    .await;
    for i in 0..120 {
        run(
            &s,
            &format!("INSERT INTO lq_types.many (id, v) VALUES ({i}, {i})"),
            0,
        )
        .await;
    }
    let r = run(&s, "SELECT id FROM lq_types.many", 10).await;
    assert_eq!(r.rows.len(), 10);
    assert!(r.truncated);
    let r = run(&s, "SELECT id FROM lq_types.many", 120).await;
    assert_eq!(r.rows.len(), 120);
    assert!(!r.truncated);
    let r = run(&s, "SELECT id FROM lq_types.many", 0).await;
    assert_eq!(r.rows.len(), 120);
    assert!(!r.truncated);

    // Statements without rows.
    let r = run(&s, "INSERT INTO lq_types.many (id, v) VALUES (500, 1)", 10).await;
    assert!(r.columns.is_none() && r.rows.is_empty());
    // A conditional write answers with [applied].
    let r = run(
        &s,
        "INSERT INTO lq_types.many (id, v) VALUES (500, 2) IF NOT EXISTS",
        10,
    )
    .await;
    assert_eq!(r.columns.as_ref().unwrap()[0].name, "[applied]");
    assert_eq!(r.rows[0][0].as_deref(), Some("false"));
    // A batch is one statement.
    run(
        &s,
        "BEGIN BATCH
           INSERT INTO lq_types.many (id, v) VALUES (501, 1);
           INSERT INTO lq_types.many (id, v) VALUES (502, 1);
         APPLY BATCH",
        0,
    )
    .await;
    let r = run(
        &s,
        "SELECT count(*) FROM lq_types.many WHERE id IN (501, 502)",
        10,
    )
    .await;
    assert_eq!(row(&r), ["2"]);

    // Errors carry the server's code; a syntax error has a position.
    let err = s
        .execute(&ExecRequest {
            sql: "SELECT * FROM lq_types.nope".into(),
            max_rows: 10,
        })
        .await
        .unwrap_err();
    assert_eq!(err.kind, ErrorKind::Database);
    assert_eq!(err.code.as_deref(), Some("Invalid"), "{err}");
    let err = s
        .execute(&ExecRequest {
            sql: "SELEC * FROM lq_types.t".into(),
            max_rows: 10,
        })
        .await
        .unwrap_err();
    assert_eq!(err.code.as_deref(), Some("SyntaxError"), "{err}");
    assert!(err.position.is_some(), "{err}");

    // USE sticks to the session.
    run(&s, "USE lq_types", 0).await;
    let r = run(&s, "SELECT count(*) FROM c", 10).await;
    assert_eq!(row(&r), ["1"]);

    // Warnings of the server come back as notices (Cassandra 4.0 and later
    // warn about aggregations without a partition key).
    let r = run(&s, "SELECT count(*) FROM lq_types.many", 10).await;
    assert_eq!(row(&r), ["123"]);
    if !s.server_version().contains("Cassandra 3.") {
        assert!(
            r.notices
                .iter()
                .any(|n| n.starts_with("WARNING: ") && n.contains("partition key")),
            "{:?}",
            r.notices
        );
    }

    // A filtering scan works.
    let r = run(
        &s,
        "SELECT * FROM lq_types.many WHERE v = 5 ALLOW FILTERING",
        10,
    )
    .await;
    assert_eq!(r.rows.len(), 1);

    if s.server_version().contains("Cassandra 5") {
        run(
            &s,
            "CREATE TABLE lq_types.vec (id int PRIMARY KEY, e vector<float, 3>)",
            0,
        )
        .await;
        run(
            &s,
            "INSERT INTO lq_types.vec (id, e) VALUES (1, [0.5, 1.5, -2])",
            0,
        )
        .await;
        let r = run(&s, "SELECT e FROM lq_types.vec", 10).await;
        assert_eq!(r.columns.as_ref().unwrap()[0].type_name, "vector<float, 3>");
        assert_eq!(row(&r), ["[0.5, 1.5, -2]"]);
    }
    run(&s, "DROP KEYSPACE lq_types", 0).await;
}

#[tokio::test]
#[ignore]
async fn cassandra_schema_ddl_and_stream() {
    let Some(s) = open().await else {
        return;
    };
    keyspace(&s, "lq_schema").await;
    run(&s, "CREATE TYPE lq_schema.addr (street text, zip int)", 0).await;
    run(
        &s,
        "CREATE TABLE lq_schema.\"Items\" (
            tenant text, day date, seq int, \"Name\" text, tags set<text>, home frozen<addr>,
            total decimal static,
            PRIMARY KEY ((tenant, day), seq)
        ) WITH CLUSTERING ORDER BY (seq DESC) AND comment = 'it''s here'
          AND gc_grace_seconds = 3600",
        0,
    )
    .await;
    run(
        &s,
        "CREATE INDEX lq_items_name ON lq_schema.\"Items\" (\"Name\")",
        0,
    )
    .await;
    // User-defined functions are off unless cassandra.yaml enables them.
    let functions = s
        .execute(&ExecRequest {
            sql: "CREATE FUNCTION lq_schema.plus (a int, b int) CALLED ON NULL INPUT \
                  RETURNS int LANGUAGE java AS 'return a + b;'"
                .into(),
            max_rows: 0,
        })
        .await
        .is_ok();

    let schemas = s.list_schemas().await.unwrap();
    let user = schemas.iter().position(|n| n == "lq_schema").unwrap();
    let system = schemas.iter().position(|n| n == "system").unwrap();
    assert!(user < system, "user keyspaces come first: {schemas:?}");

    let objects = s.list_objects("lq_schema").await.unwrap();
    let has = |kind: &str, name: &str| objects.iter().any(|o| o.kind == kind && o.name == name);
    assert!(has("table", "Items"), "{objects:?}");
    assert!(has("type", "addr"));
    assert!(has("index", "lq_items_name"));
    assert!(
        !functions || has("function", "plus(int, int)"),
        "{objects:?}"
    );

    let columns = s.list_columns("lq_schema", "Items").await.unwrap();
    let shape: Vec<(String, String, bool)> = columns
        .iter()
        .map(|c| (c.name.clone(), c.key_role.clone().unwrap(), c.primary_key))
        .collect();
    assert_eq!(
        shape,
        [
            ("tenant".into(), "partition".into(), true),
            ("day".into(), "partition".into(), true),
            ("seq".into(), "clustering".into(), true),
            ("total".into(), "static".into(), false),
            ("Name".into(), "regular".into(), false),
            ("home".into(), "regular".into(), false),
            ("tags".into(), "regular".into(), false),
        ]
    );
    assert_eq!(columns[5].type_name, "frozen<addr>");
    assert_eq!(columns[6].type_name, "set<text>");

    // The table's DDL recreates it, indexes included.
    let ddl = s.object_ddl("lq_schema", "Items", "table").await.unwrap();
    assert!(ddl.contains("CREATE TABLE lq_schema.\"Items\" ("), "{ddl}");
    assert!(ddl.contains("total decimal static"), "{ddl}");
    assert!(ddl.contains("PRIMARY KEY ((tenant, day), seq)"), "{ddl}");
    assert!(ddl.contains("CLUSTERING ORDER BY (seq DESC)"), "{ddl}");
    assert!(ddl.contains("comment = 'it''s here'"), "{ddl}");
    assert!(ddl.contains("CREATE INDEX lq_items_name ON"), "{ddl}");
    run(&s, "DROP TABLE lq_schema.\"Items\"", 0).await;
    for statement in super::ddl_fixtures::statements(&ddl, "cassandra") {
        run(&s, &statement, 0).await;
    }
    let again = s.list_columns("lq_schema", "Items").await.unwrap();
    assert_eq!(
        serde_json::to_value(&again).unwrap(),
        serde_json::to_value(&columns).unwrap()
    );

    let ddl = s.object_ddl("lq_schema", "addr", "type").await.unwrap();
    assert!(ddl.contains("CREATE TYPE lq_schema.addr ("), "{ddl}");
    let ddl = s
        .object_ddl("lq_schema", "lq_items_name", "index")
        .await
        .unwrap();
    assert!(
        ddl.starts_with("CREATE INDEX lq_items_name ON lq_schema.\"Items\""),
        "{ddl}"
    );
    if functions {
        let ddl = s
            .object_ddl("lq_schema", "plus(int, int)", "function")
            .await
            .unwrap();
        assert!(
            ddl.contains("CREATE OR REPLACE FUNCTION lq_schema.plus (a int, b int)"),
            "{ddl}"
        );
        assert!(ddl.contains("$$return a + b;$$"), "{ddl}");
        run(&s, "DROP FUNCTION lq_schema.plus(int, int)", 0).await;
        run(&s, &ddl, 0).await;
    }
    let ddl = s
        .object_ddl("lq_schema", "lq_schema", "keyspace")
        .await
        .unwrap();
    assert!(
        ddl.starts_with("CREATE KEYSPACE lq_schema WITH replication = {"),
        "{ddl}"
    );
    assert!(s.object_ddl("lq_schema", "x", "sequence").await.is_err());

    // Materialized views exist only where the server enables them.
    run(
        &s,
        "CREATE TABLE lq_schema.base (id int, day int, v text, PRIMARY KEY (id, day))",
        0,
    )
    .await;
    let view = s
        .execute(&ExecRequest {
            sql: "CREATE MATERIALIZED VIEW lq_schema.by_day AS SELECT * FROM lq_schema.base \
                  WHERE day IS NOT NULL AND id IS NOT NULL PRIMARY KEY (day, id)"
                .into(),
            max_rows: 0,
        })
        .await;
    if view.is_ok() {
        let objects = s.list_objects("lq_schema").await.unwrap();
        assert!(objects
            .iter()
            .any(|o| o.kind == "matview" && o.name == "by_day"));
        let ddl = s
            .object_ddl("lq_schema", "by_day", "matview")
            .await
            .unwrap();
        assert!(
            ddl.contains("CREATE MATERIALIZED VIEW lq_schema.by_day AS"),
            "{ddl}"
        );
        assert!(ddl.contains("PRIMARY KEY (day, id)"), "{ddl}");
        run(
            &s,
            "INSERT INTO lq_schema.base (id, day, v) VALUES (1, 2, 'x')",
            0,
        )
        .await;
        let r = run(&s, "SELECT * FROM lq_schema.by_day", 10).await;
        assert!(r.editable.is_none() && r.read_only_reason.is_some());
    }

    // Streaming reads every row, a page at a time.
    for i in 0..1200 {
        run(
            &s,
            &format!("INSERT INTO lq_schema.base (id, day, v) VALUES ({i}, 1, 'v{i}')"),
            0,
        )
        .await;
    }
    let (tx, mut rx) = tokio::sync::mpsc::channel(4);
    let sink: RowSink = tx;
    let reader = tokio::spawn(async move {
        let mut columns = 0;
        let mut rows = 0;
        while let Some(event) = rx.recv().await {
            match event {
                StreamEvent::Columns(c) => columns = c.len(),
                StreamEvent::Rows(r) => rows += r.len(),
            }
        }
        (columns, rows)
    });
    let total = s
        .stream("SELECT id, day, v FROM lq_schema.base", sink)
        .await
        .unwrap();
    assert_eq!(total, 1200);
    assert_eq!(reader.await.unwrap(), (3, 1200));
    // A reader that goes away stops the stream.
    let (tx, rx) = tokio::sync::mpsc::channel(1);
    drop(rx);
    let err = s
        .stream("SELECT id FROM lq_schema.base", tx)
        .await
        .unwrap_err();
    assert_eq!(err.kind, ErrorKind::Cancelled);

    run(&s, "DROP KEYSPACE lq_schema", 0).await;
}

#[tokio::test]
#[ignore]
async fn cassandra_result_editing() {
    let Some(s) = open().await else {
        return;
    };
    keyspace(&s, "lq_edit").await;
    run(
        &s,
        "CREATE TABLE lq_edit.items (tenant text, day date, seq int, name text, tags set<text>,
            PRIMARY KEY (tenant, day, seq))",
        0,
    )
    .await;
    run(
        &s,
        "INSERT INTO lq_edit.items (tenant, day, seq, name, tags)
         VALUES ('a', '2024-05-06', 1, 'one', {'x'})",
        0,
    )
    .await;
    run(
        &s,
        "INSERT INTO lq_edit.items (tenant, day, seq, name, tags)
         VALUES ('a', '2024-05-06', 2, 'two', null)",
        0,
    )
    .await;

    let r = run(
        &s,
        "SELECT tenant, day, seq, name, tags FROM lq_edit.items",
        100,
    )
    .await;
    assert!(r.read_only_reason.is_none(), "{:?}", r.read_only_reason);
    let target = r.editable.clone().expect("editable");
    assert_eq!(target.schema.as_deref(), Some("lq_edit"));
    assert_eq!(target.table, "items");
    assert_eq!(target.key, [0, 1, 2]);
    assert_eq!(target.columns[4].as_ref().unwrap().type_name, "set<text>");

    // Without the whole key, with an alias, or from system tables: read-only.
    let r = run(&s, "SELECT tenant, name FROM lq_edit.items", 100).await;
    assert!(r.editable.is_none());
    assert!(r.read_only_reason.unwrap().contains("primary key"));
    let r = run(
        &s,
        "SELECT tenant, day, seq, name AS n FROM lq_edit.items",
        100,
    )
    .await;
    assert!(r.editable.is_none());
    let r = run(&s, "SELECT keyspace_name FROM system_schema.keyspaces", 100).await;
    assert!(r.editable.is_none() && r.read_only_reason.is_some());

    let key = |seq: &str| {
        vec![
            Some("a".to_string()),
            Some("2024-05-06".to_string()),
            Some(seq.to_string()),
        ]
    };
    let changes = vec![
        RowChange::Update {
            key: key("1"),
            values: vec![value(3, Some("it's one")), value(4, Some("{'x', 'y'}"))],
        },
        RowChange::Delete { key: key("2") },
        RowChange::Insert {
            values: vec![
                value(0, Some("b")),
                value(1, Some("2024-05-07")),
                value(2, Some("3")),
                value(3, Some("three")),
            ],
        },
    ];
    let saved = s
        .apply_changes(&ApplyRequest {
            target: target.clone(),
            changes,
        })
        .await
        .unwrap();
    assert_eq!((saved.updated, saved.deleted, saved.inserted), (1, 1, 1));
    assert!(!saved.in_transaction);
    let r = run(&s, "SELECT seq, name, tags FROM lq_edit.items", 100).await;
    let mut rows: Vec<Vec<String>> = r
        .rows
        .iter()
        .map(|r| {
            r.iter()
                .map(|v| v.clone().unwrap_or_else(|| "<null>".into()))
                .collect()
        })
        .collect();
    rows.sort();
    assert_eq!(
        rows,
        [["1", "it's one", "{'x', 'y'}"], ["3", "three", "<null>"]]
    );

    // A batch the server refuses changes nothing.
    let bad = s
        .apply_changes(&ApplyRequest {
            target,
            changes: vec![
                RowChange::Delete { key: key("1") },
                RowChange::Delete {
                    key: vec![
                        Some("a".into()),
                        Some("not a date".into()),
                        Some("1".into()),
                    ],
                },
            ],
        })
        .await;
    assert!(bad.is_err());
    let r = run(&s, "SELECT count(*) FROM lq_edit.items", 10).await;
    assert_eq!(row(&r), ["2"]);
    run(&s, "DROP KEYSPACE lq_edit", 0).await;
}

#[tokio::test]
#[ignore]
async fn cassandra_ddl_fixtures() {
    let Some(s) = open().await else {
        return;
    };
    keyspace(&s, "lq_cql").await;
    super::ddl_fixtures::check(&s, "cassandra", "lq_cql").await;
    run(&s, "DROP KEYSPACE lq_cql", 0).await;
}

#[tokio::test]
#[ignore]
async fn cassandra_cancel_stops_waiting() {
    let Some(s) = open().await else {
        return;
    };
    let s = std::sync::Arc::new(s);
    // Nothing runs: cancel is harmless.
    s.cancel().await.unwrap();
    keyspace(&s, "lq_cancel").await;
    run(
        &s,
        "CREATE TABLE lq_cancel.t (id int PRIMARY KEY, v int)",
        0,
    )
    .await;
    let running = {
        let s = s.clone();
        tokio::spawn(async move {
            // Statements in a row, so one is in flight when the cancel comes.
            for i in 0..100_000 {
                let sql = format!("INSERT INTO lq_cancel.t (id, v) VALUES ({i}, 1)");
                if let Err(e) = s.execute(&ExecRequest { sql, max_rows: 0 }).await {
                    return Some(e);
                }
            }
            None
        })
    };
    // Cancel only acts on a request in flight: keep asking until one stops.
    while !running.is_finished() {
        tokio::time::sleep(Duration::from_millis(50)).await;
        s.cancel().await.unwrap();
    }
    let err = running.await.unwrap().expect("cancelled");
    assert_eq!(err.kind, ErrorKind::Cancelled);
    // The session still works.
    run(&s, "SELECT count(*) FROM lq_cancel.t", 10).await;
    run(&s, "DROP KEYSPACE lq_cancel", 0).await;
}
