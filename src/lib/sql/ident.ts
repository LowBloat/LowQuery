import type { Engine } from "../api";

const RESERVED = new Set([
	"ALL", "AND", "AS", "ASC", "BY", "CASE", "CHECK", "COLUMN", "CREATE", "DEFAULT", "DELETE",
	"DESC", "DISTINCT", "DROP", "ELSE", "END", "FROM", "GRANT", "GROUP", "HAVING", "IN", "INDEX",
	"INSERT", "INTO", "IS", "JOIN", "LEVEL", "LIKE", "LIMIT", "NOT", "NULL", "ON", "OR", "ORDER",
	"SELECT", "SET", "SIZE", "TABLE", "THEN", "TO", "UNION", "UNIQUE", "UPDATE", "USER", "VALUES",
	"VIEW", "WHEN", "WHERE", "WITH",
]);

/**
 * Quotes the identifier only when needed, in each engine's style.
 * PostgreSQL folds unquoted names to lowercase and Oracle to uppercase;
 * MySQL, SQL Server and SQLite keep the case and only need quotes for
 * special characters or reserved words.
 */
export function quoteIdent(name: string, engine: Engine): string {
	const reserved = RESERVED.has(name.toUpperCase());
	switch (engine) {
		case "postgres":
			return /^[a-z_][a-z0-9_$]*$/.test(name) && !reserved ? name : `"${name.replace(/"/g, '""')}"`;
		case "oracle":
			return /^[A-Z][A-Z0-9_$#]*$/.test(name) && !reserved ? name : `"${name.replace(/"/g, '""')}"`;
		case "mysql":
		case "mariadb":
			return /^[A-Za-z_][A-Za-z0-9_$]*$/.test(name) && !reserved ? name : `\`${name.replace(/`/g, "``")}\``;
		case "sqlserver":
			return /^[A-Za-z_][A-Za-z0-9_@#$]*$/.test(name) && !reserved ? name : `[${name.replace(/]/g, "]]")}]`;
		case "sqlite":
			return /^[A-Za-z_][A-Za-z0-9_]*$/.test(name) && !reserved ? name : `"${name.replace(/"/g, '""')}"`;
		case "redis":
			return redisArg(name);
	}
}

/** A redis-cli argument: bare when safe, double-quoted with escapes otherwise. */
export function redisArg(value: string): string {
	// Binary key names arrive from the driver already in redis-cli quoted form.
	if (value.length >= 2 && value.startsWith('"') && value.endsWith('"')) return value;
	if (/^[^\s"'\\]+$/.test(value)) return value;
	const escaped = value
		.replace(/[\\"]/g, (c) => `\\${c}`)
		.replace(/\n/g, "\\n")
		.replace(/\r/g, "\\r")
		.replace(/\t/g, "\\t");
	return `"${escaped}"`;
}

/** Redis command that shows a key, chosen by its type. */
export function redisPreview(key: string, kind: string): string {
	const k = redisArg(key);
	switch (kind) {
		case "hash":
			return `HGETALL ${k}`;
		case "list":
			return `LRANGE ${k} 0 199`;
		case "set":
			return `SSCAN ${k} 0 COUNT 200`;
		case "zset":
			return `ZRANGE ${k} 0 199 WITHSCORES`;
		case "stream":
			return `XRANGE ${k} - + COUNT 200`;
		case "json":
			return `JSON.GET ${k}`;
		default:
			return `GET ${k}`;
	}
}

export function qualified(schema: string, name: string, engine: Engine): string {
	return `${quoteIdent(schema, engine)}.${quoteIdent(name, engine)}`;
}

/** Preview SELECT with the row limit in each database's syntax. */
export function previewQuery(schema: string, name: string, engine: Engine): string {
	const target = qualified(schema, name, engine);
	switch (engine) {
		case "oracle":
			return `SELECT * FROM ${target} FETCH FIRST 200 ROWS ONLY`;
		case "sqlserver":
			return `SELECT TOP 200 * FROM ${target}`;
		default:
			return `SELECT * FROM ${target} LIMIT 200`;
	}
}
