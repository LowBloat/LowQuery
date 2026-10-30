import { describe, expect, it } from "bun:test";
import { previewQuery, qualified, quoteIdent, redisArg, redisPreview } from "./ident";

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

describe("quoteIdent for Cassandra", () => {
	it("keeps plain lowercase names bare and quotes the rest", () => {
		expect(quoteIdent("users", "cassandra")).toBe("users");
		expect(quoteIdent("user_events_2", "cassandra")).toBe("user_events_2");
		expect(quoteIdent("Users", "cassandra")).toBe('"Users"');
		expect(quoteIdent("_x", "cassandra")).toBe('"_x"');
		expect(quoteIdent("1st", "cassandra")).toBe('"1st"');
		expect(quoteIdent("my table", "cassandra")).toBe('"my table"');
		expect(quoteIdent('a"b', "cassandra")).toBe('"a""b"');
		expect(quoteIdent("", "cassandra")).toBe('""');
	});

	it("quotes reserved keywords in any case", () => {
		expect(quoteIdent("select", "cassandra")).toBe('"select"');
		expect(quoteIdent("table", "cassandra")).toBe('"table"');
		expect(quoteIdent("token", "cassandra")).toBe('"token"');
		expect(quoteIdent("keyspace", "cassandra")).toBe('"keyspace"');
		expect(quoteIdent("key", "cassandra")).toBe("key");
		expect(quoteIdent("user", "cassandra")).toBe("user");
	});

	it("qualifies keyspace and table", () => {
		expect(qualified("shop", "orders", "cassandra")).toBe("shop.orders");
		expect(qualified("Shop", "order", "cassandra")).toBe('"Shop"."order"');
	});
});

describe("previewQuery", () => {
	it("uses each database's row limit syntax", () => {
		expect(previewQuery("public", "t", "postgres")).toBe("SELECT * FROM public.t LIMIT 200");
		expect(previewQuery("HR", "EMP", "oracle")).toBe("SELECT * FROM HR.EMP FETCH FIRST 200 ROWS ONLY");
		expect(previewQuery("dbo", "Orders", "sqlserver")).toBe("SELECT TOP 200 * FROM dbo.Orders");
		expect(previewQuery("shop", "orders", "mysql")).toBe("SELECT * FROM shop.orders LIMIT 200");
		expect(previewQuery("main", "t", "sqlite")).toBe("SELECT * FROM main.t LIMIT 200");
		expect(previewQuery("shop", "orders", "cassandra")).toBe("SELECT * FROM shop.orders LIMIT 200");
		expect(previewQuery("Shop", "Orders", "cassandra")).toBe('SELECT * FROM "Shop"."Orders" LIMIT 200');
	});
});

describe("Redis arguments", () => {
	it("quotes keys like redis-cli", () => {
		expect(redisArg("user:1")).toBe("user:1");
		expect(redisArg("a b")).toBe('"a b"');
		expect(redisArg('say "hi"\n')).toBe(String.raw`"say \"hi\"\n"`);
		expect(redisPreview("orders", "zset")).toBe("ZRANGE orders 0 199 WITHSCORES");
		expect(redisPreview("cfg", "string")).toBe("GET cfg");
	});
});
