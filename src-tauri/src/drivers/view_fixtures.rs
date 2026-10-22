//! Runs the SQL the result grid writes to sort and filter a result that hit
//! the row limit (`src/lib/view.ts`) against a real database. The statements
//! and the ids they must return come from `src/lib/view.fixtures.json`, which
//! the frontend tests keep in step with `viewSql`.

use super::Session;
use crate::model::ExecRequest;

const FIXTURES: &str = include_str!("../../../src/lib/view.fixtures.json");

/// `dialect` as in the frontend: `postgres`, `oracle`, `mysql`, `sqlserver`
/// or `sqlite`.
pub async fn check(s: &Session, dialect: &str) {
    let fixtures: serde_json::Value = serde_json::from_str(FIXTURES).expect("view fixtures");
    let cases = fixtures["dialects"][dialect]["cases"]
        .as_array()
        .unwrap_or_else(|| panic!("no view fixtures for {dialect}"));
    assert!(!cases.is_empty());
    for case in cases {
        let label = case["label"].as_str().unwrap_or_default();
        let sql = case["sql"].as_str().expect("sql");
        let expected: Vec<String> = case["ids"]
            .as_array()
            .expect("ids")
            .iter()
            .map(|v| v.as_str().unwrap_or_default().to_string())
            .collect();
        let result = s
            .execute(&ExecRequest {
                sql: sql.into(),
                max_rows: 100,
            })
            .await
            .unwrap_or_else(|e| panic!("{dialect}, {label}: {e}\n{sql}"));
        let ids: Vec<String> = result
            .rows
            .iter()
            .map(|row| row[0].clone().unwrap_or_default())
            .collect();
        assert_eq!(ids, expected, "{dialect}, {label}:\n{sql}");
    }
}
