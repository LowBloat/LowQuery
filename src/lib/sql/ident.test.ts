import { describe, expect, it } from "bun:test";
import { previewQuery, quoteIdent } from "./ident";

describe("quoteIdent", () => {
	it("PostgreSQL quotes uppercase names and reserved words", () => {
		expect(quoteIdent("customers", "postgres")).toBe("customers");
		expect(quoteIdent("Customers", "postgres")).toBe('"Customers"');
		expect(quoteIdent("order", "postgres")).toBe('"order"');
		expect(quoteIdent('a"b', "postgres")).toBe('"a""b"');
	});

	it("Oracle quotes lowercase names", () => {
		expect(quoteIdent("EMP", "oracle")).toBe("EMP");
		expect(quoteIdent("emp", "oracle")).toBe('"emp"');
		expect(quoteIdent("TAB$1", "oracle")).toBe("TAB$1");
	});
});

describe("quoteIdent for MySQL, SQL Server and SQLite", () => {
	it("uses each engine's quote characters", () => {
		expect(quoteIdent("Orders", "mysql")).toBe("Orders");
		expect(quoteIdent("order", "mariadb")).toBe("`order`");
		expect(quoteIdent("a`b", "mysql")).toBe("`a``b`");
		expect(quoteIdent("Order Items", "sqlserver")).toBe("[Order Items]");
		expect(quoteIdent("a]b", "sqlserver")).toBe("[a]]b]");
		expect(quoteIdent("my table", "sqlite")).toBe('"my table"');
	});
});

describe("previewQuery", () => {
	it("uses each database's row limit syntax", () => {
		expect(previewQuery("public", "t", "postgres")).toBe("SELECT * FROM public.t LIMIT 200");
		expect(previewQuery("HR", "EMP", "oracle")).toBe("SELECT * FROM HR.EMP FETCH FIRST 200 ROWS ONLY");
		expect(previewQuery("dbo", "Orders", "sqlserver")).toBe("SELECT TOP 200 * FROM dbo.Orders");
		expect(previewQuery("shop", "orders", "mysql")).toBe("SELECT * FROM shop.orders LIMIT 200");
		expect(previewQuery("main", "t", "sqlite")).toBe("SELECT * FROM main.t LIMIT 200");
	});
});
