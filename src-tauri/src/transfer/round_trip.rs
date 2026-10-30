//! Exports a table with awkward values (quotes, line breaks, backslashes,
//! accents, binary, JSON, dates, identity and generated columns) and loads
//! it back: from the SQL script after dropping the table, and from CSV and
//! JSON after deleting its rows. The rows must come back the same.
//!
//! Cassandra (`cassandra_export_import_round_trip`, needs `LQ_CASSANDRA_HOST`
//! and creates the keyspace `lq_round_trip`) has no ORDER BY on this table, no
//! identity or generated columns, and keeps its collections as CQL literals.

use std::sync::atomic::AtomicBool;

use futures_util::FutureExt;

use super::export::{export, ExportContent, ExportFormat, ExportRequest};
use super::import::{import, ImportRequest, ReadOptions};
use crate::drivers::Session;
use crate::model::{ConnectionProfile, DbObject, Engine, ExecRequest, SslMode};

struct Case {
    create: &'static str,
    fill: &'static [&'static str],
    /// Columns compared (rowversion and the like change on their own).
    compare: &'static str,
}

fn case(engine: Engine) -> Case {
    match engine {
        Engine::Postgres => Case {
            create: "CREATE TABLE lq_schema.lq_xfer (
                id integer GENERATED ALWAYS AS IDENTITY PRIMARY KEY, name text,
                amount numeric(12,2), born date, seen timestamp(3), flag boolean, data bytea,
                doc jsonb, total numeric GENERATED ALWAYS AS (amount * 2) STORED)",
            fill: &[
                "INSERT INTO lq_schema.lq_xfer (name, amount, born, seen, flag, data, doc) VALUES
                 ('it''s \"quoted\"' || chr(10) || 'line \\ back', 1234.56, '2024-02-29',
                  '2024-02-29 13:45:10.25', true, '\\xdeadbeef', '{\"a\": [1, 2]}'),
                 ('Ação ☃', -0.5, NULL, NULL, false, NULL, NULL),
                 (NULL, NULL, '1999-12-31', NULL, NULL, '\\x', '[]')",
            ],
            compare: "id, name, amount, born, seen, flag, data, doc, total",
        },
        Engine::Mysql | Engine::Mariadb => Case {
            create: "CREATE TABLE lq_schema.lq_xfer (
                id INT AUTO_INCREMENT PRIMARY KEY, name TEXT, amount DECIMAL(12,2), born DATE,
                seen DATETIME(3), flag TINYINT(1), data VARBINARY(64), doc TEXT,
                total DECIMAL(14,2) AS (amount * 2) STORED)",
            fill: &[
                "INSERT INTO lq_schema.lq_xfer (name, amount, born, seen, flag, data, doc) VALUES
                 ('it''s \"quoted\"\\nline \\\\ back', 1234.56, '2024-02-29',
                  '2024-02-29 13:45:10.250', 1, X'DEADBEEF', '{\"a\": [1, 2]}'),
                 ('Ação ☃', -0.5, NULL, NULL, 0, NULL, NULL),
                 (NULL, NULL, '1999-12-31', NULL, NULL, X'', '[]')",
            ],
            compare: "id, name, amount, born, seen, flag, data, doc, total",
        },
        Engine::Sqlserver => Case {
            create: "CREATE TABLE lq_schema.lq_xfer (
                id int IDENTITY(1,1) PRIMARY KEY, name nvarchar(200), amount decimal(12,2),
                born date, seen datetime2(3), flag bit, data varbinary(64), doc nvarchar(max),
                total AS (amount * 2), rv rowversion)",
            fill: &[
                "INSERT INTO lq_schema.lq_xfer (name, amount, born, seen, flag, data, doc) VALUES
                 (N'it''s \"quoted\"' + CHAR(10) + N'line \\ back', 1234.56, '2024-02-29',
                  '2024-02-29 13:45:10.250', 1, 0xDEADBEEF, N'{\"a\": [1, 2]}'),
                 (N'Ação ☃', -0.5, NULL, NULL, 0, NULL, NULL),
                 (NULL, NULL, '1999-12-31', NULL, NULL, 0x, N'[]')",
            ],
            compare: "id, name, amount, born, seen, flag, data, doc, total",
        },
        Engine::Oracle => Case {
            create: "CREATE TABLE lq_schema.lq_xfer (
                id NUMBER(10) GENERATED ALWAYS AS IDENTITY PRIMARY KEY, name VARCHAR2(200),
                amount NUMBER(12,2), born DATE, seen TIMESTAMP(3), flag NUMBER(1), data RAW(64),
                doc CLOB, total NUMBER GENERATED ALWAYS AS (amount * 2) VIRTUAL)",
            fill: &[
                "INSERT INTO lq_schema.lq_xfer (name, amount, born, seen, flag, data, doc) VALUES
                 ('it''s \"quoted\"' || chr(10) || 'line \\ back', 1234.56, DATE '2024-02-29',
                  TIMESTAMP '2024-02-29 13:45:10.250', 1, HEXTORAW('DEADBEEF'), RPAD('x', 3000, 'y'))",
                "INSERT INTO lq_schema.lq_xfer (name, amount, flag) VALUES ('Ação ☃', -0.5, 0)",
                "INSERT INTO lq_schema.lq_xfer (born, doc) VALUES (DATE '1999-12-31', '[]')",
            ],
            compare: "id, name, amount, born, seen, flag, data, doc, total",
        },
        Engine::Cassandra => Case {
            create: "CREATE TABLE lq_schema.lq_xfer (
                id int PRIMARY KEY, name text, big varint, amount decimal, born date,
                seen timestamp, flag boolean, data blob, uid uuid, nums list<int>,
                tags set<text>, scores map<text, int>)",
            fill: &[
                "INSERT INTO lq_schema.lq_xfer (id, name, big, amount, born, seen, flag, data,
                 uid, nums, tags, scores) VALUES (1, 'it''s \"quoted\"
line \\ back; -- x', 123456789012345678901234567890, 1234567890.0987654321, '2024-02-29',
                 '2024-02-29 13:45:10.250+0000', true, 0xdeadbeef,
                 123e4567-e89b-12d3-a456-426614174000, [3, 1, 2], {'a', 'it''s, {x}'},
                 {'k': 1, 'it''s': -2})",
                "INSERT INTO lq_schema.lq_xfer (id, name, big, amount, flag, data)
                 VALUES (2, 'Ação ☃', -98765432109876543210, -0.5, false, 0x)",
                "INSERT INTO lq_schema.lq_xfer (id, born, uid) VALUES
                 (3, '1999-12-31', 00000000-0000-1000-8000-000000000000)",
            ],
            compare: "id, name, big, amount, born, seen, flag, data, uid, nums, tags, scores",
        },
        Engine::Sqlite | Engine::Redis => Case {
            create: "CREATE TABLE lq_schema.lq_xfer (
                id INTEGER PRIMARY KEY AUTOINCREMENT, name TEXT, amount NUMERIC, born TEXT,
                seen TEXT, flag INTEGER, data BLOB, doc TEXT,
                total NUMERIC GENERATED ALWAYS AS (amount * 2) STORED)",
            fill: &[
                "INSERT INTO lq_schema.lq_xfer (name, amount, born, seen, flag, data, doc) VALUES
                 ('it''s \"quoted\"' || char(10) || 'line \\ back', 1234.56, '2024-02-29',
                  '2024-02-29 13:45:10.250', 1, X'DEADBEEF', '{\"a\": [1, 2]}'),
                 ('Ação ☃', -0.5, NULL, NULL, 0, NULL, NULL),
                 (NULL, NULL, '1999-12-31', NULL, NULL, X'', '[]')",
            ],
            compare: "id, name, amount, born, seen, flag, data, doc, total",
        },
    }
}

