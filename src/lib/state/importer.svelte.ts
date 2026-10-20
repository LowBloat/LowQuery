import { toast } from "@codeatlasdev/ui";
import { open } from "@tauri-apps/plugin-dialog";
import {
	api,
	asAppError,
	errorText,
	type ConnectionProfile,
	type ImportCandidate,
	type ImportPreview,
	type ImportSource,
	type ImportTool,
} from "../api";
import { workspace } from "./workspace.svelte";

export interface ToolInfo {
	id: ImportTool;
	name: string;
	/** What is read, shown under the name. */
	reads: string;
	filters: { name: string; extensions: string[] }[];
}

export const TOOLS: ToolInfo[] = [
	{
		id: "dbeaver",
		name: "DBeaver",
		reads: "data-sources.json, with user names and saved passwords",
		filters: [{ name: "DBeaver data sources", extensions: ["json"] }],
	},
	{
		id: "datagrip",
		name: "DataGrip and JetBrains IDEs",
		reads: "dataSources.xml",
		filters: [{ name: "JetBrains data sources", extensions: ["xml"] }],
	},
	{
		id: "pgadmin",
		name: "pgAdmin 4",
		reads: "pgadmin4.db or an exported servers.json",
		filters: [{ name: "pgAdmin servers", extensions: ["json", "db"] }],
	},
	{
		id: "sql-developer",
		name: "Oracle SQL Developer",
		reads: "connections.json or connections.xml",
		filters: [{ name: "SQL Developer connections", extensions: ["json", "xml"] }],
	},
];

export const toolInfo = (id: ImportTool) => TOOLS.find((t) => t.id === id)!;

const lower = (s: string | null | undefined) => (s ?? "").trim().toLowerCase();

/** Whether two profiles point at the same database as the same user. */
export function sameTarget(a: ConnectionProfile, b: ConnectionProfile): boolean {
	if (a.engine !== b.engine) return false;
	if (a.engine === "sqlite") return lower(a.database) === lower(b.database);
	const windows = a.engine === "sqlserver" && a.windowsAuth && b.windowsAuth;
	if (!windows && lower(a.user) !== lower(b.user)) return false;
	if (a.engine === "oracle") {
		if (a.oracleConnectType !== b.oracleConnectType) return false;
		if (a.oracleConnectType === "tns") return lower(a.tnsAlias) === lower(b.tnsAlias);
		if (a.oracleConnectType === "custom") return a.connectString.trim() === b.connectString.trim();
	}
	return (
		lower(a.host) === lower(b.host) &&
		a.port === b.port &&
		lower(a.database) === lower(b.database) &&
		lower(a.instance) === lower(b.instance)
	);
}

/** "Import connections" dialog: pick a tool file, review, import. */
class ImportStore {
	open = $state(false);
	sources = $state<ImportSource[]>([]);
	detecting = $state(false);
	/** File being reviewed; null on the source step. */
	preview = $state<ImportPreview | null>(null);
	reading = $state(false);
	selected = $state<Set<string>>(new Set());
	withPasswords = $state(true);
	importing = $state(false);

	async show() {
		this.open = true;
		this.preview = null;
		await this.detect();
	}

	close() {
		this.open = false;
		this.preview = null;
	}

	async detect() {
		this.detecting = true;
		try {
			this.sources = await api.importDetect();
		} catch (e) {
			toast.error(errorText(asAppError(e)));
		} finally {
			this.detecting = false;
		}
	}

	sourcesOf(tool: ImportTool): ImportSource[] {
		return this.sources.filter((s) => s.tool === tool);
	}

	async read(tool: ImportTool, path: string) {
		this.reading = true;
		try {
			const preview = await api.importPreview(tool, path);
			this.preview = preview;
			this.withPasswords = true;
			this.selected = new Set(preview.candidates.filter((c) => !this.isAdded(c)).map((c) => c.key));
		} catch (e) {
			toast.error(errorText(asAppError(e)));
		} finally {
			this.reading = false;
		}
	}

	async pick(tool: ImportTool) {
		const path = await open({ title: `Open ${toolInfo(tool).name} connections`, multiple: false, filters: toolInfo(tool).filters });
		if (typeof path === "string") await this.read(tool, path);
	}

	back() {
		this.preview = null;
	}

	isAdded(c: ImportCandidate): boolean {
		return workspace.profiles.some((p) => sameTarget(p, c.profile));
	}

	async importSelected() {
		const p = this.preview;
		if (!p || this.selected.size === 0) return;
		this.importing = true;
		try {
			const keys = p.candidates.filter((c) => this.selected.has(c.key)).map((c) => c.key);
			const r = await api.importApply(p.tool, p.path, keys, this.withPasswords);
			await workspace.loadProfiles();
			const what = `${r.created} ${r.created === 1 ? "connection" : "connections"}`;
			const pw = r.passwords > 0 ? `, ${r.passwords} with ${r.passwords === 1 ? "its password" : "passwords"}` : "";
			toast.success(`Imported ${what} from ${toolInfo(p.tool).name}${pw}`);
			this.close();
		} catch (e) {
			toast.error(errorText(asAppError(e)));
			await workspace.loadProfiles();
		} finally {
			this.importing = false;
		}
	}
}

export const importer = new ImportStore();
