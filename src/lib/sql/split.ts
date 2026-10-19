/**
 * Splits a SQL script into statements following each database's lexical
 * rules: strings, quoted identifiers, comments, PostgreSQL dollar quoting,
 * Oracle q-quoting and PL/SQL blocks terminated by `/`.
 */

export type Dialect = "postgres" | "oracle";

export interface SqlStatement {
	/** Statement text, without the terminator (`;` or `/`). */
	text: string;
	/** Start of the text in the document. */
	start: number;
	/** End (exclusive) of the text in the document. */
	end: number;
	/** End (exclusive) including the terminator, if any. */
	until: number;
}

export interface SplitOptions {
	/** A blank line also separates statements (outside parentheses and PL/SQL). */
	blankLineDelimiter?: boolean;
}

const IDENT = /[A-Za-z0-9_$#]/;

function isIdentChar(c: string | undefined): boolean {
	return c !== undefined && IDENT.test(c);
}

/** Leading words, uppercased, skipping whitespace and comments. */
export function firstKeywords(sql: string, n: number): string[] {
	const words: string[] = [];
	let i = 0;
	while (i < sql.length && words.length < n) {
		const c = sql[i];
		if (/\s/.test(c)) i++;
		else if (c === "-" && sql[i + 1] === "-") {
			while (i < sql.length && sql[i] !== "\n") i++;
		} else if (c === "/" && sql[i + 1] === "*") {
			const close = sql.indexOf("*/", i + 2);
			i = close < 0 ? sql.length : close + 2;
		} else if (/[A-Za-z_]/.test(c)) {
			const start = i;
			while (i < sql.length && /[A-Za-z0-9_]/.test(sql[i])) i++;
			words.push(sql.slice(start, i).toUpperCase());
		} else break;
	}
	return words;
}

/** Anonymous blocks and PL/SQL units (terminated by `/` on its own line). */
export function isPlsqlStart(sql: string): boolean {
	const kw = firstKeywords(sql, 6);
	let i = 0;
	if (kw[0] === "BEGIN" || kw[0] === "DECLARE") return true;
	if (kw[0] !== "CREATE") return false;
	i = 1;
	if (kw[i] === "OR" && kw[i + 1] === "REPLACE") i += 2;
	if (kw[i] === "EDITIONABLE" || kw[i] === "NONEDITIONABLE" || kw[i] === "EDITIONING") i++;
	return ["PROCEDURE", "FUNCTION", "PACKAGE", "TRIGGER", "TYPE", "LIBRARY", "JAVA"].includes(
		kw[i] ?? "",
	);
}

const Q_CLOSE: Record<string, string> = { "[": "]", "(": ")", "{": "}", "<": ">" };

export function splitStatements(
	sql: string,
	dialect: Dialect,
	options: SplitOptions = {},
): SqlStatement[] {
	const out: SqlStatement[] = [];
	const n = sql.length;
	let stmtStart = -1; // first significant character of the current statement
	let plsql = false;
	let depth = 0;
	let i = 0;
	let lineStart = 0;

	const push = (end: number, until: number) => {
		if (stmtStart >= 0) {
			let e = end;
			while (e > stmtStart && /\s/.test(sql[e - 1])) e--;
			if (e > stmtStart) out.push({ text: sql.slice(stmtStart, e), start: stmtStart, end: e, until });
		}
		stmtStart = -1;
		plsql = false;
		depth = 0;
	};

	const begin = (at: number) => {
		if (stmtStart < 0) {
			stmtStart = at;
			plsql = dialect === "oracle" && isPlsqlStart(sql.slice(at, at + 400));
		}
	};

	while (i < n) {
		const c = sql[i];
		const next = sql[i + 1];

		if (c === "\n") {
			i++;
			lineStart = i;
			// Blank line as a separator.
			if (options.blankLineDelimiter && stmtStart >= 0 && !plsql && depth === 0) {
				let j = i;
				while (j < n && (sql[j] === " " || sql[j] === "\t" || sql[j] === "\r")) j++;
				if (sql[j] === "\n") push(i - 1, i - 1);
			}
			continue;
		}

		// A lone "/" on a line ends the statement in Oracle (SQL*Plus style).
		if (dialect === "oracle" && c === "/" && sql.slice(lineStart, i).trim() === "") {
			let j = i + 1;
			while (j < n && (sql[j] === " " || sql[j] === "\t" || sql[j] === "\r")) j++;
			if (j >= n || sql[j] === "\n") {
				push(lineStart, j);
				i = j;
				continue;
			}
		}

		if (/\s/.test(c)) {
			i++;
			continue;
		}

		// Line comment.
		if (c === "-" && next === "-") {
			while (i < n && sql[i] !== "\n") i++;
			continue;
		}

		// Block comment (nestable in PostgreSQL).
		if (c === "/" && next === "*") {
			let level = 1;
			i += 2;
			while (i < n && level > 0) {
				if (sql[i] === "*" && sql[i + 1] === "/") {
					level--;
					i += 2;
				} else if (dialect === "postgres" && sql[i] === "/" && sql[i + 1] === "*") {
					level++;
					i += 2;
				} else i++;
			}
			continue;
		}

		begin(i);

		// Oracle q-quoting: q'[ ... ]', nq'{ ... }'
		if (
			dialect === "oracle" &&
			(c === "q" || c === "Q") &&
			next === "'" &&
			!isIdentChar(sql[i - 1] === "n" || sql[i - 1] === "N" ? sql[i - 2] : sql[i - 1])
		) {
			const open = sql[i + 2];
			if (open !== undefined) {
				const close = (Q_CLOSE[open] ?? open) + "'";
				const end = sql.indexOf(close, i + 3);
				i = end < 0 ? n : end + 2;
				continue;
			}
		}

		// String literal. In PostgreSQL, E'...' accepts backslash escapes.
		if (c === "'") {
			const backslash = dialect === "postgres" && (sql[i - 1] === "E" || sql[i - 1] === "e") && !isIdentChar(sql[i - 2]);
			i++;
			while (i < n) {
				if (backslash && sql[i] === "\\") i += 2;
				else if (sql[i] === "'") {
					if (sql[i + 1] === "'") i += 2;
					else {
						i++;
						break;
					}
				} else i++;
			}
			continue;
		}

		// Quoted identifier.
		if (c === '"') {
			i++;
			while (i < n) {
				if (sql[i] === '"') {
					if (sql[i + 1] === '"') i += 2;
					else {
						i++;
						break;
					}
				} else i++;
			}
			continue;
		}

		// Dollar quoting: $$ ... $$ or $tag$ ... $tag$ ($1 is a parameter).
		if (dialect === "postgres" && c === "$" && !isIdentChar(sql[i - 1])) {
			const m = /^\$([A-Za-z_][A-Za-z0-9_]*)?\$/.exec(sql.slice(i, i + 64));
			if (m) {
				const tag = m[0];
				const end = sql.indexOf(tag, i + tag.length);
				i = end < 0 ? n : end + tag.length;
				continue;
			}
		}

		if (c === "(") depth++;
		else if (c === ")") depth = Math.max(0, depth - 1);
		else if (c === ";" && !plsql) {
			push(i, i + 1);
			i++;
			continue;
		}
		i++;
	}
	push(n, n);
	return out;
}

/**
 * Statement under the cursor: the one containing the position or, if it falls
 * between statements, the last one before it.
 */
export function statementAt(statements: SqlStatement[], pos: number): SqlStatement | null {
	if (statements.length === 0) return null;
	let best: SqlStatement | null = null;
	for (const s of statements) {
		if (s.start <= pos) best = s;
		else break;
	}
	return best ?? statements[0];
}
