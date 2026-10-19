<script lang="ts">
	import { Dialog, Button, TextField, Checkbox, Alert, Spinner, toast } from "@codeatlasdev/ui";
	import OracleClientPicker from "./OracleClientPicker.svelte";
	import { emptyProfile, asAppError, errorText, type ConnectionProfile } from "../api";
	import { workspace } from "../state/workspace.svelte";
	import { tns, describeEntry } from "../state/tns.svelte";

	let configDir = $state<string | null>(null);
	let user = $state("");
	let filter = $state("");
	let selected = $state<Set<string>>(new Set());
	let importing = $state(false);

	$effect(() => {
		if (tns.importOpen) {
			tns.load();
			selected = new Set();
			filter = "";
		}
	});

	const entries = $derived(tns.entries(configDir));
	const visible = $derived(
		entries.filter((e) => {
			const q = filter.trim().toLowerCase();
			return !q || e.alias.toLowerCase().includes(q) || describeEntry(e).toLowerCase().includes(q);
		}),
	);

	/** Aliases that already have a connection with the same client folder. */
	const existing = $derived(
		new Set(
			workspace.profiles
				.filter((p) => p.engine === "oracle" && p.oracleConnectType === "tns" && (p.configDir ?? null) === configDir)
				.map((p) => p.tnsAlias.toLowerCase()),
		),
	);
	const selectable = $derived(visible.filter((e) => !existing.has(e.alias.toLowerCase())));
	const allSelected = $derived(selectable.length > 0 && selectable.every((e) => selected.has(e.alias)));

	function toggle(alias: string, on: boolean) {
		const next = new Set(selected);
		if (on) next.add(alias);
		else next.delete(alias);
		selected = next;
	}

	function toggleAll(on: boolean) {
		const next = new Set(selected);
		for (const e of selectable) {
			if (on) next.add(e.alias);
			else next.delete(e.alias);
		}
		selected = next;
	}

	async function importSelected() {
		importing = true;
		let done = 0;
		try {
			for (const alias of selected) {
				const profile: ConnectionProfile = {
					...emptyProfile("oracle"),
					name: alias,
					user: user.trim(),
					oracleConnectType: "tns",
					tnsAlias: alias,
					configDir,
					host: "",
				};
				await workspace.saveProfile(profile, null);
				done++;
			}
			toast.success(`${done} ${done === 1 ? "connection" : "connections"} imported`);
			tns.importOpen = false;
		} catch (e) {
			toast.error(`Imported ${done}; stopped at an error: ${errorText(asAppError(e))}`);
		} finally {
			importing = false;
		}
	}
</script>

{#if tns.importOpen}
	<Dialog
		open={true}
		size="lg"
		title="Import from tnsnames.ora"
		description="Creates one Oracle connection per alias. Passwords are asked on the first connection and saved in the system keychain."
		onOpenChange={(o) => {
			if (!o) tns.importOpen = false;
		}}
	>
		<div class="lq-import">
			<OracleClientPicker value={configDir} onchange={(d) => (configDir = d)} />
			<div class="lq-import__row">
				<TextField label="User for all connections" placeholder="Optional" bind:value={user} autocomplete="off" />
				<TextField label="Filter" placeholder="Alias, host or service" bind:value={filter} autocomplete="off" />
			</div>

			{#if tns.loading && !tns.loaded}
				<div class="lq-import__center"><Spinner size={20} label="Reading tnsnames.ora" /></div>
			{:else if entries.length === 0}
				<Alert severity="info">
					No tnsnames.ora found automatically. Set TNS_ADMIN, install an Oracle client or choose its folder with Browse.
				</Alert>
			{:else}
				<div class="lq-import__head">
					<Checkbox
						label="Select all ({selectable.length})"
						checked={allSelected}
						indeterminate={!allSelected && selectable.some((e) => selected.has(e.alias))}
						disabled={selectable.length === 0}
						onchange={(on) => toggleAll(on)}
					/>
					<span>{selected.size} selected</span>
				</div>
				<ul class="lq-import__list">
					{#each visible as e (e.alias)}
						{@const added = existing.has(e.alias.toLowerCase())}
						<li class="lq-import__item" class:lq-import__item--added={added}>
							<Checkbox
								aria-label={e.alias}
								checked={selected.has(e.alias)}
								disabled={added}
								onchange={(on) => toggle(e.alias, on)}
							/>
							<span class="lq-import__alias">{e.alias}</span>
							<span class="lq-import__target">{added ? "already added" : describeEntry(e)}</span>
						</li>
					{/each}
				</ul>
			{/if}
		</div>
		{#snippet actions()}
			<Button variant="outlined" onclick={() => (tns.importOpen = false)}>Cancel</Button>
			<Button variant="filled" loading={importing} disabled={selected.size === 0} onclick={importSelected}>
				Import {selected.size || ""} {selected.size === 1 ? "connection" : "connections"}
			</Button>
		{/snippet}
	</Dialog>
{/if}

<style>
	.lq-import {
		display: flex;
		flex-direction: column;
		gap: var(--ca-space-4);
	}
	.lq-import__row {
		display: grid;
		grid-template-columns: repeat(auto-fit, minmax(min(100%, 200px), 1fr));
		gap: var(--ca-space-3);
	}
	.lq-import__center {
		display: grid;
		place-items: center;
		padding: var(--ca-space-6);
	}
	.lq-import__head {
		display: flex;
		align-items: center;
		justify-content: space-between;
		color: var(--ca-text-secondary);
		font-size: 13px;
	}
	.lq-import__list {
		max-height: min(46vh, 420px);
		margin: 0;
		padding: 0;
		overflow: auto;
		border: 1px solid var(--ca-outline-subtle);
		list-style: none;
	}
	.lq-import__item {
		display: grid;
		grid-template-columns: auto minmax(120px, auto) minmax(0, 1fr);
		align-items: center;
		gap: var(--ca-space-3);
		padding: 4px var(--ca-space-3);
		border-bottom: 1px solid var(--ca-outline-subtle);
	}
	.lq-import__item:last-child {
		border-bottom: 0;
	}
	.lq-import__item--added {
		color: var(--ca-text-hint);
	}
	.lq-import__alias {
		font-family: var(--ca-font-mono);
		font-size: 13px;
		font-weight: 600;
	}
	.lq-import__target {
		overflow: hidden;
		color: var(--ca-text-secondary);
		font-family: var(--ca-font-mono);
		font-size: 12px;
		white-space: nowrap;
		text-overflow: ellipsis;
	}
</style>
