import { toast } from "@codeatlasdev/ui";
import {
	api,
	asAppError,
	errorText,
	type AppError,
	type ConnectionProfile,
	type ExecResult,
	type SessionInfo,
} from "../api";
import { engineInfo } from "../engines";
import { firstKeywords, splitStatements, statementAt, type SqlStatement } from "../sql/split";
import { buildChanges, pendingCount, type EditMap } from "./edits";
import { canViewOnServer, describeView, emptyView, viewSql, type ResultView } from "../view";

export interface ResultSet {
	id: string;
	title: string;
	sql: string;
	result: ExecResult;
	/** Unsaved grid edits; only for results with `result.editable`. */
	edits: EditMap;
	/** Sort and filters shown in the grid. */
	view: ResultView;
	/**
	 * The view runs on the server: the query hit the row limit, so sorting or
	 * filtering the fetched rows would leave the others out. Otherwise the grid applies
	 * the view in memory.
	 */
	serverView: boolean;
	/** The SQL behind `result`: `sql`, or `sql` with the server view around it. */
	runSql: string;
}

export type LogLevel = "info" | "success" | "notice" | "error";

export interface LogEntry {
	id: number;
	at: Date;
	level: LogLevel;
	text: string;
	sql?: string;
	elapsedMs?: number;
}

/** Error positioned in the document, for the editor to highlight. */
export interface EditorError {
	from: number;
	to: number;
	message: string;
}

export type RunMode = "statement" | "script";

let seq = 0;
const uid = (p: string) => `${p}${Date.now().toString(36)}${(seq++).toString(36)}`;
let logSeq = 0;

export class QueryTab {
	id: string;
	title = $state("");
	profileId = $state<string | null>(null);
	session = $state<SessionInfo | null>(null);
	sql = $state("");
	connecting = $state(false);
	running = $state(false);
	cancelling = $state(false);
	saving = $state(false);
	autocommit = $state(true);
	inTransaction = $state(false);
	maxRows = $state(500);
	results = $state<ResultSet[]>([]);
	activeResult = $state("messages");
	log = $state<LogEntry[]>([]);
	editorError = $state<EditorError | null>(null);
	/** Cursor position and selected text, updated by the editor. */
	cursor = 0;
	selection = "";
	selectionFrom = 0;
	/** Stop request: the script halts before the next statement. */
	stopRequested = false;

	constructor(init: { id?: string; title: string; profileId: string | null; sql?: string }) {
		this.id = init.id ?? uid("t");
		this.title = init.title;
		this.profileId = init.profileId;
		this.sql = init.sql ?? "";
	}

	addLog(level: LogLevel, text: string, extra: Partial<LogEntry> = {}) {
		this.log.push({ id: logSeq++, at: new Date(), level, text, ...extra });
		if (this.log.length > 500) this.log.splice(0, this.log.length - 500);
	}

	/** Unsaved grid edits across all results of the tab. */
	get pendingEdits(): number {
		return this.results.reduce((n, r) => n + pendingCount(r.edits), 0);
	}

	clearResults() {
		this.results = [];
		this.activeResult = "messages";
	}
}

class PasswordPrompt {
	profile = $state<ConnectionProfile | null>(null);
	message = $state("");
	private resolver: ((pw: string | null) => void) | null = null;

	ask(profile: ConnectionProfile, message: string): Promise<string | null> {
		this.resolver?.(null);
		this.profile = profile;
		this.message = message;
		return new Promise((resolve) => (this.resolver = resolve));
	}

	answer(pw: string | null) {
		const r = this.resolver;
		this.resolver = null;
		this.profile = null;
		r?.(pw);
	}
}

export interface ConfirmRequest {
	title: string;
	message: string;
	confirmLabel: string;
	destructive?: boolean;
}

class ConfirmPrompt {
	request = $state<ConfirmRequest | null>(null);
	private resolver: ((ok: boolean) => void) | null = null;

	ask(request: ConfirmRequest): Promise<boolean> {
		this.resolver?.(false);
		this.request = request;
		return new Promise((resolve) => (this.resolver = resolve));
	}

