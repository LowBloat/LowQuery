//! SQL text for exports and imports: identifiers, value literals and INSERT
//! statements in each database's syntax. Values arrive as the drivers format
//! them (see the `cell` functions), and every literal reads back as the same
//! value in the same database.
//!
//! CQL (Cassandra) literals depend on the column's type, not only its kind
//! (a uuid is bare, text is quoted, both are [`ValueKind::Text`]), so they
//! come from [`literal_typed`], which hands the type to the driver's
//! `cql_literal`.

use std::io::{self, Write};

use crate::model::{Engine, ValueKind};

/// Always-quoted identifier: exports keep names exactly as the catalog has them
/// (CQL too: `"` doubled, so case-sensitive names survive).
pub fn ident(engine: Engine, name: &str) -> String {
    match engine {
        Engine::Mysql | Engine::Mariadb => format!("`{}`", name.replace('`', "``")),
        Engine::Sqlserver => format!("[{}]", name.replace(']', "]]")),
        _ => format!("\"{}\"", name.replace('"', "\"\"")),
    }
}

pub fn qualified(engine: Engine, schema: &str, name: &str) -> String {
    format!("{}.{}", ident(engine, schema), ident(engine, name))
}

/// Plain string literal for catalog queries (names, never user data).
pub fn text(engine: Engine, s: &str) -> String {
    let body = s.replace('\'', "''");
    match engine {
        Engine::Sqlserver => format!("N'{body}'"),
        _ => format!("'{body}'"),
    }
}

/// The kind of value a column holds, from its declared type (catalog type
/// names: `numeric(12,2)`, `VARCHAR2(50)`, `datetime2(3)`...).
pub fn kind_of_type(type_name: &str) -> ValueKind {
    let t = type_name.to_ascii_lowercase();
    let has = |words: &[&str]| words.iter().any(|w| t.contains(w));
    if t.starts_with("interval") {
        ValueKind::Other
    } else if has(&["bool"]) || t == "bit" {
        ValueKind::Bool
    } else if has(&[
        "int", "numeric", "decimal", "number", "float", "double", "real", "money", "serial",
    ]) {
        ValueKind::Number
    } else if has(&["date", "time", "year"]) {
        ValueKind::Temporal
    } else if has(&["bytea", "blob", "binary", "raw", "image"]) {
        ValueKind::Binary
    } else if has(&["json"]) {
        ValueKind::Json
    } else {
        ValueKind::Text
    }
}

/// The kind of value a column holds, from its CQL type as `system_schema`
/// writes it (`int`, `frozen<map<text, int>>`, `vector<float, 3>`...):
/// collections, tuples, UDTs and vectors are [`ValueKind::Other`].
pub fn kind_of_cql_type(type_name: &str) -> ValueKind {
    match cql_native_type(type_name).as_str() {
        "tinyint" | "smallint" | "int" | "bigint" | "varint" | "decimal" | "float" | "double"
        | "counter" => ValueKind::Number,
        "boolean" => ValueKind::Bool,
        "timestamp" | "date" | "time" | "duration" => ValueKind::Temporal,
        "blob" => ValueKind::Binary,
        "text" | "varchar" | "ascii" | "uuid" | "timeuuid" | "inet" => ValueKind::Text,
        _ => ValueKind::Other,
    }
}

/// The kind of a column of `engine` from its declared type.
pub fn column_kind(engine: Engine, type_name: &str) -> ValueKind {
    if engine == Engine::Cassandra {
        kind_of_cql_type(type_name)
    } else {
        kind_of_type(type_name)
    }
}

/// A CQL type name lowercased and without `frozen<...>` wrappers; what is
/// left is a native type name (`int`) or a collection, tuple, vector or UDT.
fn cql_native_type(type_name: &str) -> String {
    let mut t = type_name.trim().to_ascii_lowercase();
    while let Some(inner) = t
        .strip_prefix("frozen<")
        .and_then(|rest| rest.strip_suffix('>'))
    {
        t = inner.trim().to_string();
    }
    t
}

