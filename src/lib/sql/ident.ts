import type { Engine } from "../api";

const RESERVED = new Set([
	"ALL", "AND", "AS", "ASC", "BY", "CASE", "CHECK", "COLUMN", "CREATE", "DEFAULT", "DELETE",
	"DESC", "DISTINCT", "DROP", "ELSE", "END", "FROM", "GRANT", "GROUP", "HAVING", "IN", "INDEX",
	"INSERT", "INTO", "IS", "JOIN", "LEVEL", "LIKE", "LIMIT", "NOT", "NULL", "ON", "OR", "ORDER",
	"SELECT", "SET", "SIZE", "TABLE", "THEN", "TO", "UNION", "UNIQUE", "UPDATE", "USER", "VALUES",
	"VIEW", "WHEN", "WHERE", "WITH",
]);

/**
 * Quotes the identifier only when needed. PostgreSQL folds unquoted names
 * to lowercase; Oracle, to uppercase.
 */
export function quoteIdent(name: string, engine: Engine): string {
	const plain = engine === "postgres" ? /^[a-z_][a-z0-9_$]*$/ : /^[A-Z][A-Z0-9_$#]*$/;
	if (plain.test(name) && !RESERVED.has(name.toUpperCase())) return name;
	return `"${name.replace(/"/g, '""')}"`;
}

export function qualified(schema: string, name: string, engine: Engine): string {
	return `${quoteIdent(schema, engine)}.${quoteIdent(name, engine)}`;
}

/** Preview SELECT with the row limit in each database's syntax. */
export function previewQuery(schema: string, name: string, engine: Engine): string {
	const target = qualified(schema, name, engine);
	return engine === "oracle"
		? `SELECT * FROM ${target} FETCH FIRST 200 ROWS ONLY`
		: `SELECT * FROM ${target} LIMIT 200`;
}