	answer(ok: boolean) {
		const r = this.resolver;
		this.resolver = null;
		this.request = null;
		r?.(ok);
	}
}

const DRAFTS_KEY = "lowquery.tabs.v1";

interface Draft {
	id: string;
	title: string;
	profileId: string | null;
	sql: string;
	maxRows: number;
}

function describeCommand(sql: string, result: ExecResult): string {
	const kw = firstKeywords(sql, 3);
	const head = kw.slice(0, kw[0] === "CREATE" || kw[0] === "DROP" || kw[0] === "ALTER" ? 2 : 1).join(" ");
	const verb = head || "Statement";
	if (result.rowsAffected != null && ["INSERT", "UPDATE", "DELETE", "MERGE", "COPY", "UPSERT"].includes(kw[0] ?? "")) {
		const n = result.rowsAffected;
		return `${verb}: ${n.toLocaleString()} ${n === 1 ? "row affected" : "rows affected"}`;
	}
	return `${verb} executed`;
}

export class Workspace {
	profiles = $state<ConnectionProfile[]>([]);
	tabs = $state<QueryTab[]>([]);
	activeTabId = $state<string | null>(null);
	password = new PasswordPrompt();
	confirm = new ConfirmPrompt();
	/** Profile being edited in the dialog (null = closed). */
	editing = $state<ConnectionProfile | null>(null);

	get activeTab(): QueryTab | null {
		return this.tabs.find((t) => t.id === this.activeTabId) ?? null;
	}

	profile(id: string | null): ConnectionProfile | null {
		return this.profiles.find((p) => p.id === id) ?? null;
	}

	async init() {
		await this.loadProfiles();
		this.restoreDrafts();
		if (this.tabs.length === 0) this.newTab(this.profiles[0]?.id ?? null);
	}

	async loadProfiles() {
		try {
			this.profiles = await api.listProfiles();
		} catch (e) {
			toast.error(errorText(asAppError(e)));
		}
	}

	async saveProfile(profile: ConnectionProfile, password: string | null): Promise<ConnectionProfile> {
		const saved = await api.saveProfile(profile, password);
		await this.loadProfiles();
		return saved;
	}

	async deleteProfile(id: string) {
		await api.deleteProfile(id);
		for (const tab of this.tabs) {
			if (tab.profileId === id) await this.disconnect(tab);
		}
		await this.loadProfiles();
	}

	// ---- tabs -------------------------------------------------------------

	newTab(profileId: string | null = null, sql = ""): QueryTab {
		const n = this.tabs.length + 1;
		const tab = new QueryTab({ title: `Query ${n}`, profileId, sql });
		this.tabs.push(tab);
		this.activeTabId = tab.id;
		this.saveDrafts();
		return tab;
	}

	/** A new tab with generated SQL (DDL, templates), not run. */
	openScript(profileId: string, title: string, sql: string): QueryTab {
		const tab = this.newTab(profileId, sql);
		tab.title = title;
		this.saveDrafts();
		return tab;
	}

	async closeTab(id: string) {
		const idx = this.tabs.findIndex((t) => t.id === id);
		if (idx < 0) return;
		const tab = this.tabs[idx];
		if (!(await this.confirmDiscard(tab))) return;
		await this.disconnect(tab);
		this.tabs.splice(idx, 1);
		if (this.activeTabId === id) this.activeTabId = this.tabs[Math.min(idx, this.tabs.length - 1)]?.id ?? null;
		if (this.tabs.length === 0) this.newTab(tab.profileId);
		this.saveDrafts();
	}

	saveDrafts() {
		const drafts: Draft[] = this.tabs.map((t) => ({
			id: t.id,
			title: t.title,
			profileId: t.profileId,
			sql: t.sql,
			maxRows: t.maxRows,
		}));
		try {
			localStorage.setItem(DRAFTS_KEY, JSON.stringify({ drafts, active: this.activeTabId }));
		} catch {
			// Storage unavailable: drafts live only for this run.
		}
	}