fn is_number(v: &str) -> bool {
    let v = v.strip_prefix(['-', '+']).unwrap_or(v);
    let (mantissa, exponent) = match v.find(['e', 'E']) {
        Some(i) => (&v[..i], Some(&v[i + 1..])),
        None => (v, None),
    };
    let digits = mantissa.replacen('.', "", 1);
    let mantissa_ok = !digits.is_empty()
        && digits.bytes().all(|b| b.is_ascii_digit())
        && mantissa.matches('.').count() <= 1;
    let exponent_ok = exponent.is_none_or(|e| {
        let e = e.strip_prefix(['-', '+']).unwrap_or(e);
        !e.is_empty() && e.bytes().all(|b| b.is_ascii_digit())
    });
    mantissa_ok && exponent_ok
}

/// `true`, `false` or neither, from the words drivers and spreadsheets use.
pub fn truth(v: &str) -> Option<bool> {
    match v.trim().to_lowercase().as_str() {
        "true" | "t" | "1" | "yes" | "y" | "sim" | "s" | "verdadeiro" | "v" => Some(true),
        "false" | "f" | "0" | "no" | "n" | "não" | "nao" | "falso" => Some(false),
        _ => None,
    }
}

/// Hex digits of a binary value in any of the drivers' formats: `\x...`
/// (PostgreSQL), `0x...` (MySQL, SQL Server), `X'...'` (SQLite) or bare
/// hex (Oracle).
pub fn hex_digits(v: &str) -> Option<&str> {
    let v = v.trim();
    let digits = if let Some(rest) = v.strip_prefix("\\x") {
        rest
    } else if let Some(rest) = v.strip_prefix("0x").or_else(|| v.strip_prefix("0X")) {
        rest
    } else if (v.starts_with("X'") || v.starts_with("x'")) && v.ends_with('\'') && v.len() >= 3 {
        &v[2..v.len() - 1]
    } else {
        v
    };
    (digits.len() % 2 == 0 && digits.bytes().all(|b| b.is_ascii_hexdigit())).then_some(digits)
}

fn string_literal(engine: Engine, v: &str) -> String {
    match engine {
        Engine::Mysql | Engine::Mariadb => {
            // Backslash is an escape character in MySQL's default mode, as
            // in mysqldump output.
            format!("'{}'", v.replace('\\', "\\\\").replace('\'', "''"))
        }
        Engine::Sqlserver => format!("N'{}'", v.replace('\'', "''")),
        Engine::Oracle if v.chars().count() > 1000 => {
            // A literal holds at most 4000 bytes: longer text is built from
            // CLOB pieces.
            let chars: Vec<char> = v.chars().collect();
            chars
                .chunks(1000)
                .map(|c| {
                    format!(
                        "TO_CLOB('{}')",
                        c.iter().collect::<String>().replace('\'', "''")
                    )
                })
                .collect::<Vec<_>>()
                .join(" || ")
        }
        _ => format!("'{}'", v.replace('\'', "''")),
    }
}

/// A value as a literal of `engine`, for a column of `kind`. CQL columns
/// should go through [`literal_typed`]; here a CQL value is written as the
/// plain CQL type of its kind (`Other` cannot be told apart: it is quoted).
pub fn literal(engine: Engine, kind: ValueKind, value: Option<&str>) -> String {
    if engine == Engine::Cassandra {
        let ty = match kind {
            ValueKind::Number => "decimal",
            ValueKind::Bool => "boolean",
            ValueKind::Binary => "blob",
            ValueKind::Temporal => "timestamp",
            ValueKind::Text | ValueKind::Json | ValueKind::Other => "text",
        };
        return literal_typed(engine, kind, ty, value);
    }
    let Some(v) = value else {
        return "NULL".into();
    };
    match kind {
        ValueKind::Number if is_number(v.trim()) => v.trim().to_string(),
        ValueKind::Bool => match (truth(v), engine) {
            (Some(b), Engine::Postgres | Engine::Oracle) => if b { "TRUE" } else { "FALSE" }.into(),
            (Some(b), _) => if b { "1" } else { "0" }.into(),
            (None, _) => string_literal(engine, v),
        },
        ValueKind::Binary => match hex_digits(v) {
            Some(h) => match engine {
                Engine::Postgres => format!("decode('{h}', 'hex')"),
                Engine::Sqlserver => format!("0x{h}"),
                Engine::Oracle => format!("HEXTORAW('{h}')"),
                _ => format!("X'{h}'"),
            },
            None => string_literal(engine, v),
        },
        ValueKind::Temporal if engine == Engine::Oracle => {
            let v = v.trim();
            let date_only = v.len() == 10 && v.as_bytes().get(4) == Some(&b'-');
            if date_only {
                format!("DATE '{v}'")
            } else if v.len() >= 19 && v.as_bytes().get(4) == Some(&b'-') {
                format!("TIMESTAMP '{v}'")
            } else {
                string_literal(engine, v)
            }
        }
        _ => string_literal(engine, v),
    }
}

