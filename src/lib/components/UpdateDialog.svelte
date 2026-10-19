<script lang="ts">
	import { Dialog, Button, Progress, Alert } from "@codeatlasdev/ui";
	import { Download, RefreshCw } from "@lucide/svelte";
	import { updates } from "../state/updates.svelte";
	import { workspace } from "../state/workspace.svelte";

	/** Release notes are Markdown: headings, bullets, bold and code only. */
	interface Block {
		kind: "h" | "li" | "p";
		parts: { text: string; style: "plain" | "bold" | "code" }[];
	}

	function inline(text: string): Block["parts"] {
		const parts: Block["parts"] = [];
		const re = /(\*\*[^*]+\*\*|`[^`]+`)/g;
		let last = 0;
		for (const m of text.matchAll(re)) {
			if (m.index! > last) parts.push({ text: text.slice(last, m.index), style: "plain" });
			const token = m[0];
			parts.push(
				token.startsWith("**")
					? { text: token.slice(2, -2), style: "bold" }
					: { text: token.slice(1, -1), style: "code" },
			);
			last = m.index! + token.length;
		}
		if (last < text.length) parts.push({ text: text.slice(last), style: "plain" });
		return parts;
	}

	const blocks = $derived.by<Block[]>(() =>
		(updates.available?.notes ?? "")
			.split(/\r?\n/)
			.map((l) => l.trim())
			.filter(Boolean)
			.map((l) => {
				if (l.startsWith("#")) return { kind: "h", parts: inline(l.replace(/^#+\s*/, "")) };
				if (l.startsWith("- ")) return { kind: "li", parts: inline(l.slice(2)) };
				return { kind: "p", parts: inline(l) };
			}),
	);

	const busyTabs = $derived(workspace.tabs.filter((t) => t.running || t.inTransaction).length);
</script>

{#if updates.dialogOpen && updates.available}
	<Dialog
		open={true}
		size="lg"
		title="LowQuery {updates.available.version} is available"
		description={updates.currentVersion ? `You have ${updates.currentVersion}.` : undefined}
		onOpenChange={(o) => {
			if (!o && !updates.installing) updates.dialogOpen = false;
		}}
	>
		<div class="lq-update">
			<div class="lq-update__notes">
				{#each blocks as b, i (i)}
					<svelte:element this={b.kind === "h" ? "h3" : "p"} class="lq-update__{b.kind}">
						{#each b.parts as part, j (j)}
							{#if part.style === "bold"}<strong>{part.text}</strong>
							{:else if part.style === "code"}<code>{part.text}</code>
							{:else}{part.text}{/if}
						{/each}
					</svelte:element>
				{/each}
			</div>

			{#if updates.manualHint}
				<Alert severity="info">{updates.manualHint}</Alert>
			{:else if busyTabs > 0 && !updates.installing}
				<Alert severity="warning">
					{busyTabs} {busyTabs === 1 ? "tab has" : "tabs have"} a running query or an open transaction. Restarting
					closes every session; uncommitted changes are rolled back.
				</Alert>
			{/if}
			{#if updates.installing}
				<Progress
					value={updates.progress === null ? undefined : Math.round(updates.progress * 100)}
					indeterminate={updates.progress === null}
					label={updates.progress === 1 ? "Installing" : "Downloading the update"}
				/>
			{/if}
			{#if updates.error}
				<Alert severity="error">{updates.error}</Alert>
			{/if}
		</div>
		{#snippet actions()}
			<Button variant="outlined" disabled={updates.installing} onclick={() => (updates.dialogOpen = false)}>Later</Button>
			{#if updates.policy?.canInstall}
				<Button variant="filled" loading={updates.installing} onclick={() => updates.install()}>
					{#snippet leading()}<RefreshCw size={15} />{/snippet}
					Install and restart
				</Button>
			{:else}
				<Button variant="filled" onclick={() => updates.download()}>
					{#snippet leading()}<Download size={15} />{/snippet}
					Download
				</Button>
			{/if}
		{/snippet}
	</Dialog>
{/if}

<style>
	.lq-update {
		display: flex;
		flex-direction: column;
		gap: var(--ca-space-4);
	}
	.lq-update__notes {
		max-height: min(50vh, 420px);
		overflow: auto;
		padding: var(--ca-space-3) var(--ca-space-4);
		border: 1px solid var(--ca-outline-subtle);
		background: var(--ca-surface-sunken);
		font-size: 14px;
		line-height: 1.5;
	}
	.lq-update__notes :global(.lq-update__h) {
		margin: var(--ca-space-3) 0 var(--ca-space-1);
		font-size: 14px;
		font-weight: 700;
	}
	.lq-update__notes :global(.lq-update__h:first-child) {
		margin-top: 0;
	}
	.lq-update__notes :global(.lq-update__li) {
		margin: 0 0 var(--ca-space-1) var(--ca-space-4);
		text-indent: calc(-1 * var(--ca-space-3));
	}
	.lq-update__notes :global(.lq-update__li)::before {
		content: "• ";
	}
	.lq-update__notes :global(.lq-update__p) {
		margin: 0 0 var(--ca-space-2);
	}
	.lq-update__notes :global(code) {
		padding: 0 3px;
		background: var(--ca-action-hover);
		font-family: var(--ca-font-mono);
		font-size: 12px;
	}
</style>
