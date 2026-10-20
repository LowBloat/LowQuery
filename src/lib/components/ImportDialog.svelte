<script lang="ts">
	import { Dialog, Button, Checkbox, Alert, Spinner, TextField, Tooltip } from "@codeatlasdev/ui";
	import { ArrowLeft, ChevronRight, FolderOpen, KeyRound, TriangleAlert } from "@lucide/svelte";
	import EngineLogo from "./EngineLogo.svelte";
	import ToolLogo from "./ToolLogo.svelte";
	import { importer, TOOLS, toolInfo } from "../state/importer.svelte";
	import { tns } from "../state/tns.svelte";
	import { connectionSummary } from "../summary";
	import type { ImportCandidate } from "../api";

	let filter = $state("");

	const preview = $derived(importer.preview);
	const visible = $derived.by(() => {
		const q = filter.trim().toLowerCase();
		const all = preview?.candidates ?? [];
		if (!q) return all;
		return all.filter((c) =>
			[c.profile.name, c.folder ?? "", connectionSummary(c.profile), c.profile.database]
				.join(" ")
				.toLowerCase()
				.includes(q),
		);
	});
	const selectable = $derived(visible.filter((c) => !importer.isAdded(c)));
	const allSelected = $derived(selectable.length > 0 && selectable.every((c) => importer.selected.has(c.key)));
	const passwordsSelected = $derived(
		(preview?.candidates ?? []).filter((c) => c.hasPassword && importer.selected.has(c.key)).length,
	);

	$effect(() => {
		void preview;
		filter = "";
	});

	function toggle(key: string, on: boolean) {
		const next = new Set(importer.selected);
		if (on) next.add(key);
		else next.delete(key);
		importer.selected = next;
	}

	function toggleAll(on: boolean) {
		const next = new Set(importer.selected);
		for (const c of selectable) {
			if (on) next.add(c.key);
			else next.delete(c.key);
		}
		importer.selected = next;
	}

	function openTns() {
		importer.close();
		tns.importOpen = true;
	}

	function target(c: ImportCandidate): string {
		const p = c.profile;
		const summary = connectionSummary(p);
		if (p.engine === "sqlite" || p.engine === "redis" || !p.database) return summary;
		if (p.engine === "oracle" && (p.oracleConnectType === "tns" || p.oracleConnectType === "custom")) return summary;
		return `${summary} · ${p.database}`;
	}

	const count = (n: number, one: string, many: string) => `${n.toLocaleString()} ${n === 1 ? one : many}`;
</script>

