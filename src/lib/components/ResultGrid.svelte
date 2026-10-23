<script lang="ts">
	import { onMount, untrack } from "svelte";
	import {
		Dialog,
		Button,
		Chip,
		CopyButton,
		DropdownMenu,
		DropdownMenuItem,
		DropdownMenuSeparator,
		Select,
		TextArea,
		TextField,
		toast,
	} from "@codeatlasdev/ui";
	import {
		ArrowDown,
		ArrowUp,
		ArrowUpDown,
		Ban,
		CircleSlash,
		Copy,
		CopyPlus,
		Eye,
		Funnel,
		FunnelPlus,
		FunnelX,
		MoveHorizontal,
		Pencil,
		Plus,
		RotateCcw,
		Rows3,
		Table2,
		Trash2,
		Undo2,
	} from "@lucide/svelte";
	import type { ColumnInfo, EditTarget } from "../api";
	import { cellsToTsv, formatValue } from "../export";
	import {
		addRow as addPendingRow,
		baseId,
		cellValue,
		insertedIds,
		isCellEdited,
		revertCell,
		setCell,
		toggleDelete,
		type EditMap,
	} from "../state/edits";
	import {
		FILTER_OPS,
		describeFilter,
		emptyView,
		nextSort,
		opNeedsValue,
		viewOrder,
		withFilter,
		withoutFilter,
		type ColumnFilter,
		type FilterOp,
		type ResultView,
	} from "../view";

	interface Props {
		columns: ColumnInfo[];
		rows: (string | null)[][];
		label?: string;
		/** Set when the rows can be edited; `edits` then holds the pending changes. */
		editable?: EditTarget | null;
		edits?: EditMap;
		onsave?: () => void;
		/** Sort and filters shown in the grid. */
		view?: ResultView;
		/** Apply `view` to `rows` here; false when the rows already come sorted and filtered from the server. */
		localView?: boolean;
		onview?: (view: ResultView) => void;
		/** A query for this result is running: the header shows it and ignores clicks. */
		busy?: boolean;
	}

	let {
		columns,
		rows,
		label = "Result",
		editable = null,
		edits,
		onsave,
		view = emptyView(),
		localView = true,
		onview,
		busy = false,
	}: Props = $props();

	const ROW_H = 26;
	const HEAD_H = 32;
	const OVERSCAN = 8;
	/** Longer or multi-line values are edited in the value dialog. */
	const INLINE_MAX = 500;

	interface DisplayRow {
		id: string;
		/** Index into `rows`; null for a new row. */
		base: number | null;
	}

	interface CellEdit {
		id: string;
		base: number | null;
		col: number;
		value: string;
		/** Started from NULL: leaving it empty keeps NULL. */
		wasNull: boolean;
		/** Started by typing: keep the caret after the typed text. */
		typed: boolean;
	}

	let viewport: HTMLDivElement;
	let scrollTop = $state(0);
	let height = $state(400);
	let widths = $state<number[]>([]);
	const sort = $derived(view.sort);
	let active = $state<{ row: number; col: number } | null>(null);
	let editing = $state<CellEdit | null>(null);
	let viewer = $state<{
		title: string;
		value: string;
		type: string;
		edit: Omit<CellEdit, "value" | "typed"> | null;
	} | null>(null);
	let viewerText = $state("");
	let menu = $state<{ x: number; y: number; row: number; col: number } | null>(null);
	let headMenu = $state<{ x: number; y: number; col: number } | null>(null);
	let filterEdit = $state<{ col: number; op: FilterOp; value: string } | null>(null);
	let filterInput = $state<HTMLInputElement | null>(null);

	// Indexes of the result rows in display order (in memory, no new query).
	const order = $derived(localView ? viewOrder(columns, rows, view) : rows.map((_, i) => i));

	// Result rows, then new rows at the end.
	const display = $derived.by((): DisplayRow[] => {
		const list: DisplayRow[] = order.map((i) => ({ id: baseId(i), base: i }));
		if (edits) for (const id of insertedIds(edits)) list.push({ id, base: null });
		return list;
	});

	const rowNumWidth = $derived(Math.max(44, String(display.length).length * 9 + 22));
	const template = $derived(`${rowNumWidth}px ${widths.map((w) => `${w}px`).join(" ")}`);
	const totalWidth = $derived(rowNumWidth + widths.reduce((a, b) => a + b, 0));
	const first = $derived(Math.max(0, Math.floor(scrollTop / ROW_H) - OVERSCAN));
	const last = $derived(Math.min(display.length, Math.ceil((scrollTop + height) / ROW_H) + OVERSCAN));
	const visible = $derived(display.slice(first, last).map((d, i) => ({ ...d, display: first + i })));
	// Where a new row starts editing: key columns are often identities, so skip them.
	const firstEditable = $derived.by(() => {
		if (!editable) return 0;
		const cols = editable.columns.map((c, i) => (c ? i : -1)).filter((i) => i >= 0);
		return cols.find((i) => !editable.key.includes(i)) ?? cols[0] ?? 0;
	});

	function measureWidths() {
		const canvas = document.createElement("canvas");
		const ctx = canvas.getContext("2d");
		const mono = getComputedStyle(document.documentElement).getPropertyValue("--ca-font-mono") || "monospace";
		const sans = getComputedStyle(document.documentElement).getPropertyValue("--ca-font-sans") || "sans-serif";
		const sample = rows.slice(0, 200);
		widths = columns.map((c, ci) => {
			if (!ctx) return 160;
			ctx.font = `600 13px ${sans}`;
			// Room for the sort arrow and the filter button.
			let w = ctx.measureText(c.name).width + 52;
			ctx.font = `11px ${sans}`;
			w = Math.max(w, ctx.measureText(c.typeName).width + 20);
			ctx.font = `13px ${mono}`;
			for (const r of sample) {
				const v = r[ci];
				const text = v === null ? "NULL" : v.length > 80 ? v.slice(0, 80) : v;
				w = Math.max(w, ctx.measureText(text).width + 20);
			}
			return Math.round(Math.min(Math.max(w, 64), 420));
		});
	}

	// New columns: recompute widths, clear the selection. The same
	// columns again (the result refreshed after saving) keep the layout.
	let signature = "";
	$effect(() => {
		const sig = columns.map((c) => `${c.name}\u0000${c.typeName}`).join("\u0001");
		void rows;
		untrack(() => {
			editing = null;
			if (sig === signature) {
				if (active && active.row >= display.length) active = null;
				return;
			}
			signature = sig;
			measureWidths();
			active = null;
			if (viewport) viewport.scrollTop = 0;
			scrollTop = 0;
		});
	});

	onMount(() => {
		const ro = new ResizeObserver(() => (height = viewport.clientHeight));
		ro.observe(viewport);
		return () => ro.disconnect();
	});

	function setView(next: ResultView) {
		if (busy) return;
		commitEdit();
		onview?.(next);
	}

	function toggleSort(col: number) {
		setView({ ...view, sort: nextSort(sort, col) });
	}

	function sortBy(col: number, dir: 1 | -1 | null) {
		setView({ ...view, sort: dir ? { col, dir } : null });
	}

	// ---- filters ---------------------------------------------------------------

	const filterOf = (col: number) => view.filters.find((f) => f.col === col);

	function applyFilter(f: ColumnFilter) {
		setView(withFilter(view, f));
	}

	function clearFilter(col?: number) {
		setView(withoutFilter(view, col));
	}

	function openFilter(col: number) {
		const f = filterOf(col);
		filterEdit = { col, op: f?.op ?? "contains", value: f?.value ?? "" };
	}

	function submitFilter() {
		const e = filterEdit;
		if (!e) return;
		filterEdit = null;
		viewport.focus();
		// Containing nothing matches everything: the same as no filter.
		if (e.op === "contains" && e.value === "") {
			if (filterOf(e.col)) clearFilter(e.col);
			return;
		}
		applyFilter({ col: e.col, op: e.op, value: opNeedsValue(e.op) ? e.value : "" });
	}

	function filterByValue(row: number, col: number, exclude: boolean) {
		const v = valueAt(row, col);
		if (v === null) applyFilter({ col, op: exclude ? "notnull" : "null", value: "" });
		else applyFilter({ col, op: exclude ? "ne" : "eq", value: v });
	}

	$effect(() => {
		const el = filterInput;
		if (el) requestAnimationFrame(() => el.select());
	});

	// Column resize by dragging the header edge.
	function startResize(e: PointerEvent, col: number) {
		e.preventDefault();
		e.stopPropagation();
		const startX = e.clientX;
		const startW = widths[col];
		const target = e.currentTarget as HTMLElement;
		target.setPointerCapture(e.pointerId);
		const move = (ev: PointerEvent) => {
			widths[col] = Math.max(48, Math.round(startW + ev.clientX - startX));
		};
		const up = () => {
			target.removeEventListener("pointermove", move);
			target.removeEventListener("pointerup", up);
		};
		target.addEventListener("pointermove", move);
		target.addEventListener("pointerup", up);
	}

	function autoFit(col: number) {
		const ctx = document.createElement("canvas").getContext("2d");
		if (!ctx) return;
		const mono = getComputedStyle(document.documentElement).getPropertyValue("--ca-font-mono");
		ctx.font = `13px ${mono}`;
		let w = 64;
		for (const r of rows.slice(0, 2000)) {
			const v = r[col] ?? "NULL";
			w = Math.max(w, ctx.measureText(v.length > 200 ? v.slice(0, 200) : v).width + 20);
		}
		widths[col] = Math.round(Math.min(w, 900));
	}

	function select(row: number, col: number) {
		active = { row, col };
	}

	function moveTo(row: number, col: number) {
		row = Math.min(Math.max(row, 0), display.length - 1);
		col = Math.min(Math.max(col, 0), columns.length - 1);
		active = { row, col };
		ensureVisible(row, col);
	}

	function ensureVisible(row: number, col: number) {
		const top = row * ROW_H;
		const bodyH = viewport.clientHeight - HEAD_H;
		if (top < viewport.scrollTop) viewport.scrollTop = top;
		else if (top + ROW_H > viewport.scrollTop + bodyH) viewport.scrollTop = top + ROW_H - bodyH;
		const left = rowNumWidth + widths.slice(0, col).reduce((a, b) => a + b, 0);
		const right = left + (widths[col] ?? 0);
		if (left - rowNumWidth < viewport.scrollLeft) viewport.scrollLeft = left - rowNumWidth;
		else if (right > viewport.scrollLeft + viewport.clientWidth) viewport.scrollLeft = right - viewport.clientWidth;
		scrollTop = viewport.scrollTop;
	}

	async function copy(text: string, what: string) {
		try {
			await navigator.clipboard.writeText(text);
			toast.success(`${what} copied`);
		} catch {
			toast.error("Could not access the clipboard.");
		}
	}

	const original = (d: DisplayRow) => (d.base === null ? null : rows[d.base]);

	function valueAt(row: number, col: number): string | null {
		const d = display[row];
		if (!d) return null;
		return edits ? cellValue(edits, d.id, original(d), col) : (original(d)?.[col] ?? null);
	}

	function rowValues(row: number): (string | null)[] {
		return columns.map((_, ci) => valueAt(row, ci));
	}

	function rowTsv(row: number): string {
		return cellsToTsv([rowValues(row)]);
	}

	function allTsv(): string {
		return cellsToTsv([columns.map((c) => c.name), ...order.map((i) => rows[i])]);
	}

	// ---- editing -------------------------------------------------------------

	function canEdit(row: number, col: number): boolean {
		const d = display[row];
		return !!editable && !!edits && !!d && editable.columns[col] != null && edits[d.id]?.op !== "delete";
	}

	function startEdit(row: number, col: number, typed?: string) {
		if (!canEdit(row, col)) return;
		const d = display[row];
		const v = valueAt(row, col);
		if (typed === undefined && v !== null && (v.includes("\n") || v.length > INLINE_MAX)) {
			openViewer(row, col);
			return;
		}
		active = { row, col };
		ensureVisible(row, col);
		editing = { id: d.id, base: d.base, col, value: typed ?? v ?? "", wasNull: typed === undefined && v === null, typed: typed !== undefined };
	}

	function writeCell(id: string, base: number | null, col: number, value: string | null) {
		if (!edits) return;
		const d = { id, base };
		const current = cellValue(edits, id, original(d), col);
		if (current === value && !isCellEdited(edits, id, col)) return;
		setCell(edits, id, base, original(d), col, value);
	}

	function commitEdit() {
		const e = editing;
		if (!e) return;
		editing = null;
		writeCell(e.id, e.base, e.col, e.wasNull && e.value === "" ? null : e.value);
	}

	function focusInput(node: HTMLInputElement, typed: boolean) {
		node.focus();
		if (typed) node.setSelectionRange(node.value.length, node.value.length);
		else node.select();
	}

	function inputKey(e: KeyboardEvent) {
		e.stopPropagation();
		const mod = e.ctrlKey || e.metaKey;
		if (e.key === "Enter" || e.key === "Tab") {
			e.preventDefault();
			const cur = active;
			commitEdit();
			viewport.focus();
			if (cur) {
				if (e.key === "Enter") moveTo(cur.row + (e.shiftKey ? -1 : 1), cur.col);
				else moveTo(cur.row, cur.col + (e.shiftKey ? -1 : 1));
			}
		} else if (e.key === "Escape") {
			e.preventDefault();
			editing = null;
			viewport.focus();
		} else if (mod && e.key.toLowerCase() === "s") {
			e.preventDefault();
			commitEdit();
			viewport.focus();
			onsave?.();
		}
	}

	function setNull(row: number, col: number) {
		const d = display[row];
		if (d && canEdit(row, col)) writeCell(d.id, d.base, col, null);
	}

	function revert(row: number, col: number) {
		const d = display[row];
		if (d && edits) revertCell(edits, d.id, col);
	}

	function toggleRowDelete(row: number) {
		const d = display[row];
		if (!d || !edits || !editable) return;
		editing = null;
		toggleDelete(edits, d.id, d.base);
		if (active && active.row >= display.length) active = display.length ? { row: display.length - 1, col: active.col } : null;
	}

	function newRow(values: Record<number, string | null> = {}) {
		if (!edits || !editable) return;
		addPendingRow(edits, values);
		viewport.focus();
		startEdit(display.length - 1, firstEditable);
	}

	function duplicateRow(row: number) {
		if (!editable) return;
		// Key columns stay out: identity keys get their default, others are typed in.
		const values: Record<number, string | null> = {};
		editable.columns.forEach((c, i) => {
			if (c && !editable.key.includes(i)) values[i] = valueAt(row, i);
		});
		newRow(values);
	}

	/** Adds an empty row at the end and starts editing it. */
	export function addRow() {
		newRow();
	}

	// ---- value dialog ---------------------------------------------------------

	function openViewer(row: number, col: number) {
		const v = valueAt(row, col);
		const c = columns[col];
		const d = display[row];
		const edit = canEdit(row, col) ? { id: d.id, base: d.base, col, wasNull: v === null } : null;
		viewerText = v ?? "";
		viewer = {
			title: c.name,
			value: v === null ? "NULL" : formatValue(v, c.kind),
			type: c.typeName,
			edit,
		};
	}

	function applyViewer(value: string | null) {
		const e = viewer?.edit;
		viewer = null;
		if (!e) return;
		writeCell(e.id, e.base, e.col, value === "" && e.wasNull ? null : value);
		viewport.focus();
	}

	function onkeydown(e: KeyboardEvent) {
		// Header buttons (sort, filter) handle their own keys.
		if (e.target instanceof HTMLElement && e.target.closest("button")) return;
		if (display.length === 0 || columns.length === 0) {
			if (editable && edits && e.key === "Insert") newRow();
			return;
		}
		const cur = active ?? { row: 0, col: 0 };
		const page = Math.max(1, Math.floor((viewport.clientHeight - HEAD_H) / ROW_H) - 1);
		let { row, col } = cur;
		const mod = e.ctrlKey || e.metaKey;
		switch (e.key) {
			case "ArrowDown": row++; break;
			case "ArrowUp": row--; break;
			case "ArrowRight": col++; break;
			case "ArrowLeft": col--; break;
			case "PageDown": row += page; break;
			case "PageUp": row -= page; break;
			case "Home": if (mod) row = 0; col = 0; break;
			case "End": if (mod) row = display.length - 1; col = columns.length - 1; break;
			case "Enter":
			case "F2":
				e.preventDefault();
				if (!active) return;
				if (canEdit(active.row, active.col)) startEdit(active.row, active.col);
				else if (e.key === "Enter") openViewer(active.row, active.col);
				return;
			case "Delete":
				if (active && editable && !mod) {
					e.preventDefault();
					toggleRowDelete(active.row);
				}
				return;
			case "Insert":
				if (editable && edits) {
					e.preventDefault();
					newRow();
				}
				return;
			default: {
				const k = e.key.toLowerCase();
				if (mod && k === "c" && active) {
					e.preventDefault();
					if (e.shiftKey) copy(rowTsv(active.row), "Row");
					else copy(valueAt(active.row, active.col) ?? "", "Value");
				} else if (mod && k === "s" && onsave) {
					e.preventDefault();
					e.stopPropagation();
					onsave();
				} else if (mod && k === "z" && active && !e.shiftKey) {
					e.preventDefault();
					revert(active.row, active.col);
				} else if (e.key.length === 1 && !mod && !e.altKey && active && canEdit(active.row, active.col)) {
					// Typing over a cell starts editing it with the typed text.
					e.preventDefault();
					startEdit(active.row, active.col, e.key);
				}
				return;
			}
		}
		e.preventDefault();
		moveTo(row, col);
	}

	function onheadmenu(e: MouseEvent, col: number) {
		e.preventDefault();
		commitEdit();
		headMenu = { x: e.clientX, y: e.clientY, col };
	}

	function oncontextmenu(e: MouseEvent, row: number, col: number) {
		e.preventDefault();
		commitEdit();
		active = { row, col };
		menu = { x: e.clientX, y: e.clientY, row, col };
	}

	export function copyAll() {
		copy(allTsv(), "Result");
	}
