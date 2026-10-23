<script lang="ts">
	import { untrack } from "svelte";
	import { Dialog, Button, SegmentedControl, Progress } from "@codeatlasdev/ui";
	import { FolderOpen, TriangleAlert } from "@lucide/svelte";
	import { open, save } from "@tauri-apps/plugin-dialog";
	import { revealItemInDir } from "@tauri-apps/plugin-opener";
	import { transfer } from "../state/transfer.svelte";
	import { kindName } from "../state/actions.svelte";
	import type { ExportContent, ExportFormat } from "../api";
	import { engineInfo } from "../engines";

	const FORMATS = [
		{ value: "sql", label: "SQL" },
		{ value: "csv", label: "CSV" },
		{ value: "csv-excel", label: "CSV for Excel" },
		{ value: "json", label: "JSON" },
	];
	const CONTENTS = [
		{ value: "all", label: "Structure and data" },
		{ value: "structure", label: "Structure" },
		{ value: "data", label: "Data" },
	];
	/** Kinds with rows to export as CSV or JSON. */
	const ROWS = new Set(["table", "view", "matview", "foreign"]);

	let format = $state<ExportFormat>("sql");
	let content = $state<ExportContent>("all");

	const t = $derived(transfer.exporting);
	// Sequences, routines and the like only have a definition.
	const definitionOnly = $derived(!!t?.object && !ROWS.has(t.object.kind));
	const effFormat = $derived<ExportFormat>(definitionOnly ? "sql" : format);
	const effContent = $derived<ExportContent>(definitionOnly ? "structure" : effFormat === "sql" ? content : "data");
	const folder = $derived(effFormat !== "sql" && !t?.object);

	$effect(() => {
		if (transfer.exporting) {
			untrack(() => {
				const p = transfer.prefs();
				format = p.format;
				content = p.content;
			});
		}
	});

	const what = $derived(
		!t ? "" : t.object ? `${kindName(t.object.kind)} ${t.object.name}` : `${t.tables ? `${t.tables} tables` : "every table"} of ${t.schema}`,
	);

	const explanation = $derived.by(() => {
		if (!t) return "";
		if (effFormat === "sql") {
			const scope = t.object ? `the ${kindName(t.object.kind)}` : `the ${t.schema} schema`;
			return {
				all: `One .sql file that recreates ${scope} with its rows on ${engineInfo(t.profile.engine).label}.`,
				structure: `One .sql file with the statements that create ${scope}, without rows.`,
				data: `One .sql file with INSERT statements for the rows of ${scope}.`,
			}[effContent];
		}
		const ext = effFormat === "json" ? "JSON" : "CSV";
		const excel = effFormat === "csv-excel" ? " Uses ; between values and opens in Excel with accents intact." : "";
		return t.object ? `One ${ext} file with every row.${excel}` : `One ${ext} file per table, in a folder you choose.${excel}`;
	});

	async function start() {
		if (!t) return;
		const ext = effFormat === "json" ? "json" : effFormat === "sql" ? "sql" : "csv";
		const base = (t.object?.name ?? t.schema).replace(/\(.*\)$/, "").replace(/[<>:"/\\|?*]/g, "_");
		const path = folder
			? await open({ directory: true, title: `Folder for the files of ${t.schema}` })
			: await save({ title: `Export ${what}`, defaultPath: `${base}.${ext}`, filters: [{ name: ext.toUpperCase(), extensions: [ext] }] });
		if (typeof path === "string") await transfer.runExport(effFormat, effContent, path);
	}

	const p = $derived(transfer.progress);
</script>

{#if t}
	<Dialog
		open={true}
		size="md"
		title="Export {what}"
		onOpenChange={(o) => {
			if (!o) transfer.close();
		}}
	>
		<div class="lq-xfer">
			{#if transfer.phase === "ready"}
				{#if !definitionOnly}
					<SegmentedControl aria-label="Format" size="sm" fullWidth items={FORMATS} bind:value={format} />
					{#if format === "sql"}
						<SegmentedControl aria-label="What to export" size="sm" fullWidth items={CONTENTS} bind:value={content} />
					{/if}
				{/if}
				<p class="lq-xfer__text">{explanation}</p>
			{:else if transfer.phase === "running"}
				<Progress indeterminate={!p || p.steps <= 1} value={p ? p.step - 1 : 0} max={p?.steps ?? 1} label="Export progress" />
				<p class="lq-xfer__text">
					{#if p}
						{p.object}{p.steps > 1 ? ` (${p.step} of ${p.steps})` : ""}{p.rows ? ` · ${p.rows.toLocaleString()} rows` : ""}
					{:else}
						Starting…
					{/if}
				</p>
			{:else}
				<p class="lq-xfer__done">{transfer.result}</p>
				{#each transfer.files.slice(0, 3) as f (f)}
					<p class="lq-xfer__file">{f}</p>
				{/each}
				{#if transfer.files.length > 3}
					<p class="lq-xfer__text">and {transfer.files.length - 3} more files</p>
				{/if}
			{/if}
			{#if transfer.error}
				<p class="lq-xfer__error"><TriangleAlert size={14} aria-hidden="true" />{transfer.error}</p>
			{/if}
		</div>
		{#snippet actions()}
			{#if transfer.phase === "ready"}
				<Button variant="ghost" onclick={() => transfer.close()}>Cancel</Button>
				<Button variant="filled" onclick={start}>Export…</Button>
			{:else if transfer.phase === "running"}
				<Button variant="outlined" onclick={() => transfer.cancel()}>Stop</Button>
			{:else}
				<Button variant="outlined" onclick={() => revealItemInDir(transfer.files[0])}>
					{#snippet leading()}<FolderOpen size={15} />{/snippet}
					Show in folder
				</Button>
				<Button variant="filled" onclick={() => transfer.close()}>Close</Button>
			{/if}
		{/snippet}
	</Dialog>
{/if}

<style>
	.lq-xfer {
		display: flex;
		flex-direction: column;
		gap: var(--ca-space-3);
	}
	.lq-xfer__text,
	.lq-xfer__done,
	.lq-xfer__file,
	.lq-xfer__error {
		margin: 0;
		font-size: 13px;
	}
	.lq-xfer__text {
		color: var(--ca-text-secondary);
	}
	.lq-xfer__done {
		font-weight: 600;
	}
	.lq-xfer__file {
		overflow: hidden;
		color: var(--ca-text-secondary);
		font-family: var(--ca-font-mono);
		font-size: 12px;
		white-space: nowrap;
		text-overflow: ellipsis;
	}
	.lq-xfer__error {
		display: flex;
		gap: var(--ca-space-2);
		align-items: flex-start;
		color: var(--ca-semantic-error);
		white-space: pre-wrap;
	}
	.lq-xfer__error :global(svg) {
		flex-shrink: 0;
		margin-top: 2px;
	}
</style>
