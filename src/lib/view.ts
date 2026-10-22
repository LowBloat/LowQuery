import type { ColumnInfo } from "./api";
import { firstKeywords, type Dialect } from "./sql/split";

/**
 * How the grid shows a result. A result that fit within the row limit is
 * sorted in memory; one that hit the limit runs again on the server with the
 * view around it (`viewSql`), so the order covers every row, not only the
 * fetched ones.
 */
export interface ResultSort {
	/** Column index in the result. */
	col: number;
	dir: 1 | -1;
}

export interface ResultView {
	sort: ResultSort | null;
}

export const emptyView = (): ResultView => ({ sort: null });

export const isEmptyView = (view: ResultView): boolean => !view.sort;

/** Next sort when a column header is clicked: ascending, descending, none. */
export function nextSort(current: ResultSort | null, col: number): ResultSort | null {
	if (current?.col !== col) return { col, dir: 1 };
	return current.dir === 1 ? { col, dir: -1 } : null;
}

/** Indexes of `rows` in display order, NULLs last (in memory, no new query). */
export function viewOrder(columns: ColumnInfo[], rows: (string | null)[][], view: ResultView): number[] {
	const idx = rows.map((_, i) => i);
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

/**
 * The statement with the view applied by the database. The statement goes
 * inside a derived table and the sort refers to the column position, so no
 * column name needs quoting. A line break closes the statement in case it
 * ends with a `--` comment.
 */
export function viewSql(sql: string, view: ResultView, dialect: Dialect): string {
	const inner = sql.trim().replace(/;+$/, "").trimEnd();
	if (isEmptyView(view)) return inner;
	let out = `SELECT * FROM (\n${inner}\n) lq_view`;
	if (view.sort) {
		out += `\nORDER BY ${view.sort.col + 1} ${view.sort.dir === 1 ? "ASC" : "DESC"}`;
		if (NULLS_LAST[dialect]) out += " NULLS LAST";
	}
	return out;
}

/** Short description for the log: `font descending`. */
export function describeView(columns: ColumnInfo[], view: ResultView): string {
	if (!view.sort) return "original order";
	return `${columns[view.sort.col]?.name ?? `column ${view.sort.col + 1}`} ${view.sort.dir === 1 ? "ascending" : "descending"}`;
}
