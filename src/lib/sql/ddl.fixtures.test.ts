/**
 * The statements the sidebar menus and the table designer write (`ddl.ts`),
 * per dialect, kept in `ddl.fixtures.json` so the Rust integration tests run
 * them against real databases (`src-tauri/src/drivers/ddl_fixtures.rs`),
 * together with a round trip of the DDL each driver reads from its catalog.
 * After changing `ddl.ts`, regenerate the file with
 * `LQ_UPDATE_FIXTURES=1 bun test src/lib/sql/ddl.fixtures.test.ts`.
 *
 * `lq_schema` (`LQ_SCHEMA` in Oracle) stands for the test schema; the Rust
 * side replaces it.
 */
import { expect, it } from "bun:test";
import { readFileSync, writeFileSync } from "node:fs";
import type { Engine } from "../api";
import {
	alterTableSql,
	countSql,
	createIndexSql,
	createSchemaSql,
	createTableSql,
	dropObjectSql,
	dropSchemaSql,
	duplicateTableSql,
	foldName,
	literal,
	renameTableSql,
	truncateSql,
	type ColumnDraft,
	type ColumnSpec,
} from "./ddl";
import { qualified, quoteIdent } from "./ident";

type Dialect = "postgres" | "oracle" | "mysql" | "sqlserver" | "sqlite";

const TYPES: Record<Dialect, { int: string; text: string; short: string; dec: string; wider: string }> = {
	postgres: { int: "integer", text: "varchar(50)", short: "varchar(20)", dec: "numeric(10,2)", wider: "numeric(12,2)" },
	oracle: { int: "NUMBER(10)", text: "VARCHAR2(50)", short: "VARCHAR2(20)", dec: "NUMBER(10,2)", wider: "NUMBER(12,2)" },
	mysql: { int: "INT", text: "VARCHAR(50)", short: "VARCHAR(20)", dec: "DECIMAL(10,2)", wider: "DECIMAL(12,2)" },
	sqlserver: { int: "int", text: "nvarchar(50)", short: "nvarchar(20)", dec: "decimal(10,2)", wider: "decimal(12,2)" },
	sqlite: { int: "INTEGER", text: "TEXT", short: "TEXT", dec: "NUMERIC", wider: "REAL" },
};

type Step =
	| { label: string; sql: string }
	| { label: string; sql: string; rows: string[] }
	| { label: string; ddl: { name: string; kind: string; contains: string } }
	| { label: string; roundTrip: { name: string; kind: string; drop: string } };

const col = (name: string, type: string, extra: Partial<ColumnSpec> = {}): ColumnSpec => ({
	name,
	type,
	nullable: true,
	defaultValue: "",
	primaryKey: false,
	identity: false,
	...extra,
});

