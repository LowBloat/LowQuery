import { toast } from "@codeatlasdev/ui";
import { revealItemInDir } from "@tauri-apps/plugin-opener";
import { api, asAppError, errorText, type ConnectionProfile, type DbColumn } from "../api";
import { engineInfo } from "../engines";
import { connectionSummary } from "../summary";
import * as ddl from "../sql/ddl";
import { previewQuery, qualified, quoteIdent, redisArg, redisPreview } from "../sql/ident";
import { explorer, type ObjectNode, type SchemaNode } from "./explorer.svelte";
import { workspace } from "./workspace.svelte";

/** Values of the fields an action dialog shows. */
export interface ActionValues {
	input: string;
	options: Record<string, boolean>;
}

/**
 * A change confirmed in a dialog that shows the statements it will run. They
 * run on the connection's tree session, or open in a new tab instead.
 */
export interface SqlAction {
	title: string;
	description?: string;
	profile: ConnectionProfile;
	confirmLabel: string;
	destructive?: boolean;
	input?: { label: string; value: string; placeholder?: string };
	options?: { id: string; label: string; checked: boolean }[];
	/** The confirm button stays disabled until this is typed. */
	typeToConfirm?: string;
	build: (v: ActionValues) => ddl.SqlPlan;
	/** Redis: the database the statements run in. */
	redisDb?: string;
	/** Toast after the statements ran. */
	success: (v: ActionValues) => string;
	/** Refreshes the tree afterwards. */
	after?: () => Promise<void> | void;
}

/** The table designer: a new table (`table` null) or the columns of an existing one. */
export interface DesignerRequest {
	profile: ConnectionProfile;
	schema: string;
	table: string | null;
	columns: DbColumn[];
}

export type MenuTarget =
	| { type: "connection"; profile: ConnectionProfile }
	| { type: "schema"; profile: ConnectionProfile; schema: SchemaNode }
	| { type: "object"; profile: ConnectionProfile; schema: SchemaNode; obj: ObjectNode };

const plan = (statements: string[], warnings: string[] = []): ddl.SqlPlan => ({ statements, warnings });

const KIND_NAME: Record<string, string> = {
	table: "table",
	view: "view",
	matview: "materialized view",
	foreign: "foreign table",
	sequence: "sequence",
	function: "function",
	procedure: "procedure",
	package: "package",
	type: "type",
	synonym: "synonym",
};

/** "table", "materialized view"... and "key" for Redis keys. */
export const kindName = (kind: string) => KIND_NAME[kind] ?? "key";

class Actions {
	menu = $state<{ x: number; y: number; target: MenuTarget } | null>(null);
	request = $state<SqlAction | null>(null);
	running = $state(false);
	error = $state<string | null>(null);
	designer = $state<DesignerRequest | null>(null);
	/** Profile waiting for the delete confirmation. */
	deleting = $state<ConnectionProfile | null>(null);

	openMenu(e: MouseEvent, target: MenuTarget) {
		e.preventDefault();
		e.stopPropagation();
		let { clientX: x, clientY: y } = e;
		// The context-menu key reports no pointer position: open under the row.
		if (x === 0 && y === 0 && e.currentTarget instanceof HTMLElement) {
			const r = e.currentTarget.getBoundingClientRect();
			x = r.left + 24;
			y = r.bottom;
		}
		this.menu = { x, y, target };
	}

	ask(request: SqlAction) {
		this.error = null;
		this.running = false;
		this.request = request;
	}

	cancel() {
		if (this.running) return;
		this.request = null;
		this.error = null;
	}

