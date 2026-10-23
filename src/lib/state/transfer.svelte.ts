import { toast } from "@codeatlasdev/ui";
import { listen } from "@tauri-apps/api/event";
import { open } from "@tauri-apps/plugin-dialog";
import {
	api,
	asAppError,
	errorText,
	type ConnectionProfile,
	type DbObject,
	type ExportContent,
	type ExportFormat,
	type ImportOptions,
	type ImportPreview,
	type TransferProgress,
} from "../api";
import { engineInfo } from "../engines";
import { createTableSql, foldName, typeFor, useSchemaSql } from "../sql/ddl";
import { splitStatements } from "../sql/split";
import { actions } from "./actions.svelte";
import { explorer } from "./explorer.svelte";
import { updates } from "./updates.svelte";
import { workspace } from "./workspace.svelte";

/** What the export dialog exports: one object, or the whole schema. */
export interface ExportTarget {
	profile: ConnectionProfile;
	schema: string;
	object: DbObject | null;
	/** Tables in the schema, for the dialog's text. */
	tables: number;
}

export interface ImportTarget {
	profile: ConnectionProfile;
	schema: string;
	/** null: a new table named after the file. */
	table: string | null;
	/** Columns of the existing table. */
	columns: string[];
	path: string;
	preview: ImportPreview;
}

export interface RunFileTarget {
	profile: ConnectionProfile;
	/** Schema the statements run in, when started from a schema. */
	schema: string | null;
	path: string;
	statements: string[];
	bytes: number;
}

export type Phase = "ready" | "running" | "done";

export interface RunError {
	index: number;
	statement: string;
	message: string;
}

const PREFS_KEY = "lowquery.export.v1";

let seq = 0;
const jobId = () => `job${Date.now().toString(36)}${(seq++).toString(36)}`;
const fileName = (path: string) => path.split(/[\\/]/).pop() ?? path;

class Transfer {
	exporting = $state<ExportTarget | null>(null);
	importing = $state<ImportTarget | null>(null);
	runningFile = $state<RunFileTarget | null>(null);
	phase = $state<Phase>("ready");
	progress = $state<TransferProgress | null>(null);
	error = $state<string | null>(null);
	/** Final line of a finished job, and the files it wrote. */
	result = $state<string | null>(null);
	/** What the user should know about a finished job (not an error). */
	note = $state<string | null>(null);
	files = $state<string[]>([]);
	runErrors = $state<RunError[]>([]);
	private job: string | null = null;
	/** Session of a running SQL file, to cancel its current statement. */
	private fileSession: string | null = null;
	private stop = false;

	constructor() {
		listen<TransferProgress>("transfer-progress", (e) => {
			if (e.payload.jobId === this.job) this.progress = e.payload;
		}).catch(() => {
			// No event bridge in the browser preview.
		});
	}

	private reset() {
		this.phase = "ready";
		this.progress = null;
		this.error = null;
		this.result = null;
		this.note = null;
		this.files = [];
		this.runErrors = [];
		this.stop = false;
	}

	close() {
		if (this.phase === "running") return;
		this.exporting = null;
		this.importing = null;
		this.runningFile = null;
		this.reset();
	}

	async cancel() {
		this.stop = true;
		if (this.fileSession) await api.cancel(this.fileSession).catch(() => {});
		else if (this.job) await api.cancelTransfer(this.job).catch(() => {});
	}

	/** Runs `work` on a session of its own, so the tree and tabs stay free. */
	private async withSession<T>(profile: ConnectionProfile, work: (sessionId: string) => Promise<T>): Promise<T | null> {
		const session = await workspace.openSession(profile);
		if (!session) return null;
		try {
			return await work(session.sessionId);
		} finally {
			await api.closeSession(session.sessionId).catch(() => {});
		}
	}

	// ---- export ------------------------------------------------------------

	prefs(): { format: ExportFormat; content: ExportContent } {
		try {
			const saved = JSON.parse(localStorage.getItem(PREFS_KEY) ?? "null");
			if (saved?.format && saved?.content) return saved;
		} catch {
			// Defaults below.
		}
		return { format: "sql", content: "all" };
	}

