<script lang="ts">
	import { onMount } from "svelte";
	import { Drawer, Toaster } from "@codeatlasdev/ui";
	import Sidebar from "./lib/components/Sidebar.svelte";
	import TabBar from "./lib/components/TabBar.svelte";
	import QueryView from "./lib/components/QueryView.svelte";
	import ConnectionDialog from "./lib/components/ConnectionDialog.svelte";
	import TnsImportDialog from "./lib/components/TnsImportDialog.svelte";
	import PasswordDialog from "./lib/components/PasswordDialog.svelte";
	import { workspace } from "./lib/state/workspace.svelte";

	const SIDE_KEY = "lowquery.sidebar.width";

	let views: Record<string, QueryView | null> = $state({});
	let narrow = $state(false);
	let drawer = $state(false);
	let sideWidth = $state(readWidth());

	function readWidth(): number {
		try {
			return Number(localStorage.getItem(SIDE_KEY)) || 288;
		} catch {
			return 288;
		}
	}

	function insertIntoEditor(text: string) {
		const id = workspace.activeTabId;
		if (id) views[id]?.insert(text);
		drawer = false;
	}

	function startResize(e: PointerEvent) {
		e.preventDefault();
		const target = e.currentTarget as HTMLElement;
		target.setPointerCapture(e.pointerId);
		const move = (ev: PointerEvent) => (sideWidth = Math.min(560, Math.max(200, ev.clientX)));
		const up = () => {
			target.removeEventListener("pointermove", move);
			target.removeEventListener("pointerup", up);
			try {
				localStorage.setItem(SIDE_KEY, String(sideWidth));
			} catch {
				// Preference kept only for this run.
			}
		};
		target.addEventListener("pointermove", move);
		target.addEventListener("pointerup", up);
	}

	function onkeydown(e: KeyboardEvent) {
		const mod = e.ctrlKey || e.metaKey;
		// Reloading the page would discard open sessions and transactions.
		if (e.key === "F5" || (mod && e.key.toLowerCase() === "r")) {
			e.preventDefault();
			const tab = workspace.activeTab;
			if (e.key === "F5" && tab) workspace.run(tab, "script");
			return;
		}
		if (mod && e.key.toLowerCase() === "t") {
			e.preventDefault();
			workspace.newTab(workspace.activeTab?.profileId ?? null);
		} else if (mod && e.key.toLowerCase() === "w") {
			e.preventDefault();
			if (workspace.activeTabId) workspace.closeTab(workspace.activeTabId);
		}
	}

	onMount(() => {
		workspace.init();
		const mq = window.matchMedia("(max-width: 760px)");
		const sync = () => {
			narrow = mq.matches;
			if (!narrow) drawer = false;
		};
		sync();
		mq.addEventListener("change", sync);
		return () => mq.removeEventListener("change", sync);
	});
</script>

<svelte:window {onkeydown} />

<div class="lq-app" class:lq-app--narrow={narrow} style="--side-w:{sideWidth}px">
	{#if !narrow}
		<aside class="lq-app__side">
			<Sidebar oninsert={insertIntoEditor} />
		</aside>
		<div
			class="lq-app__resizer"
			role="separator"
			aria-orientation="vertical"
			aria-label="Resize sidebar"
			onpointerdown={startResize}
		></div>
	{/if}
	<main id="main" class="lq-app__main" tabindex="-1">
		<TabBar {narrow} onmenu={() => (drawer = true)} />
		{#each workspace.tabs as tab (tab.id)}
			<div
				id="lq-panel-{tab.id}"
				class="lq-app__panel"
				role="tabpanel"
				aria-labelledby="lq-tab-{tab.id}"
				hidden={tab.id !== workspace.activeTabId}
			>
				<QueryView {tab} bind:this={views[tab.id]} />
			</div>
		{/each}
	</main>
</div>

{#if narrow}
	<Drawer bind:open={drawer} side="left" title="Connections" hideTitle width="min(88vw, 340px)" class="lq-drawer">
		<Sidebar oninsert={insertIntoEditor} onnavigate={() => (drawer = false)} />
	</Drawer>
{/if}

<ConnectionDialog />
<TnsImportDialog />
<PasswordDialog />
<Toaster />

<style>
	.lq-app {
		display: grid;
		grid-template-columns: var(--side-w) 4px minmax(0, 1fr);
		height: var(--ca-vh, 100vh);
		overflow: hidden;
	}
	.lq-app--narrow {
		grid-template-columns: minmax(0, 1fr);
	}
	.lq-app__side {
		min-width: 0;
		min-height: 0;
		border-right: 1px solid var(--ca-outline-subtle);
	}
	.lq-app__resizer {
		cursor: col-resize;
		touch-action: none;
		background: var(--ca-surface-sunken);
	}
	.lq-app__resizer:hover {
		background: var(--ca-brand-fill);
	}
	.lq-app__main {
		display: flex;
		flex-direction: column;
		min-width: 0;
		min-height: 0;
		outline: none;
	}
	.lq-app__panel {
		flex: 1;
		min-height: 0;
	}
	.lq-app__panel[hidden] {
		display: none;
	}
</style>