	/**
	 * Runs the statements one by one on the tree's session. Stops at the
	 * first error and returns it; the tree is refreshed either way, since
	 * earlier statements may have changed it.
	 */
	async execute(profile: ConnectionProfile, statements: string[], redisDb?: string): Promise<string | null> {
		const sessionId = await explorer.sessionFor(profile);
		if (!sessionId) return "Not connected.";
		const tree = explorer.tree(profile.id);
		const switchDb = profile.engine === "redis" && redisDb && redisDb !== tree?.currentSchema;
		try {
			if (switchDb) await api.execute(sessionId, `SELECT ${redisDb.replace(/^db/, "")}`, 10);
			for (let i = 0; i < statements.length; i++) {
				try {
					await api.execute(sessionId, statements[i], 100);
				} catch (e) {
					const err = errorText(asAppError(e));
					return statements.length > 1 ? `Statement ${i + 1} of ${statements.length} failed: ${err}` : err;
				}
			}
			return null;
		} finally {
			if (switchDb && tree?.currentSchema) {
				await api.execute(sessionId, `SELECT ${tree.currentSchema.replace(/^db/, "")}`, 10).catch(() => {});
			}
		}
	}

	async confirm(values: ActionValues) {
		const req = this.request;
		if (!req || this.running) return;
		const { statements } = req.build(values);
		if (statements.length === 0) return;
		this.running = true;
		this.error = null;
		const error = await this.execute(req.profile, statements, req.redisDb);
		this.running = false;
		await req.after?.();
		if (error) {
			this.error = error;
			return;
		}
		toast.success(req.success(values));
		this.request = null;
	}

	/** The dialog's statements in a new tab, to edit or run by hand. */
	toEditor(values: ActionValues) {
		const req = this.request;
		if (!req) return;
		const { statements } = req.build(values);
		const prefix = req.redisDb && req.profile.engine === "redis" ? [`SELECT ${req.redisDb.replace(/^db/, "")}`] : [];
		workspace.openScript(req.profile.id, req.title.replace(/\?$/, ""), ddl.toScript([...prefix, ...statements], req.profile.engine));
		this.request = null;
	}

	// ---- connection ------------------------------------------------------

	newQuery(p: ConnectionProfile) {
		workspace.newTab(p.id);
	}

	async disconnect(p: ConnectionProfile) {
		await explorer.close(p.id);
	}

	duplicateProfile(p: ConnectionProfile) {
		workspace.editing = { ...$state.snapshot(p), id: "", name: `${p.name} (copy)` };
	}

	editProfile(p: ConnectionProfile) {
		workspace.editing = $state.snapshot(p);
	}

	async copy(text: string, what: string) {
		try {
			await navigator.clipboard.writeText(text);
			toast.success(`${what} copied`);
		} catch {
			toast.error("Could not copy to the clipboard.");
		}
	}

	copyConnection(p: ConnectionProfile) {
		return this.copy(`${p.name} (${engineInfo(p.engine).label}): ${connectionSummary(p)}`, "Connection details");
	}

	async revealFile(p: ConnectionProfile) {
		try {
			await revealItemInDir(p.database);
		} catch (e) {
			toast.error(`Could not show the file: ${e instanceof Error ? e.message : String(e)}`);
		}
	}

	showSessions(p: ConnectionProfile) {
		const sql = ddl.sessionsSql(p.engine);
		if (sql) workspace.openQuery(p.id, sql);
	}

	createSchema(p: ConnectionProfile, database = false) {
		const e = p.engine;
		if (e === "oracle") {
			workspace.openScript(p.id, "New schema", ddl.oracleUserTemplate());
			return;
		}
		const word = database ? "database" : ddl.schemaWord(e);
		this.ask({
			title: `Create ${word}`,
			profile: p,
			confirmLabel: "Create",
			input: { label: "Name", value: "", placeholder: `new_${word}` },
			build: ({ input }) => {
				const name = ddl.foldName(input, e);
				if (!name) return plan([], [`Type a name for the ${word}.`]);
				return plan(database ? ddl.createDatabaseSql(name, e) : ddl.createSchemaSql(name, e));
			},
			success: ({ input }) => `${cap(word)} ${ddl.foldName(input, e)} created`,
			// Other databases on the server are not in the tree.
			after: database ? undefined : () => explorer.refresh(p),
		});
	}

	// ---- schema ----------------------------------------------------------

	async newQueryIn(p: ConnectionProfile, s: SchemaNode) {
		const sql = ddl.useSchemaSql(s.name, p.engine);
		const tab = workspace.newTab(p.id);
		tab.title = s.name;
		if (sql) await workspace.openQuery(p.id, sql);
	}

