<script lang="ts">
	import { Tabs, TabsList, TabsTrigger, TabsContent, Button, Chip, Tooltip, EmptyState, Spinner, toast } from "@codeatlasdev/ui";
	import {
		Copy,
		FileDown,
		FileJson,
		MessageSquareText,
		TableProperties,
		CircleAlert,
		CircleCheck,
		Info,
		TriangleAlert,
		Lock,
		Plus,
		Save,
		Undo2,
	} from "@lucide/svelte";
	import { invoke } from "@tauri-apps/api/core";
	import { save } from "@tauri-apps/plugin-dialog";
	import ResultGrid from "./ResultGrid.svelte";
	import { workspace, type QueryTab, type ResultSet } from "../state/workspace.svelte";
	import { pendingCount } from "../state/edits";
	import { cellsToTsv, toCsv, toJson } from "../export";
	import { viewOrder, viewRows } from "../view";

	interface Props {
		tab: QueryTab;
	}

	let { tab }: Props = $props();

	const current = $derived(tab.results.find((r) => r.id === tab.activeResult) ?? null);
	const errors = $derived(tab.log.filter((l) => l.level === "error").length);
	const pending = $derived(current ? pendingCount(current.edits) : 0);

	// Rows passing the filters applied in memory (server filters already shape the rows).
	const matching = $derived(
		current && !current.serverView && current.view.filters.length > 0
			? viewOrder(current.result.columns ?? [], current.result.rows, { ...current.view, sort: null }).length
			: null,
	);

	let grids = $state<Record<string, ResultGrid | undefined>>({});

	let logEl = $state<HTMLOListElement | null>(null);
	$effect(() => {
		void tab.log.length;
		if (logEl) logEl.scrollTop = logEl.scrollHeight;
	});

	// Rows in the order the grid shows them.
	const shown = (r: ResultSet) =>
		r.serverView ? r.result.rows : viewRows(r.result.columns ?? [], r.result.rows, r.view);

	async function copyAll(r: ResultSet) {
		const cols = r.result.columns ?? [];
		try {
			await navigator.clipboard.writeText(cellsToTsv([cols.map((c) => c.name), ...shown(r)]));
			toast.success("Result copied (TSV)");
		} catch {
			toast.error("Could not access the clipboard.");
		}
	}

	async function exportAs(r: ResultSet, format: "csv" | "json") {
		const cols = r.result.columns ?? [];
		const path = await save({
			title: "Export result",
			defaultPath: `result.${format}`,
			filters: [{ name: format.toUpperCase(), extensions: [format] }],
		});
		if (!path) return;
		const rows = shown(r);
		const content = format === "csv" ? "﻿" + toCsv(cols, rows) : toJson(cols, rows);
		try {
			await invoke("write_text_file", { path, content });
			const n = rows.length;
			toast.success(`Exported ${n.toLocaleString()} ${n === 1 ? "row" : "rows"}`);
		} catch (e) {
			toast.error(`Could not save: ${(e as { message?: string }).message ?? e}`);
		}
	}

	const time = (d: Date) => d.toLocaleTimeString(undefined, { hour12: false });
</script>

