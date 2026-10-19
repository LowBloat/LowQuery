import { describe, expect, it } from "bun:test";
import { isPlsqlStart, splitStatements, statementAt } from "./split";

const texts = (sql: string, dialect: "postgres" | "oracle", blank = false) =>
	splitStatements(sql, dialect, { blankLineDelimiter: blank }).map((s) => s.text);

describe("splitStatements", () => {
	it("splits on semicolons and ignores strings and comments", () => {
		const sql = "select ';' as a; -- x;\nselect \"a;b\" from t /* ; */;\n";
		expect(texts(sql, "postgres")).toEqual(['select \';\' as a', 'select "a;b" from t /* ; */']);
	});

	it("handles PostgreSQL dollar quoting and parameters", () => {
		const sql = "create function f() returns int as $body$ begin return 1; end; $body$ language plpgsql;\nselect $1;";
		expect(texts(sql, "postgres")).toEqual([
			"create function f() returns int as $body$ begin return 1; end; $body$ language plpgsql",
			"select $1",
		]);
	});

	it("handles E'' strings with backslash escapes", () => {
		expect(texts("select E'a\\';b'; select 2", "postgres")).toEqual(["select E'a\\';b'", "select 2"]);
	});

	it("handles nested block comments in PostgreSQL", () => {
		expect(texts("/* a /* b; */ c; */ select 1; select 2", "postgres")).toEqual([
			"select 1",
			"select 2",
		]);
	});

	it("ends PL/SQL blocks at the slash", () => {
		const sql = [
			"begin",
			"  dbms_output.put_line('a;b');",
			"end;",
			"/",
			"select * from dual;",
			"create or replace package body p as",
			"  procedure x is begin null; end;",
			"end p;",
			"/",
		].join("\n");
		expect(texts(sql, "oracle")).toEqual([
			"begin\n  dbms_output.put_line('a;b');\nend;",
			"select * from dual",
			"create or replace package body p as\n  procedure x is begin null; end;\nend p;",
		]);
	});

	it("handles Oracle q-quoting", () => {
		expect(texts("select q'[it's; ok]' from dual; select 2 from dual", "oracle")).toEqual([
			"select q'[it's; ok]' from dual",
			"select 2 from dual",
		]);
	});

	it("does not mistake division 10/2 for the SQL*Plus slash", () => {
		expect(texts("select 10\n/2 from dual", "oracle")).toEqual(["select 10\n/2 from dual"]);
	});

	it("splits on blank lines when enabled, but not inside parentheses", () => {
		const sql = "select 1\n\nselect (\n\n2)\n";
		expect(texts(sql, "postgres", true)).toEqual(["select 1", "select (\n\n2)"]);
		expect(texts(sql, "postgres", false)).toEqual(["select 1\n\nselect (\n\n2)"]);
	});

	it("keeps document positions", () => {
		const sql = "  select 1;\n  select 2;";
		const [a, b] = splitStatements(sql, "postgres");
		expect(sql.slice(a.start, a.end)).toBe("select 1");
		expect(sql.slice(b.start, b.end)).toBe("select 2");
		expect(a.until).toBe(11);
	});
});

describe("statementAt", () => {
	it("picks the statement under the cursor or the previous one", () => {
		const sql = "select 1;\n\nselect 2;";
		const list = splitStatements(sql, "postgres");
		expect(statementAt(list, 3)?.text).toBe("select 1");
		expect(statementAt(list, 10)?.text).toBe("select 1");
		expect(statementAt(list, sql.length)?.text).toBe("select 2");
	});
});

describe("isPlsqlStart", () => {
	it("recognizes PL/SQL units", () => {
		expect(isPlsqlStart("-- c\nDECLARE x number;")).toBe(true);
		expect(isPlsqlStart("create or replace noneditionable trigger t")).toBe(true);
		expect(isPlsqlStart("create view v as select 1 from dual")).toBe(false);
	});
});