	createTable(p: ConnectionProfile, s: SchemaNode) {
		this.designer = { profile: p, schema: s.name, table: null, columns: [] };
	}

	template(p: ConnectionProfile, s: SchemaNode, kind: "view" | "function" | "procedure" | "sequence") {
		workspace.openScript(p.id, `New ${kind}`, ddl.objectTemplate(kind, s.name, p.engine));
	}

	renameSchema(p: ConnectionProfile, s: SchemaNode) {
		this.ask({
			title: `Rename schema ${s.name}`,
			profile: p,
			confirmLabel: "Rename",
			input: { label: "New name", value: s.name },
			build: ({ input }) => {
				const name = ddl.foldName(input, p.engine);
				return !name || name === s.name ? plan([], ["Type a new name."]) : plan(ddl.renameSchemaSql(s.name, name, p.engine));
			},
			success: ({ input }) => `Schema renamed to ${ddl.foldName(input, p.engine)}`,
			after: () => explorer.refresh(p),
		});
	}

	dropSchema(p: ConnectionProfile, s: SchemaNode) {
		const e = p.engine;
		if (e === "redis") {
			this.ask({
				title: `Delete every key in ${s.name}?`,
				description: "FLUSHDB removes all keys of this database. It cannot be undone.",
				profile: p,
				confirmLabel: "Flush database",
				destructive: true,
				typeToConfirm: s.name,
				redisDb: s.name,
				build: () => plan(["FLUSHDB"]),
				success: () => `${s.name} flushed`,
				after: () => explorer.refreshSchema(p.id, s.name),
			});
			return;
		}
		const word = ddl.schemaWord(e);
		const cascade = e === "postgres" || e === "oracle";
		this.ask({
			title: `Drop ${word} ${s.name}?`,
			description:
				e === "oracle"
					? "Drops the user and, with CASCADE, every object it owns. It cannot be undone."
					: e === "sqlite"
						? "Detaches the database from this connection. The file is not deleted."
						: `Everything in the ${word} is lost. It cannot be undone.`,
			profile: p,
			confirmLabel: e === "sqlite" ? "Detach" : "Drop",
			destructive: true,
			typeToConfirm: e === "sqlite" ? undefined : s.name,
			options: cascade ? [{ id: "cascade", label: e === "oracle" ? "CASCADE: drop the objects it owns" : "CASCADE: drop the objects in it", checked: false }] : undefined,
			build: ({ options }) => plan(ddl.dropSchemaSql(s.name, e, options.cascade)),
			success: () => `${cap(word)} ${s.name} dropped`,
			after: () => explorer.refresh(p),
		});
	}

	// ---- objects ---------------------------------------------------------

	/** First rows of a table or view, or a Redis key's value. */
	preview(p: ConnectionProfile, s: SchemaNode, o: ObjectNode) {
		if (p.engine === "redis") {
			// A key in another database needs SELECT first; the tab session
			// starts in the profile's database.
			const command = redisPreview(o.name, o.kind);
			const db = `db${p.database.trim() || "0"}`;
			workspace.openQuery(p.id, s.name === db ? command : `SELECT ${s.name.replace(/^db/, "")}
${command}`);
		} else {
			workspace.openQuery(p.id, previewQuery(s.name, o.name, p.engine));
		}
	}

	async showDdl(p: ConnectionProfile, s: SchemaNode, o: ObjectNode) {
		const sessionId = await explorer.sessionFor(p);
		if (!sessionId) return;
		try {
			const text = await api.objectDdl(sessionId, s.name, o.name, o.kind);
			workspace.openScript(p.id, `DDL ${o.name.replace(/\(.*\)$/, "")}`, text.trimEnd() + "\n");
		} catch (e) {
			toast.error(errorText(asAppError(e)));
		}
	}

	countRows(p: ConnectionProfile, s: SchemaNode, o: ObjectNode) {
		workspace.openQuery(p.id, ddl.countSql(s.name, o.name, p.engine));
	}