<section class="lq-results" aria-label="Results">
	<Tabs bind:value={tab.activeResult} variant="line" class="lq-results__tabs">
		<div class="lq-results__bar">
			<TabsList fade>
				{#each tab.results as r (r.id)}
					<TabsTrigger value={r.id}>
						{#snippet icon()}<TableProperties size={14} />{/snippet}
						{r.title}
					</TabsTrigger>
				{/each}
				<TabsTrigger value="messages">
					{#snippet icon()}<MessageSquareText size={14} />{/snippet}
					Messages{errors > 0 ? ` (${errors})` : ""}
				</TabsTrigger>
			</TabsList>
			{#if current}
				<div class="lq-results__meta">
					{#if current.result.editable}
						{#if pending > 0}
							<span class="lq-results__pending" aria-live="polite">
								{pending} {pending === 1 ? "change" : "changes"}
							</span>
							<Button size="sm" variant="ghost" disabled={tab.saving} onclick={() => workspace.discardEdits(current)}>
								{#snippet leading()}<Undo2 size={14} />{/snippet}
								Discard
							</Button>
							<Tooltip content="Write the changes to {current.result.editable.table} (Ctrl+S)">
								{#snippet triggerChild(props)}
									<Button
										{...props}
										size="sm"
										variant="filled"
										loading={tab.saving}
										disabled={tab.running}
										onclick={() => workspace.saveEdits(tab, current)}
									>
										{#snippet leading()}<Save size={14} />{/snippet}
										Save
									</Button>
								{/snippet}
							</Tooltip>
						{/if}
						<Tooltip content="Add a row to {current.result.editable.table} (Insert)">
							{#snippet triggerChild(props)}
								<Button {...props} size="sm" variant="ghost" onclick={() => grids[current.id]?.addRow()}>
									{#snippet leading()}<Plus size={14} />{/snippet}
									Row
								</Button>
							{/snippet}
						</Tooltip>
					{:else if current.result.readOnlyReason}
						<Tooltip content={current.result.readOnlyReason}>
							<Chip label="Read-only" size="sm" icon={Lock} />
						</Tooltip>
					{/if}
					<span class="lq-results__count">
						{#if matching !== null}{matching.toLocaleString()} of{/if}
						{current.result.rows.length.toLocaleString()}
						{current.result.rows.length === 1 ? "row" : "rows"} · {current.result.elapsedMs} ms
					</span>
					{#if current.result.truncated}
						<Tooltip
							content="More rows available. Raise the limit to fetch more. {current.serverView
								? 'Sorting and filtering run the query again on the server.'
								: 'Sorting and filtering cover the fetched rows only.'}"
						>
							<Chip label="Limit {tab.maxRows.toLocaleString()}" variant="warning" size="sm" />
						</Tooltip>
					{/if}
					<Button size="sm" variant="ghost" onclick={() => copyAll(current)} aria-label="Copy result">
						{#snippet leading()}<Copy size={14} />{/snippet}
						Copy
					</Button>
					<Button size="sm" variant="ghost" onclick={() => exportAs(current, "csv")}>
						{#snippet leading()}<FileDown size={14} />{/snippet}
						CSV
					</Button>
					<Button size="sm" variant="ghost" onclick={() => exportAs(current, "json")}>
						{#snippet leading()}<FileJson size={14} />{/snippet}
						JSON
					</Button>
				</div>
			{/if}
		</div>

		{#each tab.results as r (r.id)}
			<TabsContent value={r.id} class="lq-results__panel">
				{#if (r.result.columns ?? []).length > 0}
					<ResultGrid
						bind:this={grids[r.id]}
						columns={r.result.columns ?? []}
						rows={r.result.rows}
						label={r.title}
						editable={r.result.editable}
						edits={r.edits}
						onsave={() => workspace.saveEdits(tab, r)}
						view={r.view}
						localView={!r.serverView}
						onview={(v) => workspace.setView(tab, r, v)}
						busy={tab.running}
					/>
				{/if}
			</TabsContent>
		{/each}

		<TabsContent value="messages" class="lq-results__panel">
			{#if tab.log.length === 0}
				{#if tab.running}
					<div class="lq-results__center"><Spinner size={22} label="Running" /></div>
				{:else}
					<EmptyState
						icon={MessageSquareText}
						title="No messages"
						description="Results, server notices and errors appear here."
					/>
				{/if}
			{:else}
				<ol class="lq-log" bind:this={logEl}>
					{#each tab.log as entry (entry.id)}
						<li class="lq-log__item lq-log__item--{entry.level}">
							<span class="lq-log__icon" aria-hidden="true">
								{#if entry.level === "error"}<CircleAlert size={14} />
								{:else if entry.level === "success"}<CircleCheck size={14} />
								{:else if entry.level === "notice"}<TriangleAlert size={14} />
								{:else}<Info size={14} />{/if}
							</span>
							<time class="lq-log__time">{time(entry.at)}</time>
							<div class="lq-log__body">
								<p class="lq-log__text">{entry.text}{entry.elapsedMs != null ? ` · ${entry.elapsedMs} ms` : ""}</p>
								{#if entry.sql}
									<code class="lq-log__sql">{entry.sql.length > 240 ? entry.sql.slice(0, 240) + "…" : entry.sql}</code>
								{/if}
							</div>
						</li>
					{/each}
				</ol>
			{/if}
		</TabsContent>
	</Tabs>
</section>

<style>
	.lq-results {
		container-type: inline-size;
		display: flex;
		flex-direction: column;
		height: 100%;
		min-height: 0;
		background: var(--ca-surface-raised);
	}
	.lq-results :global(.lq-results__tabs) {
		display: flex;
		flex-direction: column;
		flex: 1;
		min-height: 0;
	}
	.lq-results__bar {
		display: flex;
		align-items: center;
		gap: var(--ca-space-2);
		min-width: 0;
		padding: 0 var(--ca-space-2);
		border-bottom: 1px solid var(--ca-outline-subtle);
		background: var(--ca-surface-sunken);
	}
	.lq-results__bar :global(.ca-tabs-list) {
		flex: 1;
		min-width: 0;
		border-bottom: 0;
	}
	.lq-results__meta :global(.ca-chip) {
		white-space: nowrap;
	}
	.lq-results__meta {
		display: flex;
		align-items: center;
		gap: var(--ca-space-1);
		flex-shrink: 0;
	}
	.lq-results__pending {
		padding: 0 var(--ca-space-1);
		color: var(--ca-text-primary);
		font-size: 12px;
		font-weight: 600;
		white-space: nowrap;
	}
	.lq-results__count {
		padding-right: var(--ca-space-2);
		color: var(--ca-text-secondary);
		font-family: var(--ca-font-mono);
		font-size: 12px;
		white-space: nowrap;
	}
	.lq-results :global(.lq-results__panel) {
		flex: 1;
		min-height: 0;
		padding: 0;
		overflow: hidden;
	}
	.lq-results__center {
		display: grid;
		place-items: center;
		height: 100%;
	}
	.lq-log {
		height: 100%;
		margin: 0;
		padding: var(--ca-space-2) 0;
		overflow: auto;
		list-style: none;
		font-size: 13px;
	}
	.lq-log__item {
		display: grid;
		grid-template-columns: 18px 64px minmax(0, 1fr);
		gap: var(--ca-space-2);
		padding: 6px var(--ca-space-4);
		border-bottom: 1px solid var(--ca-outline-subtle);
	}
	.lq-log__icon {
		padding-top: 2px;
		color: var(--ca-text-hint);
	}
	.lq-log__item--error .lq-log__icon,
	.lq-log__item--error .lq-log__text {
		color: var(--ca-semantic-error);
	}
	.lq-log__item--success .lq-log__icon {
		color: var(--ca-semantic-success);
	}
	.lq-log__item--notice .lq-log__icon {
		color: var(--ca-semantic-warning);
	}
	.lq-log__time {
		color: var(--ca-text-hint);
		font-family: var(--ca-font-mono);
		font-size: 12px;
		line-height: 20px;
	}
	.lq-log__text {
		margin: 0;
		line-height: 20px;
		white-space: pre-wrap;
		word-break: break-word;
	}
	.lq-log__sql {
		display: block;
		margin-top: 2px;
		overflow: hidden;
		color: var(--ca-text-secondary);
		font-family: var(--ca-font-mono);
		font-size: 12px;
		white-space: nowrap;
		text-overflow: ellipsis;
	}
	@container (max-width: 680px) {
		.lq-results__bar {
			flex-wrap: wrap;
		}
		.lq-results__meta {
			flex-wrap: wrap;
			width: 100%;
			justify-content: flex-start;
			padding-bottom: var(--ca-space-1);
		}
	}
</style>
