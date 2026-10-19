/**
 * Mock backend for developing the UI in a browser, without Tauri.
 * Loaded only by `vite dev` when the page is not running inside the app.
 */
import { mockIPC } from "@tauri-apps/api/mocks";
import type { ConnectionProfile, DbColumn, ExecResult } from "./api";
import { emptyProfile } from "./api";
import { workspace } from "./state/workspace.svelte";
import { explorer } from "./state/explorer.svelte";

const profiles: ConnectionProfile[] = [
	{ ...emptyProfile("postgres"), id: "pg1", name: "Local", database: "shop", color: "#b8f238" },
	{
		...emptyProfile("oracle"),
		id: "ora1",
		name: "ERP production",
		host: "erp-db.internal",
		database: "ERPPDB",
		user: "REPORTS",
		color: "#dc2626",
	},
];

const FIRST = [
	"Olivia", "Liam", "Emma", "Noah", "Ava", "Ethan", "Sophia", "Mason", "Isabella", "Lucas",
	"Mia", "James", "Amelia", "Benjamin", "Harper", "Henry", "Evelyn", "Alexander", "Abigail", "Daniel",
];
const LAST = [
	"Smith", "Johnson", "Williams", "Brown", "Jones", "Garcia", "Miller", "Davis", "Martinez", "Wilson",
	"Anderson", "Taylor", "Thomas", "Moore", "Jackson", "Martin", "Lee", "Thompson", "White", "Clark",
	"Lewis", "Walker", "Hall", "Young",
];
const CITIES = ["London", "Toronto", "Austin", "Dublin", "Melbourne", "Seattle", "Amsterdam", "Chicago", "Lisbon"];

/** Deterministic pseudo-random number in [0, 1) (mulberry32). */
function rand(seed: number): number {
	let t = (seed + 0x6d2b79f5) | 0;
	t = Math.imul(t ^ (t >>> 15), t | 1);
	t ^= t + Math.imul(t ^ (t >>> 7), t | 61);
	return ((t ^ (t >>> 14)) >>> 0) / 4294967296;
}

const CUSTOMERS: (string | null)[][] = Array.from({ length: 1800 }, (_, i) => {
	const first = FIRST[Math.floor(rand(i * 3 + 1) * FIRST.length)];
	const last = LAST[Math.floor(rand(i * 3 + 2) * LAST.length)];
	const created = new Date(Date.UTC(2024, 0, 1) + Math.floor(rand(i * 3 + 3) * 700 * 86_400_000));
	return [
		String(i + 1),
		`${first} ${last}`,
		i % 7 === 3 ? null : `${first}.${last}${i % 100}@example.com`.toLowerCase(),
		CITIES[Math.floor(rand(i + 7_000) * CITIES.length)],
		(50 + rand(i + 9_000) * 24_950).toFixed(2),
		`${created.toISOString().slice(0, 19).replace("T", " ")}+00`,
		rand(i + 5_000) > 0.18 ? "true" : "false",
	];
});

const CUSTOMER_COLUMNS: DbColumn[] = [
	{ name: "id", typeName: "bigint", nullable: false, primaryKey: true },
	{ name: "name", typeName: "character varying(120)", nullable: false, primaryKey: false },
	{ name: "email", typeName: "character varying(200)", nullable: true, primaryKey: false },
	{ name: "city", typeName: "text", nullable: true, primaryKey: false },
	{ name: "balance", typeName: "numeric(12,2)", nullable: false, primaryKey: false },
	{ name: "created_at", typeName: "timestamp with time zone", nullable: false, primaryKey: false },
	{ name: "active", typeName: "boolean", nullable: false, primaryKey: false },
];

const TABLE_COLUMNS: Record<string, DbColumn[]> = {
	customers: CUSTOMER_COLUMNS,
	orders: [
		{ name: "id", typeName: "bigint", nullable: false, primaryKey: true },
		{ name: "customer_id", typeName: "bigint", nullable: false, primaryKey: false },
		{ name: "status", typeName: "text", nullable: false, primaryKey: false },
		{ name: "total", typeName: "numeric(12,2)", nullable: false, primaryKey: false },
		{ name: "placed_at", typeName: "timestamp with time zone", nullable: false, primaryKey: false },
	],
	order_items: [
		{ name: "order_id", typeName: "bigint", nullable: false, primaryKey: true },
		{ name: "line", typeName: "integer", nullable: false, primaryKey: true },
		{ name: "product", typeName: "text", nullable: false, primaryKey: false },
		{ name: "quantity", typeName: "integer", nullable: false, primaryKey: false },
		{ name: "unit_price", typeName: "numeric(12,2)", nullable: false, primaryKey: false },
	],
	vw_revenue: [
		{ name: "month", typeName: "date", nullable: true, primaryKey: false },
		{ name: "revenue", typeName: "numeric", nullable: true, primaryKey: false },
	],
};