/// A value as a literal of `engine`, for a column of `kind` and declared
/// type `type_name` (`DbColumn.type_name`, the same text as
/// `ColumnInfo.type_name`). Only CQL reads the type: numbers, booleans,
/// uuids, blobs and durations are bare, text and temporal values quoted, and
/// collections, tuples, UDTs and vectors (whose cell text is already a CQL
/// literal) are kept as they are; the driver's `cql_literal` decides.
pub fn literal_typed(
    engine: Engine,
    kind: ValueKind,
    type_name: &str,
    value: Option<&str>,
) -> String {
    if engine == Engine::Cassandra {
        crate::drivers::cassandra::cql_literal(type_name, value)
    } else {
        literal(engine, kind, value)
    }
}

/// Where rows go: the table and the columns written, with their kinds.
pub struct InsertTarget<'a> {
    pub engine: Engine,
    /// Qualified, quoted table name.
    pub table: &'a str,
    /// Quoted column names.
    pub columns: &'a [String],
    pub kinds: &'a [ValueKind],
    /// Declared types of the columns, as `DbColumn.type_name`. Only CQL reads
    /// them; other engines may pass an empty slice.
    pub types: &'a [String],
    /// Whether each column is part of the primary key. Only CQL reads it (to
    /// address counter updates); may be empty.
    pub keys: &'a [bool],
}

/// Rows per INSERT where the engine takes several (SQL Server allows 1000).
pub const ROWS_PER_INSERT: usize = 100;

/// INSERT statements for `rows`, each ending in `;` and a new line. Several
/// rows share a statement except in Oracle; PostgreSQL inserts override
/// identity columns that only accept generated values.
pub fn write_inserts(
    out: &mut impl Write,
    t: &InsertTarget<'_>,
    rows: &[Vec<Option<String>>],
) -> io::Result<()> {
    for sql in insert_statements(t, rows) {
        out.write_all(sql.as_bytes())?;
        out.write_all(b";\n")?;
    }
    Ok(())
}

/// The INSERT statements for `rows`, without terminators.
pub fn insert_statements(t: &InsertTarget<'_>, rows: &[Vec<Option<String>>]) -> Vec<String> {
    let head = format!(
        "INSERT INTO {} ({}){}",
        t.table,
        t.columns.join(", "),
        if t.engine == Engine::Postgres {
            " OVERRIDING SYSTEM VALUE"
        } else {
            ""
        }
    );
    let ty = |i: usize| t.types.get(i).map_or("", String::as_str);
    if t.engine == Engine::Cassandra && (0..t.columns.len()).any(|i| is_counter(ty(i))) {
        return counter_updates(t, rows);
    }
    let tuple = |row: &Vec<Option<String>>| {
        let values: Vec<String> = row
            .iter()
            .zip(t.kinds)
            .enumerate()
            .map(|(i, (v, k))| literal_typed(t.engine, *k, ty(i), v.as_deref()))
            .collect();
        format!("({})", values.join(", "))
    };
    // CQL has no multi-row INSERT.
    let per = if matches!(t.engine, Engine::Oracle | Engine::Cassandra) {
        1
    } else {
        ROWS_PER_INSERT
    };
    rows.chunks(per)
        .map(|chunk| {
            let values: Vec<String> = chunk.iter().map(tuple).collect();
            if values.len() == 1 {
                format!("{head} VALUES {}", values[0])
            } else {
                format!("{head} VALUES\n{}", values.join(",\n"))
            }
        })
        .collect()
}

