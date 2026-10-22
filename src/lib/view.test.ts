import { describe, expect, it } from "bun:test";
import type { ColumnInfo } from "./api";
import { canViewOnServer, emptyView, nextSort, viewOrder, viewRows, viewSql } from "./view";

const columns: ColumnInfo[] = [
	{ name: "id", typeName: "int4", kind: "number" },
	{ name: "name", typeName: "text", kind: "text" },
];
const rows: (string | null)[][] = [
	["10", "b"],
	["9", null],
	["100", "A"],
	[null, "c"],
];

describe("nextSort", () => {
	it("cycles ascending, descending, none", () => {
		expect(nextSort(null, 1)).toEqual({ col: 1, dir: 1 });
		expect(nextSort({ col: 1, dir: 1 }, 1)).toEqual({ col: 1, dir: -1 });
		expect(nextSort({ col: 1, dir: -1 }, 1)).toBeNull();
		expect(nextSort({ col: 1, dir: -1 }, 0)).toEqual({ col: 0, dir: 1 });
	});
});

describe("viewOrder", () => {
	it("keeps the original order without a sort", () => {
		expect(viewOrder(columns, rows, emptyView())).toEqual([0, 1, 2, 3]);
	});

	it("sorts numbers by value and keeps NULLs last both ways", () => {
		expect(viewOrder(columns, rows, { sort: { col: 0, dir: 1 } })).toEqual([1, 0, 2, 3]);
		expect(viewOrder(columns, rows, { sort: { col: 0, dir: -1 } })).toEqual([2, 0, 1, 3]);
	});

	it("sorts text without regard to case", () => {
		expect(viewOrder(columns, rows, { sort: { col: 1, dir: 1 } })).toEqual([2, 0, 3, 1]);
	});

	it("returns the rows in display order", () => {
		expect(viewRows(columns, rows, { sort: { col: 1, dir: -1 } }).map((r) => r[1])).toEqual(["c", "b", "A", null]);
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

describe("viewSql", () => {
	it("returns the statement as is without a view", () => {
		expect(viewSql("select * from t;", emptyView(), "postgres")).toBe("select * from t");
	});

	it("orders by column position, NULLs last where the dialect supports it", () => {
		const sort = { sort: { col: 2, dir: -1 as const } };
		expect(viewSql("select * from t", sort, "postgres")).toBe("SELECT * FROM (\nselect * from t\n) lq_view\nORDER BY 3 DESC NULLS LAST");
		expect(viewSql("select * from t", sort, "oracle")).toEndWith("ORDER BY 3 DESC NULLS LAST");
		expect(viewSql("select * from t", sort, "sqlite")).toEndWith("ORDER BY 3 DESC NULLS LAST");
		expect(viewSql("select * from t", sort, "mysql")).toEndWith("ORDER BY 3 DESC");
		expect(viewSql("select * from t", sort, "sqlserver")).toEndWith("ORDER BY 3 DESC");
	});

	it("closes the derived table on its own line after a trailing comment", () => {
		const sql = viewSql("select * from t -- note", { sort: { col: 0, dir: 1 } }, "postgres");
		expect(sql).toContain("-- note\n) lq_view");
	});
});