function execute(sql: string, maxRows: number): ExecResult {
	const s = sql.trim().toLowerCase();
	const base = { truncated: false, rowsAffected: null, notices: [], inTransaction: false, elapsedMs: 12 };
	if (s.includes("error")) {
		throw { kind: "database", code: "42P01", message: 'relation "error" does not exist', position: s.indexOf("error") };
	}
	if (s.startsWith("select") || s.startsWith("with")) {
		let rows = CUSTOMERS;
		if (/\bwhere\s+active\b/.test(s)) rows = rows.filter((r) => r[6] === "true");
		if (/\border\s+by\s+balance\s+desc\b/.test(s)) rows = [...rows].sort((a, b) => Number(b[4]) - Number(a[4]));
		const total = rows.length;
		return {
			...base,
			elapsedMs: 38,
			truncated: maxRows > 0 && total > maxRows,
			columns: [
				{ name: "id", typeName: "int8", kind: "number" },
				{ name: "name", typeName: "varchar", kind: "text" },
				{ name: "email", typeName: "varchar", kind: "text" },
				{ name: "city", typeName: "text", kind: "text" },
				{ name: "balance", typeName: "numeric", kind: "number" },
				{ name: "created_at", typeName: "timestamptz", kind: "temporal" },
				{ name: "active", typeName: "bool", kind: "bool" },
			],
			rows: maxRows > 0 ? rows.slice(0, maxRows) : rows,
		};
	}
	if (s.startsWith("update") || s.startsWith("delete") || s.startsWith("insert")) {
		return { ...base, columns: null, rows: [], rowsAffected: 3, notices: ["NOTICE: 3 rows changed"] };
	}
	return { ...base, columns: null, rows: [] };
}

export function installDevMock() {
	let n = 0;
	mockIPC(async (cmd, payload) => {
		const args = (payload ?? {}) as Record<string, any>;
		await new Promise((r) => setTimeout(r, 120));
		switch (cmd) {
			case "list_profiles":
				return profiles;
			case "save_profile": {
				const p = { ...args.profile, id: args.profile.id || `p${++n}` };
				const i = profiles.findIndex((x) => x.id === p.id);
				if (i >= 0) profiles[i] = p;
				else profiles.push(p);
				return p;
			}
			case "delete_profile":
				profiles.splice(profiles.findIndex((x) => x.id === args.id), 1);
				return null;
			case "test_connection":
				return { serverVersion: "PostgreSQL 17.2" };
			case "open_session": {
				const p = profiles.find((x) => x.id === args.profileId)!;
				return {
					sessionId: `s${++n}`,
					profileId: p.id,
					engine: p.engine,
					serverVersion: p.engine === "postgres" ? "PostgreSQL 17.2" : "Oracle 23.6.0.24.10",
					currentUser: p.user,
					currentSchema: p.engine === "postgres" ? "public" : p.user,
					autocommit: true,
				};
			}
			case "execute":
				return execute(args.request.sql, args.request.maxRows);
			case "list_schemas":
				return ["public", "sales", "audit", "information_schema", "pg_catalog"];
			case "list_objects":
				return [
					{ name: "customers", kind: "table" },
					{ name: "orders", kind: "table" },
					{ name: "order_items", kind: "table" },
					{ name: "vw_revenue", kind: "view" },
					{ name: "orders_id_seq", kind: "sequence" },
					{ name: "recalc_balance(integer)", kind: "function" },
				];
			case "list_columns":
				return TABLE_COLUMNS[args.table] ?? CUSTOMER_COLUMNS;
			case "set_autocommit":
				return { autocommit: args.on, inTransaction: false };
			case "commit":
			case "rollback":
				return { autocommit: false, inTransaction: false };
			default:
				return null;
		}
	});
}

const DEMO_SQL = `-- Active customers by balance
select id, name, email, city, balance, created_at, active
  from customers
 where active
 order by balance desc;`;

/**
 * Demo mode (`?demo` in the URL), used for README screenshots: one query tab
 * with a sample query already executed and the first connection's tree open
 * down to the columns of `customers`. Must run before the app mounts.
 */
export function setupDemo() {
	try {
		// Replace any stale drafts and layout preferences so the result is deterministic.
		localStorage.removeItem("lowquery.sidebar.width");
		localStorage.setItem(
			"lowquery.tabs.v1",
			JSON.stringify({
				drafts: [{ id: "demo", title: "Query 1", profileId: profiles[0].id, sql: DEMO_SQL, maxRows: 500 }],
				active: "demo",
			}),
		);
	} catch {
		// Storage unavailable: the app starts with an empty tab.
	}

	const init = workspace.init.bind(workspace);
	workspace.init = async () => {
		await init();
		const profile = workspace.profiles[0];
		const tab = workspace.activeTab;
		if (!profile) return;
		// `load` opens the current schema (`public`) on its own.
		await explorer.load(profile);
		const schema = explorer.tree(profile.id)?.schemas?.find((s) => s.name === "public");
		const table = schema?.objects?.find((o) => o.name === "customers");
		if (schema && table) await explorer.toggleObject(profile.id, schema, table);
		if (tab) await workspace.run(tab, "script");
	};
}
