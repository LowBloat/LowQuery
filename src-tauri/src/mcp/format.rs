//! Text for agents: compact result tables, value and size caps, and error
//! messages with the engine's code and a position hint.

use crate::error::{AppError, ErrorKind};
use crate::model::ColumnInfo;

/// Longest value shown, in characters.
pub const VALUE_CAP: usize = 1_000;
/// Rough size of one text result, in bytes.
pub const TEXT_CAP: usize = 60_000;

/// Cuts a value to [`VALUE_CAP`] characters, saying how long it was.
pub fn cut_value(v: &str) -> String {
    let total = v.chars().count();
    if total <= VALUE_CAP {
        return v.to_string();
    }
    let head: String = v.chars().take(VALUE_CAP).collect();
    format!("{head}… ({total} chars)")
}

/// A value as one table cell: on one line and without bare `|`.
fn cell(v: Option<&str>) -> String {
    match v {
        None => "NULL".into(),
        Some(v) => cut_value(v)
            .replace('\\', "\\\\")
            .replace('|', "\\|")
            .replace('\r', "\\r")
            .replace('\n', "\\n")
            .replace('\t', "\\t"),
    }
}

fn line(cells: impl Iterator<Item = String>) -> String {
    let mut out = String::from("|");
    for c in cells {
        out.push(' ');
        out.push_str(&c);
        out.push_str(" |");
    }
    out.push('\n');
    out
}

/// A Markdown table of `rows`, within `budget` bytes. Returns the text, the
/// rows it holds (values already cut) and how many fit.
pub fn table(
    columns: &[ColumnInfo],
    rows: &[Vec<Option<String>>],
    budget: usize,
) -> (String, Vec<Vec<Option<String>>>) {
    let mut text = line(columns.iter().map(|c| cell(Some(&c.name))));
    text.push_str(&line(columns.iter().map(|_| "---".to_string())));
    let mut kept = Vec::new();
    for row in rows {
        let r = line(row.iter().map(|v| cell(v.as_deref())));
        if text.len() + r.len() > budget && !kept.is_empty() {
            break;
        }
        text.push_str(&r);
        kept.push(row.iter().map(|v| v.as_deref().map(cut_value)).collect());
    }
    (text, kept)
}

/// "1 row" / "3 rows".
pub fn plural(n: usize, word: &str) -> String {
    if n == 1 {
        format!("1 {word}")
    } else if ["s", "x", "ch", "sh"].iter().any(|e| word.ends_with(e)) {
        format!("{n} {word}es")
    } else {
        format!("{n} {word}s")
    }
}

/// Line and column (1-based) of a character offset.
fn line_col(sql: &str, pos: usize) -> (usize, usize) {
    let mut line = 1;
    let mut col = 1;
    for c in sql.chars().take(pos) {
        if c == '\n' {
            line += 1;
            col = 1;
        } else {
            col += 1;
        }
    }
    (line, col)
}

/// The error as an agent should read it: engine code and message, then
/// detail, hint and where in the SQL it happened.
pub fn error_text(e: &AppError, sql: Option<&str>) -> String {
    let mut out = match (&e.code, e.kind) {
        (Some(code), _) => format!("Error {code}: {}", e.message),
        (None, ErrorKind::Database) => format!("Database error: {}", e.message),
        (None, ErrorKind::Connection) => format!("Connection error: {}", e.message),
        (None, ErrorKind::Cancelled) => format!("Cancelled: {}", e.message),
        (None, _) => e.message.clone(),
    };
    if let Some(d) = &e.detail {
        out.push_str(&format!("\nDetail: {d}"));
    }
    if let Some(h) = &e.hint {
        out.push_str(&format!("\nHint: {h}"));
    }
    if let (Some(pos), Some(sql)) = (e.position, sql) {
        let (line, col) = line_col(sql, pos);
        let near: String = sql
            .chars()
            .skip(pos)
            .take(40)
            .collect::<String>()
            .replace('\n', " ");
        out.push_str(&format!("\nAt line {line}, column {col}"));
        if !near.trim().is_empty() {
            out.push_str(&format!(", near: {}", near.trim_end()));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::ValueKind;

    fn col(name: &str) -> ColumnInfo {
        ColumnInfo {
            name: name.into(),
            type_name: "text".into(),
            kind: ValueKind::Text,
        }
    }

    #[test]
    fn values_are_cut_with_their_length() {
        assert_eq!(cut_value("abc"), "abc");
        let long = "é".repeat(1500);
        let cut = cut_value(&long);
        assert!(cut.starts_with(&"é".repeat(1000)));
        assert!(cut.ends_with("… (1500 chars)"));
    }

    #[test]
    fn table_is_one_line_per_row() {
        let rows = vec![
            vec![Some("1".into()), Some("a|b\nc".into())],
            vec![Some("2".into()), None],
        ];
        let (text, kept) = table(&[col("id"), col("name")], &rows, TEXT_CAP);
        assert_eq!(
            text,
            "| id | name |\n| --- | --- |\n| 1 | a\\|b\\nc |\n| 2 | NULL |\n"
        );
        assert_eq!(kept.len(), 2);
    }

    #[test]
    fn table_stops_at_the_budget() {
        let rows: Vec<_> = (0..1000)
            .map(|i| vec![Some(format!("{i:0>100}"))])
            .collect();
        let (text, kept) = table(&[col("v")], &rows, 5_000);
        assert!(text.len() <= 5_000);
        assert!(kept.len() < 1000 && !kept.is_empty());
        assert_eq!(text.lines().count(), kept.len() + 2);
    }

    #[test]
    fn errors_show_code_and_position() {
        let mut e = AppError::new(ErrorKind::Database, "syntax error at or near \"frm\"");
        e.code = Some("42601".into());
        e.position = Some(16);
        let text = error_text(&e, Some("select *\n  from frm users"));
        assert_eq!(
            text,
            "Error 42601: syntax error at or near \"frm\"\nAt line 2, column 8, near: frm users"
        );
    }
}