	private restoreDrafts() {
		try {
			const raw = localStorage.getItem(DRAFTS_KEY);
			if (!raw) return;
			const data = JSON.parse(raw) as { drafts: Draft[]; active: string | null };
			for (const d of data.drafts ?? []) {
				const profileId = this.profile(d.profileId) ? d.profileId : null;
				const tab = new QueryTab({ id: d.id, title: d.title, profileId, sql: d.sql });
				tab.maxRows = d.maxRows ?? 500;
				this.tabs.push(tab);
			}
			this.activeTabId = this.tabs.find((t) => t.id === data.active)?.id ?? this.tabs[0]?.id ?? null;
		} catch {
			// Corrupt drafts are ignored.
		}
	}

	/**
	 * Opens a statement generated by the UI (table preview): in the active
	 * tab if it uses the same connection, otherwise in a new tab.
	 */
	async openQuery(profileId: string, sql: string, run = true) {
		let tab = this.activeTab;
		if (!tab || tab.profileId !== profileId || tab.running) {
			tab = tab && !tab.sql.trim() && !tab.running && !tab.session ? tab : this.newTab(profileId);
			tab.profileId = profileId;
		}
		const prefix = tab.sql.trim() ? `${tab.sql.replace(/\s+$/, "")}\n\n` : "";
		tab.sql = prefix + sql;
		tab.cursor = prefix.length;
		// Several commands (Redis SELECT + preview) run as a selection, so only
		// the appended text runs, not the whole document.
		tab.selection = sql.includes("\n") ? sql : "";
		tab.selectionFrom = prefix.length;
		this.activeTabId = tab.id;
		this.saveDrafts();
		if (run) await this.run(tab, "statement");
	}

	// ---- sessions ---------------------------------------------------------

	/** Opens a session, asking for the password when needed. */
	async openSession(profile: ConnectionProfile): Promise<SessionInfo | null> {
		let password: string | null = null;
		for (;;) {
			try {
				return await api.openSession(profile.id, password);
			} catch (e) {
				const err = asAppError(e);
				if (err.kind !== "password_required") throw err;
				const message =
					password !== null ? "Password rejected by the server. Try again." : err.message;
				password = await this.password.ask(profile, message);
				if (password === null) return null;
			}
		}
	}

	async setConnection(tab: QueryTab, profileId: string | null) {
		if (tab.profileId === profileId && tab.session) return;
		if (!(await this.confirmDiscard(tab))) return;
		if (tab.session && tab.inTransaction) {
			tab.addLog("notice", "The pending transaction was rolled back when switching connections.");
		}
		await this.disconnect(tab);
		tab.profileId = profileId;
		this.saveDrafts();
		if (profileId) await this.connect(tab);
	}

	async connect(tab: QueryTab): Promise<boolean> {
		if (tab.session) return true;
		const profile = this.profile(tab.profileId);
		if (!profile) {
			toast.warning("Choose a connection for this tab.");
			return false;
		}
		tab.connecting = true;
		try {
			const session = await this.openSession(profile);
			if (!session) return false;
			tab.session = session;
			tab.inTransaction = false;
			if (!tab.autocommit) await api.setAutocommit(session.sessionId, false);
			tab.addLog("info", `Connected to ${profile.name} · ${session.serverVersion}`);
			return true;
		} catch (e) {
			const err = asAppError(e);
			tab.addLog("error", errorText(err));
			toast.error(`Could not connect: ${err.message}`);
			return false;
		} finally {
			tab.connecting = false;
		}
	}

	async disconnect(tab: QueryTab) {
		const s = tab.session;
		tab.session = null;
		tab.inTransaction = false;
		if (s) await api.closeSession(s.sessionId).catch(() => {});
	}

	// ---- execution -------------------------------------------------------

