import { invoke } from "@tauri-apps/api/core";
import { ENGINES, engineInfo, type Engine } from "./engines";

export type { Engine } from "./engines";
export type SslMode = "disable" | "prefer" | "require" | "verify-full";
export type OracleConnectType = "service" | "sid" | "tns" | "custom";
export type OraclePrivilege =
	| "none"
	| "sysdba"
	| "sysoper"
	| "sysbackup"
	| "sysdg"
	| "syskm"
	| "sysrac"
	| "sysasm";

export interface ConnectionProfile {
	id: string;
	name: string;
	engine: Engine;
	color: string | null;
	host: string;
	port: number;
	database: string;
	user: string;
	savePassword: boolean;
	sslMode: SslMode;
	oracleConnectType: OracleConnectType;
	connectString: string;
	/** Alias looked up in tnsnames.ora when `oracleConnectType` is "tns". */
	tnsAlias: string;
	/** Folder with tnsnames.ora/sqlnet.ora; null means automatic discovery. */
	configDir: string | null;
	oraclePrivilege: OraclePrivilege;
	/** SQL Server named instance. */
	instance: string;
	/** SQL Server: sign in with the current Windows account. */
	windowsAuth: boolean;
	/** SQLite: create the file when it does not exist. */
	createFile: boolean;
	/** What AI agents may do through `lowquery mcp`. */
	aiAccess: AiAccess;
}

export interface SessionInfo {
	sessionId: string;
	profileId: string;
	engine: Engine;
	serverVersion: string;
	currentUser: string | null;
	currentSchema: string | null;
	autocommit: boolean;
}

export type ValueKind = "number" | "text" | "bool" | "temporal" | "binary" | "json" | "other";

export interface ColumnInfo {
	name: string;
	typeName: string;
	kind: ValueKind;
}

/** The table behind an editable result (see src-tauri/src/model.rs). */
export interface EditTarget {
	schema: string | null;
	table: string;
	/** Per result column: the table column it shows, or null for expressions. */
	columns: ({ name: string; typeName: string } | null)[];
	/** Result column indexes forming the key. */
	key: number[];
}

export interface CellValue {
	column: number;
	value: string | null;
}

export type RowChange =
	| { op: "update"; key: (string | null)[]; values: CellValue[] }
	| { op: "delete"; key: (string | null)[] }
	| { op: "insert"; values: CellValue[] };

export interface ApplyResult {
	updated: number;
	deleted: number;
	inserted: number;
	elapsedMs: number;
	inTransaction: boolean;
}

export interface ExecResult {
	columns: ColumnInfo[] | null;
	rows: (string | null)[][];
	truncated: boolean;
	rowsAffected: number | null;
	elapsedMs: number;
	notices: string[];
	inTransaction: boolean;
	editable: EditTarget | null;
	readOnlyReason: string | null;
}

export interface TxState {
	autocommit: boolean;
	inTransaction: boolean;
}

export interface DbObject {
	name: string;
	kind: string;
}

export interface TnsEntry {
	alias: string;
	descriptor: string;
	host: string | null;
	port: number | null;
	serviceName: string | null;
	sid: string | null;
	protocol: string | null;
}

/** A folder with a tnsnames.ora found on this machine or picked by the user. */
export type AiAccess = "off" | "read" | "write";

/** An AI client and how to make it start `lowquery mcp`. */
export interface AiClient {
	id: string;
	name: string;
	/** JSON file LowQuery can update; null when configured by hand. */
	configPath: string | null;
	installed: boolean;
	configured: boolean;
	snippet: string;
	snippetKind: "json" | "toml" | "shell";
}

export interface AiSetup {
	command: string;
	args: string[];
	clients: AiClient[];
}

/** One tool call made by an AI agent. */
export interface AiActivity {
	at: string;
	client: string;
	tool: string;
	connectionId: string | null;
	connectionName: string | null;
	detail: string | null;
	ok: boolean;
	rows: number | null;
	error: string | null;
	elapsedMs: number;
}

/** Database tools LowQuery imports connections from. */
export type ImportTool = "dbeaver" | "datagrip" | "pgadmin" | "sql-developer";

/** A connection file of another tool found on this machine. */
export interface ImportSource {
	tool: ImportTool;
	label: string;
	path: string;
	count: number;
	error: string | null;
}

export interface ImportCandidate {
	key: string;
	/** The profile to create (empty id). */
	profile: ConnectionProfile;
	folder: string | null;
	hasPassword: boolean;
	warnings: string[];
}

export interface ImportPreview {
	tool: ImportTool;
	path: string;
	candidates: ImportCandidate[];
	skipped: { name: string; reason: string }[];
	notes: string[];
}

export interface ImportResult {
	created: number;
	passwords: number;
}

export interface TnsSource {
	dir: string;
	/** Oracle home name (OraClient19Home1) or the folder. */
	label: string;
	/** TNS_ADMIN, ORACLE_HOME, Oracle home, Instant Client on PATH... */
	origin: string;
	entries: TnsEntry[];
	error: string | null;
}

