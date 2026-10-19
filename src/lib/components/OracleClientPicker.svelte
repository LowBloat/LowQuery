<script lang="ts">
	import { Select, Button, toast } from "@codeatlasdev/ui";
	import { FolderOpen } from "@lucide/svelte";
	import { open as openDialog } from "@tauri-apps/plugin-dialog";
	import { asAppError, errorText } from "../api";
	import { tns } from "../state/tns.svelte";

	interface Props {
		/** Folder with tnsnames.ora; null = automatic discovery. */
		value: string | null;
		onchange: (dir: string | null) => void;
	}

	let { value, onchange }: Props = $props();

	$effect(() => {
		tns.load();
	});

	const options = $derived([
		{ value: "", label: tns.loading && !tns.loaded ? "Automatic · searching…" : tns.automaticLabel },
		...tns.sources.map((s) => ({
			value: s.dir,
			label: `${s.label} · ${s.origin} (${s.entries.length} ${s.entries.length === 1 ? "alias" : "aliases"})`,
		})),
		// A folder saved in the profile that is not on this machine anymore.
		...(value && !tns.source(value) ? [{ value, label: `${value} (not found)` }] : []),
	]);

	async function browse() {
		const dir = await openDialog({
			directory: true,
			title: "Oracle client, Instant Client or network/admin folder",
		});
		if (typeof dir !== "string") return;
		try {
			const source = await tns.addFolder(dir);
			onchange(source.dir);
		} catch (e) {
			toast.error(errorText(asAppError(e)));
		}
	}
</script>

<div class="lq-client">
	<Select
		label="Oracle client (tnsnames.ora)"
		description="Automatic searches TNS_ADMIN, ORACLE_HOME, registered Oracle homes and Instant Client folders on PATH."
		{options}
		value={value ?? ""}
		onchange={(v) => onchange(v || null)}
	/>
	<Button variant="outlined" onclick={browse} aria-label="Browse for a folder">
		{#snippet leading()}<FolderOpen size={15} />{/snippet}
		Browse…
	</Button>
</div>

<style>
	.lq-client {
		display: grid;
		grid-template-columns: minmax(0, 1fr) auto;
		align-items: start;
		gap: var(--ca-space-2);
	}
	/* Aligns the button with the select, below its label. */
	.lq-client > :global(.ca-btn) {
		margin-top: calc(var(--ca-font-size-label) * 1.5 + var(--ca-space-1));
	}
</style>