async fn run(s: &Session, sql: &str) -> Vec<Vec<Option<String>>> {
    s.execute(&ExecRequest {
        sql: sql.into(),
        max_rows: 0,
    })
    .await
    .unwrap_or_else(|e| panic!("{e}\n{sql}"))
    .rows
}

fn temp(name: &str) -> String {
    std::env::temp_dir()
        .join(format!("lq_xfer_{}_{name}", std::process::id()))
        .to_string_lossy()
        .into_owned()
}

/// `schema` replaces `lq_schema`; Oracle names fold to uppercase.
pub async fn check(s: &Session, schema: &str) {
    let e = s.engine();
    let c = case(e);
    let up = |t: &str| {
        if e == Engine::Oracle {
            t.to_uppercase()
        } else {
            t.to_string()
        }
    };
    let sql = |t: &str| t.replace("lq_schema", schema);
    let table = up("lq_xfer");
    let target = super::sql::qualified(e, schema, &table);
    let _ = s
        .execute(&ExecRequest {
            sql: format!("DROP TABLE {target}"),
            max_rows: 0,
        })
        .await;
    run(s, &sql(c.create)).await;
    for f in c.fill {
        run(s, &sql(f)).await;
    }
    // CQL cannot ORDER BY a regular column: rows are sorted here instead (the
    // first column is the integer key).
    let order = if e == Engine::Cassandra {
        ""
    } else {
        " ORDER BY 1"
    };
    let select = format!("SELECT {} FROM {target}{order}", up(c.compare));
    let fetch = |sel: String| async move {
        let mut rows = run(s, &sel).await;
        if e == Engine::Cassandra {
            rows.sort_by_key(|r| r[0].as_deref().and_then(|k| k.parse::<i64>().ok()));
        }
        rows
    };
    let original = fetch(select.clone()).await;
    assert_eq!(original.len(), 3);

    let cancel = AtomicBool::new(false);
    let quiet = |_| {};
    let request = |format, path: &str| ExportRequest {
        job_id: "test".into(),
        session_id: String::new(),
        schema: schema.into(),
        objects: Some(vec![DbObject {
            name: table.clone(),
            kind: "table".into(),
        }]),
        format,
        content: ExportContent::All,
        path: path.into(),
        source: "LowQuery test export".into(),
    };

    // SQL script: drop the table, run the script.
    let path = temp("script.sql");
    export(s, &request(ExportFormat::Sql, &path), &cancel, &quiet)
        .await
        .expect("SQL export");
    let script = std::fs::read_to_string(&path).unwrap();
    run(s, &format!("DROP TABLE {target}")).await;
    for stmt in crate::drivers::ddl_fixtures::statements(&script, dialect(e)) {
        s.execute(&ExecRequest {
            sql: stmt.clone(),
            max_rows: 0,
        })
        .await
        .unwrap_or_else(|err| panic!("{err}\n{stmt}\n\nscript:\n{script}"));
    }
    assert_eq!(
        fetch(select.clone()).await,
        original,
        "SQL round trip:\n{script}"
    );
    let _ = std::fs::remove_file(&path);

    // CSV and JSON: delete the rows, import the file.
    for (format, name) in [
        (ExportFormat::Csv, "rows.csv"),
        (ExportFormat::CsvExcel, "excel.csv"),
        (ExportFormat::Json, "rows.json"),
    ] {
        let path = temp(name);
        export(s, &request(format, &path), &cancel, &quiet)
            .await
            .unwrap_or_else(|err| panic!("{name}: {err}"));
        let summary = import(
            s,
            &ImportRequest {
                job_id: "test".into(),
                session_id: String::new(),
                path: path.clone(),
                options: ReadOptions::default(),
                schema: schema.into(),
                table: table.clone(),
                replace: true,
            },
            &cancel,
            &quiet,
        )
        .await
        .unwrap_or_else(|err| {
            panic!(
                "{name}: {err}\n{}",
                std::fs::read_to_string(&path).unwrap_or_default()
            )
        });
        assert_eq!(summary.rows, 3, "{name}");
        assert_eq!(fetch(select.clone()).await, original, "{name} round trip");
        let _ = std::fs::remove_file(&path);
    }

    // Identity generators moved past the imported values (CQL has none: its
    // keys are always given).
    if e != Engine::Cassandra {
        let name_col = super::sql::ident(e, &up("name"));
        run(
            s,
            &format!("INSERT INTO {target} ({name_col}) VALUES ('after')"),
        )
        .await;
    }
    run(s, &format!("DROP TABLE {target}")).await;
}

