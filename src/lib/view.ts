import type { ColumnInfo, ValueKind } from "./api";
import { firstKeywords, type Dialect } from "./sql/split";

/**
 * How the grid shows a result: sorted by one column and filtered by any.
 * A result that fit within the row limit is sorted and filtered in memory;
 * one that hit the limit runs again on the server with the view around it
 * (`viewSql`), so the view covers every row, not only the fetched ones.
 */
export interface ResultSort {
	/** Column index in the result. */
	col: number;
	dir: 1 | -1;
}

export type FilterOp = "contains" | "eq" | "ne" | "null" | "notnull";

/** One filter per column; filters on several columns combine with AND. */
export interface ColumnFilter {
	col: number;
	op: FilterOp;
	/** Unused by `null` and `notnull`. */
	value: string;
}

export interface ResultView {
	sort: ResultSort | null;
	filters: ColumnFilter[];
}

export const FILTER_OPS: { value: FilterOp; label: string }[] = [
	{ value: "contains", label: "Contains" },
	{ value: "eq", label: "Equals" },
	{ value: "ne", label: "Not equal to" },
	{ value: "null", label: "Is NULL" },
	{ value: "notnull", label: "Is not NULL" },
];

export const opNeedsValue = (op: FilterOp): boolean => op !== "null" && op !== "notnull";

export const emptyView = (): ResultView => ({ sort: null, filters: [] });

export const isEmptyView = (view: ResultView): boolean => !view.sort && view.filters.length === 0;

/** Next sort when a column header is clicked: ascending, descending, none. */
export function nextSort(current: ResultSort | null, col: number): ResultSort | null {
	if (current?.col !== col) return { col, dir: 1 };
	return current.dir === 1 ? { col, dir: -1 } : null;
}

/** The view with `filter` replacing any filter on the same column. */
export function withFilter(view: ResultView, filter: ColumnFilter): ResultView {
	const filters = view.filters.filter((f) => f.col !== filter.col);
	filters.push(filter);
	return { ...view, filters };
}

/** The view without the filter on `col` (or without any filter). */
export function withoutFilter(view: ResultView, col?: number): ResultView {
	return { ...view, filters: col === undefined ? [] : view.filters.filter((f) => f.col !== col) };
}

const NUMBER = /^[-+]?(\d+\.?\d*|\.\d+)([eE][-+]?\d+)?$/;

function sameValue(value: string, wanted: string, kind: ValueKind | undefined): boolean {
	if (kind === "number" && NUMBER.test(value.trim()) && NUMBER.test(wanted.trim())) return Number(value) === Number(wanted);
	return value === wanted;
}

/**
 * Whether a value passes a filter. Text search ignores case; `ne` keeps
 * NULLs, as a person excluding one value expects (the SQL does the same).
 */
export function matchesFilter(value: string | null, f: ColumnFilter, kind?: ValueKind): boolean {
	switch (f.op) {
		case "null":
			return value === null;
		case "notnull":
			return value !== null;
		case "contains":
			return value !== null && value.toLocaleLowerCase().includes(f.value.toLocaleLowerCase());
		case "eq":
			return value !== null && sameValue(value, f.value, kind);
		case "ne":
			return value === null || !sameValue(value, f.value, kind);
	}
}

/** Indexes of `rows` that pass the filters, in display order, NULLs last (in memory, no new query). */
export function viewOrder(columns: ColumnInfo[], rows: (string | null)[][], view: ResultView): number[] {
	let idx = rows.map((_, i) => i);
	if (view.filters.length > 0) {
		idx = idx.filter((i) => view.filters.every((f) => matchesFilter(rows[i][f.col] ?? null, f, columns[f.col]?.kind)));
	}
	if (!view.sort) return idx;
	const { col, dir } = view.sort;
	const numeric = columns[col]?.kind === "number";
	const collator = new Intl.Collator(undefined, { numeric: true, sensitivity: "base" });
	return idx.sort((a, b) => {
		const va = rows[a][col];
		const vb = rows[b][col];
		if (va === vb) return 0;
		if (va === null) return 1;
		if (vb === null) return -1;
		if (numeric) {
			const d = Number(va) - Number(vb);
			if (!Number.isNaN(d) && d !== 0) return d * dir;
		}
		return collator.compare(va, vb) * dir;
	});
}

/** The rows as the grid shows them. */
export function viewRows(columns: ColumnInfo[], rows: (string | null)[][], view: ResultView): (string | null)[][] {
	return isEmptyView(view) ? rows : viewOrder(columns, rows, view).map((i) => rows[i]);
}

/** A query the view can wrap: Redis commands and non-SELECT statements cannot. */
export function canViewOnServer(sql: string, dialect: Dialect): boolean {
	if (dialect === "redis") return false;
	const kw = firstKeywords(sql, 1)[0];
	return kw === "SELECT" || kw === "WITH";
}

// PostgreSQL, Oracle and SQLite sort NULLs first when descending unless told
// otherwise; MySQL and SQL Server have no NULLS LAST and keep their default.
const NULLS_LAST: Partial<Record<Dialect, boolean>> = { postgres: true, oracle: true, sqlite: true };