	/**
	 * Runs the selection, the statement under the cursor (`statement`) or the
	 * whole script (`script`). Each statement is a separate call: progress
	 * shows up as it runs and a stop request halts between statements.
	 */
	async run(tab: QueryTab, mode: RunMode) {
		if (tab.running) return;
		const profile = this.profile(tab.profileId);
		if (!profile) {
			toast.warning("Choose a connection for this tab.");
			return;
		}
		const dialect = engineInfo(profile.engine).dialect;
		let statements: SqlStatement[];
		if (tab.selection.trim()) {
			statements = splitStatements(tab.selection, dialect).map((s) => ({
				...s,
				start: s.start + tab.selectionFrom,
				end: s.end + tab.selectionFrom,
				until: s.until + tab.selectionFrom,
			}));
		} else if (mode === "script") {
			statements = splitStatements(tab.sql, dialect);
		} else {
			const current = statementAt(splitStatements(tab.sql, dialect, { blankLineDelimiter: true }), tab.cursor);
			statements = current ? [current] : [];
		}
		if (statements.length === 0) {
			toast.info("Nothing to run.");
			return;
		}
		if (!(await this.confirmDiscard(tab))) return;
		if (!(await this.connect(tab)) || !tab.session) return;

		tab.running = true;
		tab.cancelling = false;
		tab.editorError = null;
		tab.clearResults();
		const sessionId = tab.session.sessionId;
		let ok = 0;
		const started = performance.now();
		try {
			for (const [index, stmt] of statements.entries()) {
				if (tab.stopRequested) break;
				try {
					const result = await api.execute(sessionId, stmt.text, tab.maxRows);
					ok++;
					tab.inTransaction = result.inTransaction;
					for (const n of result.notices) tab.addLog("notice", n, { sql: stmt.text });
					if (result.columns) {
						const id = uid("r");
						const count = result.rows.length;
						tab.results.push({
							id,
							title: statements.length > 1 ? `Result ${tab.results.length + 1}` : "Result",
							sql: stmt.text,
							result,
							edits: {},
							view: emptyView(),
							serverView: result.truncated && canViewOnServer(stmt.text, dialect),
							runSql: stmt.text,
						});
						if (tab.results.length === 1) tab.activeResult = id;
						tab.addLog(
							"success",
							`${count.toLocaleString()} ${count === 1 ? "row" : "rows"}${result.truncated ? " (limit reached)" : ""}`,
							{ sql: stmt.text, elapsedMs: result.elapsedMs },
						);
					} else {
						tab.addLog("success", describeCommand(stmt.text, result), {
							sql: stmt.text,
							elapsedMs: result.elapsedMs,
						});
					}
				} catch (e) {
					const err = asAppError(e);
					this.reportError(tab, err, stmt, index, statements.length);
					if (err.kind === "connection" || err.kind === "not_found") {
						tab.session = null;
						tab.inTransaction = false;
					}
					break;
				}
			}
		} finally {
			tab.running = false;
			tab.cancelling = false;
			tab.stopRequested = false;
		}
		if (statements.length > 1) {
			const ms = Math.round(performance.now() - started);
			tab.addLog("info", `${ok} of ${statements.length} statements executed in ${ms} ms`);
		}
		if (tab.results.length === 0) tab.activeResult = "messages";
	}

