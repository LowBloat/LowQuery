import type { EditTarget, RowChange } from "../api";

/**
 * Pending grid edits for one result. Rows are keyed `b<index>` for rows of
 * the result and `n<seq>` for new rows; `values` holds only edited cells.
 */
export interface PendingChange {
	op: "update" | "delete" | "insert";
	/** Index into the result rows; null for new rows. */
	base: number | null;
	values: Record<number, string | null>;
}

export type EditMap = Record<string, PendingChange>;

let insertSeq = 0;

export const baseId = (index: number) => `b${index}`;

/** Value shown for a cell: the edit when there is one, else the original. */
export function cellValue(map: EditMap, id: string, original: (string | null)[] | null, col: number): string | null {
	const change = map[id];
	if (change && col in change.values) return change.values[col];
	return original?.[col] ?? null;
}

export function isCellEdited(map: EditMap, id: string, col: number): boolean {
	const change = map[id];
	return !!change && change.op !== "delete" && col in change.values;
}

/** Sets a cell. Going back to the original value drops the edit. */
export function setCell(
	map: EditMap,
	id: string,
	base: number | null,
	original: (string | null)[] | null,
	col: number,
	value: string | null,
): void {
	const change = map[id] ?? (map[id] = { op: base === null ? "insert" : "update", base, values: {} });
	if (change.op === "delete") return;
	if (change.op === "update" && (original?.[col] ?? null) === value) delete change.values[col];
	else change.values[col] = value;
	if (change.op === "update" && Object.keys(change.values).length === 0) delete map[id];
}

/** Reverts one cell of a row of the result. */
export function revertCell(map: EditMap, id: string, col: number): void {
	const change = map[id];
	if (!change || change.op === "delete") return;
	delete change.values[col];
	if (change.op === "update" && Object.keys(change.values).length === 0) delete map[id];
}

/** Marks a row for deletion, or unmarks it. New rows are simply removed. */
export function toggleDelete(map: EditMap, id: string, base: number | null): void {
	const change = map[id];
	if (base === null) {
		delete map[id];
		return;
	}
	if (change?.op === "delete") delete map[id];
	else map[id] = { op: "delete", base, values: {} };
}

/** Adds a new row, optionally prefilled (duplicate). Returns its id. */
export function addRow(map: EditMap, values: Record<number, string | null> = {}): string {
	const id = `n${++insertSeq}`;
	map[id] = { op: "insert", base: null, values: { ...values } };
	return id;
}

export function pendingCount(map: EditMap): number {
	return Object.keys(map).length;
}

/** New rows, in the order they were added. */
export function insertedIds(map: EditMap): string[] {
	return Object.keys(map)
		.filter((id) => map[id].op === "insert")
		.sort((a, b) => Number(a.slice(1)) - Number(b.slice(1)));
}

/** Changes in the shape the backend applies. */
export function buildChanges(target: EditTarget, rows: (string | null)[][], map: EditMap): RowChange[] {
	const out: RowChange[] = [];
	const ids = Object.keys(map).sort((a, b) => {
		// Result rows in order, then new rows in the order they were added.
		const ka = a[0] === "b" ? 0 : 1;
		const kb = b[0] === "b" ? 0 : 1;
		return ka - kb || Number(a.slice(1)) - Number(b.slice(1));
	});
	for (const id of ids) {
		const c = map[id];
		const values = Object.entries(c.values).map(([col, value]) => ({ column: Number(col), value }));
		if (c.op === "insert") {
			out.push({ op: "insert", values });
			continue;
		}
		const row = rows[c.base!];
		const key = target.key.map((k) => row[k] ?? null);
		out.push(c.op === "delete" ? { op: "delete", key } : { op: "update", key, values });
	}
	return out;
}
