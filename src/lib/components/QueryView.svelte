<script lang="ts">
	import { Button, IconButton, Select, Switch, Tooltip, Kbd, Chip } from "@codeatlasdev/ui";
	import { Play, ListVideo, Square, Check, Undo2, Plug, Unplug } from "@lucide/svelte";
	import SqlEditor from "./SqlEditor.svelte";
	import ResultsPanel from "./ResultsPanel.svelte";
	import { workspace, type QueryTab } from "../state/workspace.svelte";
	import { explorer } from "../state/explorer.svelte";
	import { ENGINE_LABEL } from "../api";

	interface Props {
		tab: QueryTab;
	}

	let { tab }: Props = $props();

	let editor = $state<SqlEditor | null>(null);
	let split = $state(0.48);
	let body: HTMLDivElement;

	const profile = $derived(workspace.profile(tab.profileId));
	const engine = $derived(profile?.engine ?? "postgres");
	const completion = $derived(explorer.completion(tab.profileId));

	const connectionOptions = $derived([
		{ value: "", label: "No connection" },
		...workspace.profiles.map((p) => ({ value: p.id, label: `${p.name} · ${ENGINE_LABEL[p.engine]}` })),
	]);

	const limitOptions = [
		{ value: "100", label: "100 rows" },
		{ value: "500", label: "500 rows" },
		{ value: "1000", label: "1,000 rows" },
		{ value: "5000", label: "5,000 rows" },
		{ value: "50000", label: "50,000 rows" },
		{ value: "0", label: "No limit" },
	];

	let draftTimer: ReturnType<typeof setTimeout> | undefined;
	function onchange(v: string) {
		tab.sql = v;
		clearTimeout(draftTimer);
		draftTimer = setTimeout(() => workspace.saveDrafts(), 600);
	}

	function startSplit(e: PointerEvent) {
		e.preventDefault();
		const target = e.currentTarget as HTMLElement;
		target.setPointerCapture(e.pointerId);
		const rect = body.getBoundingClientRect();
		const move = (ev: PointerEvent) => {
			split = Math.min(0.85, Math.max(0.12, (ev.clientY - rect.top) / rect.height));
		};
		const up = () => {
			target.removeEventListener("pointermove", move);
			target.removeEventListener("pointerup", up);
		};
		target.addEventListener("pointermove", move);
		target.addEventListener("pointerup", up);
	}

	function splitKey(e: KeyboardEvent) {
		if (e.key === "ArrowUp") split = Math.max(0.12, split - 0.04);
		else if (e.key === "ArrowDown") split = Math.min(0.85, split + 0.04);
		else return;
		e.preventDefault();
	}

	export function insert(text: string) {
		editor?.insert(text);
	}
</script>

