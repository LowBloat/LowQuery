import { describe, expect, it } from "bun:test";
import { isCqlBatchStart, isPlsqlStart, splitStatements, statementAt, type Dialect } from "./split";

const texts = (sql: string, dialect: Dialect, blank = false) =>
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

describe("MySQL", () => {
	const my = (sql: string) => texts(sql, "mysql");

	it("handles # comments, backticks and backslash escapes", () => {
		expect(my(String.raw`select 'it\'s; ok', ` + "`a;b` from t; # c;\nselect 2")).toEqual([
			String.raw`select 'it\'s; ok', ` + "`a;b` from t",
			"select 2",
		]);
	});

	it("only treats -- followed by a space as a comment", () => {
		// The comment stays in the statement text; the server ignores it.
		expect(my("select 1--2;\nselect 3 -- x;\n;")).toEqual(["select 1--2", "select 3 -- x;"]);
	});

	it("follows DELIMITER for routine bodies", () => {
		const sql = [
			"DELIMITER $$",
			"CREATE PROCEDURE p() BEGIN SELECT 1; SELECT 2; END$$",
			"DELIMITER ;",
			"CALL p();",
		].join("\n");
		expect(my(sql)).toEqual(["CREATE PROCEDURE p() BEGIN SELECT 1; SELECT 2; END", "CALL p()"]);
	});
});

describe("SQL Server", () => {
	const ms = (sql: string, blank = false) => texts(sql, "sqlserver", blank);

	it("splits on GO and keeps procedure bodies whole", () => {
		const sql = [
			"CREATE OR ALTER PROCEDURE dbo.p AS",
			"  SELECT 1;",
			"  SELECT 2;",
			"GO",
			"EXEC dbo.p;",
			"go 2",
			"SELECT [a;b] FROM t",
		].join("\n");
		expect(ms(sql)).toEqual([
			"CREATE OR ALTER PROCEDURE dbo.p AS\n  SELECT 1;\n  SELECT 2;",
			"EXEC dbo.p",
			"SELECT [a;b] FROM t",
		]);
	});

	it("keeps BEGIN...END blocks together but not BEGIN TRAN", () => {
		expect(ms("BEGIN TRAN; IF 1=1 BEGIN SELECT 1; SELECT CASE WHEN 1=1 THEN 2 END; END; COMMIT;")).toEqual([
			"BEGIN TRAN",
			"IF 1=1 BEGIN SELECT 1; SELECT CASE WHEN 1=1 THEN 2 END; END",
			"COMMIT",
		]);
		expect(ms("BEGIN TRY SELECT 1/0; END TRY BEGIN CATCH SELECT 2; END CATCH; SELECT 3")).toEqual([
			"BEGIN TRY SELECT 1/0; END TRY BEGIN CATCH SELECT 2; END CATCH",
			"SELECT 3",
		]);
	});

	it("does not mistake a column named go for a batch separator", () => {
		expect(ms("select go\nfrom t;")).toEqual(["select go\nfrom t"]);
	});
});

describe("SQLite", () => {
	it("keeps trigger bodies whole", () => {
		const sql =
			"CREATE TRIGGER t AFTER INSERT ON a BEGIN INSERT INTO b VALUES (1); UPDATE c SET x = 1; END;\nBEGIN;\nSELECT [a;b], `c;d` FROM t;";
		expect(texts(sql, "sqlite")).toEqual([
			"CREATE TRIGGER t AFTER INSERT ON a BEGIN INSERT INTO b VALUES (1); UPDATE c SET x = 1; END",
			"BEGIN",
			"SELECT [a;b], `c;d` FROM t",
		]);
	});
});

describe("Redis", () => {
	it("runs one command per line and skips comments", () => {
		const sql = '# cache keys\nSET "a b" "x; y"\n\n  HGETALL user:1  \n// done\n';
		const list = splitStatements(sql, "redis");
		expect(list.map((s) => s.text)).toEqual(['SET "a b" "x; y"', "HGETALL user:1"]);
		expect(sql.slice(list[1].start, list[1].end)).toBe("HGETALL user:1");
	});
});