fn is_counter(type_name: &str) -> bool {
    cql_native_type(type_name) == "counter"
}

/// Rows of a CQL table with counter columns. Counters cannot be INSERTed, so
/// each row becomes `UPDATE t SET c = c + n WHERE key = ...`: loaded into an
/// empty table (a fresh export script, or an import that deleted the rows)
/// the counters hold the exported values, and on a table that has rows the
/// values are added to them. A row without a counter value has nothing to
/// add and is left out. With no key columns known no UPDATE can address the
/// row, so INSERTs are written and the server refuses them with its message.
fn counter_updates(t: &InsertTarget<'_>, rows: &[Vec<Option<String>>]) -> Vec<String> {
    let ty = |i: usize| t.types.get(i).map_or("", String::as_str);
    let key = |i: usize| t.keys.get(i).copied().unwrap_or(false);
    let n = t.columns.len();
    let lit = |row: &[Option<String>], i: usize| {
        let kind = t.kinds.get(i).copied().unwrap_or(ValueKind::Other);
        literal_typed(t.engine, kind, ty(i), row.get(i).and_then(|v| v.as_deref()))
    };
    if !(0..n).any(key) {
        let head = format!("INSERT INTO {} ({})", t.table, t.columns.join(", "));
        return rows
            .iter()
            .map(|row| {
                let values: Vec<String> = (0..n).map(|i| lit(row, i)).collect();
                format!("{head} VALUES ({})", values.join(", "))
            })
            .collect();
    }
    rows.iter()
        .filter_map(|row| {
            let sets: Vec<String> = (0..n)
                .filter(|&i| is_counter(ty(i)) && row.get(i).is_some_and(Option::is_some))
                .map(|i| {
                    let (c, v) = (&t.columns[i], lit(row, i));
                    match v.strip_prefix('-') {
                        Some(abs) => format!("{c} = {c} - {abs}"),
                        None => format!("{c} = {c} + {v}"),
                    }
                })
                .collect();
            if sets.is_empty() {
                return None;
            }
            let wheres: Vec<String> = (0..n)
                .filter(|&i| key(i))
                .map(|i| format!("{} = {}", t.columns[i], lit(row, i)))
                .collect();
            Some(format!(
                "UPDATE {} SET {} WHERE {}",
                t.table,
                sets.join(", "),
                wheres.join(" AND ")
            ))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn numbers_stay_bare_only_when_numeric() {
        assert!(is_number("12"));
        assert!(is_number("-0.5"));
        assert!(is_number(".5"));
        assert!(is_number("1.5e-10"));
        assert!(!is_number("NaN"));
        assert!(!is_number("1.2.3"));
        assert!(!is_number("$12.00"));
        assert_eq!(
            literal(Engine::Postgres, ValueKind::Number, Some("12.50")),
            "12.50"
        );
        assert_eq!(
            literal(Engine::Postgres, ValueKind::Number, Some("NaN")),
            "'NaN'"
        );
    }

    #[test]
    fn strings_escape_per_engine() {
        let v = Some("it's a \\ path");
        assert_eq!(
            literal(Engine::Postgres, ValueKind::Text, v),
            "'it''s a \\ path'"
        );
        assert_eq!(
            literal(Engine::Mysql, ValueKind::Text, v),
            "'it''s a \\\\ path'"
        );
        assert_eq!(
            literal(Engine::Sqlserver, ValueKind::Text, v),
            "N'it''s a \\ path'"
        );
        assert_eq!(literal(Engine::Oracle, ValueKind::Text, None), "NULL");
        let long = "a".repeat(2500);
        let lit = literal(Engine::Oracle, ValueKind::Text, Some(&long));
        assert_eq!(lit.matches("TO_CLOB(").count(), 3);
    }

    #[test]
    fn binary_and_booleans() {
        assert_eq!(
            literal(Engine::Postgres, ValueKind::Binary, Some("\\xdeadbeef")),
            "decode('deadbeef', 'hex')"
        );
        assert_eq!(
            literal(Engine::Sqlserver, ValueKind::Binary, Some("0x00FF")),
            "0x00FF"
        );
        assert_eq!(literal(Engine::Mysql, ValueKind::Binary, Some("0x")), "X''");
        assert_eq!(
            literal(Engine::Sqlite, ValueKind::Binary, Some("X'0A'")),
            "X'0A'"
        );
        assert_eq!(
            literal(Engine::Oracle, ValueKind::Binary, Some("DEADBEEF")),
            "HEXTORAW('DEADBEEF')"
        );
        assert_eq!(
            literal(Engine::Postgres, ValueKind::Bool, Some("true")),
            "TRUE"
        );
        assert_eq!(
            literal(Engine::Sqlserver, ValueKind::Bool, Some("false")),
            "0"
        );
        assert_eq!(literal(Engine::Mysql, ValueKind::Bool, Some("Sim")), "1");
    }

    #[test]
    fn oracle_dates_use_typed_literals() {
        assert_eq!(
            literal(Engine::Oracle, ValueKind::Temporal, Some("2024-02-29")),
            "DATE '2024-02-29'"
        );
        assert_eq!(
            literal(
                Engine::Oracle,
                ValueKind::Temporal,
                Some("2024-02-29 13:45:10.250 -03:00")
            ),
            "TIMESTAMP '2024-02-29 13:45:10.250 -03:00'"
        );
        assert_eq!(
            literal(Engine::Postgres, ValueKind::Temporal, Some("infinity")),
            "'infinity'"
        );
    }

    #[test]
    fn types_map_to_kinds() {
        assert_eq!(kind_of_type("numeric(12,2)"), ValueKind::Number);
        assert_eq!(kind_of_type("BINARY_DOUBLE"), ValueKind::Number);
        assert_eq!(kind_of_type("interval"), ValueKind::Other);
        assert_eq!(
            kind_of_type("timestamp with time zone"),
            ValueKind::Temporal
        );
        assert_eq!(kind_of_type("varbinary(max)"), ValueKind::Binary);
        assert_eq!(kind_of_type("LONG RAW"), ValueKind::Binary);
        assert_eq!(kind_of_type("bit"), ValueKind::Bool);
        assert_eq!(kind_of_type("character varying(50)"), ValueKind::Text);
        assert_eq!(kind_of_type("jsonb"), ValueKind::Json);
    }

    #[test]
    fn inserts_group_rows_except_in_oracle() {
        let cols = vec!["\"a\"".to_string(), "\"b\"".to_string()];
        let kinds = [ValueKind::Number, ValueKind::Text];
        let rows = vec![
            vec![Some("1".into()), Some("x".into())],
            vec![Some("2".into()), None],
        ];
        let t = InsertTarget {
            engine: Engine::Postgres,
            table: "\"s\".\"t\"",
            columns: &cols,
            kinds: &kinds,
            types: &[],
            keys: &[],
        };
        assert_eq!(
            insert_statements(&t, &rows),
            ["INSERT INTO \"s\".\"t\" (\"a\", \"b\") OVERRIDING SYSTEM VALUE VALUES\n(1, 'x'),\n(2, NULL)"]
        );
        let t = InsertTarget {
            engine: Engine::Oracle,
            ..t
        };
        assert_eq!(insert_statements(&t, &rows).len(), 2);
    }

    fn cql(kind: ValueKind, ty: &str, v: &str) -> String {
        literal_typed(Engine::Cassandra, kind, ty, Some(v))
    }

    #[test]
    fn cql_identifiers_and_text() {
        assert_eq!(ident(Engine::Cassandra, "My\"Tbl"), "\"My\"\"Tbl\"");
        assert_eq!(qualified(Engine::Cassandra, "ks", "t"), "\"ks\".\"t\"");
        assert_eq!(text(Engine::Cassandra, "it's"), "'it''s'");
    }

    #[test]
    fn cql_numbers_and_booleans_are_bare() {
        use ValueKind::{Bool, Number};
        assert_eq!(cql(Number, "int", "42"), "42");
        assert_eq!(
            cql(Number, "varint", "-123456789012345678901234567890"),
            "-123456789012345678901234567890"
        );
        assert_eq!(
            cql(Number, "decimal", "1234567890.0987654321"),
            "1234567890.0987654321"
        );
        assert_eq!(cql(Number, "double", "1.5e-10"), "1.5e-10");
        assert_eq!(cql(Number, "double", "NaN"), "NaN");
        assert_eq!(cql(Number, "float", "-Infinity"), "-Infinity");
        assert_eq!(cql(Number, "counter", "7"), "7");
        assert_eq!(cql(Bool, "boolean", "true"), "true");
        assert_eq!(cql(Bool, "boolean", "false"), "false");
        assert_eq!(
            literal_typed(Engine::Cassandra, ValueKind::Text, "text", None),
            "null"
        );
    }

    #[test]
    fn cql_uuids_blobs_and_durations_are_bare() {
        use ValueKind::{Binary, Temporal, Text};
        let id = "123e4567-e89b-12d3-a456-426614174000";
        assert_eq!(cql(Text, "uuid", id), id);
        assert_eq!(cql(Text, "timeuuid", id), id);
        // Same kind, but a text column: quoted.
        assert_eq!(cql(Text, "text", id), format!("'{id}'"));
        assert_eq!(cql(Binary, "blob", "0xDEADBEEF"), "0xDEADBEEF");
        assert_eq!(cql(Binary, "blob", "0x"), "0x");
        assert_eq!(
            cql(Temporal, "duration", "1y2mo3d4h5m6s7ms8us9ns"),
            "1y2mo3d4h5m6s7ms8us9ns"
        );
    }

    #[test]
    fn cql_text_and_temporal_are_quoted() {
        use ValueKind::{Temporal, Text};
        assert_eq!(
            cql(Text, "text", "it's \\ a \"x\"\nline"),
            "'it''s \\ a \"x\"\nline'"
        );
        assert_eq!(cql(Text, "ascii", "abc"), "'abc'");
        assert_eq!(cql(Text, "varchar", "Ação ☃"), "'Ação ☃'");
        assert_eq!(cql(Text, "inet", "10.0.0.1"), "'10.0.0.1'");
        assert_eq!(
            cql(Temporal, "timestamp", "2024-05-06 07:08:09.123+0000"),
            "'2024-05-06 07:08:09.123+0000'"
        );
        assert_eq!(cql(Temporal, "date", "2024-05-06"), "'2024-05-06'");
        assert_eq!(
            cql(Temporal, "time", "07:08:09.123456789"),
            "'07:08:09.123456789'"
        );
        // Without a type the kind decides.
        assert_eq!(literal(Engine::Cassandra, Text, Some("a'b")), "'a''b'");
        assert_eq!(
            literal(Engine::Cassandra, ValueKind::Number, Some("5")),
            "5"
        );
        assert_eq!(
            literal(Engine::Cassandra, ValueKind::Bool, Some("true")),
            "true"
        );
        assert_eq!(literal(Engine::Cassandra, Temporal, None), "null");
    }

    #[test]
    fn cql_collections_tuples_udts_and_vectors_are_verbatim() {
        for (ty, v) in [
            ("list<int>", "[1, 2]"),
            ("set<text>", "{'a', 'it''s ; here'}"),
            ("map<text, int>", "{'a': 1, 'b': 2}"),
            (
                "frozen<map<text, frozen<list<int>>>>",
                "{'a': [1], 'b': []}",
            ),
            ("tuple<int, text>", "(1, 'x')"),
            ("frozen<address>", "{street: 'Main', zip: 12345}"),
            ("vector<float, 3>", "[0.1, 0.2, 0.3]"),
        ] {
            assert_eq!(cql(ValueKind::Other, ty, v), v, "{ty}");
        }
    }

    #[test]
    fn cql_types_map_to_kinds() {
        assert_eq!(kind_of_cql_type("int"), ValueKind::Number);
        assert_eq!(kind_of_cql_type("varint"), ValueKind::Number);
        assert_eq!(kind_of_cql_type("counter"), ValueKind::Number);
        assert_eq!(kind_of_cql_type("boolean"), ValueKind::Bool);
        assert_eq!(kind_of_cql_type("timestamp"), ValueKind::Temporal);
        assert_eq!(kind_of_cql_type("duration"), ValueKind::Temporal);
        assert_eq!(kind_of_cql_type("blob"), ValueKind::Binary);
        assert_eq!(kind_of_cql_type("uuid"), ValueKind::Text);
        assert_eq!(kind_of_cql_type("inet"), ValueKind::Text);
        // `kind_of_type` would call these numbers and dates.
        assert_eq!(kind_of_cql_type("list<int>"), ValueKind::Other);
        assert_eq!(
            kind_of_cql_type("frozen<map<text, timestamp>>"),
            ValueKind::Other
        );
        assert_eq!(kind_of_cql_type("tuple<int, text>"), ValueKind::Other);
        assert_eq!(kind_of_cql_type("vector<float, 3>"), ValueKind::Other);
        assert_eq!(kind_of_cql_type("frozen<address>"), ValueKind::Other);
        assert_eq!(
            column_kind(Engine::Cassandra, "list<int>"),
            ValueKind::Other
        );
        assert_eq!(column_kind(Engine::Postgres, "integer"), ValueKind::Number);
    }

    #[test]
    fn cql_inserts_are_one_row_each() {
        let cols = vec![
            "\"id\"".to_string(),
            "\"name\"".to_string(),
            "\"tags\"".to_string(),
        ];
        let kinds = [ValueKind::Text, ValueKind::Text, ValueKind::Other];
        let types = [
            "uuid".to_string(),
            "text".to_string(),
            "set<text>".to_string(),
        ];
        let rows = vec![
            vec![
                Some("123e4567-e89b-12d3-a456-426614174000".into()),
                Some("it's".into()),
                Some("{'a', 'b'}".into()),
            ],
            vec![
                Some("123e4567-e89b-12d3-a456-426614174001".into()),
                None,
                None,
            ],
        ];
        let t = InsertTarget {
            engine: Engine::Cassandra,
            table: "\"ks\".\"t\"",
            columns: &cols,
            kinds: &kinds,
            types: &types,
            keys: &[true, false, false],
        };
        assert_eq!(
            insert_statements(&t, &rows),
            [
                "INSERT INTO \"ks\".\"t\" (\"id\", \"name\", \"tags\") VALUES (123e4567-e89b-12d3-a456-426614174000, 'it''s', {'a', 'b'})",
                "INSERT INTO \"ks\".\"t\" (\"id\", \"name\", \"tags\") VALUES (123e4567-e89b-12d3-a456-426614174001, null, null)",
            ]
        );
    }

    #[test]
    fn cql_counter_tables_are_written_as_updates() {
        let cols = vec![
            "\"id\"".to_string(),
            "\"seq\"".to_string(),
            "\"hits\"".to_string(),
            "\"misses\"".to_string(),
        ];
        let kinds = [
            ValueKind::Text,
            ValueKind::Number,
            ValueKind::Number,
            ValueKind::Number,
        ];
        let types = [
            "text".to_string(),
            "int".to_string(),
            "counter".to_string(),
            "counter".to_string(),
        ];
        let rows = vec![
            vec![
                Some("a'b".into()),
                Some("1".into()),
                Some("5".into()),
                Some("-2".into()),
            ],
            vec![Some("c".into()), Some("2".into()), Some("9".into()), None],
            vec![Some("d".into()), Some("3".into()), None, None],
        ];
        let t = InsertTarget {
            engine: Engine::Cassandra,
            table: "\"ks\".\"c\"",
            columns: &cols,
            kinds: &kinds,
            types: &types,
            keys: &[true, true, false, false],
        };
        assert_eq!(
            insert_statements(&t, &rows),
            [
                "UPDATE \"ks\".\"c\" SET \"hits\" = \"hits\" + 5, \"misses\" = \"misses\" - 2 WHERE \"id\" = 'a''b' AND \"seq\" = 1",
                "UPDATE \"ks\".\"c\" SET \"hits\" = \"hits\" + 9 WHERE \"id\" = 'c' AND \"seq\" = 2",
            ]
        );
    }
}