<div class="lq-query">
	<div class="lq-toolbar" role="toolbar" aria-label="Query actions">
		<div class="lq-toolbar__group lq-toolbar__conn">
			<span
				class="lq-conn-dot"
				class:lq-conn-dot--on={!!tab.session}
				style="--conn-color:{profile?.color ?? 'transparent'}"
				aria-hidden="true"
			></span>
			<Select
				size="sm"
				aria-label="Connection"
				value={tab.profileId ?? ""}
				options={connectionOptions}
				onchange={(v) => workspace.setConnection(tab, v || null)}
			/>
			{#if tab.session}
				<Tooltip content="Disconnect">
					{#snippet triggerChild(props)}<IconButton {...props} size="sm" variant="text" aria-label="Disconnect" onclick={() => workspace.disconnect(tab)}>
						<Unplug size={16} />
					</IconButton>{/snippet}
				</Tooltip>
			{:else if profile}
				<Tooltip content="Connect">
					{#snippet triggerChild(props)}<IconButton {...props} size="sm" variant="text" aria-label="Connect" loading={tab.connecting} onclick={() => workspace.connect(tab)}>
						<Plug size={16} />
					</IconButton>{/snippet}
				</Tooltip>
			{/if}
		</div>

		<div class="lq-toolbar__group">
			{#if tab.running}
				<Button size="sm" variant="destructive" loading={tab.cancelling} onclick={() => workspace.cancel(tab)}>
					{#snippet leading()}<Square size={14} />{/snippet}
					Stop
				</Button>
			{:else}
				<Tooltip content="Run the selection or the statement under the cursor (Ctrl+Enter)">
					{#snippet triggerChild(props)}<Button {...props} size="sm" variant="filled" disabled={!profile} onclick={() => workspace.run(tab, "statement")}>
						{#snippet leading()}<Play size={14} />{/snippet}
						Run
					</Button>{/snippet}
				</Tooltip>
				<Tooltip content="Run all statements in order (Ctrl+Shift+Enter or F5)">
					{#snippet triggerChild(props)}<Button {...props} size="sm" variant="outlined" disabled={!profile} onclick={() => workspace.run(tab, "script")}>
						{#snippet leading()}<ListVideo size={14} />{/snippet}
						Run script
					</Button>{/snippet}
				</Tooltip>
			{/if}
		</div>

		<div class="lq-toolbar__group">
			<Switch
				label="Autocommit"
				checked={tab.autocommit}
				onchange={(on) => workspace.setAutocommit(tab, on)}
			/>
			{#if !tab.autocommit || tab.inTransaction}
				<Tooltip content="Commit transaction">
					{#snippet triggerChild(props)}<Button {...props} size="sm" variant="outlined" disabled={!tab.inTransaction || tab.running} onclick={() => workspace.endTransaction(tab, true)}>
						{#snippet leading()}<Check size={14} />{/snippet}
						Commit
					</Button>{/snippet}
				</Tooltip>
				<Tooltip content="Roll back transaction">
					{#snippet triggerChild(props)}<Button {...props} size="sm" variant="ghost" disabled={!tab.inTransaction || tab.running} onclick={() => workspace.endTransaction(tab, false)}>
						{#snippet leading()}<Undo2 size={14} />{/snippet}
						Rollback
					</Button>{/snippet}
				</Tooltip>
			{/if}
			{#if tab.inTransaction}
				<Chip label="Open transaction" variant="warning" size="sm" />
			{/if}
		</div>

		<span class="ca-toolbar-spacer"></span>

		<div class="lq-toolbar__group">
			<Select
				size="sm"
				aria-label="Row limit"
				value={String(tab.maxRows)}
				options={limitOptions}
				onchange={(v) => {
					tab.maxRows = Number(v);
					workspace.saveDrafts();
				}}
			/>
		</div>
	</div>

	<div class="lq-query__body" bind:this={body} style="--split:{split * 100}%">
		<div class="lq-query__editor">
			<SqlEditor
				bind:this={editor}
				value={tab.sql}
				{engine}
				schema={completion.schema}
				defaultSchema={completion.defaultSchema}
				error={tab.editorError}
				label="SQL editor for {tab.title}"
				{onchange}
				oncursor={(pos, sel, from) => {
					tab.cursor = pos;
					tab.selection = sel;
					tab.selectionFrom = from;
				}}
				onrun={(mode) => workspace.run(tab, mode)}
			/>
		</div>
		<!-- A focusable separator is an interactive widget in WAI-ARIA (arrow keys resize). -->
		<!-- svelte-ignore a11y_no_noninteractive_tabindex, a11y_no_noninteractive_element_interactions -->
		<div
			class="lq-query__splitter"
			role="separator"
			aria-orientation="horizontal"
			aria-label="Resize editor and results"
			aria-valuenow={Math.round(split * 100)}
			aria-valuemin={12}
			aria-valuemax={85}
			tabindex="0"
			onpointerdown={startSplit}
			onkeydown={splitKey}
		></div>
		<div class="lq-query__results">
			<ResultsPanel {tab} />
		</div>
	</div>

	<footer class="lq-status">
		{#if tab.session}
			<span>{tab.session.serverVersion}</span>
			<span>{tab.session.currentUser}{tab.session.currentSchema ? ` · ${tab.session.currentSchema}` : ""}</span>
		{:else if profile}
			<span>{profile.name} · disconnected</span>
		{:else}
			<span>Choose a connection</span>
		{/if}
		<span class="ca-toolbar-spacer"></span>
		<span class="lq-status__keys"><Kbd>Ctrl</Kbd>+<Kbd>Enter</Kbd> statement · <Kbd>F5</Kbd> script</span>
	</footer>
</div>

<style>
	.lq-query {
		container-type: inline-size;
		display: flex;
		flex-direction: column;
		height: 100%;
		min-height: 0;
	}
	.lq-toolbar {
		display: flex;
		flex-wrap: wrap;
		align-items: center;
		gap: var(--ca-space-2) var(--ca-space-4);
		padding: var(--ca-space-2) var(--ca-space-3);
		border-bottom: 1px solid var(--ca-outline-subtle);
		background: var(--ca-surface-base);
	}
	.lq-toolbar__group {
		display: flex;
		align-items: center;
		gap: var(--ca-space-2);
	}
	.lq-toolbar__conn :global(.ca-sel) {
		min-width: 220px;
		max-width: 320px;
	}
	.lq-conn-dot {
		width: 10px;
		height: 10px;
		flex-shrink: 0;
		border: 2px solid var(--ca-outline-strong);
		background: var(--conn-color);
	}
	.lq-conn-dot--on {
		border-color: var(--ca-semantic-success);
		box-shadow: inset 0 0 0 2px var(--ca-surface-base);
		background: var(--ca-semantic-success);
	}
	.lq-query__body {
		display: grid;
		grid-template-rows: var(--split) 6px minmax(0, 1fr);
		flex: 1;
		min-height: 0;
	}
	.lq-query__editor,
	.lq-query__results {
		min-height: 0;
		overflow: hidden;
	}
	.lq-query__splitter {
		background: var(--ca-surface-sunken);
		border-top: 1px solid var(--ca-outline-subtle);
		border-bottom: 1px solid var(--ca-outline-subtle);
		cursor: row-resize;
		touch-action: none;
	}
	.lq-query__splitter:hover,
	.lq-query__splitter:focus-visible {
		background: var(--ca-brand-fill);
		outline: none;
	}
	.lq-status {
		display: flex;
		align-items: center;
		gap: var(--ca-space-4);
		min-height: 26px;
		padding: 0 var(--ca-space-3);
		border-top: 1px solid var(--ca-outline-subtle);
		background: var(--ca-surface-sunken);
		color: var(--ca-text-secondary);
		font-family: var(--ca-font-mono);
		font-size: 12px;
		white-space: nowrap;
		overflow: hidden;
	}
	.lq-status__keys {
		font-family: var(--ca-font-sans);
	}
	@container (max-width: 640px) {
		.lq-toolbar__conn {
			width: 100%;
		}
		.lq-toolbar__conn :global(.ca-sel) {
			flex: 1;
			min-width: 0;
			max-width: none;
		}
		.lq-status__keys {
			display: none;
		}
	}
</style>
