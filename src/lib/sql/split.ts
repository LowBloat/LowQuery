/**
 * Splits a SQL script into statements following each database's lexical
 * rules: strings, quoted identifiers, comments, PostgreSQL dollar quoting,
 * Oracle q-quoting and PL/SQL blocks ended by `/`, MySQL `DELIMITER`,
 * SQL Server `GO` batches and BEGIN...END blocks, SQLite trigger bodies, and
 * Cassandra `BEGIN BATCH ... APPLY BATCH`, `$$` strings and `//` comments.
 */

export type Dialect = "postgres" | "oracle" | "mysql" | "sqlserver" | "sqlite" | "redis" | "cassandra";

export interface SqlStatement {
	/** Statement text, without the terminator (`;`, `/`, `GO` or a custom delimiter). */
	text: string;
	/** Start of the text in the document. */
	start: number;
	/** End (exclusive) of the text in the document. */
	end: number;
	/** End (exclusive) including the terminator, if any. */
	until: number;
}

export interface SplitOptions {
	/** A blank line also separates statements (outside parentheses and blocks). */
	blankLineDelimiter?: boolean;
}

const IDENT = /[A-Za-z0-9_$#]/;
const WORD_START = /[A-Za-z_]/;

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
		else if ((c === "-" && sql[i + 1] === "-") || c === "#") {
			while (i < sql.length && sql[i] !== "\n") i++;
		} else if (c === "/" && sql[i + 1] === "*") {
			const close = sql.indexOf("*/", i + 2);
			i = close < 0 ? sql.length : close + 2;
		} else if (WORD_START.test(c)) {
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

/** T-SQL modules whose body runs to the end of the batch (`GO`). */
export function isTsqlModuleStart(sql: string): boolean {
	const kw = firstKeywords(sql, 4);
	if (kw[0] !== "CREATE" && kw[0] !== "ALTER") return false;
	const i = kw[1] === "OR" && kw[2] === "ALTER" ? 3 : 1;
	return ["PROCEDURE", "PROC", "FUNCTION", "TRIGGER", "VIEW"].includes(kw[i] ?? "");
}

/** CQL `BEGIN [UNLOGGED | COUNTER] BATCH`, whose `;` belong to the batch up to APPLY BATCH. */
export function isCqlBatchStart(sql: string): boolean {
	const kw = firstKeywords(sql, 3);
	if (kw[0] !== "BEGIN") return false;
	return kw[1] === "BATCH" || ((kw[1] === "UNLOGGED" || kw[1] === "COUNTER") && kw[2] === "BATCH");
}

/** SQLite `CREATE [TEMP] TRIGGER`, whose body holds `;` up to its END. */
export function isSqliteTriggerStart(sql: string): boolean {
	const kw = firstKeywords(sql, 3);
	if (kw[0] !== "CREATE") return false;
	const i = kw[1] === "TEMP" || kw[1] === "TEMPORARY" ? 2 : 1;
	return kw[i] === "TRIGGER";
}

const Q_CLOSE: Record<string, string> = { "[": "]", "(": ")", "{": "}", "<": ">" };

/** Reads a quoted run closed by `close`, where a doubled `close` is an escape. */
function skipQuoted(sql: string, i: number, close: string, backslash: boolean): number {
	const n = sql.length;
	i++;
	while (i < n) {
		if (backslash && sql[i] === "\\") i += 2;
		else if (sql[i] === close) {
			if (sql[i + 1] === close) i += 2;
			else return i + 1;
		} else i++;
	}
	return n;
}

/** Redis: one command per line, as in redis-cli; `#` and `//` start comments. */
function splitLines(text: string): SqlStatement[] {
	const out: SqlStatement[] = [];
	let lineStart = 0;
	while (lineStart <= text.length) {
		let lineEnd = text.indexOf("\n", lineStart);
		if (lineEnd < 0) lineEnd = text.length;
		let start = lineStart;
		let end = lineEnd;
		while (start < end && /\s/.test(text[start])) start++;
		while (end > start && /\s/.test(text[end - 1])) end--;
		const line = text.slice(start, end);
		if (line && !line.startsWith("#") && !line.startsWith("//")) {
			out.push({ text: line, start, end, until: lineEnd });
		}
		lineStart = lineEnd + 1;
	}
	return out;
}

export function splitStatements(
	sql: string,
	dialect: Dialect,
	options: SplitOptions = {},
): SqlStatement[] {
	if (dialect === "redis") return splitLines(sql);
	const out: SqlStatement[] = [];
	const n = sql.length;
	let stmtStart = -1; // first significant character of the current statement
	// "plsql": ends only at "/" (Oracle); "batch": ends only at GO (T-SQL modules);
	// "cql-batch": ends at the first ";" after APPLY BATCH (Cassandra).
	let mode = "normal" as "normal" | "plsql" | "batch" | "cql-batch";
	// BEGIN/CASE ... END nesting (T-SQL blocks, SQLite trigger bodies).
	let blocks = 0;
	let trackBlocks = false;
	let parens = 0;
	let delimiter = ";";
	let i = 0;
	let lineStart = 0;

	const push = (end: number, until: number) => {
		if (stmtStart >= 0) {
			let e = end;
			while (e > stmtStart && /\s/.test(sql[e - 1])) e--;
			if (e > stmtStart) out.push({ text: sql.slice(stmtStart, e), start: stmtStart, end: e, until });
		}
		stmtStart = -1;
		mode = "normal";
		blocks = 0;
		trackBlocks = false;
		parens = 0;
	};

	const begin = (at: number) => {
		if (stmtStart >= 0) return;
		stmtStart = at;
		const head = sql.slice(at, at + 400);
		if (dialect === "oracle" && isPlsqlStart(head)) mode = "plsql";
		else if (dialect === "sqlserver" && isTsqlModuleStart(head)) mode = "batch";
		else if (dialect === "cassandra" && isCqlBatchStart(head)) mode = "cql-batch";
		trackBlocks = dialect === "sqlserver" || (dialect === "sqlite" && isSqliteTriggerStart(head));
	};

	const lineEnd = (from: number) => {
		const e = sql.indexOf("\n", from);
		return e < 0 ? n : e;
	};

	while (i < n) {
		const c = sql[i];
		const next = sql[i + 1];

		if (c === "\n") {
			i++;
			lineStart = i;
			// Blank line as a separator.
			if (options.blankLineDelimiter && stmtStart >= 0 && mode === "normal" && parens === 0 && blocks === 0) {
				let j = i;
				while (j < n && (sql[j] === " " || sql[j] === "\t" || sql[j] === "\r")) j++;
				if (sql[j] === "\n") push(i - 1, i - 1);
			}
			continue;
		}

		const atLineStart = sql.slice(lineStart, i).trim() === "";

		// A lone "/" on a line ends the statement in Oracle (SQL*Plus style).
		if (dialect === "oracle" && c === "/" && atLineStart) {
			let j = i + 1;
			while (j < n && (sql[j] === " " || sql[j] === "\t" || sql[j] === "\r")) j++;
			if (j >= n || sql[j] === "\n") {
				push(lineStart, j);
				i = j;
				continue;
			}
		}

		// SQL Server: "GO" (optionally with a count) on its own line ends the batch.
		if (dialect === "sqlserver" && atLineStart && (c === "G" || c === "g")) {
			const end = lineEnd(i);
			if (/^go(\s+\d+)?\s*(--.*)?$/i.test(sql.slice(i, end).trim())) {
				push(lineStart, end);
				i = end;
				continue;
			}
		}

		// MySQL client command: "DELIMITER $$" changes the terminator.
		if (dialect === "mysql" && atLineStart && stmtStart < 0 && (c === "D" || c === "d")) {
			const end = lineEnd(i);
			const m = /^delimiter\s+(\S+)/i.exec(sql.slice(i, end));
			if (m) {
				delimiter = m[1];
				i = end;
				continue;
			}
		}

		if (/\s/.test(c)) {
			i++;
			continue;
		}

		// Line comments. MySQL needs whitespace after "--" and also accepts "#"; CQL accepts "//".
		const dashComment = c === "-" && next === "-" && (dialect !== "mysql" || next === undefined || /\s/.test(sql[i + 2] ?? " "));
		if (dashComment || (dialect === "mysql" && c === "#") || (dialect === "cassandra" && c === "/" && next === "/")) {
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

		// Custom MySQL delimiter (checked before any token it could start with).
		if (dialect === "mysql" && delimiter !== ";" && sql.startsWith(delimiter, i) && parens === 0) {
			push(i, i + delimiter.length);
			i += delimiter.length;
			continue;
		}

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

		// String literal. MySQL always accepts backslash escapes; PostgreSQL only in E'...'.
		if (c === "'") {
			const backslash =
				dialect === "mysql" ||
				(dialect === "postgres" && (sql[i - 1] === "E" || sql[i - 1] === "e") && !isIdentChar(sql[i - 2]));
			i = skipQuoted(sql, i, "'", backslash);
			continue;
		}

		// Double quotes: identifier (string with escapes in MySQL).
		if (c === '"') {
			i = skipQuoted(sql, i, '"', dialect === "mysql");
			continue;
		}

		// Backtick identifiers (MySQL, SQLite) and [bracket] identifiers (SQL Server, SQLite).
		if (c === "`" && (dialect === "mysql" || dialect === "sqlite")) {
			i = skipQuoted(sql, i, "`", false);
			continue;
		}
		if (c === "[" && (dialect === "sqlserver" || dialect === "sqlite")) {
			i = skipQuoted(sql, i, "]", false);
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

		// CQL string between two bare "$$" (function bodies).
		if (dialect === "cassandra" && c === "$" && next === "$") {
			const end = sql.indexOf("$$", i + 2);
			i = end < 0 ? n : end + 2;
			continue;
		}

		// Words: BEGIN/CASE/END nesting where `;` can appear inside a block.
		if (WORD_START.test(c) && !isIdentChar(sql[i - 1])) {
			let j = i;
			// A custom delimiter can follow a word directly ("END$$").
			while (j < n && isIdentChar(sql[j]) && !(delimiter !== ";" && sql.startsWith(delimiter, j))) j++;
			if (mode === "cql-batch" && sql.slice(i, j).toUpperCase() === "APPLY") {
				if (firstKeywords(sql.slice(j, j + 40), 1)[0] === "BATCH") mode = "normal";
			}
			if (trackBlocks) {
				const word = sql.slice(i, j).toUpperCase();
				if (word === "BEGIN") {
					const after = firstKeywords(sql.slice(j, j + 40), 1)[0] ?? "";
					const transaction = ["TRAN", "TRANSACTION", "DISTRIBUTED", "DIALOG", "CONVERSATION"].includes(after);
					// "BEGIN;" alone is a transaction in SQLite and T-SQL.
					const bare = /^\s*;/.test(sql.slice(j, j + 20));
					if (!transaction && !bare) blocks++;
				} else if (word === "CASE") blocks++;
				else if (word === "END") blocks = Math.max(0, blocks - 1);
			}
			i = j;
			continue;
		}

		if (c === "(") parens++;
		else if (c === ")") parens = Math.max(0, parens - 1);
		else if (c === ";" && delimiter === ";" && mode === "normal" && blocks === 0) {
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
