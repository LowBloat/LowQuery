import { describe, expect, it } from "bun:test";
import type { ColumnInfo } from "./api";
import {
	canViewOnServer,
	describeView,
	emptyView,
	filterSql,
	matchesFilter,
	nextSort,
	viewOrder,
	viewRows,
	viewSql,
	withFilter,
	withoutFilter,
	type ColumnFilter,
	type ResultView,
} from "./view";

const columns: ColumnInfo[] = [
	{ name: "id", typeName: "int4", kind: "number" },
	{ name: "name", typeName: "text", kind: "text" },
	{ name: "created", typeName: "timestamp", kind: "temporal" },
];
const rows: (string | null)[][] = [
	["10", "b", null],
	["9", null, null],
	["100", "A", null],
	[null, "c", null],
];
const sorted = (col: number, dir: 1 | -1): ResultView => ({ sort: { col, dir }, filters: [] });
const filtered = (...filters: ColumnFilter[]): ResultView => ({ sort: null, filters });

describe("nextSort", () => {
	it("cycles ascending, descending, none", () => {
		expect(nextSort(null, 1)).toEqual({ col: 1, dir: 1 });
		expect(nextSort({ col: 1, dir: 1 }, 1)).toEqual({ col: 1, dir: -1 });
		expect(nextSort({ col: 1, dir: -1 }, 1)).toBeNull();
		expect(nextSort({ col: 1, dir: -1 }, 0)).toEqual({ col: 0, dir: 1 });
	});
});

describe("viewOrder", () => {
	it("keeps the original order without a view", () => {
		expect(viewOrder(columns, rows, emptyView())).toEqual([0, 1, 2, 3]);
	});

	it("sorts numbers by value and keeps NULLs last both ways", () => {
		expect(viewOrder(columns, rows, sorted(0, 1))).toEqual([1, 0, 2, 3]);
		expect(viewOrder(columns, rows, sorted(0, -1))).toEqual([2, 0, 1, 3]);
	});

	it("sorts text without regard to case", () => {
		expect(viewOrder(columns, rows, sorted(1, 1))).toEqual([2, 0, 3, 1]);
	});

	it("filters, then sorts", () => {
		const view = { sort: { col: 0, dir: -1 as const }, filters: [{ col: 1, op: "notnull" as const, value: "" }] };
		expect(viewOrder(columns, rows, view)).toEqual([2, 0, 3]);
	});

	it("combines filters with AND", () => {
		const view = filtered({ col: 1, op: "notnull", value: "" }, { col: 0, op: "ne", value: "10" });
		expect(viewOrder(columns, rows, view)).toEqual([2, 3]);
	});

	it("returns the rows in display order", () => {
		expect(viewRows(columns, rows, sorted(1, -1)).map((r) => r[1])).toEqual(["c", "b", "A", null]);
	});
});

describe("matchesFilter", () => {
	const f = (op: ColumnFilter["op"], value = ""): ColumnFilter => ({ col: 0, op, value });

	it("finds text in any case", () => {
		expect(matchesFilter("DEBITO_DIRETO", f("contains", "debito"))).toBe(true);
		expect(matchesFilter("PIX", f("contains", "debito"))).toBe(false);
		expect(matchesFilter(null, f("contains", "x"))).toBe(false);
	});

	it("compares numbers by value", () => {
		expect(matchesFilter("10.50", f("eq", "10.5"), "number")).toBe(true);
		expect(matchesFilter("10.50", f("eq", "10.5"), "text")).toBe(false);
	});

	it("keeps NULLs when excluding a value", () => {
		expect(matchesFilter(null, f("ne", "x"))).toBe(true);
		expect(matchesFilter("x", f("ne", "x"))).toBe(false);
		expect(matchesFilter("y", f("ne", "x"))).toBe(true);
	});

	it("tests for NULL", () => {
		expect(matchesFilter(null, f("null"))).toBe(true);
		expect(matchesFilter("", f("null"))).toBe(false);
		expect(matchesFilter("", f("notnull"))).toBe(true);
	});
});

describe("withFilter", () => {
	it("keeps one filter per column", () => {
		let view = withFilter(emptyView(), { col: 1, op: "contains", value: "a" });
		view = withFilter(view, { col: 0, op: "null", value: "" });
		view = withFilter(view, { col: 1, op: "eq", value: "b" });
		expect(view.filters).toEqual([
			{ col: 0, op: "null", value: "" },
			{ col: 1, op: "eq", value: "b" },
		]);
		expect(withoutFilter(view, 0).filters).toEqual([{ col: 1, op: "eq", value: "b" }]);
		expect(withoutFilter(view).filters).toEqual([]);
	});
});

describe("canViewOnServer", () => {
	it("accepts SELECT and WITH, after comments", () => {
		expect(canViewOnServer("-- top\nselect * from t", "postgres")).toBe(true);
		expect(canViewOnServer("/* x */ WITH a AS (select 1) select * from a", "oracle")).toBe(true);
	});

	it("rejects other statements and Redis", () => {
		expect(canViewOnServer("show tables", "mysql")).toBe(false);
		expect(canViewOnServer("exec sp_who", "sqlserver")).toBe(false);
		expect(canViewOnServer("pragma table_info(t)", "sqlite")).toBe(false);
		expect(canViewOnServer("select", "redis")).toBe(false);
	});
});