{#if importer.open}
	<Dialog
		open={true}
		size="lg"
		title={preview ? `Import from ${toolInfo(preview.tool).name}` : "Import connections"}
		description={preview ? undefined : "Bring your connections from another database tool. Nothing changes in the other tool."}
		onOpenChange={(o) => {
			if (!o) importer.close();
		}}
	>
		{#if !preview}
			<div class="lq-imp">
				{#each TOOLS as t (t.id)}
					{@const found = importer.sourcesOf(t.id)}
					<section class="lq-imp__tool" aria-labelledby="imp-{t.id}">
						<header class="lq-imp__tool-head">
							<ToolLogo tool={t.id} />
							<div class="lq-imp__tool-text">
								<h3 id="imp-{t.id}">{t.name}</h3>
								<p>{t.reads}</p>
							</div>
							<Button size="sm" variant="ghost" disabled={importer.reading} onclick={() => importer.pick(t.id)}>
								{#snippet leading()}<FolderOpen size={14} />{/snippet}
								Choose file…
							</Button>
						</header>
						{#if importer.detecting}
							<p class="lq-imp__hint">Looking for {t.name} files…</p>
						{:else if found.length === 0}
							<p class="lq-imp__hint">Not found on this computer.</p>
						{:else}
							<ul class="lq-imp__files">
								{#each found as s (s.path)}
									<li>
										<button
											type="button"
											class="lq-imp__file"
											disabled={importer.reading || (!!s.error && s.count === 0)}
											onclick={() => importer.read(s.tool, s.path)}
										>
											<span class="lq-imp__file-label">{s.label}</span>
											<span class="lq-imp__file-count" class:lq-imp__file-count--error={!!s.error}>
												{s.error ? s.error : count(s.count, "connection", "connections")}
											</span>
											<span class="lq-imp__file-path" title={s.path}>{s.path}</span>
											<ChevronRight size={16} aria-hidden="true" />
										</button>
									</li>
								{/each}
							</ul>
						{/if}
					</section>
				{/each}
				<section class="lq-imp__tool" aria-labelledby="imp-tns">
					<header class="lq-imp__tool-head">
						<ToolLogo tool="tnsnames" />
						<div class="lq-imp__tool-text">
							<h3 id="imp-tns">Oracle tnsnames.ora</h3>
							<p>One connection per alias of the Oracle client on this computer</p>
						</div>
						<Button size="sm" variant="ghost" onclick={openTns}>
							Choose aliases
							{#snippet trailing()}<ChevronRight size={14} />{/snippet}
						</Button>
					</header>
				</section>
				{#if importer.reading}
					<div class="lq-imp__center"><Spinner size={20} label="Reading connections" /></div>
				{/if}
			</div>
		{:else}
			<div class="lq-imp">
				<p class="lq-imp__path" title={preview.path}>{preview.path}</p>
				{#each preview.notes as note, i (i)}
					<Alert severity="info">{note}</Alert>
				{/each}
				{#if preview.candidates.length === 0}
					<Alert severity="warning">No connection in this file can be imported.</Alert>
				{:else}
					<TextField label="Filter" placeholder="Name, folder, host or database" bind:value={filter} autocomplete="off" />
					<div class="lq-imp__head">
						<Checkbox
							label="Select all ({selectable.length})"
							checked={allSelected}
							indeterminate={!allSelected && selectable.some((c) => importer.selected.has(c.key))}
							disabled={selectable.length === 0}
							onchange={(on) => toggleAll(on)}
						/>
						<span>{importer.selected.size} selected</span>
					</div>
					<ul class="lq-imp__list">
						{#each visible as c (c.key)}
							{@const added = importer.isAdded(c)}
							<li class="lq-imp__item" class:lq-imp__item--added={added}>
								<Checkbox
									aria-label={c.profile.name}
									checked={importer.selected.has(c.key)}
									disabled={added}
									onchange={(on) => toggle(c.key, on)}
								/>
								<EngineLogo engine={c.profile.engine} size={18} />
								<span class="lq-imp__name">
									{#if c.profile.color}<span class="lq-imp__dot" style="background:{c.profile.color}"></span>{/if}
									{c.profile.name}
								</span>
								<span class="lq-imp__target">{added ? "already added" : target(c)}</span>
								<span class="lq-imp__badges">
									{#if c.folder}<span class="lq-imp__folder" title={c.folder}>{c.folder}</span>{/if}
									{#if c.hasPassword}
										<Tooltip content="The saved password can be imported">
											<span class="lq-imp__icon" aria-label="Saved password"><KeyRound size={14} /></span>
										</Tooltip>
									{/if}
									{#if c.warnings.length > 0}
										<Tooltip content={c.warnings.join(" ")}>
											<span class="lq-imp__icon lq-imp__icon--warn" aria-label="Settings not imported"
												><TriangleAlert size={14} /></span
											>
										</Tooltip>
									{/if}
								</span>
							</li>
						{/each}
					</ul>
				{/if}
				{#if preview.skipped.length > 0}
					<details class="lq-imp__skipped">
						<summary>{count(preview.skipped.length, "connection cannot", "connections cannot")} be imported</summary>
						<ul>
							{#each preview.skipped as s, i (i)}
								<li><strong>{s.name}</strong> · {s.reason}</li>
							{/each}
						</ul>
					</details>
				{/if}
				{#if preview.candidates.some((c) => c.hasPassword)}
					<Checkbox
						label="Import saved passwords into the system keychain ({passwordsSelected})"
						checked={importer.withPasswords}
						onchange={(on) => (importer.withPasswords = on)}
					/>
				{/if}
			</div>
		{/if}
		{#snippet actions()}
			{#if preview}
				<Button variant="ghost" onclick={() => importer.back()}>
					{#snippet leading()}<ArrowLeft size={14} />{/snippet}
					Back
				</Button>
			{/if}
			<Button variant="outlined" onclick={() => importer.close()}>Cancel</Button>
			{#if preview}
				<Button
					variant="filled"
					loading={importer.importing}
					disabled={importer.selected.size === 0}
					onclick={() => importer.importSelected()}
				>
					Import {importer.selected.size || ""}
					{importer.selected.size === 1 ? "connection" : "connections"}
				</Button>
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
	.lq-imp__tool {
		border: 1px solid var(--ca-outline-subtle);
	}
	.lq-imp__tool-head {
		display: flex;
		align-items: center;
		gap: var(--ca-space-3);
		padding: var(--ca-space-2) var(--ca-space-3);
	}
	.lq-imp__tool-text {
		flex: 1;
		min-width: 0;
	}
	.lq-imp__tool-text h3 {
		margin: 0;
		font-size: 14px;
		font-weight: 650;
	}
	.lq-imp__tool-text p,
	.lq-imp__hint {
		margin: 0;
		color: var(--ca-text-secondary);
		font-size: 12px;
	}
	.lq-imp__hint {
		padding: 0 var(--ca-space-3) var(--ca-space-2) calc(var(--ca-space-3) * 2 + 22px);
		color: var(--ca-text-hint);
	}
	.lq-imp__files {
		margin: 0;
		padding: 0;
		border-top: 1px solid var(--ca-outline-subtle);
		list-style: none;
	}
	.lq-imp__file {
		display: grid;
		grid-template-columns: auto auto minmax(0, 1fr) auto;
		align-items: center;
		gap: var(--ca-space-3);
		width: 100%;
		padding: 6px var(--ca-space-3) 6px calc(var(--ca-space-3) * 2 + 22px);
		border: 0;
		border-bottom: 1px solid var(--ca-outline-subtle);
		background: transparent;
		color: var(--ca-text-primary);
		font: inherit;
		font-size: 13px;
		text-align: left;
		cursor: pointer;
	}
	li:last-child > .lq-imp__file {
		border-bottom: 0;
	}
	.lq-imp__file:hover:not(:disabled) {
		background: var(--ca-action-hover);
	}
	.lq-imp__file:focus-visible {
		outline: 2px solid var(--ca-focus-ring);
		outline-offset: -2px;
	}
	.lq-imp__file:disabled {
		color: var(--ca-text-hint);
		cursor: default;
	}
	.lq-imp__file-label {
		font-weight: 600;
	}
	.lq-imp__file-count {
		color: var(--ca-text-secondary);
		white-space: nowrap;
	}
	.lq-imp__file-count--error {
		color: var(--ca-semantic-error);
	}
	.lq-imp__file-path {
		overflow: hidden;
		color: var(--ca-text-hint);
		font-family: var(--ca-font-mono);
		font-size: 12px;
		white-space: nowrap;
		text-overflow: ellipsis;
		direction: rtl;
		text-align: left;
	}
	.lq-imp__center {
		display: grid;
		place-items: center;
		padding: var(--ca-space-3);
	}
	.lq-imp__head {
		display: flex;
		align-items: center;
		justify-content: space-between;
		color: var(--ca-text-secondary);
		font-size: 13px;
	}
	.lq-imp__list {
		max-height: min(46vh, 420px);
		margin: 0;
		padding: 0;
		overflow: auto;
		border: 1px solid var(--ca-outline-subtle);
		list-style: none;
	}
	.lq-imp__item {
		display: grid;
		grid-template-columns: auto auto minmax(120px, auto) minmax(0, 1fr) auto;
		align-items: center;
		gap: var(--ca-space-3);
		padding: 4px var(--ca-space-3);
		border-bottom: 1px solid var(--ca-outline-subtle);
	}
	.lq-imp__item:last-child {
		border-bottom: 0;
	}
	.lq-imp__item--added {
		color: var(--ca-text-hint);
	}
	.lq-imp__path {
		margin: 0;
		overflow: hidden;
		color: var(--ca-text-secondary);
		font-family: var(--ca-font-mono);
		font-size: 12px;
		white-space: nowrap;
		text-overflow: ellipsis;
		direction: rtl;
		text-align: left;
	}
	.lq-imp__name {
		display: inline-flex;
		align-items: center;
		gap: 6px;
		min-width: 0;
		overflow: hidden;
		font-size: 13px;
		font-weight: 600;
		white-space: nowrap;
	}
	.lq-imp__dot {
		width: 8px;
		height: 8px;
		flex-shrink: 0;
	}
	.lq-imp__target {
		overflow: hidden;
		color: var(--ca-text-secondary);
		font-family: var(--ca-font-mono);
		font-size: 12px;
		white-space: nowrap;
		text-overflow: ellipsis;
	}
	.lq-imp__badges {
		display: inline-flex;
		align-items: center;
		gap: var(--ca-space-2);
	}
	.lq-imp__folder {
		max-width: 160px;
		overflow: hidden;
		padding: 0 6px;
		background: var(--ca-surface-sunken);
		color: var(--ca-text-secondary);
		font-size: 11px;
		line-height: 18px;
		white-space: nowrap;
		text-overflow: ellipsis;
	}
	.lq-imp__icon {
		display: inline-flex;
		color: var(--ca-text-secondary);
	}
	.lq-imp__icon--warn {
		color: var(--ca-semantic-warning);
	}
	.lq-imp__skipped {
		color: var(--ca-text-secondary);
		font-size: 13px;
	}
	.lq-imp__skipped summary {
		cursor: pointer;
	}
	.lq-imp__skipped ul {
		margin: var(--ca-space-2) 0 0;
		padding-left: var(--ca-space-5);
	}
	@media (max-width: 640px) {
		.lq-imp__item {
			grid-template-columns: auto auto minmax(0, 1fr) auto;
			row-gap: 0;
		}
		.lq-imp__name {
			grid-column: 3;
			grid-row: 1;
		}
		.lq-imp__badges {
			grid-column: 4;
			grid-row: 1;
		}
		.lq-imp__folder {
			max-width: 90px;
		}
		.lq-imp__target {
			grid-column: 3 / -1;
			grid-row: 2;
		}
		.lq-imp__file {
			grid-template-columns: minmax(0, 1fr) auto auto;
		}
		.lq-imp__file-path {
			display: none;
		}
	}
</style>