fn dialect(e: Engine) -> &'static str {
    match e {
        Engine::Postgres => "postgres",
        Engine::Oracle => "oracle",
        Engine::Mysql | Engine::Mariadb => "mysql",
        Engine::Sqlserver => "sqlserver",
        Engine::Cassandra => "cassandra",
        Engine::Sqlite | Engine::Redis => "sqlite",
    }
}

fn env(name: &str) -> Option<String> {
    std::env::var(name).ok().filter(|v| !v.is_empty())
}

#[tokio::test]
#[ignore]
async fn cassandra_export_import_round_trip() {
    let Some(host) = env("LQ_CASSANDRA_HOST") else {
        return;
    };
    let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
    let profile = ConnectionProfile {
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
    let s = Session::open(&profile, password.as_deref())
        .await
        .expect("connection");
    let keyspace = "lq_round_trip";
    let _ = s
        .execute(&ExecRequest {
            sql: format!("DROP KEYSPACE IF EXISTS {keyspace}"),
            max_rows: 0,
        })
        .await;
    run(
        &s,
        &format!(
            "CREATE KEYSPACE {keyspace} WITH replication = \
             {{'class': 'SimpleStrategy', 'replication_factor': 1}}"
        ),
    )
    .await;
    // Also when a check fails: the keyspace must not outlive the test.
    let outcome = std::panic::AssertUnwindSafe(check(&s, keyspace))
        .catch_unwind()
        .await;
    let _ = s
        .execute(&ExecRequest {
            sql: format!("DROP KEYSPACE IF EXISTS {keyspace}"),
            max_rows: 0,
        })
        .await;
    if let Err(panic) = outcome {
        std::panic::resume_unwind(panic);
    }
}