export interface DbColumn {
	name: string;
	typeName: string;
	nullable: boolean;
	primaryKey: boolean;
}

export type ErrorKind =
	| "database"
	| "connection"
	| "password_required"
	| "not_found"
	| "cancelled"
	| "invalid"
	| "internal";

/** Structured error returned by the backend. */
export interface AppError {
	kind: ErrorKind;
	message: string;
	code?: string;
	position?: number;
	detail?: string;
	hint?: string;
}

export function asAppError(e: unknown): AppError {
	if (e && typeof e === "object" && "message" in e && "kind" in e) return e as AppError;
	return { kind: "internal", message: e instanceof Error ? e.message : String(e) };
}

export function errorText(e: AppError): string {
	return e.code ? `${e.code}: ${e.message}` : e.message;
}

export const ENGINE_LABEL = Object.fromEntries(ENGINES.map((e) => [e.id, e.label])) as Record<Engine, string>;

/** Defaults a new profile starts with, per engine. */
const DEFAULTS: Partial<Record<Engine, { database: string; user: string }>> = {
	postgres: { database: "postgres", user: "postgres" },
	mysql: { database: "", user: "root" },
	mariadb: { database: "", user: "root" },
	sqlserver: { database: "master", user: "sa" },
	redis: { database: "0", user: "" },
};

export function emptyProfile(engine: Engine = "postgres"): ConnectionProfile {
	return {
		id: "",
		name: "",
		engine,
		color: null,
		host: engineInfo(engine).file ? "" : "localhost",
		port: engineInfo(engine).defaultPort ?? 0,
		database: DEFAULTS[engine]?.database ?? "",
		user: DEFAULTS[engine]?.user ?? "",
		savePassword: true,
		sslMode: "prefer",
		oracleConnectType: "service",
		connectString: "",
		tnsAlias: "",
		configDir: null,
		oraclePrivilege: "none",
		instance: "",
		windowsAuth: false,
		createFile: false,
		aiAccess: "off",
	};
}

/** Calls to the Rust backend. Errors arrive as {@link AppError}. */
export const api = {
	listProfiles: () => invoke<ConnectionProfile[]>("list_profiles"),
	saveProfile: (profile: ConnectionProfile, password: string | null) =>
		invoke<ConnectionProfile>("save_profile", { profile, password }),
	deleteProfile: (id: string) => invoke<void>("delete_profile", { id }),
	testConnection: (profile: ConnectionProfile, password: string | null) =>
		invoke<{ serverVersion: string }>("test_connection", { profile, password }),
	openSession: (profileId: string, password: string | null = null) =>
		invoke<SessionInfo>("open_session", { profileId, password }),
	closeSession: (sessionId: string) => invoke<void>("close_session", { sessionId }),
	execute: (sessionId: string, sql: string, maxRows: number) =>
		invoke<ExecResult>("execute", { sessionId, request: { sql, maxRows } }),
	applyChanges: (sessionId: string, target: EditTarget, changes: RowChange[]) =>
		invoke<ApplyResult>("apply_changes", { sessionId, request: { target, changes } }),
	cancel: (sessionId: string) => invoke<void>("cancel_query", { sessionId }),
	setAutocommit: (sessionId: string, on: boolean) =>
		invoke<TxState>("set_autocommit", { sessionId, on }),
	commit: (sessionId: string) => invoke<TxState>("commit", { sessionId }),
	rollback: (sessionId: string) => invoke<TxState>("rollback", { sessionId }),
	listSchemas: (sessionId: string) => invoke<string[]>("list_schemas", { sessionId }),
	listObjects: (sessionId: string, schema: string) =>
		invoke<DbObject[]>("list_objects", { sessionId, schema }),
	oracleTnsSources: () => invoke<TnsSource[]>("oracle_tns_sources"),
	oracleTnsFolder: (path: string) => invoke<TnsSource>("oracle_tns_folder", { path }),
	aiSetup: () => invoke<AiSetup>("ai_setup"),
	aiConnectClient: (id: string) => invoke<string>("ai_connect_client", { id }),
	aiActivity: (limit = 200) => invoke<AiActivity[]>("ai_activity", { limit }),
	aiActivityClear: () => invoke<void>("ai_activity_clear"),
	importDetect: () => invoke<ImportSource[]>("import_detect"),
	importPreview: (tool: ImportTool, path: string) => invoke<ImportPreview>("import_preview", { tool, path }),
	/** Passwords are read again from the tool file by the backend, never sent here. */
	importApply: (tool: ImportTool, path: string, keys: string[], withPasswords: boolean) =>
		invoke<ImportResult>("import_apply", { tool, path, keys, withPasswords }),
	listColumns: (sessionId: string, schema: string, table: string) =>
		invoke<DbColumn[]>("list_columns", { sessionId, schema, table }),
};