	private reportError(tab: QueryTab, err: AppError, stmt: SqlStatement, index: number, total: number) {
		const where = total > 1 ? ` (statement ${index + 1} of ${total})` : "";
		let text = errorText(err) + where;
		if (err.detail) text += `\n${err.detail}`;
		if (err.hint) text += `\nHint: ${err.hint}`;
		tab.addLog(err.kind === "cancelled" ? "notice" : "error", text, { sql: stmt.text });
		tab.activeResult = "messages";
		// With a position, highlight the word the server points at; otherwise the whole statement.
		const pos = err.position != null ? stmt.start + err.position : stmt.start;
		let to = stmt.end;
		if (err.position != null) {
			to = pos + 1;
			while (to < stmt.end && /[\w$#".]/.test(tab.sql[to] ?? "")) to++;
		}
		tab.editorError = { from: pos, to: Math.max(to, pos + 1), message: err.message };
	}

	/** Asks before an action that would drop unsaved grid edits. */
	async confirmDiscard(tab: QueryTab): Promise<boolean> {
		const n = tab.pendingEdits;
		if (n === 0) return true;
		return this.confirm.ask({
			title: "Discard unsaved changes?",
			message: `${n} ${n === 1 ? "row has" : "rows have"} unsaved changes in the result grid. Save them first to keep them.`,
			confirmLabel: "Discard changes",
			destructive: true,
		});
	}

	/**
	 * Writes the pending grid edits of a result: one transaction with
	 * autocommit on, the open transaction otherwise. On success the query
	 * runs again so the grid shows what the database now holds.
	 */
	async saveEdits(tab: QueryTab, rs: ResultSet): Promise<boolean> {
		const target = rs.result.editable;
		if (!target || tab.saving || tab.running) return false;
		const changes = buildChanges(target, rs.result.rows, rs.edits);
		if (changes.length === 0) return true;
		if (!tab.session) {
			toast.warning("The connection was closed. Run the query again to edit its rows.");
			return false;
		}
		const table = target.schema ? `${target.schema}.${target.table}` : target.table;
		tab.saving = true;
		try {
			const r = await api.applyChanges(tab.session.sessionId, target, changes);
			tab.inTransaction = r.inTransaction;
			rs.edits = {};
			const parts = [
				r.updated && `${r.updated} updated`,
				r.inserted && `${r.inserted} inserted`,
				r.deleted && `${r.deleted} deleted`,
			].filter(Boolean);
			tab.addLog("success", `Saved to ${table}: ${parts.join(", ") || "no changes"}`, { elapsedMs: r.elapsedMs });
			if (r.inTransaction) {
				tab.addLog("notice", "The changes are in the open transaction: COMMIT to keep them or ROLLBACK to undo.");
			}
			toast.success(r.inTransaction ? "Saved in the open transaction" : "Changes saved");
			await this.refreshResult(tab, rs);
			return true;
		} catch (e) {
			const err = asAppError(e);
			let text = errorText(err);
			if (err.detail) text += `\n${err.detail}`;
			tab.addLog("error", `Changes to ${table} were not saved. ${text}`);
			toast.error(err.message);
			if (err.kind === "connection" || err.kind === "not_found") {
				tab.session = null;
				tab.inTransaction = false;
			}
			return false;
		} finally {
			tab.saving = false;
		}
	}

	discardEdits(rs: ResultSet) {
		rs.edits = {};
	}

	/** Runs a result's query again and replaces its rows in place. */
	private async refreshResult(tab: QueryTab, rs: ResultSet) {
		if (!tab.session) return;
		try {
			const result = await api.execute(tab.session.sessionId, rs.runSql, tab.maxRows);
			tab.inTransaction = result.inTransaction;
			if (result.columns) rs.result = keepEditTarget(rs.result, result);
		} catch (e) {
			tab.addLog("notice", `The result could not be refreshed: ${asAppError(e).message}`, { sql: rs.runSql });
		}
	}

	/**
	 * Sorts and filters a result. In memory when every row is loaded;
	 * otherwise the query runs again with the view around it. If the database
	 * rejects that query, the grid works on the fetched rows and the log says why.
	 */
	async setView(tab: QueryTab, rs: ResultSet, view: ResultView) {
		if (!rs.serverView) {
			rs.view = view;
			return;
		}
		if (tab.running || tab.saving) return;
		const columns = rs.result.columns ?? [];
		const local = (reason: string, sql?: string) => {
			rs.serverView = false;
			rs.view = view;
			const n = rs.result.rows.length.toLocaleString();
			tab.addLog("notice", `${reason.replace(/\.?$/, ".")} The grid sorts and filters the ${n} fetched rows only.`, { sql });
			toast.warning(`Sorting and filtering cover the ${n} fetched rows only. See Messages.`);
		};
		const profile = this.profile(tab.profileId);
		if (!tab.session || !profile) {
			local("The connection is closed, so the query could not run again with the sort and filters.");
			return;
		}
		const n = pendingCount(rs.edits);
		if (
			n > 0 &&
			!(await this.confirm.ask({
				title: "Discard unsaved changes?",
				message: `Sorting or filtering runs the query again. ${n} ${n === 1 ? "row has" : "rows have"} unsaved changes in this result.`,
				confirmLabel: "Discard changes",
				destructive: true,
			}))
		)
			return;

		const dialect = engineInfo(profile.engine).dialect;
		const sql = viewSql(rs.sql, view, columns, dialect);
		const sessionId = tab.session.sessionId;
		// A failed statement aborts an open PostgreSQL transaction; a savepoint keeps it usable.
		const savepoint = dialect === "postgres" && tab.inTransaction;
		tab.running = true;
		try {
			if (savepoint) await api.execute(sessionId, "SAVEPOINT lq_view", 0);
			let result: ExecResult;
			try {
				result = await api.execute(sessionId, sql, tab.maxRows);
			} catch (e) {
				if (savepoint) await api.execute(sessionId, "ROLLBACK TO SAVEPOINT lq_view", 0).catch(() => {});
				throw e;
			}
			if (savepoint) result.inTransaction = (await api.execute(sessionId, "RELEASE SAVEPOINT lq_view", 0)).inTransaction;
			tab.inTransaction = result.inTransaction;
			if (!result.columns) throw new Error("The query returned no rows.");
			rs.result = keepEditTarget(rs.result, result);
			rs.edits = {};
			rs.view = view;
			rs.runSql = sql;
			const count = result.rows.length;
			tab.addLog(
				"success",
				`Ran on the server (${describeView(columns, view)}): ${count.toLocaleString()} ${count === 1 ? "row" : "rows"}${result.truncated ? " (limit reached)" : ""}`,
				{ sql, elapsedMs: result.elapsedMs },
			);
		} catch (e) {
			const err = asAppError(e);
			if (err.kind === "connection" || err.kind === "not_found") {
				tab.session = null;
				tab.inTransaction = false;
			}
			if (err.kind === "cancelled") tab.addLog("notice", "Sorting and filtering cancelled.", { sql });
			else local(`The database could not run the query sorted and filtered: ${err.message}`, sql);
		} finally {
			tab.running = false;
			tab.cancelling = false;
			tab.stopRequested = false;
		}
	}

	async cancel(tab: QueryTab) {
		if (!tab.session || !tab.running) return;
		tab.stopRequested = true;
		tab.cancelling = true;
		try {
			await api.cancel(tab.session.sessionId);
		} catch (e) {
			const err = asAppError(e);
			tab.addLog("notice", `${err.message} Execution stops after the current statement.`);
		}
	}

	async setAutocommit(tab: QueryTab, on: boolean) {
		if (!tab.session) {
			tab.autocommit = on;
			return;
		}
		try {
			const st = await api.setAutocommit(tab.session.sessionId, on);
			if (on && tab.inTransaction) tab.addLog("info", "Transaction committed when autocommit was turned on.");
			tab.autocommit = st.autocommit;
			tab.inTransaction = st.inTransaction;
		} catch (e) {
			toast.error(errorText(asAppError(e)));
		}
	}

	async endTransaction(tab: QueryTab, commit: boolean) {
		if (!tab.session) return;
		try {
			const st = commit ? await api.commit(tab.session.sessionId) : await api.rollback(tab.session.sessionId);
			tab.inTransaction = st.inTransaction;
			tab.addLog("success", commit ? "Transaction committed (COMMIT)" : "Transaction rolled back (ROLLBACK)");
		} catch (e) {
			const err = asAppError(e);
			tab.addLog("error", errorText(err));
			toast.error(err.message);
		}
	}
}

export const workspace = new Workspace();

/**
 * Editing depends on the statement, not on how it is sorted: the view keeps
 * the columns of the first run, so its edit target still applies even when
 * the database cannot trace the wrapped query back to the table.
 */
function keepEditTarget(previous: ExecResult, next: ExecResult): ExecResult {
	const a = previous.columns ?? [];
	const b = next.columns ?? [];
	const same = a.length === b.length && a.every((c, i) => c.name === b[i].name && c.typeName === b[i].typeName);
	if (same) {
		next.editable = previous.editable;
		next.readOnlyReason = previous.readOnlyReason;
	}
	return next;
}
