import { invoke } from "@tauri-apps/api/core";

export type Engine = "postgres" | "oracle";
export type SslMode = "disable" | "prefer" | "require" | "verify-full";
export type OracleConnectType = "service" | "sid" | "custom";
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
	configDir: string | null;
	oraclePrivilege: OraclePrivilege;
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

export interface ExecResult {
	columns: ColumnInfo[] | null;
	rows: (string | null)[][];
	truncated: boolean;
	rowsAffected: number | null;
	elapsedMs: number;
	notices: string[];
	inTransaction: boolean;
}

export interface TxState {
	autocommit: boolean;
	inTransaction: boolean;
}

export interface DbObject {
	name: string;
	kind: string;
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

export const DEFAULT_PORTS: Record<Engine, number> = { postgres: 5432, oracle: 1521 };

export const ENGINE_LABEL: Record<Engine, string> = { postgres: "PostgreSQL", oracle: "Oracle" };

export function emptyProfile(engine: Engine = "postgres"): ConnectionProfile {
	return {
		id: "",
		name: "",
		engine,
		color: null,
		host: "localhost",
		port: DEFAULT_PORTS[engine],
		database: engine === "postgres" ? "postgres" : "",
		user: engine === "postgres" ? "postgres" : "",
		savePassword: true,
		sslMode: "prefer",
		oracleConnectType: "service",
		connectString: "",
		configDir: null,
		oraclePrivilege: "none",
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
	cancel: (sessionId: string) => invoke<void>("cancel_query", { sessionId }),
	setAutocommit: (sessionId: string, on: boolean) =>
		invoke<TxState>("set_autocommit", { sessionId, on }),
	commit: (sessionId: string) => invoke<TxState>("commit", { sessionId }),
	rollback: (sessionId: string) => invoke<TxState>("rollback", { sessionId }),
	listSchemas: (sessionId: string) => invoke<string[]>("list_schemas", { sessionId }),
	listObjects: (sessionId: string, schema: string) =>
		invoke<DbObject[]>("list_objects", { sessionId, schema }),
	listColumns: (sessionId: string, schema: string, table: string) =>
		invoke<DbColumn[]>("list_columns", { sessionId, schema, table }),
};
