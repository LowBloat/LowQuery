//! Runs the statements the sidebar menus and the table designer write
//! (`src/lib/sql/ddl.ts`) against a real database, and checks that the DDL
//! each driver reads from its catalog (`object_ddl`) recreates the object.
//! The steps come from `src/lib/sql/ddl.fixtures.json`, which the frontend
//! tests keep in step with `ddl.ts`.

use super::Session;
use crate::model::ExecRequest;

const FIXTURES: &str = include_str!("../../../src/lib/sql/ddl.fixtures.json");

async fn run(s: &Session, sql: &str) -> crate::error::AppResult<crate::model::ExecResult> {
    s.execute(&ExecRequest {
        sql: sql.into(),
        max_rows: 100,
    })
    .await
}

/// Statements of a DDL script: lines ending in `;` close a statement (the
/// fixtures hold no routine bodies), SQL Server batches end at `GO`. Parts
/// that are only comments are left out.
pub(crate) fn statements(ddl: &str, dialect: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut current = String::new();
    for line in ddl.lines() {
        let trimmed = line.trim();
        if dialect == "sqlserver" {
            if trimmed.eq_ignore_ascii_case("GO") {
                out.push(std::mem::take(&mut current));
            } else {
                current.push_str(line);
                current.push('\n');
            }
            continue;
        }
        current.push_str(line);
        current.push('\n');
        if trimmed.ends_with(';') {
            let stmt = std::mem::take(&mut current);
            out.push(stmt.trim().trim_end_matches(';').to_string());
        }
    }
    out.push(current);
    out.into_iter()
        .filter(|s| {
            s.lines()
                .any(|l| !l.trim().is_empty() && !l.trim_start().starts_with("--"))
        })
        .collect()
}

/// `dialect` as in the frontend: `postgres`, `oracle`, `mysql`, `sqlserver`
/// or `sqlite`. `schema` replaces the fixtures' placeholder schema.
pub async fn check(s: &Session, dialect: &str, schema: &str) {
    let fixtures: serde_json::Value = serde_json::from_str(FIXTURES).expect("ddl fixtures");
    let d = &fixtures["dialects"][dialect];
    let token = d["schema"].as_str().expect("schema placeholder");
    let sql_of = |v: &serde_json::Value| v.as_str().expect("sql").replace(token, schema);
    let cleanup = d["cleanup"].as_array().expect("cleanup");
    for sql in cleanup {
        let _ = run(s, &sql_of(sql)).await;
    }

    for step in d["steps"].as_array().expect("steps") {
        let label = step["label"].as_str().unwrap_or_default();
        if let Some(rt) = step.get("roundTrip") {
            let name = rt["name"].as_str().expect("name");
            let kind = rt["kind"].as_str().expect("kind");
            let ddl = s
                .object_ddl(schema, name, kind)
                .await
                .unwrap_or_else(|e| panic!("{dialect}, {label}: {e}"));
            let columns = |cols: Vec<crate::model::DbColumn>| serde_json::to_value(cols).unwrap();
            let before = columns(s.list_columns(schema, name).await.expect("columns"));
            let drop = sql_of(&rt["drop"]);
            run(s, &drop)
                .await
                .unwrap_or_else(|e| panic!("{dialect}, {label}: {e}\n{drop}"));
            for stmt in statements(&ddl, dialect) {
                run(s, &stmt)
                    .await
                    .unwrap_or_else(|e| panic!("{dialect}, {label}: {e}\n{stmt}\n\nfrom:\n{ddl}"));
            }
            let after = columns(s.list_columns(schema, name).await.expect("columns"));
            assert_eq!(after, before, "{dialect}, {label}:\n{ddl}");
        } else if let Some(def) = step.get("ddl") {
            let name = def["name"].as_str().expect("name");
            let ddl = s
                .object_ddl(schema, name, def["kind"].as_str().expect("kind"))
                .await
                .unwrap_or_else(|e| panic!("{dialect}, {label}: {e}"));
            let wanted = def["contains"].as_str().expect("contains");
            assert!(
                ddl.to_lowercase().contains(wanted),
                "{dialect}, {label}: {wanted} not in\n{ddl}"
            );
        } else {
            let sql = sql_of(&step["sql"]);
            let result = run(s, &sql)
                .await
                .unwrap_or_else(|e| panic!("{dialect}, {label}: {e}\n{sql}"));
            if let Some(rows) = step.get("rows") {
                let expected: Vec<String> = rows
                    .as_array()
                    .expect("rows")
                    .iter()
                    .map(|v| v.as_str().unwrap_or_default().to_string())
                    .collect();
                let got: Vec<String> = result
                    .rows
                    .iter()
                    .map(|row| row[0].clone().unwrap_or_default())
                    .collect();
                assert_eq!(got, expected, "{dialect}, {label}:\n{sql}");
            }
        }
    }

    for sql in cleanup {
        let _ = run(s, &sql_of(sql)).await;
    }
}