describe("Cassandra", () => {
	const cql = (sql: string, blank = false) => texts(sql, "cassandra", blank);

	it("splits on semicolons and skips strings, quoted names and comments", () => {
		const sql = [
			"-- first; comment",
			"SELECT 'a;b', \"c;d\" FROM ks.t; // second; comment",
			"/* third; */ INSERT INTO ks.t (k, v) VALUES (1, 'it''s; fine');",
			"SELECT * FROM \"we\"\"ird;name\"",
		].join("\n");
		expect(cql(sql)).toEqual([
			"SELECT 'a;b', \"c;d\" FROM ks.t",
			"INSERT INTO ks.t (k, v) VALUES (1, 'it''s; fine')",
			'SELECT * FROM "we""ird;name"',
		]);
	});

	it("does not treat a backslash as an escape", () => {
		expect(cql("SELECT 'a\\'; SELECT 'b';")).toEqual(["SELECT 'a\\'", "SELECT 'b'"]);
		expect(cql('SELECT "a\\"; SELECT 2;')).toEqual(['SELECT "a\\"', "SELECT 2"]);
	});

	it("does not nest block comments", () => {
		expect(cql("/* a /* b */ SELECT 1; SELECT 2;")).toEqual(["SELECT 1", "SELECT 2"]);
	});

	it("keeps a batch whole up to APPLY BATCH", () => {
		const batch = "BEGIN BATCH\n  INSERT INTO ks.t (k) VALUES (1);\n  INSERT INTO ks.t (k) VALUES (2);\nAPPLY BATCH";
		expect(cql(`${batch};\nSELECT 1;`)).toEqual([batch, "SELECT 1"]);
		expect(cql(`${batch};\nSELECT 1;`, true)).toEqual([batch, "SELECT 1"]);
	});

	it("recognizes UNLOGGED and COUNTER batches, USING TIMESTAMP and any case", () => {
		const unlogged = "begin unlogged batch using timestamp 5 insert into t (k) values (1); apply batch";
		const counter = "BEGIN COUNTER BATCH UPDATE t SET c = c + 1 WHERE k = 1; APPLY BATCH";
		expect(cql(`${unlogged}; ${counter}; SELECT 1`)).toEqual([unlogged, counter, "SELECT 1"]);
	});

	it("keeps blank lines inside a batch together", () => {
		const batch = "BEGIN BATCH\nINSERT INTO t (k) VALUES (1);\n\nINSERT INTO t (k) VALUES (2);\nAPPLY BATCH";
		expect(cql(`${batch};`, true)).toEqual([batch]);
	});

	it("ignores APPLY BATCH inside strings and comments, and an unclosed batch runs to the end", () => {
		const batch = "BEGIN BATCH INSERT INTO t (k, v) VALUES (1, 'APPLY BATCH;'); -- apply batch;\nAPPLY BATCH";
		expect(cql(`${batch}; SELECT 1;`)).toEqual([batch, "SELECT 1"]);
		expect(cql("BEGIN BATCH INSERT INTO t (k) VALUES (1); INSERT INTO t (k) VALUES (2);")).toEqual([
			"BEGIN BATCH INSERT INTO t (k) VALUES (1); INSERT INTO t (k) VALUES (2);",
		]);
	});

	it("does not treat other BEGIN statements as batches", () => {
		expect(cql("BEGIN; SELECT 1;")).toEqual(["BEGIN", "SELECT 1"]);
	});

	it("keeps $$ strings whole", () => {
		const fn =
			"CREATE FUNCTION ks.f(a int) CALLED ON NULL INPUT RETURNS int LANGUAGE java AS $$ int x = a; return x; $$";
		expect(cql(`${fn};\nSELECT 1;`)).toEqual([fn, "SELECT 1"]);
		expect(cql("SELECT $$a'b;c$$ FROM t; SELECT 2;")).toEqual(["SELECT $$a'b;c$$ FROM t", "SELECT 2"]);
	});

	it("does not treat tagged dollar quotes as strings", () => {
		expect(cql("SELECT $a$ ; $a$;")).toEqual(["SELECT $a$", "$a$"]);
	});

	it("splits statements separated by blank lines like other dialects", () => {
		expect(cql("SELECT 1\n\nSELECT 2", true)).toEqual(["SELECT 1", "SELECT 2"]);
	});

	it("reports statement ranges", () => {
		const sql = "USE ks;\n// c\nSELECT * FROM t;";
		const list = splitStatements(sql, "cassandra");
		expect(list.map((s) => sql.slice(s.start, s.end))).toEqual(["USE ks", "SELECT * FROM t"]);
	});
});

describe("isCqlBatchStart", () => {
	it("matches batch openers only", () => {
		expect(isCqlBatchStart("BEGIN BATCH")).toBe(true);
		expect(isCqlBatchStart("-- c\nbegin counter batch")).toBe(true);
		expect(isCqlBatchStart("BEGIN TRANSACTION")).toBe(false);
		expect(isCqlBatchStart("SELECT 1")).toBe(false);
	});
});