export function quoteIdent(name: string, dialect: Dialect): string {
	if (dialect === "mysql") return "`" + name.replaceAll("`", "``") + "`";
	if (dialect === "sqlserver") return "[" + name.replaceAll("]", "]]") + "]";
	return '"' + name.replaceAll('"', '""') + '"';
}

export function quoteText(value: string, dialect: Dialect): string {
	let v = value.replaceAll("'", "''");
	// MySQL reads backslash escapes in strings unless NO_BACKSLASH_ESCAPES is set; doubled, they are literal either way.
	if (dialect === "mysql") v = v.replaceAll("\\", "\\\\");
	return dialect === "sqlserver" ? `N'${v}'` : `'${v}'`;
}

/** A column as text, close to how the grid shows it. */
function textOf(ident: string, kind: ValueKind | undefined, dialect: Dialect): string {
	switch (dialect) {
		case "oracle":
			return kind === "temporal" ? `TO_CHAR(${ident}, 'YYYY-MM-DD HH24:MI:SS')` : `TO_CHAR(${ident})`;
		case "sqlserver":
			return kind === "temporal" ? `CONVERT(nvarchar(max), ${ident}, 121)` : `CAST(${ident} AS nvarchar(max))`;
		case "mysql":
			return `CAST(${ident} AS CHAR)`;
		default:
			return `CAST(${ident} AS TEXT)`;
	}
}

/** Case-insensitive substring test in the dialect's own functions. */
function containsSql(text: string, needle: string, dialect: Dialect): string {
	switch (dialect) {
		case "postgres":
			return `STRPOS(LOWER(${text}), LOWER(${needle})) > 0`;
		case "oracle":
			return `INSTR(UPPER(${text}), UPPER(${needle})) > 0`;
		case "mysql":
			return `LOCATE(LOWER(${needle}), LOWER(${text})) > 0`;
		case "sqlserver":
			return `CHARINDEX(LOWER(${needle}), LOWER(${text})) > 0`;
		default:
			return `INSTR(LOWER(${text}), LOWER(${needle})) > 0`;
	}
}

/** The SQL condition for one filter. */
export function filterSql(f: ColumnFilter, column: ColumnInfo, dialect: Dialect): string {
	const ident = quoteIdent(column.name, dialect);
	const equals = () =>
		column.kind === "number" && NUMBER.test(f.value.trim())
			? `${ident} = ${f.value.trim()}`
			: `${textOf(ident, column.kind, dialect)} = ${quoteText(f.value, dialect)}`;
	switch (f.op) {
		case "null":
			return `${ident} IS NULL`;
		case "notnull":
			return `${ident} IS NOT NULL`;
		case "contains":
			return containsSql(textOf(ident, column.kind, dialect), quoteText(f.value, dialect), dialect);
		case "eq":
			return equals();
		case "ne":
			return `(${ident} IS NULL OR NOT (${equals()}))`;
	}
}

/**
 * The statement with the view applied by the database. The statement goes
 * inside a derived table; filters name the result's columns and the sort
 * refers to the column position. A line break closes the statement in case
 * it ends with a `--` comment.
 */
export function viewSql(sql: string, view: ResultView, columns: ColumnInfo[], dialect: Dialect): string {
	const inner = sql.trim().replace(/;+$/, "").trimEnd();
	if (isEmptyView(view)) return inner;
	let out = `SELECT * FROM (\n${inner}\n) lq_view`;
	const conditions = view.filters.filter((f) => columns[f.col]).map((f) => filterSql(f, columns[f.col], dialect));
	if (conditions.length > 0) out += `\nWHERE ${conditions.join("\n  AND ")}`;
	if (view.sort) {
		out += `\nORDER BY ${view.sort.col + 1} ${view.sort.dir === 1 ? "ASC" : "DESC"}`;
		if (NULLS_LAST[dialect]) out += " NULLS LAST";
	}
	return out;
}

/** Short text for a filter chip: `font contains "PIX"`. */
export function describeFilter(f: ColumnFilter, columns: ColumnInfo[]): string {
	const name = columns[f.col]?.name ?? `column ${f.col + 1}`;
	switch (f.op) {
		case "null":
			return `${name} is NULL`;
		case "notnull":
			return `${name} is not NULL`;
		case "contains":
			return `${name} contains "${f.value}"`;
		case "eq":
			return `${name} = "${f.value}"`;
		case "ne":
			return `${name} ≠ "${f.value}"`;
	}
}

/** Short description for the log: `font contains "PIX"; name descending`. */
export function describeView(columns: ColumnInfo[], view: ResultView): string {
	const parts = view.filters.map((f) => describeFilter(f, columns));
	if (view.sort) {
		const name = columns[view.sort.col]?.name ?? `column ${view.sort.col + 1}`;
		parts.push(`${name} ${view.sort.dir === 1 ? "ascending" : "descending"}`);
	}
	return parts.join("; ") || "no sort or filter";
}
