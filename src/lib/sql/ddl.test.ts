import { describe, expect, it } from "bun:test";
import {
	alterTableSql,
	createTableSql,
	dropObjectSql,
	foldName,
	refreshMatviewSql,
	renameTableSql,
	toScript,
	useSchemaSql,
	type ColumnDraft,
	type ColumnSpec,
} from "./ddl";
import { splitStatements } from "./split";

const col = (name: string, type: string, extra: Partial<ColumnSpec> = {}): ColumnSpec => ({
	name,
	type,
	nullable: true,
	defaultValue: "",
	primaryKey: false,
	identity: false,
	...extra,
});
const keep = (c: ColumnSpec, extra: Partial<ColumnDraft> = {}): ColumnDraft => ({ ...c, origin: c.name, ...extra });

describe("foldName", () => {
	it("folds plain names like the database does", () => {
		expect(foldName("Orders", "postgres")).toBe("orders");
		expect(foldName("orders", "oracle")).toBe("ORDERS");
		expect(foldName("Orders", "mysql")).toBe("Orders");
		expect(foldName("  Order Items ", "postgres")).toBe("Order Items");
	});
});

describe("createTableSql", () => {
	it("needs a name and named, typed columns", () => {
		expect(createTableSql("public", "", [col("a", "int")], "postgres").statements).toEqual([]);
		const plan = createTableSql("public", "t", [col("a", ""), col("A", "int")], "postgres");
		expect(plan.statements).toEqual([]);
		expect(plan.warnings).toContain("Column a needs a type.");
		expect(plan.warnings).toContain("Column A appears twice.");
	});

	it("uses each engine's identity clause", () => {
		const cols = [col("id", "bigint", { primaryKey: true, identity: true }), col("name", "text", { defaultValue: "'x'" })];
		expect(createTableSql("dbo", "t", cols, "sqlserver").statements[0]).toBe(
			"CREATE TABLE dbo.t (\n    id bigint IDENTITY(1,1) NOT NULL,\n    name text NULL DEFAULT 'x',\n    PRIMARY KEY (id)\n)",
		);
		expect(createTableSql("db", "t", cols, "mysql").statements[0]).toContain("id bigint NOT NULL AUTO_INCREMENT");
		expect(createTableSql("main", "t", [col("id", "INTEGER", { primaryKey: true, identity: true })], "sqlite").statements[0]).toBe(
			"CREATE TABLE main.t (\n    id INTEGER PRIMARY KEY AUTOINCREMENT\n)",
		);
	});

	it("warns when SQLite cannot generate the values", () => {
		const plan = createTableSql("main", "t", [col("id", "TEXT", { primaryKey: true, identity: true })], "sqlite");
		expect(plan.warnings[0]).toContain("INTEGER PRIMARY KEY");
	});
});

describe("alterTableSql", () => {
	const before = [col("id", "int", { primaryKey: true, nullable: false }), col("name", "text")];

	it("reports no change", () => {
		const plan = alterTableSql("public", "t", before, before.map((c) => keep(c)), "postgres");
		expect(plan.statements).toEqual([]);
		expect(plan.warnings).toEqual(["Nothing changed."]);
	});

	it("renames the table last", () => {
		const plan = alterTableSql("public", "t", before, [keep(before[0]), keep(before[1], { name: "title" })], "postgres", "u");
		expect(plan.statements).toEqual(["ALTER TABLE public.t RENAME COLUMN name TO title", "ALTER TABLE public.t RENAME TO u"]);
	});

	it("rewrites a MySQL column with MODIFY and warns about it", () => {
		const plan = alterTableSql("db", "t", before, [keep(before[0]), keep(before[1], { nullable: false })], "mysql");
		expect(plan.statements).toEqual(["ALTER TABLE db.t MODIFY COLUMN name text NOT NULL"]);
		expect(plan.warnings[0]).toContain("MODIFY COLUMN");
	});

	it("explains what SQLite cannot change", () => {
		const plan = alterTableSql("main", "t", before, [keep(before[0]), keep(before[1], { type: "INTEGER" })], "sqlite");
		expect(plan.statements).toEqual([]);
		expect(plan.warnings[0]).toContain("SQLite cannot change the type of name");
	});

	it("replaces the primary key where the engine can drop it by kind", () => {
		const after = [keep(before[0], { primaryKey: false }), keep(before[1], { primaryKey: true })];
		expect(alterTableSql("APP", "T", before, after, "oracle").statements).toEqual([
			"ALTER TABLE APP.T DROP PRIMARY KEY",
			// Lowercase names are case-sensitive in Oracle, hence the quotes.
			'ALTER TABLE APP.T ADD PRIMARY KEY ("name")',
		]);
		const pg = alterTableSql("public", "t", before, after, "postgres");
		expect(pg.statements).toEqual(["ALTER TABLE public.t ADD PRIMARY KEY (name)"]);
		expect(pg.warnings[0]).toContain("drop the existing primary key");
	});
});

describe("objects", () => {
	it("drops PostgreSQL functions with their argument types", () => {
		expect(dropObjectSql("public", "Calc(integer, text)", "function", "postgres", true)).toEqual([
			'DROP FUNCTION public."Calc"(integer, text) CASCADE',
		]);
	});

	it("drops Oracle public synonyms and cascades constraints", () => {
		expect(dropObjectSql("PUBLIC", "DUAL2", "synonym", "oracle")).toEqual(["DROP PUBLIC SYNONYM DUAL2"]);
		expect(dropObjectSql("APP", "T", "table", "oracle", true)).toEqual(["DROP TABLE APP.T CASCADE CONSTRAINTS"]);
		expect(dropObjectSql("db", "t", "table", "mysql", true)).toEqual(["DROP TABLE db.t"]);
	});

	it("renames with each engine's statement", () => {
		expect(renameTableSql("dbo", "a", "b", "sqlserver")).toEqual(["EXEC sp_rename N'dbo.a', N'b'"]);
		expect(renameTableSql("db", "a", "b", "mariadb")).toEqual(["RENAME TABLE db.a TO db.b"]);
	});

	it("switches the session's schema", () => {
		expect(useSchemaSql("sales", "postgres")).toBe("SET search_path TO sales");
		expect(useSchemaSql("db3", "redis")).toBe("SELECT 3");
		expect(useSchemaSql("dbo", "sqlserver")).toBeNull();
	});
});

describe("toScript", () => {
	it("keeps PL/SQL blocks whole for the Oracle splitter", () => {
		const script = toScript([...refreshMatviewSql("APP", "MV", "oracle"), "DROP TABLE APP.T"], "oracle");
		const parts = splitStatements(script, "oracle").map((s) => s.text);
		expect(parts).toHaveLength(2);
		expect(parts[0]).toContain("DBMS_MVIEW.REFRESH('APP.MV');");
	});

	it("keeps T-SQL blocks whole for the SQL Server splitter", () => {
		const before = [col("id", "int", { primaryKey: true }), col("v", "int", { defaultValue: "((0))" })];
		const plan = alterTableSql("dbo", "t", before, [keep(before[0])], "sqlserver");
		expect(plan.statements).toHaveLength(2);
		const parts = splitStatements(toScript(plan.statements, "sqlserver"), "sqlserver");
		expect(parts).toHaveLength(2);
		expect(parts[0].text).toStartWith("BEGIN");
		expect(parts[1].text).toBe("ALTER TABLE dbo.t DROP COLUMN v");
	});
});