	async generate(p: ConnectionProfile, s: SchemaNode, o: ObjectNode, what: "select" | "insert" | "update" | "delete") {
		let cols: DbColumn[];
		try {
			cols = await explorer.columns(p, s, o);
		} catch (e) {
			toast.error(errorText(asAppError(e)));
			return;
		}
		const names = cols.map((c) => c.name);
		const key = cols.filter((c) => c.primaryKey).map((c) => c.name);
		const e = p.engine;
		const sql = {
			select: () => ddl.selectSql(s.name, o.name, names, e),
			insert: () => ddl.insertSql(s.name, o.name, names, e),
			update: () => ddl.updateSql(s.name, o.name, names, key, e),
			delete: () => ddl.deleteSql(s.name, o.name, names, key, e),
		}[what]();
		workspace.openScript(p.id, `${what.toUpperCase()} ${o.name}`, ddl.toScript([sql], e));
	}

	async createIndex(p: ConnectionProfile, s: SchemaNode, o: ObjectNode) {
		const cols = await explorer.columns(p, s, o).catch(() => []);
		const sql = ddl.createIndexSql(s.name, o.name, cols[0]?.name ?? "column_name", p.engine);
		workspace.openScript(p.id, `Index on ${o.name}`, ddl.toScript([sql], p.engine));
	}

	async editTable(p: ConnectionProfile, s: SchemaNode, o: ObjectNode) {
		try {
			// Fresh columns: the designer compares against what the table has now.
			o.columns = null;
			const columns = await explorer.columns(p, s, o);
			this.designer = { profile: p, schema: s.name, table: o.name, columns };
		} catch (e) {
			toast.error(errorText(asAppError(e)));
		}
	}

	renameTable(p: ConnectionProfile, s: SchemaNode, o: ObjectNode) {
		this.ask({
			title: `Rename table ${o.name}`,
			profile: p,
			confirmLabel: "Rename",
			input: { label: "New name", value: o.name },
			build: ({ input }) => {
				const name = ddl.foldName(input, p.engine);
				return !name || name === o.name ? plan([], ["Type a new name."]) : plan(ddl.renameTableSql(s.name, o.name, name, p.engine));
			},
			success: ({ input }) => `Table renamed to ${ddl.foldName(input, p.engine)}`,
			after: () => explorer.refreshSchema(p.id, s.name),
		});
	}

	duplicateTable(p: ConnectionProfile, s: SchemaNode, o: ObjectNode) {
		this.ask({
			title: `Duplicate table ${o.name}`,
			profile: p,
			confirmLabel: "Duplicate",
			input: { label: "Name of the copy", value: ddl.foldName(`${o.name}_copy`, p.engine) },
			options: [{ id: "data", label: "Copy the rows too", checked: true }],
			build: ({ input, options }) => {
				const name = ddl.foldName(input, p.engine);
				if (!name || name === o.name) return plan([], ["Type a name for the copy."]);
				return ddl.duplicateTableSql(s.name, o.name, name, options.data, p.engine);
			},
			success: ({ input }) => `Table ${ddl.foldName(input, p.engine)} created`,
			after: () => explorer.refreshSchema(p.id, s.name),
		});
	}

	truncate(p: ConnectionProfile, s: SchemaNode, o: ObjectNode) {
		this.ask({
			title: `Empty table ${o.name}?`,
			description:
				p.engine === "sqlite"
					? "Deletes every row of the table."
					: "TRUNCATE removes every row at once. Most databases cannot roll it back.",
			profile: p,
			confirmLabel: "Empty table",
			destructive: true,
			build: () => plan(ddl.truncateSql(s.name, o.name, p.engine)),
			success: () => `Table ${o.name} emptied`,
		});
	}

