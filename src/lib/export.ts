import type { ColumnInfo, ValueKind } from "./api";

/** TSV for pasting into spreadsheets. NULL becomes an empty cell. */
export function cellsToTsv(rows: (string | null)[][]): string {
	return rows
		.map((r) =>
			r
				.map((v) => {
					if (v === null) return "";
					return /[\t\n\r"]/.test(v) ? `"${v.replace(/"/g, '""')}"` : v;
				})
				.join("\t"),
		)
		.join("\n");
}

/**
 * RFC 4180 CSV with a configurable separator. Values starting with `=`, `+`,
 * `-` or `@` get an apostrophe prefix so spreadsheets do not treat them as
 * formulas, except numbers.
 */
export function toCsv(columns: ColumnInfo[], rows: (string | null)[][], sep = ","): string {
	const esc = (v: string | null, kind: ValueKind) => {
		if (v === null) return "";
		let s = v;
		if (kind !== "number" && /^[=+\-@\t\r]/.test(s)) s = `'${s}`;
		return s.includes(sep) || /["\n\r]/.test(s) ? `"${s.replace(/"/g, '""')}"` : s;
	};
	const head = columns.map((c) => esc(c.name, "text")).join(sep);
	const body = rows.map((r) => r.map((v, i) => esc(v, columns[i]?.kind ?? "text")).join(sep));
	return [head, ...body].join("\r\n");
}

export function toJson(columns: ColumnInfo[], rows: (string | null)[][]): string {
	const objs = rows.map((r) => Object.fromEntries(columns.map((c, i) => [c.name, r[i]])));
	return JSON.stringify(objs, null, 2);
}

/** Value for the viewer: indented JSON when possible. */
export function formatValue(v: string, kind: ValueKind): string {
	if (kind === "json" || /^\s*[[{]/.test(v)) {
		try {
			return JSON.stringify(JSON.parse(v), null, 2);
		} catch {
			return v;
		}
	}
	return v;
}

export function download(filename: string, content: string, type: string) {
	const blob = new Blob([content], { type });
	const url = URL.createObjectURL(blob);
	const a = document.createElement("a");
	a.href = url;
	a.download = filename;
	document.body.append(a);
	a.click();
	a.remove();
	setTimeout(() => URL.revokeObjectURL(url), 1000);
}
