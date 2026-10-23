<script lang="ts">
	import { Dialog, Button, Checkbox, Progress } from "@codeatlasdev/ui";
	import { transfer } from "../state/transfer.svelte";

	let stopOnError = $state(true);

	const t = $derived(transfer.runningFile);
	const fileName = $derived(t?.path.split(/[\\/]/).pop() ?? "");
	const size = $derived(
		!t ? "" : t.bytes >= 1 << 20 ? `${(t.bytes / (1 << 20)).toFixed(1)} MB` : `${Math.max(1, Math.round(t.bytes / 1024))} KB`,
	);
	const p = $derived(transfer.progress);
	const firstLine = (s: string) => s.split("\n").find((l) => l.trim() && !l.trim().startsWith("--"))?.trim() ?? s;
</script>

{#if t}
	<Dialog
		open={true}
		size="lg"
		title="Run {fileName}"
		description="{t.statements.length.toLocaleString()} statements · {size} · on {t.profile.name}{t.schema ? `, in ${t.schema}` : ''}"
		onOpenChange={(o) => {
			if (!o) transfer.close();
		}}
	>
		<div class="lq-run">
			{#if transfer.phase === "ready"}
				<Checkbox label="Stop at the first error" bind:checked={stopOnError} />
				<p class="lq-run__text">Statements run one by one on a connection of their own, with autocommit, as in a query tab.</p>
			{:else if transfer.phase === "running"}
				<Progress value={p ? p.step - 1 : 0} max={t.statements.length} label="Progress" />
				<p class="lq-run__text">Statement {p?.step ?? 0} of {t.statements.length.toLocaleString()}</p>
			{:else}
				<p class="lq-run__done">{transfer.result}</p>
			{/if}
			{#if transfer.runErrors.length}
				<ul class="lq-run__errors">
					{#each transfer.runErrors.slice(0, 20) as e (e.index)}
						<li>
							<span class="lq-run__stmt">#{e.index} {firstLine(e.statement)}</span>
							<span class="lq-run__msg">{e.message}</span>
						</li>
					{/each}
				</ul>
				{#if transfer.runErrors.length > 20}
					<p class="lq-run__text">and {transfer.runErrors.length - 20} more errors</p>
				{/if}
			{/if}
		</div>
		{#snippet actions()}
			{#if transfer.phase === "ready"}
				<Button variant="ghost" onclick={() => transfer.close()}>Cancel</Button>
				<Button variant="filled" onclick={() => transfer.runFile(stopOnError)}>Run</Button>
			{:else if transfer.phase === "running"}
				<Button variant="outlined" onclick={() => transfer.cancel()}>Stop</Button>
			{:else}
				<Button variant="filled" onclick={() => transfer.close()}>Close</Button>
			{/if}
		{/snippet}
	</Dialog>
{/if}

<style>
	.lq-run {
		display: flex;
		flex-direction: column;
		gap: var(--ca-space-3);
	}
	.lq-run__text,
	.lq-run__done {
		margin: 0;
		font-size: 13px;
	}
	.lq-run__text {
		color: var(--ca-text-secondary);
	}
	.lq-run__done {
		font-weight: 600;
	}
	.lq-run__errors {
		display: flex;
		flex-direction: column;
		gap: var(--ca-space-2);
		max-height: 220px;
		margin: 0;
		padding: 0;
		overflow: auto;
		list-style: none;
	}
	.lq-run__errors li {
		display: flex;
		flex-direction: column;
		gap: 2px;
		font-size: 12px;
	}
	.lq-run__stmt {
		overflow: hidden;
		font-family: var(--ca-font-mono);
		white-space: nowrap;
		text-overflow: ellipsis;
	}
	.lq-run__msg {
		color: var(--ca-semantic-error);
	}
</style>