describe("filterSql", () => {
	const text = columns[1];
	const num = columns[0];
	const when = columns[2];

	it("searches text case-insensitively in each dialect", () => {
		const f: ColumnFilter = { col: 1, op: "contains", value: "it's" };
		expect(filterSql(f, text, "postgres")).toBe(`STRPOS(LOWER(CAST("name" AS TEXT)), LOWER('it''s')) > 0`);
		expect(filterSql(f, text, "oracle")).toBe(`INSTR(UPPER(TO_CHAR("name")), UPPER('it''s')) > 0`);
		expect(filterSql(f, text, "mysql")).toBe("LOCATE(LOWER('it''s'), LOWER(CAST(`name` AS CHAR))) > 0");
		expect(filterSql(f, text, "sqlserver")).toBe(`CHARINDEX(LOWER(N'it''s'), LOWER(CAST([name] AS nvarchar(max)))) > 0`);
		expect(filterSql(f, text, "sqlite")).toBe(`INSTR(LOWER(CAST("name" AS TEXT)), LOWER('it''s')) > 0`);
	});

	it("doubles backslashes for MySQL only", () => {
		const f: ColumnFilter = { col: 1, op: "eq", value: "a\\b" };
		expect(filterSql(f, text, "mysql")).toBe("CAST(`name` AS CHAR) = 'a\\\\b'");
		expect(filterSql(f, text, "postgres")).toBe(`CAST("name" AS TEXT) = 'a\\b'`);
	});

	it("quotes identifiers per dialect", () => {
		const odd: ColumnInfo = { name: 'a"b]`c', typeName: "text", kind: "text" };
		const f: ColumnFilter = { col: 0, op: "null", value: "" };
		expect(filterSql(f, odd, "postgres")).toBe(`"a""b]\`c" IS NULL`);
		expect(filterSql(f, odd, "mysql")).toBe('`a"b]``c` IS NULL');
		expect(filterSql(f, odd, "sqlserver")).toBe('[a"b]]`c] IS NULL');
	});

	it("compares numbers as numbers and other values as text", () => {
		expect(filterSql({ col: 0, op: "eq", value: " 10.5 " }, num, "oracle")).toBe(`"id" = 10.5`);
		expect(filterSql({ col: 0, op: "eq", value: "1; drop table t" }, num, "postgres")).toBe(
			`CAST("id" AS TEXT) = '1; drop table t'`,
		);
		expect(filterSql({ col: 2, op: "eq", value: "2026-10-22 10:00:00" }, when, "oracle")).toBe(
			`TO_CHAR("created", 'YYYY-MM-DD HH24:MI:SS') = '2026-10-22 10:00:00'`,
		);
		expect(filterSql({ col: 2, op: "contains", value: "2026" }, when, "sqlserver")).toBe(
			`CHARINDEX(LOWER(N'2026'), LOWER(CONVERT(nvarchar(max), [created], 121))) > 0`,
		);
	});

	it("keeps NULLs when excluding a value", () => {
		expect(filterSql({ col: 0, op: "ne", value: "3" }, num, "postgres")).toBe(`("id" IS NULL OR NOT ("id" = 3))`);
	});
});

describe("viewSql", () => {
	it("returns the statement as is without a view", () => {
		expect(viewSql("select * from t;", emptyView(), columns, "postgres")).toBe("select * from t");
	});

	it("orders by column position, NULLs last where the dialect supports it", () => {
		const view = sorted(2, -1);
		expect(viewSql("select * from t", view, columns, "postgres")).toBe(
			"SELECT * FROM (\nselect * from t\n) lq_view\nORDER BY 3 DESC NULLS LAST",
		);
		expect(viewSql("select * from t", view, columns, "oracle")).toEndWith("ORDER BY 3 DESC NULLS LAST");
		expect(viewSql("select * from t", view, columns, "sqlite")).toEndWith("ORDER BY 3 DESC NULLS LAST");
		expect(viewSql("select * from t", view, columns, "mysql")).toEndWith("ORDER BY 3 DESC");
		expect(viewSql("select * from t", view, columns, "sqlserver")).toEndWith("ORDER BY 3 DESC");
	});

	it("puts the filters in a WHERE before the sort", () => {
		const view: ResultView = {
			sort: { col: 0, dir: 1 },
			filters: [
				{ col: 1, op: "notnull", value: "" },
				{ col: 0, op: "eq", value: "7" },
			],
		};
		expect(viewSql("select * from t", view, columns, "postgres")).toBe(
			'SELECT * FROM (\nselect * from t\n) lq_view\nWHERE "name" IS NOT NULL\n  AND "id" = 7\nORDER BY 1 ASC NULLS LAST',
		);
	});

	it("closes the derived table on its own line after a trailing comment", () => {
		expect(viewSql("select * from t -- note", sorted(0, 1), columns, "postgres")).toContain("-- note\n) lq_view");
	});
});

describe("describeView", () => {
	it("lists filters, then the sort", () => {
		const view: ResultView = { sort: { col: 1, dir: -1 }, filters: [{ col: 1, op: "contains", value: "PIX" }] };
		expect(describeView(columns, view)).toBe('name contains "PIX"; name descending');
	});
});
