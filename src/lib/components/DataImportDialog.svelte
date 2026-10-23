<script lang="ts">
	import { untrack } from "svelte";
	import { Dialog, Button, TextField, Checkbox, Select, Progress } from "@codeatlasdev/ui";
	import { TriangleAlert } from "@lucide/svelte";
	import { transfer } from "../state/transfer.svelte";
	import { workspace } from "../state/workspace.svelte";
	import { foldName } from "../sql/ddl";
	import { previewQuery } from "../sql/ident";

	const DELIMITERS = [
		{ value: ",", label: "Comma ," },
		{ value: ";", label: "Semicolon ;" },
		{ value: "\t", label: "Tab" },
		{ value: "|", label: "Pipe |" },
	];

	let header = $state(true);
	let delimiter = $state(",");
	let replace = $state(false);
	let newName = $state("");

	const t = $derived(transfer.importing);
	const pv = $derived(t?.preview);
	const fileName = $derived(t?.path.split(/[\\/]/).pop() ?? "");

	$effect(() => {
		const target = transfer.importing;
		if (!target) return;
		untrack(() => {
			header = target.preview.header;
			delimiter = target.preview.delimiter;
			replace = false;
			newName = foldName(fileName.replace(/\.[^.]+$/, "").replace(/[^\p{L}\p{N}_]+/gu, "_"), target.profile.engine);
		});
	});

	/** Same matching as the import: case, spaces, `_` and `-` do not matter. */
	const key = (n: string) => n.trim().toLowerCase().replace(/[ _-]/g, "");
	const matched = $derived(
		!t ? [] : t.table ? pv!.columns.map((c) => t.columns.some((tc) => key(tc) === key(c))) : pv!.columns.map(() => true),
	);
	const matchCount = $derived(matched.filter(Boolean).length);
	const newColumns = $derived(t && !t.table ? transfer.newColumns(t) : []);

	function reread() {
		transfer.reread({ delimiter, header });
	}

	function start() {
		transfer.runImport({ delimiter: pv?.format === "csv" ? delimiter : null, header }, replace, newName);
	}

	const p = $derived(transfer.progress);
	const total = $derived(pv?.totalRows ?? 0);
</script>

