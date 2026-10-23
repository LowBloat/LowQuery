//! Exports a table with awkward values (quotes, line breaks, backslashes,
//! accents, binary, JSON, dates, identity and generated columns) and loads
//! it back: from the SQL script after dropping the table, and from CSV and
//! JSON after deleting its rows. The rows must come back the same.

use std::sync::atomic::AtomicBool;

use super::export::{export, ExportContent, ExportFormat, ExportRequest};
use super::import::{import, ImportRequest, ReadOptions};
use crate::drivers::Session;
use crate::model::{DbObject, Engine, ExecRequest};

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
    let select = format!("SELECT {} FROM {target} ORDER BY 1", up(c.compare));
    let original = run(s, &select).await;
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
    assert_eq!(run(s, &select).await, original, "SQL round trip:\n{script}");
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
        assert_eq!(run(s, &select).await, original, "{name} round trip");
        let _ = std::fs::remove_file(&path);
    }

    // Identity generators moved past the imported values.
    let name_col = super::sql::ident(e, &up("name"));
    run(
        s,
        &format!("INSERT INTO {target} ({name_col}) VALUES ('after')"),
    )
    .await;
    run(s, &format!("DROP TABLE {target}")).await;
}

fn dialect(e: Engine) -> &'static str {
    match e {
        Engine::Postgres => "postgres",
        Engine::Oracle => "oracle",
        Engine::Mysql | Engine::Mariadb => "mysql",
        Engine::Sqlserver => "sqlserver",
        _ => "sqlite",
    }
}