	openExport(profile: ConnectionProfile, schema: string, object: DbObject | null) {
		this.reset();
		const node = explorer.tree(profile.id)?.schemas?.find((s) => s.name === schema);
		const tables = node?.objects?.filter((o) => o.kind === "table").length ?? 0;
		this.exporting = { profile, schema, object, tables };
	}

	async runExport(format: ExportFormat, content: ExportContent, path: string) {
		const t = this.exporting;
		if (!t) return;
		try {
			localStorage.setItem(PREFS_KEY, JSON.stringify({ format, content }));
		} catch {
			// Remembered only for this run.
		}
		this.reset();
		this.phase = "running";
		this.job = jobId();
		const job = this.job;
		const source = [
			`LowQuery ${updates.currentVersion ?? ""} export`.replace("  ", " "),
			`Source: ${t.profile.name} (${engineInfo(t.profile.engine).label}), ${t.schema}${t.object ? `.${t.object.name}` : ""}`,
			`Date: ${new Date().toISOString()}`,
		].join("\n");
		try {
			const summary = await this.withSession(t.profile, (sessionId) =>
				api.exportData({
					jobId: job,
					sessionId,
					schema: t.schema,
					objects: t.object ? [{ name: t.object.name, kind: t.object.kind }] : null,
					format,
					content,
					path,
					source,
				}),
			);
			if (!summary) {
				this.phase = "ready";
				return;
			}
			const rows = summary.rows.toLocaleString();
			this.result =
				content === "structure" && format === "sql"
					? `${summary.objects} object${summary.objects === 1 ? "" : "s"} exported.`
					: `${rows} row${summary.rows === 1 ? "" : "s"}${summary.objects > 1 ? ` from ${summary.objects} tables` : ""} exported.`;
			if (summary.warnings.length) this.error = `Not exported:\n${summary.warnings.join("\n")}`;
			this.files = summary.files;
			this.phase = "done";
		} catch (e) {
			const err = asAppError(e);
			this.error = err.kind === "cancelled" ? "Export cancelled. No file was written." : errorText(err);
			this.phase = "ready";
		} finally {
			this.job = null;
		}
	}

	// ---- import ------------------------------------------------------------

	/** Asks for a CSV or JSON file, then shows how it will be imported. */
	async chooseImport(profile: ConnectionProfile, schema: string, table: string | null) {
		const path = await open({
			title: table ? `Import into ${table}` : `Import a file as a new table in ${schema}`,
			multiple: false,
			filters: [{ name: "CSV or JSON", extensions: ["csv", "tsv", "txt", "json", "ndjson"] }],
		});
		if (typeof path === "string") await this.openImport(profile, schema, table, path);
	}

	/** Reads the file and opens the import dialog for a table (or a new one). */
	async openImport(profile: ConnectionProfile, schema: string, table: string | null, path: string) {
		this.reset();
		try {
			const preview = await api.dataImportPreview(path, { delimiter: null, header: null });
			let columns: string[] = [];
			if (table) {
				const sessionId = await explorer.sessionFor(profile);
				if (!sessionId) return;
				columns = (await api.listColumns(sessionId, schema, table)).map((c) => c.name);
			}
			this.importing = { profile, schema, table, columns, path, preview };
		} catch (e) {
			toast.error(`Could not read ${fileName(path)}: ${errorText(asAppError(e))}`);
		}
	}

	async reread(options: ImportOptions) {
		const t = this.importing;
		if (!t) return;
		try {
			t.preview = await api.dataImportPreview(t.path, options);
		} catch (e) {
			this.error = errorText(asAppError(e));
		}
	}

	/** File column names as the new table's column names. */
	newColumns(t: ImportTarget) {
		const e = t.profile.engine;
		return t.preview.columns.map((name, i) => ({
			name: foldName(name, e),
			type: typeFor(t.preview.inferred[i], t.preview.maxLengths[i], e),
		}));
	}

