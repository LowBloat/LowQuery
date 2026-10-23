//! SQL text for exports and imports: identifiers, value literals and INSERT
//! statements in each database's syntax. Values arrive as the drivers format
//! them (see the `cell` functions), and every literal reads back as the same
//! value in the same database.

use std::io::{self, Write};

use crate::model::{Engine, ValueKind};

/// Always-quoted identifier: exports keep names exactly as the catalog has them.
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

/// A value as a literal of `engine`, for a column of `kind`.
pub fn literal(engine: Engine, kind: ValueKind, value: Option<&str>) -> String {
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

/// Where rows go: the table and the columns written, with their kinds.
pub struct InsertTarget<'a> {
    pub engine: Engine,
    /// Qualified, quoted table name.
    pub table: &'a str,
    /// Quoted column names.
    pub columns: &'a [String],
    pub kinds: &'a [ValueKind],
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
    let tuple = |row: &Vec<Option<String>>| {
        let values: Vec<String> = row
            .iter()
            .zip(t.kinds)
            .map(|(v, k)| literal(t.engine, *k, v.as_deref()))
            .collect();
        format!("({})", values.join(", "))
    };
    let per = if t.engine == Engine::Oracle {
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
}
