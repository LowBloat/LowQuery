/**
 * The SQL the grid writes to sort and filter a result, per dialect, kept in
 * `view.fixtures.json` so the Rust integration tests run the same statements
 * against real databases (`src-tauri/src/drivers/view_fixtures.rs`) and check
 * the ids they return. After changing `viewSql`, regenerate the file with
 * `LQ_UPDATE_FIXTURES=1 bun test src/lib/view.fixtures.test.ts`.
 */
import { expect, it } from "bun:test";
import { readFileSync, writeFileSync } from "node:fs";
import type { ColumnInfo } from "./api";
import type { Dialect } from "./sql/split";
import { viewSql, type ColumnFilter, type ResultView } from "./view";

type SqlDialect = Exclude<Dialect, "redis" | "cassandra">;

// Four rows: 1 Alpha 2026-10-22, 2 beta 2025-01-05, 3 NULL NULL, 4 it's 2026-10-01.
const BASE: Record<SqlDialect, string> = {
	postgres:
		"SELECT 1 AS id, 'Alpha' AS name, DATE '2026-10-22' AS created\nUNION ALL SELECT 2, 'beta', DATE '2025-01-05'\nUNION ALL SELECT 3, NULL, NULL\nUNION ALL SELECT 4, 'it''s', DATE '2026-10-01'",
	oracle:
		"SELECT 1 AS id, 'Alpha' AS name, DATE '2026-10-22' AS created FROM dual\nUNION ALL SELECT 2, 'beta', DATE '2025-01-05' FROM dual\nUNION ALL SELECT 3, NULL, NULL FROM dual\nUNION ALL SELECT 4, 'it''s', DATE '2026-10-01' FROM dual",
	mysql:
		"SELECT 1 AS id, 'Alpha' AS name, DATE '2026-10-22' AS created\nUNION ALL SELECT 2, 'beta', DATE '2025-01-05'\nUNION ALL SELECT 3, NULL, NULL\nUNION ALL SELECT 4, 'it''s', DATE '2026-10-01'",
	sqlserver:
		"SELECT 1 AS id, N'Alpha' AS name, CAST('2026-10-22' AS date) AS created\nUNION ALL SELECT 2, N'beta', CAST('2025-01-05' AS date)\nUNION ALL SELECT 3, NULL, NULL\nUNION ALL SELECT 4, N'it''s', CAST('2026-10-01' AS date)",
	sqlite:
		"SELECT 1 AS id, 'Alpha' AS name, '2026-10-22' AS created\nUNION ALL SELECT 2, 'beta', '2025-01-05'\nUNION ALL SELECT 3, NULL, NULL\nUNION ALL SELECT 4, 'it''s', '2026-10-01'",
};

// Result column names as each database reports them.
const columns = (d: SqlDialect): ColumnInfo[] => {
	const name = (n: string) => (d === "oracle" ? n.toUpperCase() : n);
	return [
		{ name: name("id"), typeName: "int", kind: "number" },
		{ name: name("name"), typeName: "text", kind: "text" },
		{ name: name("created"), typeName: "date", kind: d === "sqlite" ? "text" : "temporal" },
	];
};

const byId = { col: 0, dir: 1 as const };
const f = (col: number, op: ColumnFilter["op"], value = ""): ColumnFilter => ({ col, op, value });
// MySQL and SQL Server put NULLs first when ascending; the others are told NULLS LAST.
const nullsFirst = (d: SqlDialect) => d === "mysql" || d === "sqlserver";

const CASES: { label: string; view: ResultView; ids: (d: SqlDialect) => number[] }[] = [
	{ label: "sort text descending", view: { sort: { col: 1, dir: -1 }, filters: [] }, ids: () => [4, 2, 1, 3] },
	{
		label: "sort text ascending",
		view: { sort: { col: 1, dir: 1 }, filters: [] },
		ids: (d) => (nullsFirst(d) ? [3, 1, 2, 4] : [1, 2, 4, 3]),
	},
	{ label: "contains, any case", view: { sort: byId, filters: [f(1, "contains", "ALP")] }, ids: () => [1] },
	{ label: "contains a quote", view: { sort: byId, filters: [f(1, "contains", "'")] }, ids: () => [4] },
	{ label: "contains in a date", view: { sort: byId, filters: [f(2, "contains", "2026-10")] }, ids: () => [1, 4] },
	{ label: "equals a number", view: { sort: byId, filters: [f(0, "eq", "2")] }, ids: () => [2] },
	{ label: "equals text", view: { sort: byId, filters: [f(1, "eq", "Alpha")] }, ids: () => [1] },
	{ label: "number column, text value", view: { sort: byId, filters: [f(0, "eq", "x")] }, ids: () => [] },
	{ label: "not equal keeps NULL", view: { sort: byId, filters: [f(1, "ne", "beta")] }, ids: () => [1, 3, 4] },
	{ label: "is NULL", view: { sort: byId, filters: [f(1, "null")] }, ids: () => [3] },
	{
		label: "is not NULL, descending",
		view: { sort: { col: 0, dir: -1 }, filters: [f(1, "notnull")] },
		ids: () => [4, 2, 1],
	},
	{
		label: "filters combine with AND",
		view: { sort: { col: 0, dir: -1 }, filters: [f(1, "notnull"), f(0, "ne", "1")] },
		ids: () => [4, 2],
	},
];

function build() {
	const dialects: Record<string, { cases: { label: string; sql: string; ids: string[] }[] }> = {};
	for (const d of Object.keys(BASE) as SqlDialect[]) {
		dialects[d] = {
			cases: CASES.map((c) => ({
				label: c.label,
				sql: viewSql(BASE[d], c.view, columns(d), d),
				ids: c.ids(d).map(String),
			})),
		};
	}
	return { dialects };
}

const PATH = new URL("./view.fixtures.json", import.meta.url);

it("view.fixtures.json matches the SQL viewSql writes", () => {
	const text = JSON.stringify(build(), null, "\t") + "\n";
	if (process.env.LQ_UPDATE_FIXTURES) writeFileSync(PATH, text);
	expect(readFileSync(PATH, "utf8").replaceAll("\r\n", "\n")).toBe(text);
});
