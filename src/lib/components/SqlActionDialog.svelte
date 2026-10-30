<script lang="ts">
	import { untrack } from "svelte";
	import { Dialog, Button, TextField, Checkbox } from "@codeatlasdev/ui";
	import { FileCode, TriangleAlert } from "@lucide/svelte";
	import { actions as act, type ActionValues } from "../state/actions.svelte";
	import { toScript } from "../sql/ddl";

	let input = $state("");
	let options = $state<Record<string, boolean>>({});
	let typed = $state("");
	let field = $state<HTMLInputElement | null>(null);
	let confirmField = $state<HTMLInputElement | null>(null);

	const req = $derived(act.request);

	$effect(() => {
		if (req) untrack(() => reset());
	});

	function reset() {
		if (!req) return;
		input = req.input?.value ?? "";
		options = Object.fromEntries((req.options ?? []).map((o) => [o.id, o.checked]));
		typed = "";
		queueMicrotask(() => {
			const el = field ?? confirmField;
			el?.focus();
			el?.select();
		});
	}

	const values = $derived<ActionValues>({ input, options });
	const plan = $derived(req ? req.build(values) : { statements: [], warnings: [] });
	const sql = $derived(req ? toScript(plan.statements, req.profile.engine) : "");
	const confirmed = $derived(!req?.typeToConfirm || typed === req.typeToConfirm);
	const ready = $derived(plan.statements.length > 0 && confirmed && !act.running);

	function submit() {
		if (ready) act.confirm(values);
	}
</script>

{#if req}
	<Dialog
		open={true}
		size="lg"
		title={req.title}
		description={req.description}
		onOpenChange={(o) => {
			if (!o) act.cancel();
		}}
	>
		<form
			class="lq-act"
			onsubmit={(e) => {
				e.preventDefault();
				submit();
			}}
		>
			{#if req.input}
				<TextField label={req.input.label} placeholder={req.input.placeholder ?? ""} bind:value={input} bind:ref={field} />
			{/if}
			{#each req.options ?? [] as o (o.id)}
				<Checkbox label={o.label} checked={options[o.id]} onchange={(c) => (options = { ...options, [o.id]: c })} />
			{/each}

			<div class="lq-act__sql">
				<span class="lq-act__label">{req.profile.engine === "redis" ? "Commands" : req.profile.engine === "cassandra" ? "CQL" : "SQL"}</span>
				{#if sql}
					<pre>{sql}</pre>
				{:else}
					<p class="lq-act__none">Nothing to run yet.</p>
				{/if}
			</div>

			{#each plan.warnings as w (w)}
				<p class="lq-act__warn"><TriangleAlert size={14} aria-hidden="true" />{w}</p>
			{/each}

			{#if req.typeToConfirm}
				<TextField
					label="Type {req.typeToConfirm} to confirm"
					autocomplete="off"
					bind:value={typed}
					bind:ref={confirmField}
				/>
			{/if}

			{#if act.error}
				<p class="lq-act__error" role="alert">{act.error}</p>
			{/if}
		</form>
		{#snippet actions()}
			<Button variant="ghost" disabled={act.running} onclick={() => act.cancel()}>Cancel</Button>
			<Button variant="outlined" disabled={plan.statements.length === 0 || act.running} onclick={() => act.toEditor(values)}>
				{#snippet leading()}<FileCode size={15} />{/snippet}
				Open in editor
			</Button>
			<Button variant={req.destructive ? "destructive" : "filled"} disabled={!ready} loading={act.running} onclick={submit}>
				{req.confirmLabel}
			</Button>
		{/snippet}
	</Dialog>
{/if}

<style>
	.lq-act {
		display: flex;
		flex-direction: column;
		gap: var(--ca-space-3);
	}
	.lq-act__sql {
		display: flex;
		flex-direction: column;
		gap: var(--ca-space-1);
	}
	.lq-act__label {
		color: var(--ca-text-secondary);
		font-family: var(--ca-font-mono);
		font-size: 11px;
		font-weight: 600;
		letter-spacing: 0.06em;
		text-transform: uppercase;
	}
	.lq-act__sql pre {
		max-height: 240px;
		margin: 0;
		padding: var(--ca-space-2) var(--ca-space-3);
		overflow: auto;
		border: 1px solid var(--ca-outline-subtle);
		background: var(--ca-surface-sunken);
		font-family: var(--ca-font-mono);
		font-size: 12px;
		line-height: 1.5;
		white-space: pre-wrap;
		overflow-wrap: anywhere;
	}
	.lq-act__none {
		margin: 0;
		color: var(--ca-text-hint);
		font-size: 13px;
	}
	.lq-act__warn,
	.lq-act__error {
		display: flex;
		gap: var(--ca-space-2);
		align-items: flex-start;
		margin: 0;
		font-size: 13px;
	}
	.lq-act__warn {
		color: var(--ca-text-secondary);
	}
	.lq-act__warn :global(svg) {
		flex-shrink: 0;
		margin-top: 2px;
		color: var(--ca-semantic-warning);
	}
	.lq-act__error {
		color: var(--ca-semantic-error);
		white-space: pre-wrap;
	}
</style>
