<script lang="ts">
	import { IconButton, Tooltip } from "@codeatlasdev/ui";
	import { Plus, X, Menu, LoaderCircle } from "@lucide/svelte";
	import { workspace, type QueryTab } from "../state/workspace.svelte";

	interface Props {
		narrow?: boolean;
		onmenu?: () => void;
	}

	let { narrow = false, onmenu }: Props = $props();

	let renaming = $state<string | null>(null);
	let draft = $state("");

	function startRename(tab: QueryTab) {
		renaming = tab.id;
		draft = tab.title;
	}

	function commitRename(tab: QueryTab) {
		if (draft.trim()) tab.title = draft.trim();
		renaming = null;
		workspace.saveDrafts();
	}

	function onkeydown(e: KeyboardEvent, index: number) {
		const tabs = workspace.tabs;
		let next = index;
		if (e.key === "ArrowRight") next = (index + 1) % tabs.length;
		else if (e.key === "ArrowLeft") next = (index - 1 + tabs.length) % tabs.length;
		else if (e.key === "Home") next = 0;
		else if (e.key === "End") next = tabs.length - 1;
		else if (e.key === "Delete") {
			workspace.closeTab(tabs[index].id);
			return;
		} else if (e.key === "F2") {
			startRename(tabs[index]);
			return;
		} else return;
		e.preventDefault();
		workspace.activeTabId = tabs[next].id;
		document.getElementById(`lq-tab-${tabs[next].id}`)?.focus();
	}

	function autofocus(node: HTMLInputElement) {
		node.focus();
		node.select();
	}
</script>

<div class="lq-tabbar">
	{#if narrow}
		<IconButton size="sm" variant="text" aria-label="Open connections" onclick={onmenu}>
			<Menu size={18} />
		</IconButton>
	{/if}
	<div class="lq-tabbar__list" role="tablist" aria-label="Open queries">
		{#each workspace.tabs as tab, i (tab.id)}
			{@const profile = workspace.profile(tab.profileId)}
			{@const selected = tab.id === workspace.activeTabId}
			<div class="lq-tab" class:lq-tab--active={selected} style="--conn-color:{profile?.color ?? 'transparent'}">
				{#if renaming === tab.id}
					<input
						class="lq-tab__rename"
						aria-label="Query name"
						bind:value={draft}
						use:autofocus
						onblur={() => commitRename(tab)}
						onkeydown={(e) => {
							if (e.key === "Enter") commitRename(tab);
							if (e.key === "Escape") renaming = null;
						}}
					/>
				{:else}
					<button
						id="lq-tab-{tab.id}"
						type="button"
						role="tab"
						class="lq-tab__btn"
						aria-selected={selected}
						aria-controls="lq-panel-{tab.id}"
						tabindex={selected ? 0 : -1}
						title={profile ? `${tab.title} · ${profile.name}` : tab.title}
						onclick={() => (workspace.activeTabId = tab.id)}
						ondblclick={() => startRename(tab)}
						onkeydown={(e) => onkeydown(e, i)}
						onauxclick={(e) => {
							if (e.button === 1) workspace.closeTab(tab.id);
						}}
					>
						{#if tab.running}
							<LoaderCircle size={13} class="lq-tab__spin" aria-label="Running" />
						{:else}
							<span class="lq-tab__dot" class:lq-tab__dot--tx={tab.inTransaction} aria-hidden="true"></span>
						{/if}
						<span class="lq-tab__title">{tab.title}</span>
						{#if profile && !narrow}
							<span class="lq-tab__conn">{profile.name}</span>
						{/if}
					</button>
				{/if}
				<button type="button" class="lq-tab__close" aria-label="Close {tab.title}" onclick={() => workspace.closeTab(tab.id)}>
					<X size={13} />
				</button>
			</div>
		{/each}
	</div>
	<Tooltip content="New query (Ctrl+T)">
		{#snippet triggerChild(props)}
			<IconButton {...props} size="sm" variant="text" aria-label="New query" onclick={() => workspace.newTab(workspace.activeTab?.profileId ?? null)}>
				<Plus size={16} />
			</IconButton>
		{/snippet}
	</Tooltip>
</div>

<style>
	.lq-tabbar {
		display: flex;
		align-items: stretch;
		gap: var(--ca-space-1);
		height: 38px;
		padding: 0 var(--ca-space-2) 0 0;
		border-bottom: 1px solid var(--ca-outline-subtle);
		background: var(--ca-surface-sunken);
	}
	.lq-tabbar > :global(.ca-icon-btn) {
		align-self: center;
	}
	.lq-tabbar__list {
		display: flex;
		flex: 0 1 auto;
		min-width: 0;
		overflow-x: auto;
		scrollbar-width: none;
	}
	.lq-tab {
		position: relative;
		display: flex;
		align-items: center;
		flex-shrink: 0;
		max-width: 260px;
		border-right: 1px solid var(--ca-outline-subtle);
		box-shadow: inset 0 3px 0 var(--conn-color);
		color: var(--ca-text-secondary);
	}
	.lq-tab--active {
		background: var(--ca-surface-base);
		color: var(--ca-text-primary);
	}
	.lq-tab--active::after {
		content: "";
		position: absolute;
		right: 0;
		bottom: -1px;
		left: 0;
		height: 1px;
		background: var(--ca-surface-base);
	}
	.lq-tab__btn {
		display: flex;
		align-items: center;
		gap: 6px;
		min-width: 0;
		height: 100%;
		padding: 0 4px 0 12px;
		border: 0;
		background: transparent;
		color: inherit;
		font-size: 13px;
		cursor: pointer;
	}
	.lq-tab__btn:focus-visible,
	.lq-tab__close:focus-visible {
		outline: 2px solid var(--ca-focus-ring);
		outline-offset: -2px;
	}
	.lq-tab__dot {
		width: 6px;
		height: 6px;
		flex-shrink: 0;
		background: var(--ca-outline-default);
	}
	.lq-tab__dot--tx {
		background: var(--ca-semantic-warning);
	}
	.lq-tab__btn :global(.lq-tab__spin) {
		animation: lq-spin 0.9s linear infinite;
	}
	.lq-tab__title {
		overflow: hidden;
		font-weight: 550;
		white-space: nowrap;
		text-overflow: ellipsis;
	}
	.lq-tab__conn {
		overflow: hidden;
		color: var(--ca-text-hint);
		font-family: var(--ca-font-mono);
		font-size: 11px;
		white-space: nowrap;
		text-overflow: ellipsis;
	}
	.lq-tab__close {
		display: grid;
		place-items: center;
		width: 24px;
		height: 24px;
		margin-right: 4px;
		padding: 0;
		border: 0;
		background: transparent;
		color: var(--ca-text-hint);
		cursor: pointer;
	}
	.lq-tab__close:hover {
		background: var(--ca-action-hover);
		color: var(--ca-text-primary);
	}
	.lq-tab__rename {
		width: 160px;
		margin: 0 4px 0 8px;
		padding: 2px 6px;
		border: 1px solid var(--ca-focus-ring);
		background: var(--ca-surface-raised);
		color: var(--ca-text-primary);
		font-size: 16px;
	}
	@keyframes lq-spin {
		to {
			transform: rotate(360deg);
		}
	}
	@media (prefers-reduced-motion: reduce) {
		.lq-tab__btn :global(.lq-tab__spin) {
			animation: none;
		}
	}
</style>
