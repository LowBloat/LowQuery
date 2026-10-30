//! Catalog facts exports and imports need beyond what the tree shows:
//! generated columns (never inserted), identity columns (inserted with their
//! values, then the generator moved past them) and foreign keys (tables in
//! dependency order). Cassandra has none of the three: the answers are empty.

use std::collections::{HashMap, HashSet};

use super::sql::{qualified, text};
use crate::drivers::Session;
use crate::error::AppResult;
use crate::model::{Engine, ExecRequest};

async fn first_column(s: &Session, sql: String) -> AppResult<Vec<Vec<Option<String>>>> {
    Ok(s.execute(&ExecRequest { sql, max_rows: 0 }).await?.rows)
}

fn names(rows: Vec<Vec<Option<String>>>) -> HashSet<String> {
    rows.into_iter()
        .filter_map(|r| r.into_iter().next().flatten())
        .collect()
}

/// Columns the database computes (generated, virtual, rowversion): an
/// INSERT cannot set them.
pub async fn generated_columns(
    s: &Session,
    schema: &str,
    table: &str,
) -> AppResult<HashSet<String>> {
    let e = s.engine();
    let sql = match e {
        Engine::Postgres => format!(
            "SELECT a.attname::text FROM pg_attribute a
             WHERE a.attrelid = to_regclass({}) AND a.attnum > 0 AND NOT a.attisdropped
               AND COALESCE(to_jsonb(a) ->> 'attgenerated', '') <> ''",
            text(e, &qualified(e, schema, table))
        ),
        Engine::Mysql | Engine::Mariadb => format!(
            "SELECT COLUMN_NAME FROM information_schema.COLUMNS
             WHERE TABLE_SCHEMA = {} AND TABLE_NAME = {}
               AND (EXTRA LIKE '%VIRTUAL GENERATED%' OR EXTRA LIKE '%STORED GENERATED%'
                    OR EXTRA LIKE '%PERSISTENT%')",
            text(e, schema),
            text(e, table)
        ),
        Engine::Sqlserver => format!(
            "SELECT name FROM sys.columns
             WHERE object_id = OBJECT_ID({}) AND (is_computed = 1 OR system_type_id = 189)",
            text(e, &qualified(e, schema, table))
        ),
        Engine::Oracle => format!(
            "SELECT column_name FROM all_tab_cols
             WHERE owner = {} AND table_name = {} AND virtual_column = 'YES'",
            text(e, schema),
            text(e, table)
        ),
        Engine::Sqlite => format!(
            "SELECT name FROM pragma_table_xinfo({}, {}) WHERE hidden IN (2, 3)",
            text(e, table),
            text(e, schema)
        ),
        // CQL has no generated columns (counters are written by UPDATE).
        Engine::Redis | Engine::Cassandra => return Ok(HashSet::new()),
    };
    Ok(names(first_column(s, sql).await?))
}

/// An identity or serial column and what keeps its next value.
pub struct Identity {
    pub column: String,
    /// PostgreSQL: the sequence; Oracle: `ALWAYS` or `BY DEFAULT`.
    pub detail: String,
}

pub async fn identity_columns(s: &Session, schema: &str, table: &str) -> AppResult<Vec<Identity>> {
    let e = s.engine();
    let q = qualified(e, schema, table);
    let sql = match e {
        Engine::Postgres => format!(
            "SELECT a.attname::text, pg_get_serial_sequence({t}, a.attname)
             FROM pg_attribute a
             WHERE a.attrelid = to_regclass({t}) AND a.attnum > 0 AND NOT a.attisdropped
               AND pg_get_serial_sequence({t}, a.attname) IS NOT NULL",
            t = text(e, &q)
        ),
        Engine::Sqlserver => format!(
            "SELECT name, '' FROM sys.identity_columns WHERE object_id = OBJECT_ID({})",
            text(e, &q)
        ),
        Engine::Oracle => format!(
            "SELECT column_name, generation_type FROM all_tab_identity_cols
             WHERE owner = {} AND table_name = {}",
            text(e, schema),
            text(e, table)
        ),
        _ => return Ok(Vec::new()),
    };
    // all_tab_identity_cols only exists from 12c on.
    let rows = match first_column(s, sql).await {
        Ok(rows) => rows,
        Err(_) if e == Engine::Oracle => Vec::new(),
        Err(err) => return Err(err),
    };
    Ok(rows
        .into_iter()
        .filter_map(|mut r| {
            let detail = r.pop().flatten().unwrap_or_default();
            Some(Identity {
                column: r.pop().flatten()?,
                detail,
            })
        })
        .collect())
}

