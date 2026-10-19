<script lang="ts">
	import { onMount } from "svelte";
	import {
		Dialog,
		Button,
		CopyButton,
		DropdownMenu,
		DropdownMenuItem,
		DropdownMenuSeparator,
		toast,
	} from "@codeatlasdev/ui";
	import { ArrowDown, ArrowUp, Copy, Rows3, Table2, Eye } from "@lucide/svelte";
	import type { ColumnInfo } from "../api";
	import { cellsToTsv, formatValue } from "../export";

	interface Props {
		columns: ColumnInfo[];
		rows: (string | null)[][];
		label?: string;
	}

	let { columns, rows, label = "Result" }: Props = $props();

	const ROW_H = 26;
	const HEAD_H = 32;
	const OVERSCAN = 8;

	let viewport: HTMLDivElement;
	let scrollTop = $state(0);
	let height = $state(400);
	let widths = $state<number[]>([]);
	let sort = $state<{ col: number; dir: 1 | -1 } | null>(null);
	let active = $state<{ row: number; col: number } | null>(null);
	let viewer = $state<{ title: string; value: string; type: string } | null>(null);
	let menu = $state<{ x: number; y: number; row: number; col: number } | null>(null);

	// Display indexes (local sort, no new query).
	const order = $derived.by(() => {
		const idx = rows.map((_, i) => i);
		if (!sort) return idx;
		const { col, dir } = sort;
		const numeric = columns[col]?.kind === "number";
		const collator = new Intl.Collator(undefined, { numeric: true, sensitivity: "base" });
		idx.sort((a, b) => {
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
		return idx;
	});

	const rowNumWidth = $derived(Math.max(44, String(rows.length).length * 9 + 22));
	const template = $derived(`${rowNumWidth}px ${widths.map((w) => `${w}px`).join(" ")}`);
	const totalWidth = $derived(rowNumWidth + widths.reduce((a, b) => a + b, 0));
	const first = $derived(Math.max(0, Math.floor(scrollTop / ROW_H) - OVERSCAN));
	const last = $derived(Math.min(order.length, Math.ceil((scrollTop + height) / ROW_H) + OVERSCAN));
	const visible = $derived(order.slice(first, last).map((r, i) => ({ display: first + i, row: r })));

	function measureWidths() {
		const canvas = document.createElement("canvas");
		const ctx = canvas.getContext("2d");
		const mono = getComputedStyle(document.documentElement).getPropertyValue("--ca-font-mono") || "monospace";
		const sans = getComputedStyle(document.documentElement).getPropertyValue("--ca-font-sans") || "sans-serif";
		const sample = rows.slice(0, 200);
		widths = columns.map((c, ci) => {
			if (!ctx) return 160;
			ctx.font = `600 13px ${sans}`;
			let w = ctx.measureText(c.name).width + 34;
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

	// New result: recompute widths, clear sort and selection.
	$effect(() => {
		void columns;
		void rows;
		measureWidths();
		sort = null;
		active = null;
		if (viewport) viewport.scrollTop = 0;
		scrollTop = 0;
	});

	onMount(() => {
		const ro = new ResizeObserver(() => (height = viewport.clientHeight));
		ro.observe(viewport);
		return () => ro.disconnect();
	});

	function toggleSort(col: number) {
		if (sort?.col !== col) sort = { col, dir: 1 };
		else if (sort.dir === 1) sort = { col, dir: -1 };
		else sort = null;
	}

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

	function select(display: number, col: number) {
		active = { row: display, col };
	}

	function ensureVisible(display: number, col: number) {
		const top = display * ROW_H;
		const bodyH = viewport.clientHeight - HEAD_H;
		if (top < viewport.scrollTop) viewport.scrollTop = top;
		else if (top + ROW_H > viewport.scrollTop + bodyH) viewport.scrollTop = top + ROW_H - bodyH;
		const left = rowNumWidth + widths.slice(0, col).reduce((a, b) => a + b, 0);
		const right = left + (widths[col] ?? 0);
		if (left - rowNumWidth < viewport.scrollLeft) viewport.scrollLeft = left - rowNumWidth;
		else if (right > viewport.scrollLeft + viewport.clientWidth) viewport.scrollLeft = right - viewport.clientWidth;
	}

	async function copy(text: string, what: string) {
		try {
			await navigator.clipboard.writeText(text);
			toast.success(`${what} copied`);
		} catch {
			toast.error("Could not access the clipboard.");
		}
	}

	function valueAt(display: number, col: number): string | null {
		return rows[order[display]]?.[col] ?? null;
	}

	function rowTsv(display: number): string {
		return cellsToTsv([rows[order[display]]]);
	}

	function allTsv(): string {
		return cellsToTsv([columns.map((c) => c.name), ...order.map((i) => rows[i])]);
	}

	function openViewer(display: number, col: number) {
		const v = valueAt(display, col);
		const c = columns[col];
		viewer = { title: c.name, value: v === null ? "NULL" : formatValue(v, c.kind), type: c.typeName };
	}

	function onkeydown(e: KeyboardEvent) {
		if (rows.length === 0 || columns.length === 0) return;
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
			case "End": if (mod) row = rows.length - 1; col = columns.length - 1; break;
			case "Enter":
				if (active) openViewer(active.row, active.col);
				e.preventDefault();
				return;
			case "c":
			case "C":
				if (mod && active) {
					e.preventDefault();
					if (e.shiftKey) copy(rowTsv(active.row), "Row");
					else copy(valueAt(active.row, active.col) ?? "", "Value");
				}
				return;
			default:
				return;
		}
		e.preventDefault();
		row = Math.min(Math.max(row, 0), rows.length - 1);
		col = Math.min(Math.max(col, 0), columns.length - 1);
		active = { row, col };
		ensureVisible(row, col);
	}

	function oncontextmenu(e: MouseEvent, display: number, col: number) {
		e.preventDefault();
		active = { row: display, col };
		menu = { x: e.clientX, y: e.clientY, row: display, col };
	}

	export function copyAll() {
		copy(allTsv(), "Result");
	}
</script>

<div
	class="lq-grid"
	bind:this={viewport}
	onscroll={() => (scrollTop = viewport.scrollTop)}
	role="grid"
	aria-label={label}
	aria-rowcount={rows.length + 1}
	aria-colcount={columns.length + 1}
	tabindex="0"
	{onkeydown}
>
	<div class="lq-grid__inner" style="--cols:{template};--total-w:{totalWidth}px;--total-h:{HEAD_H + rows.length * ROW_H}px">
		<div class="lq-grid__head" role="row" aria-rowindex={1}>
			<div class="lq-grid__corner" role="columnheader">#</div>
			{#each columns as c, ci (ci)}
				<div
					class="lq-grid__th"
					class:lq-grid__th--num={c.kind === "number"}
					role="columnheader"
					aria-sort={sort?.col === ci ? (sort.dir === 1 ? "ascending" : "descending") : "none"}
				>
					<button class="lq-grid__th-btn" type="button" onclick={() => toggleSort(ci)} title="{c.name} · {c.typeName}">
						<span class="lq-grid__th-name">{c.name}</span>
						<span class="lq-grid__th-type">{c.typeName}</span>
						{#if sort?.col === ci}
							{#if sort.dir === 1}<ArrowUp size={12} aria-hidden="true" />{:else}<ArrowDown size={12} aria-hidden="true" />{/if}
						{/if}
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
		{#each visible as v (v.row)}
			<div
				class="lq-grid__row"
				class:lq-grid__row--odd={v.display % 2 === 1}
				role="row"
				aria-rowindex={v.display + 2}
				style="--top:{HEAD_H + v.display * ROW_H}px"
			>
				<div class="lq-grid__rownum" role="rowheader">{v.display + 1}</div>
				{#each columns as c, ci (ci)}
					{@const value = rows[v.row][ci]}
					<div
						class="lq-grid__td"
						class:lq-grid__td--num={c.kind === "number"}
						class:lq-grid__td--null={value === null}
						class:lq-grid__td--active={active?.row === v.display && active?.col === ci}
						role="gridcell"
						tabindex="-1"
						aria-colindex={ci + 2}
						aria-selected={active?.row === v.display && active?.col === ci}
						onclick={() => select(v.display, ci)}
						ondblclick={() => openViewer(v.display, ci)}
						oncontextmenu={(e) => oncontextmenu(e, v.display, ci)}
						onkeydown={() => {}}
					>
						{value === null ? "NULL" : value.length > 300 ? value.slice(0, 300) + "…" : value}
					</div>
				{/each}
			</div>
		{/each}
	</div>
</div>

{#if menu}
	<DropdownMenu
		open={true}
		anchor={{ x: menu.x, y: menu.y }}
		onOpenChange={(o) => {
			if (!o) menu = null;
		}}
	>
		<DropdownMenuItem onclick={() => menu && copy(valueAt(menu.row, menu.col) ?? "", "Value")}>
			{#snippet icon()}<Copy size={15} />{/snippet}
			Copy value
		</DropdownMenuItem>
		<DropdownMenuItem onclick={() => menu && copy(rowTsv(menu.row), "Row")}>
			{#snippet icon()}<Rows3 size={15} />{/snippet}
			Copy row
		</DropdownMenuItem>
		<DropdownMenuItem onclick={() => menu && copy(columns[menu.col].name, "Column name")}>
			{#snippet icon()}<Table2 size={15} />{/snippet}
			Copy column name
		</DropdownMenuItem>
		<DropdownMenuSeparator />
		<DropdownMenuItem onclick={() => menu && openViewer(menu.row, menu.col)}>
			{#snippet icon()}<Eye size={15} />{/snippet}
			View full value
		</DropdownMenuItem>
		<DropdownMenuItem onclick={() => copy(allTsv(), "Result")}>
			{#snippet icon()}<Copy size={15} />{/snippet}
			Copy all (with header)
		</DropdownMenuItem>
	</DropdownMenu>
{/if}

{#if viewer}
	<Dialog
		open={true}
		title={viewer.title}
		description={viewer.type}
		size="lg"
		onOpenChange={(o) => {
			if (!o) viewer = null;
		}}
	>
		<pre class="lq-viewer">{viewer.value}</pre>
		{#snippet actions()}
			<CopyButton value={viewer?.value ?? ""} label="Copy" variant="outlined" />
			<Button variant="filled" onclick={() => (viewer = null)}>Close</Button>
		{/snippet}
	</Dialog>
{/if}

<style>
	.lq-grid {
		position: relative;
		height: 100%;
		min-height: 0;
		overflow: auto;
		background: var(--ca-surface-raised);
		font-family: var(--ca-font-mono);
		font-size: 13px;
		outline: none;
		overscroll-behavior: contain;
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
	.lq-grid__th-btn {
		display: grid;
		grid-template-columns: minmax(0, 1fr) auto;
		grid-template-rows: auto auto;
		align-items: center;
		column-gap: 4px;
		width: 100%;
		height: 100%;
		padding: 2px 10px 2px 8px;
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
	.lq-grid__td--active {
		box-shadow: inset 0 0 0 2px var(--ca-state-indicator);
		background: var(--ca-action-selected);
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
</style>