	async runImport(options: ImportOptions, replace: boolean, newTable: string) {
		const t = this.importing;
		if (!t) return;
		this.reset();
		this.phase = "running";
		const e = t.profile.engine;
		let table = t.table;
		if (!table) {
			table = foldName(newTable, e);
			const plan = createTableSql(
				t.schema,
				table,
				this.newColumns(t).map((c) => ({ ...c, nullable: true, defaultValue: "", primaryKey: false, identity: false })),
				e,
			);
			if (plan.statements.length === 0) {
				this.error = plan.warnings.join(" ");
				this.phase = "ready";
				return;
			}
			const err = await actions.execute(t.profile, plan.statements);
			if (err) {
				this.error = `The table could not be created: ${err}`;
				this.phase = "ready";
				return;
			}
			await explorer.refreshSchema(t.profile.id, t.schema);
			t.table = table;
		}
		this.job = jobId();
		const job = this.job;
		try {
			const summary = await this.withSession(t.profile, (sessionId) =>
				api.importData({ jobId: job, sessionId, path: t.path, options, schema: t.schema, table: table!, replace }),
			);
			if (!summary) {
				this.phase = "ready";
				return;
			}
			this.result = `${summary.rows.toLocaleString()} row${summary.rows === 1 ? "" : "s"} imported into ${table}.`;
			if (summary.skippedColumns.length) {
				this.note = `Columns not in the table were left out: ${summary.skippedColumns.join(", ")}.`;
			}
			this.phase = "done";
		} catch (err) {
			const ae = asAppError(err);
			this.error =
				ae.kind === "cancelled" ? "Import cancelled. The table was not changed." : `${errorText(ae)}\nThe table was not changed.`;
			this.phase = "ready";
		} finally {
			this.job = null;
		}
	}

	// ---- SQL files -----------------------------------------------------------

	async chooseSqlFile(profile: ConnectionProfile, schema: string | null) {
		const path = await open({
			title: "Run a SQL file",
			multiple: false,
			filters: [{ name: "SQL script", extensions: ["sql", "txt"] }],
		});
		if (typeof path === "string") await this.openRunFile(profile, schema, path);
	}

	async openRunFile(profile: ConnectionProfile, schema: string | null, path: string) {
		this.reset();
		try {
			const text = await api.readSqlFile(path);
			const statements = splitStatements(text, engineInfo(profile.engine).dialect).map((s) => s.text);
			if (statements.length === 0) {
				toast.warning(`${fileName(path)} has no statements.`);
				return;
			}
			this.runningFile = { profile, schema, path, statements, bytes: new Blob([text]).size };
		} catch (e) {
			toast.error(`Could not read ${fileName(path)}: ${errorText(asAppError(e))}`);
		}
	}

	async runFile(stopOnError: boolean) {
		const t = this.runningFile;
		if (!t) return;
		this.reset();
		this.phase = "running";
		const total = t.statements.length;
		let ok = 0;
		await this.withSession(t.profile, async (sessionId) => {
			this.job = jobId();
			this.fileSession = sessionId;
			const use = t.schema ? useSchemaSql(t.schema, t.profile.engine) : null;
			if (use) await api.execute(sessionId, use, 1).catch(() => {});
			for (let i = 0; i < total; i++) {
				if (this.stop) break;
				this.progress = { jobId: this.job!, object: fileName(t.path), step: i + 1, steps: total, rows: 0 };
				try {
					await api.execute(sessionId, t.statements[i], 1);
					ok++;
				} catch (e) {
					const err = asAppError(e);
					if (err.kind === "cancelled" || this.stop) break;
					this.runErrors.push({ index: i + 1, statement: t.statements[i], message: errorText(err) });
					if (stopOnError) break;
				}
			}
		});
		this.job = null;
		this.fileSession = null;
		const failed = this.runErrors.length;
		const stopped = this.stop || (stopOnError && failed > 0);
		this.result = `${ok.toLocaleString()} of ${total.toLocaleString()} statements ran${failed ? `, ${failed} failed` : ""}${stopped && ok + failed < total ? " (stopped)" : ""}.`;
		this.phase = "done";
		if (t.schema) await explorer.refreshSchema(t.profile.id, t.schema);
		else if (explorer.tree(t.profile.id)?.sessionId) await explorer.refresh(t.profile);
	}
}

export const transfer = new Transfer();
