/**
 * Mock backend for developing the UI in a browser, without Tauri.
 * Loaded only by `vite dev` when the page is not running inside the app.
 */
import { mockIPC } from "@tauri-apps/api/mocks";
import type { ApplyResult, ConnectionProfile, DbColumn, EditTarget, ExecResult, RowChange } from "./api";
import { emptyProfile } from "./api";
import { workspace } from "./state/workspace.svelte";
import { explorer } from "./state/explorer.svelte";

const profiles: ConnectionProfile[] = [
	{ ...emptyProfile("postgres"), id: "pg1", name: "Local", database: "shop", color: "#b8f238", aiAccess: "write" },
	{
		...emptyProfile("oracle"),
		id: "ora1",
		name: "ERP production",
		host: "erp-db.internal",
		database: "ERPPDB",
		user: "REPORTS",
		color: "#dc2626",
	},
	{ ...emptyProfile("mysql"), id: "my1", name: "Storefront", host: "store-db.internal", database: "store", user: "app" },
	{
		...emptyProfile("sqlserver"),
		id: "ms1",
		name: "Finance",
		host: "fin-sql.internal",
		instance: "REPORTING",
		database: "Finance",
		windowsAuth: true,
	},
	{ ...emptyProfile("sqlite"), id: "lite1", name: "Local cache", database: String.raw`C:\data\cache.db` },
];

// tnsnames.ora entries as the backend reports them (see src-tauri/src/drivers/tns.rs).
function tnsEntry(alias: string, host: string, port: number, service: string) {
	return {
		alias,
		descriptor: `(DESCRIPTION=(ADDRESS=(PROTOCOL=TCP)(HOST=${host})(PORT=${port}))(CONNECT_DATA=(SERVICE_NAME=${service})))`,
		host,
		port,
		serviceName: service,
		sid: null,
		protocol: "TCP",
	};
}