function build(d: Dialect) {
	const e = d as Engine;
	const t = TYPES[d];
	const schema = d === "oracle" ? "LQ_SCHEMA" : "lq_schema";
	const n = (s: string) => foldName(s, e);
	const q = (s: string) => quoteIdent(n(s), e);
	const tbl = (s: string) => qualified(schema, n(s), e);
	const items = n("lq_ddl_items");
	const steps: Step[] = [];
	const run = (label: string, sqls: string[]) => sqls.forEach((sql, i) => steps.push({ label: sqls.length > 1 ? `${label} (${i + 1})` : label, sql }));
	const check = (label: string, sql: string, rows: (string | number)[]) => steps.push({ label, sql, rows: rows.map(String) });

	const cleanup = [
		...dropObjectSql(schema, n("lq_ddl_view"), "view", e),
		...(d === "postgres" ? dropObjectSql(schema, "lq_ddl_fn(x integer)", "function", e) : d === "sqlite" ? [] : dropObjectSql(schema, n("lq_ddl_fn"), "function", e)),
		...(d === "mysql" || d === "sqlite" ? [] : dropObjectSql(schema, n("lq_ddl_seq"), "sequence", e)),
		...["lq_ddl_items", "lq_ddl_items2", "lq_ddl_copy", "lq_ddl_nopk", "lq_ddl_parent"].flatMap((x) => dropObjectSql(schema, n(x), "table", e, true)),
		...(d === "oracle" || d === "sqlite" ? [] : dropSchemaSql(n("lq_ddl_schema"), e, true)),
	];

	// Create, fill and index a table, with a foreign key to another.
	const parent = createTableSql(schema, n("lq_ddl_parent"), [col(n("id"), t.int, { primaryKey: true })], e);
	run("create parent", parent.statements);
	run("insert parent", [`INSERT INTO ${tbl("lq_ddl_parent")} (${q("id")}) VALUES (1)`]);
	const before: ColumnSpec[] = [
		col(n("id"), t.int, { primaryKey: true, identity: true, nullable: false }),
		col(n("name"), t.text, { nullable: false, defaultValue: literal("x") }),
		col(n("price"), t.dec),
		col(n("note"), t.short),
		col(n("obsolete"), t.short, { defaultValue: literal("z") }),
		col(n("parent_id"), t.int),
	];
	const created = createTableSql(schema, items, before, e);
	expect(created.warnings).toEqual([]);
	run("create table", created.statements);
	run("create index", [createIndexSql(schema, items, n("name"), e)]);
	if (d !== "sqlite") {
		run("add foreign key", [
			`ALTER TABLE ${tbl("lq_ddl_items")} ADD CONSTRAINT ${q("lq_ddl_items_parent_fk")} FOREIGN KEY (${q("parent_id")}) REFERENCES ${tbl("lq_ddl_parent")} (${q("id")})`,
		]);
	}
	const insert = (name: string, price: string, note: string) =>
		`INSERT INTO ${tbl("lq_ddl_items")} (${q("name")}, ${q("price")}, ${q("note")}, ${q("parent_id")}) VALUES (${name}, ${price}, ${note}, 1)`;
	run("insert rows", [insert("'a'", "1.5", "'n1'"), insert("'b'", "2", "NULL")]);

	// Designer edits: drop a column with a default, rename one, widen a type
	// and make it NOT NULL, change a default, add a NOT NULL column.
	const after: ColumnDraft[] = [
		{ ...before[0], origin: before[0].name, identity: false },
		{ ...before[1], origin: before[1].name, defaultValue: literal("y") },
		{ ...before[2], origin: before[2].name, type: t.wider, nullable: false },
		{ ...before[3], origin: before[3].name, name: n("remark") },
		{ ...before[5], origin: before[5].name },
		{ ...col(n("qty"), t.int, { nullable: false, defaultValue: "0" }), origin: null },
	];
	const altered = alterTableSql(schema, items, before, after, e);
	if (d === "sqlite") expect(altered.warnings.length).toBeGreaterThan(0);
	run("alter table", altered.statements);
	run("insert with defaults", [`INSERT INTO ${tbl("lq_ddl_items")} (${q("price")}) VALUES (3)`]);
	check("new column filled with its default", `SELECT ${q("qty")} FROM ${tbl("lq_ddl_items")} ORDER BY ${q("id")}`, [0, 0, 0]);
	check("renamed column kept its values", `SELECT COUNT(${q("remark")}) FROM ${tbl("lq_ddl_items")}`, [1]);
	check(
		"changed default",
		`SELECT ${q("name")} FROM ${tbl("lq_ddl_items")} WHERE ${q("price")} = 3`,
		[d === "sqlite" ? "x" : "y"],
	);

	// The DDL each driver reads back recreates the same objects.
	run("create view", [`CREATE VIEW ${tbl("lq_ddl_view")} AS SELECT ${q("id")}, ${q("name")} FROM ${tbl("lq_ddl_items")}`]);
	steps.push({
		label: "view DDL round trip",
		roundTrip: { name: n("lq_ddl_view"), kind: "view", drop: dropObjectSql(schema, n("lq_ddl_view"), "view", e)[0] },
	});
	run("drop view", dropObjectSql(schema, n("lq_ddl_view"), "view", e));
	steps.push({
		label: "table DDL round trip",
		roundTrip: { name: items, kind: "table", drop: dropObjectSql(schema, items, "table", e)[0] },
	});
	check("recreated table is empty", countSql(schema, items, e), [0]);

	const fn: Partial<Record<Dialect, [string, string]>> = {
		postgres: [`CREATE FUNCTION ${tbl("lq_ddl_fn")}(x integer) RETURNS integer LANGUAGE sql AS 'SELECT x * 2'`, "lq_ddl_fn(x integer)"],
		oracle: [`CREATE OR REPLACE FUNCTION ${tbl("lq_ddl_fn")}(x NUMBER) RETURN NUMBER IS BEGIN RETURN x * 2; END;`, n("lq_ddl_fn")],
		mysql: [`CREATE FUNCTION ${tbl("lq_ddl_fn")}(x INT) RETURNS INT DETERMINISTIC RETURN x * 2`, n("lq_ddl_fn")],
		sqlserver: [`CREATE FUNCTION ${tbl("lq_ddl_fn")}(@x int) RETURNS int AS BEGIN RETURN @x * 2 END`, n("lq_ddl_fn")],
	};
	const f = fn[d];
	if (f) {
		run("create function", [f[0]]);
		steps.push({ label: "function DDL", ddl: { name: f[1], kind: "function", contains: "lq_ddl_fn" } });
		run("drop function", dropObjectSql(schema, f[1], "function", e));
	}
	if (d === "postgres" || d === "oracle" || d === "sqlserver") {
		run("create sequence", [`CREATE SEQUENCE ${tbl("lq_ddl_seq")} START WITH 5 INCREMENT BY 2`]);
		steps.push({
			label: "sequence DDL round trip",
			roundTrip: { name: n("lq_ddl_seq"), kind: "sequence", drop: dropObjectSql(schema, n("lq_ddl_seq"), "sequence", e)[0] },
		});
		run("drop sequence", dropObjectSql(schema, n("lq_ddl_seq"), "sequence", e));
	}

	// Table actions: rename, duplicate with and without rows, truncate, drop.
	const refill = (name: string, price: number) =>
		`INSERT INTO ${tbl("lq_ddl_items")} (${q("name")}, ${q("price")}, ${q("parent_id")}) VALUES (${name}, ${price}, 1)`;
	run("fill again", [refill("'c'", 4), refill("'d'", 5)]);
	run("rename table", renameTableSql(schema, items, n("lq_ddl_items2"), e));
	run("duplicate with rows", duplicateTableSql(schema, n("lq_ddl_items2"), n("lq_ddl_copy"), true, e).statements);
	check("copy has the rows", countSql(schema, n("lq_ddl_copy"), e), [2]);
	run("drop copy", dropObjectSql(schema, n("lq_ddl_copy"), "table", e));
	run("duplicate structure", duplicateTableSql(schema, n("lq_ddl_items2"), n("lq_ddl_copy"), false, e).statements);
	check("empty copy", countSql(schema, n("lq_ddl_copy"), e), [0]);
	run("truncate", truncateSql(schema, n("lq_ddl_copy"), e));
	run("truncate original", truncateSql(schema, n("lq_ddl_items2"), e));
	check("truncated", countSql(schema, n("lq_ddl_items2"), e), [0]);
	run("drop tables", [
		...dropObjectSql(schema, n("lq_ddl_copy"), "table", e),
		...dropObjectSql(schema, n("lq_ddl_items2"), "table", e, true),
	]);

	// A primary key added to a table without one.
	const nopk = [col(n("a"), t.int, { nullable: false }), col(n("b"), t.short)];
	run("create table without key", createTableSql(schema, n("lq_ddl_nopk"), nopk, e).statements);
	const withPk: ColumnDraft[] = [
		{ ...nopk[0], origin: nopk[0].name, primaryKey: true },
		{ ...nopk[1], origin: nopk[1].name },
	];
	run("add primary key", alterTableSql(schema, n("lq_ddl_nopk"), nopk, withPk, e).statements);
	run("drop keyed table", dropObjectSql(schema, n("lq_ddl_nopk"), "table", e));
	run("drop parent", dropObjectSql(schema, n("lq_ddl_parent"), "table", e));

	if (d !== "oracle" && d !== "sqlite") {
		run("create schema", createSchemaSql(n("lq_ddl_schema"), e));
		run("drop schema", dropSchemaSql(n("lq_ddl_schema"), e, true));
	}
	return { schema, cleanup, steps };
}

const PATH = new URL("./ddl.fixtures.json", import.meta.url);

it("ddl.fixtures.json matches the SQL ddl.ts writes", () => {
	const dialects: Record<string, ReturnType<typeof build>> = {};
	for (const d of Object.keys(TYPES) as Dialect[]) dialects[d] = build(d);
	const text = JSON.stringify({ dialects }, null, "\t") + "\n";
	if (process.env.LQ_UPDATE_FIXTURES) writeFileSync(PATH, text);
	expect(readFileSync(PATH, "utf8").replaceAll("\r\n", "\n")).toBe(text);
});
