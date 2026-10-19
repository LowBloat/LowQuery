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

describe("previewQuery", () => {
	it("uses each database's row limit syntax", () => {
		expect(previewQuery("public", "t", "postgres")).toBe("SELECT * FROM public.t LIMIT 200");
		expect(previewQuery("HR", "EMP", "oracle")).toBe("SELECT * FROM HR.EMP FETCH FIRST 200 ROWS ONLY");
	});
});