</script>

<div class="lq-gridbox">
{#if view.filters.length > 0}
	<div class="lq-filters" role="toolbar" aria-label="Filters">
		<span class="lq-filters__icon" aria-hidden="true"><Funnel size={13} /></span>
		{#each view.filters as f (f.col)}
			<Chip
				size="sm"
				variant="brand"
				label={describeFilter(f, columns)}
				deletable
				ondelete={() => clearFilter(f.col)}
				onclick={() => openFilter(f.col)}
			/>
		{/each}
		<Button size="sm" variant="ghost" disabled={busy} onclick={() => clearFilter()}>
			{#snippet leading()}<FunnelX size={14} />{/snippet}
			Clear filters
		</Button>
	</div>
{/if}
<div
	class="lq-grid"
	class:lq-grid--busy={busy}
	aria-busy={busy}
	bind:this={viewport}
	onscroll={() => (scrollTop = viewport.scrollTop)}
	role="grid"
	aria-label={label}
	aria-rowcount={display.length + 1}
	aria-colcount={columns.length + 1}
	tabindex="0"
	{onkeydown}
>
	<div class="lq-grid__inner" style="--cols:{template};--total-w:{totalWidth}px;--total-h:{HEAD_H + display.length * ROW_H}px">
		<div class="lq-grid__head" role="row" aria-rowindex={1}>
			<div class="lq-grid__corner" role="columnheader">#</div>
			{#each columns as c, ci (ci)}
				{@const readOnly = !!editable && editable.columns[ci] == null}
				{@const filter = filterOf(ci)}
				<div
					class="lq-grid__th"
					class:lq-grid__th--num={c.kind === "number"}
					class:lq-grid__th--ro={readOnly}
					class:lq-grid__th--filtered={!!filter}
					role="columnheader"
					tabindex="-1"
					aria-sort={sort?.col === ci ? (sort.dir === 1 ? "ascending" : "descending") : "none"}
					oncontextmenu={(e) => onheadmenu(e, ci)}
				>
					<button
						class="lq-grid__th-btn"
						type="button"
						onclick={() => toggleSort(ci)}
						title="{c.name} · {c.typeName}{readOnly ? ' · read-only (not a column of ' + editable?.table + ')' : ''}"
					>
						<span class="lq-grid__th-name">{c.name}</span>
						<span class="lq-grid__th-type">{c.typeName}</span>
						{#if sort?.col === ci}
							{#if sort.dir === 1}<ArrowUp size={12} aria-hidden="true" />{:else}<ArrowDown size={12} aria-hidden="true" />{/if}
						{/if}
					</button>
					<button
						class="lq-grid__th-filter"
						type="button"
						aria-label="Filter {c.name}"
						aria-pressed={!!filter}
						title={filter ? `Filter: ${describeFilter(filter, columns)}` : `Filter ${c.name}`}
						onclick={() => openFilter(ci)}
					>
						<Funnel size={12} aria-hidden="true" />
					</button>
					<span
						class="lq-grid__resize"
						role="separator"
						aria-orientation="vertical"
						aria-label="Resize {c.name}"
						onpointerdown={(e) => startResize(e, ci)}
						ondblclick={() => autoFit(ci)}
					></span>
				</div>
			{/each}
		</div>
		{#each visible as v (v.id)}
			{@const change = edits?.[v.id]}
			<div
				class="lq-grid__row"
				class:lq-grid__row--odd={v.display % 2 === 1}
				class:lq-grid__row--deleted={change?.op === "delete"}
				class:lq-grid__row--inserted={change?.op === "insert"}
				role="row"
				aria-rowindex={v.display + 2}
				style="--top:{HEAD_H + v.display * ROW_H}px"
			>
				<div
					class="lq-grid__rownum"
					role="rowheader"
					title={change?.op === "insert" ? "New row" : change?.op === "delete" ? "Deleted on save" : undefined}
				>
					{change?.op === "insert" ? "+" : v.display + 1}
				</div>
				{#each columns as c, ci (ci)}
					{@const value = edits ? cellValue(edits, v.id, original(v), ci) : rows[v.base ?? 0][ci]}
					{@const isDefault = change?.op === "insert" && !(ci in change.values)}
					{@const isActive = active?.row === v.display && active?.col === ci}
					{@const isEditing = editing?.id === v.id && editing?.col === ci}
					<div
						class="lq-grid__td"
						class:lq-grid__td--num={c.kind === "number"}
						class:lq-grid__td--null={value === null}
						class:lq-grid__td--edited={!!edits && isCellEdited(edits, v.id, ci)}
						class:lq-grid__td--active={isActive}
						class:lq-grid__td--editing={isEditing}
						role="gridcell"
						tabindex="-1"
						aria-colindex={ci + 2}
						aria-selected={isActive}
						onclick={() => select(v.display, ci)}
						ondblclick={() => (canEdit(v.display, ci) ? startEdit(v.display, ci) : openViewer(v.display, ci))}
						oncontextmenu={(e) => oncontextmenu(e, v.display, ci)}
						onkeydown={() => {}}
					>
						{#if isEditing && editing}
							<input
								class="lq-grid__input"
								aria-label="Edit {c.name}"
								spellcheck="false"
								autocomplete="off"
								placeholder={editing.wasNull ? "NULL" : ""}
								bind:value={editing.value}
								use:focusInput={editing.typed}
								onkeydown={inputKey}
								onblur={commitEdit}
							/>
						{:else if isDefault}
							<span class="lq-grid__default">DEFAULT</span>
						{:else}
							{value === null ? "NULL" : value.length > 300 ? value.slice(0, 300) + "…" : value}
						{/if}
					</div>
				{/each}
			</div>
		{/each}
	</div>
	{#if display.length === 0 && view.filters.length > 0}
		<p class="lq-grid__empty">No rows match the filters.</p>
	{/if}
</div>
</div>

{#if headMenu}
	{@const h = headMenu}
	{@const f = filterOf(h.col)}
	<DropdownMenu
		open={true}
		anchor={{ x: h.x, y: h.y }}
		onOpenChange={(o) => {
			// After the menu has finished closing: it still reads its anchor.
			if (!o) queueMicrotask(() => (headMenu = null));
		}}
	>
		<DropdownMenuItem onclick={() => sortBy(h.col, 1)}>
			{#snippet icon()}<ArrowUp size={15} />{/snippet}
			Sort ascending
		</DropdownMenuItem>
		<DropdownMenuItem onclick={() => sortBy(h.col, -1)}>
			{#snippet icon()}<ArrowDown size={15} />{/snippet}
			Sort descending
		</DropdownMenuItem>
		{#if sort}
			<DropdownMenuItem onclick={() => sortBy(h.col, null)}>
				{#snippet icon()}<ArrowUpDown size={15} />{/snippet}
				Clear sort
			</DropdownMenuItem>
		{/if}
		<DropdownMenuSeparator />
		<DropdownMenuItem onclick={() => openFilter(h.col)}>
			{#snippet icon()}<Funnel size={15} />{/snippet}
			Filter…
		</DropdownMenuItem>
		<DropdownMenuItem onclick={() => applyFilter({ col: h.col, op: "null", value: "" })}>
			{#snippet icon()}<CircleSlash size={15} />{/snippet}
			Show only NULL
		</DropdownMenuItem>
		<DropdownMenuItem onclick={() => applyFilter({ col: h.col, op: "notnull", value: "" })}>
			{#snippet icon()}<Ban size={15} />{/snippet}
			Hide NULL
		</DropdownMenuItem>
		{#if f}
			<DropdownMenuItem onclick={() => clearFilter(h.col)}>
				{#snippet icon()}<FunnelX size={15} />{/snippet}
				Clear filter on {columns[h.col].name}
			</DropdownMenuItem>
		{/if}
		{#if view.filters.length > (f ? 1 : 0)}
			<DropdownMenuItem onclick={() => clearFilter()}>
				{#snippet icon()}<FunnelX size={15} />{/snippet}
				Clear all filters
			</DropdownMenuItem>
		{/if}
		<DropdownMenuSeparator />
		<DropdownMenuItem onclick={() => copy(columns[h.col].name, "Column name")}>
			{#snippet icon()}<Table2 size={15} />{/snippet}
			Copy column name
		</DropdownMenuItem>
		<DropdownMenuItem onclick={() => autoFit(h.col)}>
			{#snippet icon()}<MoveHorizontal size={15} />{/snippet}
			Fit width to values
		</DropdownMenuItem>
	</DropdownMenu>
{/if}

{#if menu}
	{@const m = menu}
	{@const cellEditable = canEdit(m.row, m.col)}
	{@const rowChange = edits?.[display[m.row]?.id ?? ""]}
	<DropdownMenu
		open={true}
		anchor={{ x: m.x, y: m.y }}
		onOpenChange={(o) => {
			if (!o) queueMicrotask(() => (menu = null));
		}}
	>
		{#if cellEditable}
			<DropdownMenuItem onclick={() => startEdit(m.row, m.col)}>
				{#snippet icon()}<Pencil size={15} />{/snippet}
				Edit value
			</DropdownMenuItem>
			<DropdownMenuItem onclick={() => setNull(m.row, m.col)}>
				{#snippet icon()}<CircleSlash size={15} />{/snippet}
				Set NULL
			</DropdownMenuItem>
			{#if edits && isCellEdited(edits, display[m.row].id, m.col)}
				<DropdownMenuItem onclick={() => revert(m.row, m.col)}>
					{#snippet icon()}<Undo2 size={15} />{/snippet}
					Revert value
				</DropdownMenuItem>
			{/if}
			<DropdownMenuSeparator />
		{/if}
		<DropdownMenuItem onclick={() => copy(valueAt(m.row, m.col) ?? "", "Value")}>
			{#snippet icon()}<Copy size={15} />{/snippet}
			Copy value
		</DropdownMenuItem>
		<DropdownMenuItem onclick={() => copy(rowTsv(m.row), "Row")}>
			{#snippet icon()}<Rows3 size={15} />{/snippet}
			Copy row
		</DropdownMenuItem>
		<DropdownMenuItem onclick={() => copy(columns[m.col].name, "Column name")}>
			{#snippet icon()}<Table2 size={15} />{/snippet}
			Copy column name
		</DropdownMenuItem>
		<DropdownMenuSeparator />
		<DropdownMenuItem onclick={() => filterByValue(m.row, m.col, false)}>
			{#snippet icon()}<FunnelPlus size={15} />{/snippet}
			Filter by this value
		</DropdownMenuItem>
		<DropdownMenuItem onclick={() => filterByValue(m.row, m.col, true)}>
			{#snippet icon()}<Ban size={15} />{/snippet}
			Exclude this value
		</DropdownMenuItem>
		{#if filterOf(m.col)}
			<DropdownMenuItem onclick={() => clearFilter(m.col)}>
				{#snippet icon()}<FunnelX size={15} />{/snippet}
				Clear filter on {columns[m.col].name}
			</DropdownMenuItem>
		{/if}
		<DropdownMenuSeparator />
		<DropdownMenuItem onclick={() => openViewer(m.row, m.col)}>
			{#snippet icon()}<Eye size={15} />{/snippet}
			View full value
		</DropdownMenuItem>
		<DropdownMenuItem onclick={() => copy(allTsv(), "Result")}>
			{#snippet icon()}<Copy size={15} />{/snippet}
			Copy all (with header)
		</DropdownMenuItem>
		{#if editable && edits}
			<DropdownMenuSeparator />
			<DropdownMenuItem onclick={() => newRow()}>
				{#snippet icon()}<Plus size={15} />{/snippet}
				Add row
			</DropdownMenuItem>
			{#if rowChange?.op !== "delete"}
				<DropdownMenuItem onclick={() => duplicateRow(m.row)}>
					{#snippet icon()}<CopyPlus size={15} />{/snippet}
					Duplicate row
				</DropdownMenuItem>
			{/if}
			{#if rowChange?.op === "delete"}
				<DropdownMenuItem onclick={() => toggleRowDelete(m.row)}>
					{#snippet icon()}<RotateCcw size={15} />{/snippet}
					Restore row
				</DropdownMenuItem>
			{:else}
				<DropdownMenuItem onclick={() => toggleRowDelete(m.row)}>
					{#snippet icon()}<Trash2 size={15} />{/snippet}
					{rowChange?.op === "insert" ? "Remove new row" : "Delete row"}
				</DropdownMenuItem>
			{/if}
		{/if}
	</DropdownMenu>
{/if}

{#if filterEdit}
	{@const fe = filterEdit}
	{@const fc = columns[fe.col]}
	<Dialog
		open={true}
		title="Filter {fc.name}"
		description={fc.typeName}
		size="sm"
		onOpenChange={(o) => {
			if (!o) filterEdit = null;
		}}
	>
		<!-- svelte-ignore a11y_no_static_element_interactions -->
		<div
			class="lq-filter-form"
			onkeydown={(e) => {
				if (e.key === "Enter" && e.target instanceof HTMLInputElement) {
					e.preventDefault();
					submitFilter();
				}
			}}
		>
			<Select
				label="Condition"
				size="sm"
				options={FILTER_OPS}
				value={fe.op}
				onchange={(v) => {
					if (filterEdit) filterEdit.op = v as FilterOp;
				}}
			/>
			{#if opNeedsValue(fe.op)}
				<TextField
					label="Value"
					size="sm"
					spellcheck="false"
					autocomplete="off"
					placeholder={fe.op === "contains" ? "Text to find (any case)" : "Exact value"}
					bind:ref={filterInput}
					bind:value={fe.value}
				/>
			{/if}
		</div>
		{#snippet actions()}
			{#if filterEdit && filterOf(filterEdit.col)}
				<Button
					variant="ghost"
					onclick={() => {
						const col = filterEdit?.col;
						filterEdit = null;
						clearFilter(col);
					}}
				>
					{#snippet leading()}<FunnelX size={14} />{/snippet}
					Remove filter
				</Button>
			{/if}
			<Button variant="outlined" onclick={() => (filterEdit = null)}>Cancel</Button>
			<Button variant="filled" onclick={submitFilter}>Apply</Button>
		{/snippet}
	</Dialog>
{/if}

{#if viewer}
	<Dialog
		open={true}
		title={viewer.title}
		description={viewer.edit ? `${viewer.type} · editing` : viewer.type}
		size="lg"
		onOpenChange={(o) => {
			if (!o) viewer = null;
		}}
	>
		{#if viewer.edit}
			<TextArea
				label="Value"
				hideLabel
				class="lq-viewer-edit"
				rows={14}
				spellcheck={false}
				placeholder={viewer.edit.wasNull ? "NULL" : ""}
				bind:value={viewerText}
			/>
		{:else}
			<pre class="lq-viewer">{viewer.value}</pre>
		{/if}
		{#snippet actions()}
			{#if viewer?.edit}
				<Button variant="ghost" onclick={() => applyViewer(null)}>
					{#snippet leading()}<CircleSlash size={14} />{/snippet}
					Set NULL
				</Button>
				<Button variant="outlined" onclick={() => (viewer = null)}>Cancel</Button>
				<Button variant="filled" onclick={() => applyViewer(viewerText)}>Apply</Button>
			{:else}
				<CopyButton value={viewer?.value ?? ""} label="Copy" variant="outlined" />
				<Button variant="filled" onclick={() => (viewer = null)}>Close</Button>
			{/if}
		{/snippet}
	</Dialog>
{/if}

<style>
	.lq-gridbox {
		display: flex;
		flex-direction: column;
		height: 100%;
		min-height: 0;
	}
	.lq-filters {
		display: flex;
		flex-wrap: wrap;
		align-items: center;
		gap: 6px;
		padding: 6px 8px;
		border-bottom: 1px solid var(--ca-outline-subtle);
		background: var(--ca-surface-sunken);
	}
	.lq-filters__icon {
		display: inline-flex;
		color: var(--ca-text-hint);
	}
	.lq-grid {
		position: relative;
		flex: 1;
		height: 100%;
		min-height: 0;
		overflow: auto;
		background: var(--ca-surface-raised);
		font-family: var(--ca-font-mono);
		font-size: 13px;
		outline: none;
		overscroll-behavior: contain;
	}
	.lq-grid--busy .lq-grid__inner {
		opacity: 0.55;
		cursor: progress;
	}
	.lq-grid:focus-visible {
		box-shadow: inset 0 0 0 2px var(--ca-focus-ring);
	}
	.lq-grid__inner {
		position: relative;
		width: var(--total-w);
		min-width: 100%;
		height: var(--total-h);
	}
	.lq-grid__head,
	.lq-grid__row {
		display: grid;
		grid-template-columns: var(--cols);
		width: var(--total-w);
	}
	.lq-grid__head {
		position: sticky;
		top: 0;
		z-index: 2;
		height: 32px;
		background: var(--ca-surface-sunken);
		border-bottom: 1px solid var(--ca-outline-default);
		font-family: var(--ca-font-sans);
	}
	.lq-grid__corner,
	.lq-grid__rownum {
		position: sticky;
		left: 0;
		z-index: 1;
		display: flex;
		align-items: center;
		justify-content: flex-end;
		padding: 0 8px;
		background: var(--ca-surface-sunken);
		border-right: 1px solid var(--ca-outline-subtle);
		color: var(--ca-text-hint);
		font-size: 12px;
		font-variant-numeric: tabular-nums;
	}
	.lq-grid__corner {
		z-index: 3;
	}
	.lq-grid__th {
		position: relative;
		min-width: 0;
		border-right: 1px solid var(--ca-outline-subtle);
	}
	.lq-grid__th-filter {
		position: absolute;
		top: 50%;
		right: 6px;
		z-index: 1;
		display: inline-flex;
		align-items: center;
		justify-content: center;
		width: 20px;
		height: 20px;
		padding: 0;
		border: 0;
		background: transparent;
		color: var(--ca-text-hint);
		opacity: 0;
		cursor: pointer;
		transform: translateY(-50%);
	}
	.lq-grid__th:hover .lq-grid__th-filter,
	.lq-grid__th-filter:focus-visible,
	.lq-grid__th--filtered .lq-grid__th-filter {
		opacity: 1;
	}
	.lq-grid__th-filter:hover {
		background: var(--ca-action-hover);
		color: var(--ca-text-primary);
	}
	.lq-grid__th-filter:focus-visible {
		outline: 2px solid var(--ca-focus-ring);
		outline-offset: -2px;
	}
	.lq-grid__th--filtered .lq-grid__th-filter,
	.lq-grid__th--filtered .lq-grid__th-name {
		color: var(--ca-brand-text, var(--ca-state-indicator));
	}
	.lq-grid__empty {
		position: sticky;
		left: 0;
		margin: 0;
		padding: 16px;
		color: var(--ca-text-hint);
		font-family: var(--ca-font-sans);
		font-size: 13px;
	}
	.lq-filter-form {
		display: flex;
		flex-direction: column;
		gap: 12px;
	}
	.lq-grid__th-btn {
		display: grid;
		grid-template-columns: minmax(0, 1fr) auto;
		grid-template-rows: auto auto;
		align-items: center;
		column-gap: 4px;
		width: 100%;
		height: 100%;
		padding: 2px 30px 2px 8px;
		border: 0;
		background: transparent;
		color: var(--ca-text-primary);
		text-align: left;
		cursor: pointer;
	}
	.lq-grid__th--num .lq-grid__th-btn {
		text-align: right;
	}
	.lq-grid__th-btn:hover {
		background: var(--ca-action-hover);
	}
	.lq-grid__th-btn:focus-visible {
		outline: 2px solid var(--ca-focus-ring);
		outline-offset: -2px;
	}
	.lq-grid__th-btn :global(svg) {
		grid-row: 1 / span 2;
		grid-column: 2;
	}
	.lq-grid__th-name {
		grid-row: 1;
		grid-column: 1;
		overflow: hidden;
		font-size: 13px;
		font-weight: 650;
		line-height: 15px;
		white-space: nowrap;
		text-overflow: ellipsis;
	}
	.lq-grid__th-type {
		grid-row: 2;
		grid-column: 1;
		overflow: hidden;
		color: var(--ca-text-hint);
		font-size: 11px;
		line-height: 12px;
		white-space: nowrap;
		text-overflow: ellipsis;
	}
	.lq-grid__th--ro .lq-grid__th-name {
		color: var(--ca-text-secondary);
		font-weight: 500;
	}
	.lq-grid__th--ro .lq-grid__th-type {
		font-style: italic;
	}
	.lq-grid__resize {
		position: absolute;
		top: 0;
		right: -4px;
		z-index: 1;
		width: 8px;
		height: 100%;
		cursor: col-resize;
		touch-action: none;
	}
	.lq-grid__resize:hover {
		background: linear-gradient(var(--ca-brand-main), var(--ca-brand-main)) center / 2px 100% no-repeat;
	}
	.lq-grid__row {
		position: absolute;
		left: 0;
		top: 0;
		height: 26px;
		transform: translateY(var(--top));
		border-bottom: 1px solid var(--ca-outline-subtle);
	}
	.lq-grid__row--odd {
		background: var(--lq-zebra);
	}
	.lq-grid__row--odd .lq-grid__rownum {
		background: color-mix(in oklab, var(--ca-surface-sunken) 92%, var(--ca-text-primary));
	}
	.lq-grid__row:hover {
		background: var(--ca-action-hover);
	}
	.lq-grid__row--inserted {
		background: color-mix(in oklab, var(--ca-semantic-success) 9%, transparent);
	}
	.lq-grid__row--inserted .lq-grid__rownum {
		color: var(--ca-semantic-success);
		font-weight: 700;
	}
	.lq-grid__row--deleted {
		background: color-mix(in oklab, var(--ca-semantic-error) 9%, transparent);
	}
	.lq-grid__row--deleted .lq-grid__td {
		color: var(--ca-text-hint);
		text-decoration: line-through;
		text-decoration-color: var(--ca-semantic-error);
	}
	.lq-grid__row--deleted .lq-grid__rownum {
		color: var(--ca-semantic-error);
	}
	.lq-grid__td {
		overflow: hidden;
		min-width: 0;
		padding: 0 8px;
		border-right: 1px solid var(--ca-outline-subtle);
		line-height: 25px;
		white-space: pre;
		text-overflow: ellipsis;
		cursor: default;
	}
	.lq-grid__td--num {
		text-align: right;
		font-variant-numeric: tabular-nums;
	}
	.lq-grid__td--null {
		color: var(--ca-text-hint);
		font-style: italic;
	}
	.lq-grid__td--edited {
		background: color-mix(in oklab, var(--ca-brand-main) 30%, transparent);
		color: var(--ca-text-primary);
	}
	.lq-grid__td--active {
		box-shadow: inset 0 0 0 2px var(--ca-state-indicator);
		background: var(--ca-action-selected);
	}
	.lq-grid__td--active.lq-grid__td--edited {
		background: color-mix(in oklab, var(--ca-brand-main) 42%, transparent);
	}
	.lq-grid__td--editing {
		padding: 0;
		overflow: visible;
	}
	.lq-grid__input {
		display: block;
		width: 100%;
		height: 100%;
		margin: 0;
		padding: 0 7px;
		border: 0;
		outline: 2px solid var(--ca-brand-main);
		outline-offset: -2px;
		background: var(--ca-surface-raised);
		color: var(--ca-text-primary);
		font: inherit;
		font-style: normal;
		text-align: inherit;
	}
	.lq-grid__input::placeholder {
		color: var(--ca-text-hint);
		font-style: italic;
	}
	.lq-grid__default {
		color: var(--ca-text-hint);
		font-size: 11px;
		font-style: italic;
		letter-spacing: 0.04em;
	}
	.lq-viewer {
		max-height: 60vh;
		margin: 0;
		padding: 12px;
		overflow: auto;
		background: var(--ca-surface-sunken);
		border: 1px solid var(--ca-outline-subtle);
		font-family: var(--ca-font-mono);
		font-size: 13px;
		white-space: pre-wrap;
		word-break: break-word;
	}
	:global(.lq-viewer-edit textarea) {
		font-family: var(--ca-font-mono);
		font-size: 13px;
	}
</style>
