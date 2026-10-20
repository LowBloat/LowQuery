import type { ConnectionProfile } from "./api";

/** One-line description of a connection: `user@host`, the alias or the file. */
export function connectionSummary(p: ConnectionProfile): string {
	if (p.engine === "sqlite") return p.database.split(/[\\/]/).pop() ?? p.database;
	const target =
		p.engine === "oracle" && p.oracleConnectType === "tns"
			? p.tnsAlias
			: p.engine === "oracle" && p.oracleConnectType === "custom"
				? "descriptor"
				: p.instance
					? `${p.host}\\${p.instance}`
					: p.host;
	if (p.engine === "sqlserver" && p.windowsAuth) return `Windows · ${target}`;
	return p.user ? `${p.user}@${target}` : target;
}