{#if t && pv}
	<Dialog
		open={true}
		size="xl"
		title={t.table ? `Import into ${t.table}` : `Import ${fileName} as a new table`}
		description="{fileName} · {pv.format === 'csv' ? 'CSV' : 'JSON'} · {pv.encoding} · {total.toLocaleString()} rows"
		onOpenChange={(o) => {
			if (!o) transfer.close();
		}}
	>
		<div class="lq-imp">
			{#if transfer.phase === "ready"}
				{#if !t.table}
					<TextField label="Table name" bind:value={newName} />
				{/if}
				<div class="lq-imp__options">
					{#if pv.format === "csv"}
						<div class="lq-imp__delimiter">
							<Select size="sm" aria-label="Delimiter" options={DELIMITERS} bind:value={delimiter} onchange={reread} />
						</div>
						<Checkbox label="First row has the column names" bind:checked={header} onchange={reread} />
					{/if}
					{#if t.table}
						<Checkbox label="Replace the table's rows" bind:checked={replace} />
					{/if}
				</div>

				<div class="lq-imp__preview" role="region" aria-label="First rows of the file" tabindex="-1">
					<table>
						<thead>
							<tr>
								{#each pv.columns as c, i (i)}
									<th class:lq-imp__skip={!matched[i]} title={matched[i] ? "" : "No column of the table has this name; it is not imported"}>
										{t.table ? c : newColumns[i]?.name}
										<span>{t.table ? (matched[i] ? "" : "not imported") : newColumns[i]?.type}</span>
									</th>
								{/each}
							</tr>
						</thead>
						<tbody>
							{#each pv.rows.slice(0, 6) as row, r (r)}
								<tr>
									{#each row as v, i (i)}
										<td class:lq-imp__skip={!matched[i]} class:lq-imp__null={v === null || v === ""}>{v === null || v === "" ? "NULL" : v}</td>
									{/each}
								</tr>
							{/each}
						</tbody>
					</table>
				</div>
				{#if t.table && matchCount === 0}
					<p class="lq-imp__error"><TriangleAlert size={14} aria-hidden="true" />No column of the file has the name of a column of {t.table}.</p>
				{:else}
					<p class="lq-imp__text">
						{#if t.table}
							Columns are matched by name; {matchCount} of {pv.columns.length} match. Empty values become NULL, and numbers and dates like 1.234,56 and 31/12/2024 are understood.
						{:else}
							Column types come from the values in the file. Empty values become NULL.
						{/if}
						{t.table ? "Everything is imported in one transaction: an error leaves the table as it was." : "The rows go in one transaction: an error leaves the new table empty."}
					</p>
				{/if}
			{:else if transfer.phase === "running"}
				<Progress value={p?.rows ?? 0} max={Math.max(total, 1)} label="Import progress" />
				<p class="lq-imp__text">{(p?.rows ?? 0).toLocaleString()} of {total.toLocaleString()} rows</p>
			{:else}
				<p class="lq-imp__done">{transfer.result}</p>
				{#if transfer.note}<p class="lq-imp__text">{transfer.note}</p>{/if}
			{/if}
			{#if transfer.error}
				<p class="lq-imp__error"><TriangleAlert size={14} aria-hidden="true" />{transfer.error}</p>
			{/if}
		</div>
		{#snippet actions()}
			{#if transfer.phase === "ready"}
				<Button variant="ghost" onclick={() => transfer.close()}>Cancel</Button>
				<Button variant="filled" disabled={(!!t.table && matchCount === 0) || (!t.table && !newName.trim()) || total === 0} onclick={start}>
					{t.table ? `Import ${total.toLocaleString()} rows` : `Create table and import ${total.toLocaleString()} rows`}
				</Button>
			{:else if transfer.phase === "running"}
				<Button variant="outlined" onclick={() => transfer.cancel()}>Stop</Button>
			{:else}
				<Button
					variant="outlined"
					onclick={() => {
						if (t.table) workspace.openQuery(t.profile.id, previewQuery(t.schema, t.table, t.profile.engine));
						transfer.close();
					}}>Show rows</Button
				>
				<Button variant="filled" onclick={() => transfer.close()}>Close</Button>
			{/if}
		{/snippet}
	</Dialog>
{/if}

<style>
	.lq-imp {
		display: flex;
		flex-direction: column;
		gap: var(--ca-space-3);
	}
	.lq-imp__options {
		display: flex;
		flex-wrap: wrap;
		gap: var(--ca-space-4);
		align-items: center;
	}
	.lq-imp__delimiter {
		width: 170px;
	}
	.lq-imp__preview {
		max-height: 260px;
		overflow: auto;
		border: 1px solid var(--ca-outline-subtle);
	}
	.lq-imp__preview table {
		border-collapse: collapse;
		font-size: 12px;
	}
	.lq-imp__preview th,
	.lq-imp__preview td {
		max-width: 220px;
		padding: 4px 8px;
		overflow: hidden;
		border-right: 1px solid var(--ca-outline-subtle);
		border-bottom: 1px solid var(--ca-outline-subtle);
		text-align: left;
		white-space: nowrap;
		text-overflow: ellipsis;
	}
	.lq-imp__preview th {
		position: sticky;
		top: 0;
		background: var(--ca-surface-sunken);
		font-weight: 600;
	}
	.lq-imp__preview th span {
		display: block;
		color: var(--ca-text-hint);
		font-family: var(--ca-font-mono);
		font-size: 10px;
		font-weight: 400;
	}
	.lq-imp__preview td {
		font-family: var(--ca-font-mono);
	}
	.lq-imp__skip {
		opacity: 0.45;
	}
	.lq-imp__null {
		color: var(--ca-text-hint);
		font-style: italic;
	}
	.lq-imp__text,
	.lq-imp__done,
	.lq-imp__error {
		margin: 0;
		font-size: 13px;
	}
	.lq-imp__text {
		color: var(--ca-text-secondary);
	}
	.lq-imp__done {
		font-weight: 600;
	}
	.lq-imp__error {
		display: flex;
		gap: var(--ca-space-2);
		align-items: flex-start;
		color: var(--ca-semantic-error);
		white-space: pre-wrap;
	}
	.lq-imp__error :global(svg) {
		flex-shrink: 0;
		margin-top: 2px;
	}
</style>