	drop(p: ConnectionProfile, s: SchemaNode, o: ObjectNode) {
		const kind = kindName(o.kind);
		const cascade = ddl.dropCascades(o.kind, p.engine);
		this.ask({
			title: `Drop ${kind} ${o.name}?`,
			description: o.kind === "table" ? "The table and its rows are lost. It cannot be undone." : "It cannot be undone.",
			profile: p,
			confirmLabel: "Drop",
			destructive: true,
			typeToConfirm: o.kind === "table" ? o.name : undefined,
			options: cascade
				? [{ id: "cascade", label: p.engine === "oracle" && o.kind === "table" ? "CASCADE CONSTRAINTS: drop foreign keys that point to it" : "CASCADE: drop the objects that depend on it", checked: false }]
				: undefined,
			build: ({ options }) => plan(ddl.dropObjectSql(s.name, o.name, o.kind, p.engine, options.cascade)),
			success: () => `${cap(kind)} ${o.name} dropped`,
			after: () => explorer.refreshSchema(p.id, s.name),
		});
	}

	refreshMatview(p: ConnectionProfile, s: SchemaNode, o: ObjectNode) {
		this.ask({
			title: `Refresh ${o.name}`,
			description: "Runs the view's query again and replaces its rows.",
			profile: p,
			confirmLabel: "Refresh",
			build: () => plan(ddl.refreshMatviewSql(s.name, o.name, p.engine)),
			success: () => `${o.name} refreshed`,
		});
	}

	copyName(p: ConnectionProfile, s: SchemaNode | null, name: string, full: boolean) {
		const bare = name.replace(/\(.*\)$/, "");
		const text = p.engine === "redis" ? name : full && s ? qualified(s.name, bare, p.engine) : quoteIdent(bare, p.engine);
		return this.copy(text, "Name");
	}

	async copyColumns(p: ConnectionProfile, s: SchemaNode, o: ObjectNode) {
		try {
			const cols = await explorer.columns(p, s, o);
			await this.copy(cols.map((c) => quoteIdent(c.name, p.engine)).join(", "), "Column list");
		} catch (e) {
			toast.error(errorText(asAppError(e)));
		}
	}

	// ---- Redis keys --------------------------------------------------------

	redisKey(p: ConnectionProfile, s: SchemaNode, o: ObjectNode, action: "delete" | "rename" | "expire" | "persist") {
		const base = { profile: p, redisDb: s.name, after: () => explorer.refreshSchema(p.id, s.name) };
		switch (action) {
			case "delete":
				this.ask({
					...base,
					title: `Delete key ${o.name}?`,
					confirmLabel: "Delete",
					destructive: true,
					build: () => plan([ddl.redisKeySql("delete", o.name)]),
					success: () => `Key ${o.name} deleted`,
				});
				break;
			case "rename":
				this.ask({
					...base,
					title: `Rename key ${o.name}`,
					confirmLabel: "Rename",
					input: { label: "New name", value: o.name },
					build: ({ input }) => (!input || input === o.name ? plan([], ["Type a new name."]) : plan([ddl.redisKeySql("rename", o.name, input)])),
					success: ({ input }) => `Key renamed to ${input}`,
				});
				break;
			case "expire":
				this.ask({
					...base,
					title: `Expire key ${o.name}`,
					description: "The key is deleted after this many seconds.",
					confirmLabel: "Set expiry",
					input: { label: "Seconds", value: "3600" },
					build: ({ input }) =>
						/^\d+$/.test(input.trim()) ? plan([ddl.redisKeySql("expire", o.name, input.trim())]) : plan([], ["Type a whole number of seconds."]),
					success: ({ input }) => `Key ${o.name} expires in ${input.trim()} s`,
				});
				break;
			case "persist":
				this.ask({
					...base,
					title: `Keep key ${o.name}`,
					description: "Removes the key's expiry.",
					confirmLabel: "Remove expiry",
					build: () => plan([ddl.redisKeySql("persist", o.name)]),
					success: () => `Key ${o.name} no longer expires`,
				});
				break;
		}
	}

	redisTtl(p: ConnectionProfile, s: SchemaNode, o: ObjectNode) {
		const db = explorer.tree(p.id)?.currentSchema;
		const cmd = `TTL ${redisArg(o.name)}`;
		workspace.openQuery(p.id, s.name === db ? cmd : `SELECT ${s.name.replace(/^db/, "")}\n${cmd}`);
	}
}

const cap = (s: string) => s.charAt(0).toUpperCase() + s.slice(1);

export const actions = new Actions();