const tnsSources = [
	{
		dir: String.raw`C:\oracle\product\19.0.0\client_1\network\admin`,
		label: "OraClient19Home1",
		origin: "Oracle home",
		error: null,
		entries: [
			tnsEntry("ERPPRD", "erp-db.internal", 1521, "erppdb"),
			tnsEntry("ERPPRD.WORLD", "erp-db.internal", 1521, "erppdb"),
			tnsEntry("ERPHML", "erp-hml.internal", 1521, "erphml"),
			tnsEntry("BI", "bi-db.internal", 1522, "bipdb"),
			tnsEntry("LEGACY", "legacy-db.internal", 1521, "orcl"),
		],
	},
	{
		dir: String.raw`C:\oracle\instantclient_23_5\network\admin`,
		label: String.raw`C:\oracle\instantclient_23_5`,
		origin: "Instant Client on PATH",
		error: null,
		entries: [tnsEntry("DEVDB", "localhost", 1521, "FREEPDB1")],
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
	const base = {
		truncated: false,
		rowsAffected: null,
		notices: [],
		inTransaction: false,
		elapsedMs: 12,
		editable: null,
		readOnlyReason: null,
	};
	if (s.includes("error")) {
		throw { kind: "database", code: "42P01", message: 'relation "error" does not exist', position: s.indexOf("error") };
	}
	if (s.startsWith("select") || s.startsWith("with")) {
		let rows = CUSTOMERS;
		if (/\bwhere\s+active\b/.test(s)) rows = rows.filter((r) => r[6] === "true");
		if (/\border\s+by\s+balance\s+desc\b/.test(s)) rows = [...rows].sort((a, b) => Number(b[4]) - Number(a[4]));
		const total = rows.length;
		const joined = /\bjoin\b|,\s*orders\b/.test(s);
		return {
			...base,
			elapsedMs: 38,
			editable: joined ? null : CUSTOMER_TARGET,
			readOnlyReason: joined ? "Read-only: the result combines columns from several tables." : null,
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

const CUSTOMER_TARGET = {
	schema: "public",
	table: "customers",
	columns: CUSTOMER_COLUMNS.map((c) => ({ name: c.name, typeName: c.typeName })),
	key: [0],
};

// Applies grid edits to the in-memory customers, all or nothing.
function applyChanges(req: { target: EditTarget; changes: RowChange[] }): ApplyResult {
	const next = CUSTOMERS.map((r) => [...r]);
	const out = { updated: 0, deleted: 0, inserted: 0, elapsedMs: 9, inTransaction: false };
	for (const [i, change] of req.changes.entries()) {
		if (change.op === "insert") {
			const row: (string | null)[] = [String(Math.max(...next.map((r) => Number(r[0]))) + 1), "", null, null, "0.00", new Date().toISOString().slice(0, 19).replace("T", " ") + "+00", "true"];
			for (const v of change.values) row[v.column] = v.value;
			next.push(row);
			out.inserted++;
			continue;
		}
		const at = next.findIndex((r) => r[0] === change.key[0]);
		if (at < 0) {
			throw { kind: "database", message: `Row ${i + 1} was not saved: 0 rows matched its key (changed or removed by someone else).` };
		}
		if (change.op === "delete") {
			next.splice(at, 1);
			out.deleted++;
		} else {
			for (const v of change.values) {
				if (v.column === 4 && v.value !== null && Number.isNaN(Number(v.value))) {
					throw { kind: "database", code: "22P02", message: `invalid input syntax for type numeric: "${v.value}"` };
				}
				next[at][v.column] = v.value;
			}
			out.updated++;
		}
	}
	CUSTOMERS.splice(0, CUSTOMERS.length, ...next);
	return out;
}

const MCP_EXE = String.raw`C:\Users\dev\AppData\Local\LowQuery\lowquery.exe`;
const mcpJson = (key: string, extra: Record<string, string> = {}) =>
	JSON.stringify({ [key]: { lowquery: { ...extra, command: MCP_EXE, args: ["mcp"] } } }, null, 2);

const AI_SETUP = {
	command: MCP_EXE,
	args: ["mcp"],
	clients: [
		{ id: "claude-desktop", name: "Claude Desktop", configPath: String.raw`C:\Users\dev\AppData\Roaming\Claude\claude_desktop_config.json`, installed: true, configured: false, snippet: mcpJson("mcpServers"), snippetKind: "json" },
		{ id: "claude-code", name: "Claude Code", configPath: null, installed: true, configured: false, snippet: `claude mcp add --scope user lowquery -- ${MCP_EXE} mcp`, snippetKind: "shell" },
		{ id: "cursor", name: "Cursor", configPath: String.raw`C:\Users\dev\.cursor\mcp.json`, installed: true, configured: true, snippet: mcpJson("mcpServers"), snippetKind: "json" },
		{ id: "vscode", name: "VS Code", configPath: String.raw`C:\Users\dev\AppData\Roaming\Code\User\mcp.json`, installed: false, configured: false, snippet: mcpJson("servers", { type: "stdio" }), snippetKind: "json" },
		{ id: "windsurf", name: "Windsurf", configPath: String.raw`C:\Users\dev\.codeium\windsurf\mcp_config.json`, installed: false, configured: false, snippet: mcpJson("mcpServers"), snippetKind: "json" },
		{ id: "gemini", name: "Gemini CLI", configPath: String.raw`C:\Users\dev\.gemini\settings.json`, installed: false, configured: false, snippet: mcpJson("mcpServers"), snippetKind: "json" },
		{ id: "codex", name: "Codex", configPath: null, installed: false, configured: false, snippet: `[mcp_servers.lowquery]\ncommand = "${MCP_EXE.replaceAll("\\", "\\\\")}"\nargs = ["mcp"]`, snippetKind: "toml" },
	],
};

let aiActivity = [
	{ at: "2026-10-20T14:02:11Z", client: "claude-ai 0.1.0", tool: "execute", connectionId: "pg1", connectionName: "Local", detail: "update customers set active = false where id = 1694", ok: true, rows: 1, error: null, elapsedMs: 9 },
	{ at: "2026-10-20T14:01:40Z", client: "claude-ai 0.1.0", tool: "query", connectionId: "pg1", connectionName: "Local", detail: "select id, name, balance\n  from customers\n where active\n order by balance desc\n fetch first 10 rows only", ok: true, rows: 10, error: null, elapsedMs: 31 },
	{ at: "2026-10-20T14:01:22Z", client: "claude-ai 0.1.0", tool: "describe_table", connectionId: "pg1", connectionName: "Local", detail: "public.customers", ok: true, rows: 7, error: null, elapsedMs: 12 },
	{ at: "2026-10-20T14:01:05Z", client: "cursor 1.7.2", tool: "query", connectionId: "pg1", connectionName: "Local", detail: "delete from orders", ok: false, rows: null, error: "This connection is read-only for AI agents. Ask the user to allow changes in LowQuery (connection → AI access).", elapsedMs: 0 },
	{ at: "2026-10-20T14:00:58Z", client: "claude-ai 0.1.0", tool: "list_connections", connectionId: null, connectionName: null, detail: null, ok: true, rows: 1, error: null, elapsedMs: 2 },
];

const IMPORT_SOURCES = [
	{
		tool: "dbeaver",
		label: "DBeaver · General",
		path: String.raw`C:\Users\dev\AppData\Roaming\DBeaverData\workspace6\General\.dbeaver\data-sources.json`,
		count: 5,
		error: null,
	},
	{
		tool: "dbeaver",
		label: "DBeaver · Clients",
		path: String.raw`C:\Users\dev\AppData\Roaming\DBeaverData\workspace6\Clients\.dbeaver\data-sources.json`,
		count: 0,
		error: "This is not a DBeaver data sources file.",
	},
	{
		tool: "sql-developer",
		label: "SQL Developer 23.1",
		path: String.raw`C:\Users\dev\AppData\Roaming\SQL Developer\system23.1.0.097.1607\o.jdeveloper.db.connection\connections.json`,
		count: 2,
		error: null,
	},
];

// What the backend returns for a tool file (see src-tauri/src/import).
function importPreview(tool: string) {
	const cand = (key: string, profile: Partial<ConnectionProfile>, extra: Record<string, unknown> = {}) => ({
		key,
		profile: { ...emptyProfile(profile.engine ?? "postgres"), ...profile, id: "" },
		folder: null as string | null,
		hasPassword: false,
		warnings: [] as string[],
		...extra,
	});
	if (tool === "sql-developer") {
		return {
			candidates: [
				cand("c1", { engine: "oracle", name: "ERP produção", host: "", oracleConnectType: "tns", tnsAlias: "ERPPRD", user: "REPORTS", color: "#dc2626" }),
				cand("c2", { engine: "oracle", name: "BI", host: "bi-db.internal", port: 1522, database: "bipdb", user: "BI_READ" }),
			],
			skipped: [],
			notes: ["SQL Developer encrypts passwords with a key tied to its installation; type them on the first connection."],
		};
	}
	return {
		candidates: [
			cand("pg-local", { engine: "postgres", name: "Local", host: "localhost", port: 5432, database: "shop", user: "postgres" }, { hasPassword: true }),
			cand(
				"pg-sales",
				{ engine: "postgres", name: "Sales replica", host: "sales-ro.internal", database: "sales", user: "analyst", sslMode: "verify-full" },
				{ folder: "Production", hasPassword: true, warnings: ["The SSH tunnel through bastion.internal was not imported."] },
			),
			cand(
				"ora-erp",
				{ engine: "oracle", name: "ERP (prod)", host: "", oracleConnectType: "tns", tnsAlias: "ERPPRD", configDir: String.raw`C:\oracle\network\admin`, user: "SIGACON", color: "#dc2626" },
				{ folder: "Production/ERP", hasPassword: true },
			),
			cand(
				"ms-fin",
				{ engine: "sqlserver", name: "Finance", host: "fin-sql.internal", port: 1433, instance: "REPORTING", database: "Finance", windowsAuth: true, user: "" },
				{ folder: "Production" },
			),
			cand("my-shop", { engine: "mariadb", name: "Storefront staging", host: "store-stg.internal", port: 3306, database: "store", user: "app", color: "#f59e0b" }, { hasPassword: true }),
		],
		skipped: [
			{ name: "Warehouse", reason: "Snowflake is not supported by LowQuery." },
			{ name: "Mainframe", reason: "DB2 for z/OS is not supported by LowQuery." },
		],
		notes: [] as string[],
	};
}

export function installDevMock() {
	let n = 0;
	mockIPC(async (cmd, payload) => {
		const args = (payload ?? {}) as Record<string, any>;
		await new Promise((r) => setTimeout(r, 120));
		switch (cmd) {
			case "list_profiles":
				return profiles;
			case "oracle_tns_sources":
				return tnsSources;
			case "oracle_tns_folder":
				return { ...tnsSources[1], dir: args.path, label: args.path, origin: "Selected folder" };
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
			case "apply_changes":
				return applyChanges(args.request);
			case "ai_setup":
				return AI_SETUP;
			case "ai_connect_client": {
				const c = AI_SETUP.clients.find((x) => x.id === args.id)!;
				c.configured = true;
				return c.configPath;
			}
			case "ai_activity":
				return aiActivity;
			case "ai_activity_clear":
				aiActivity = [];
				return null;
			case "import_detect":
				return IMPORT_SOURCES;
			case "import_preview":
				return { tool: args.tool, path: args.path, ...importPreview(args.tool) };
			case "import_apply": {
				const { candidates } = importPreview(args.tool);
				let passwords = 0;
				for (const key of args.keys as string[]) {
					const c = candidates.find((x) => x.key === key)!;
					if (c.hasPassword && args.withPasswords) passwords++;
					profiles.push({ ...c.profile, id: `p${++n}` });
				}
				return { created: args.keys.length, passwords };
			}
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