/// Statements around inserting rows that carry identity values: before
/// (SQL Server's IDENTITY_INSERT, an Oracle ALWAYS identity made BY DEFAULT)
/// and after (back off, and the generator moved past the largest value).
pub fn identity_statements(
    e: Engine,
    table: &str,
    ids: &[Identity],
    columns: &[String],
) -> (Vec<String>, Vec<String>) {
    let ids: Vec<&Identity> = ids.iter().filter(|i| columns.contains(&i.column)).collect();
    if ids.is_empty() {
        return (Vec::new(), Vec::new());
    }
    let id = |n: &str| super::sql::ident(e, n);
    match e {
        Engine::Sqlserver => (
            vec![format!("SET IDENTITY_INSERT {table} ON")],
            vec![format!("SET IDENTITY_INSERT {table} OFF")],
        ),
        Engine::Postgres => (
            Vec::new(),
            ids.iter()
                .map(|i| {
                    format!(
                        "SELECT setval({}, COALESCE(MAX({}), 0) + 1, false) FROM {table}",
                        text(e, &i.detail),
                        id(&i.column)
                    )
                })
                .collect(),
        ),
        Engine::Oracle => {
            let before = ids
                .iter()
                .filter(|i| i.detail == "ALWAYS")
                .map(|i| {
                    format!(
                        "ALTER TABLE {table} MODIFY ({} GENERATED BY DEFAULT AS IDENTITY)",
                        id(&i.column)
                    )
                })
                .collect();
            let after = ids
                .iter()
                .map(|i| {
                    let how = if i.detail == "ALWAYS" { "ALWAYS" } else { "BY DEFAULT" };
                    format!(
                        "ALTER TABLE {table} MODIFY ({} GENERATED {how} AS IDENTITY (START WITH LIMIT VALUE))",
                        id(&i.column)
                    )
                })
                .collect();
            (before, after)
        }
        _ => (Vec::new(), Vec::new()),
    }
}

/// Tables ordered so that a table comes after the tables its foreign keys
/// point to (as far as cycles allow).
pub async fn dependency_order(s: &Session, schema: &str, tables: Vec<String>) -> Vec<String> {
    let e = s.engine();
    let sql = match e {
        Engine::Postgres => format!(
            "SELECT cl.relname::text, rf.relname::text FROM pg_constraint c
             JOIN pg_class cl ON cl.oid = c.conrelid JOIN pg_namespace n ON n.oid = cl.relnamespace
             JOIN pg_class rf ON rf.oid = c.confrelid JOIN pg_namespace rn ON rn.oid = rf.relnamespace
             WHERE c.contype = 'f' AND n.nspname = {s} AND rn.nspname = {s}",
            s = text(e, schema)
        ),
        Engine::Mysql | Engine::Mariadb => format!(
            "SELECT TABLE_NAME, REFERENCED_TABLE_NAME FROM information_schema.REFERENTIAL_CONSTRAINTS
             WHERE CONSTRAINT_SCHEMA = {s} AND UNIQUE_CONSTRAINT_SCHEMA = {s}",
            s = text(e, schema)
        ),
        Engine::Sqlserver => format!(
            "SELECT OBJECT_NAME(parent_object_id), OBJECT_NAME(referenced_object_id)
             FROM sys.foreign_keys
             WHERE OBJECT_SCHEMA_NAME(parent_object_id) = {s}
               AND OBJECT_SCHEMA_NAME(referenced_object_id) = {s}",
            s = text(e, schema)
        ),
        Engine::Oracle => format!(
            "SELECT c.table_name, r.table_name FROM all_constraints c
             JOIN all_constraints r ON r.owner = c.r_owner AND r.constraint_name = c.r_constraint_name
             WHERE c.constraint_type = 'R' AND c.owner = {s} AND r.owner = {s}",
            s = text(e, schema)
        ),
        Engine::Sqlite => format!(
            "SELECT m.name, f.\"table\" FROM {}.sqlite_schema m,
                    pragma_foreign_key_list(m.name, {}) f
             WHERE m.type = 'table'",
            super::sql::ident(e, schema),
            text(e, schema)
        ),
        // No foreign keys in Redis or CQL.
        Engine::Redis | Engine::Cassandra => return tables,
    };
    let Ok(rows) = first_column(s, sql).await else {
        return tables;
    };
    let mut parents: HashMap<String, HashSet<String>> = HashMap::new();
    for mut r in rows {
        let (Some(Some(parent)), Some(Some(child))) = (r.pop(), r.pop()) else {
            continue;
        };
        if parent != child {
            parents.entry(child).or_default().insert(parent);
        }
    }
    order(tables, &parents)
}

/// Kahn's order, keeping the given order among independent tables.
fn order(tables: Vec<String>, parents: &HashMap<String, HashSet<String>>) -> Vec<String> {
    let set: HashSet<&String> = tables.iter().collect();
    let mut done: HashSet<String> = HashSet::new();
    let mut out = Vec::with_capacity(tables.len());
    let mut left = tables.clone();
    while !left.is_empty() {
        let before = left.len();
        left.retain(|t| {
            let ready = parents
                .get(t)
                .is_none_or(|ps| ps.iter().all(|p| done.contains(p) || !set.contains(p)));
            if ready {
                done.insert(t.clone());
                out.push(t.clone());
            }
            !ready
        });
        if left.len() == before {
            // A cycle: keep the rest in their order.
            out.append(&mut left);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parents_come_first() {
        let mut parents = HashMap::new();
        parents.insert("items".to_string(), HashSet::from(["orders".to_string()]));
        parents.insert(
            "orders".to_string(),
            HashSet::from(["customers".to_string()]),
        );
        let tables = vec![
            "items".into(),
            "orders".into(),
            "customers".into(),
            "logs".into(),
        ];
        assert_eq!(
            order(tables, &parents),
            ["customers", "logs", "orders", "items"]
        );
        // A cycle keeps the original order.
        let mut cyc = HashMap::new();
        cyc.insert("a".to_string(), HashSet::from(["b".to_string()]));
        cyc.insert("b".to_string(), HashSet::from(["a".to_string()]));
        assert_eq!(order(vec!["a".into(), "b".into()], &cyc), ["a", "b"]);
    }
}
