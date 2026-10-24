<script lang="ts">
	import { onMount } from "svelte";
	import { Drawer, Toaster, colorScheme, toast } from "@codeatlasdev/ui";
	import { getVersion } from "@tauri-apps/api/app";
	import { openUrl } from "@tauri-apps/plugin-opener";
	import { exit } from "@tauri-apps/plugin-process";
	import TitleBar from "./lib/components/TitleBar.svelte";
	import { IS_MAC, appMenu, setNativeAppMenu } from "./lib/app-menu";
	import { windowFrame } from "./lib/state/window-frame.svelte";
	import { importer } from "./lib/state/importer.svelte";
	import { ai } from "./lib/state/ai.svelte";
	import { transfer } from "./lib/state/transfer.svelte";
	import { emptyProfile } from "./lib/api";
	import Sidebar from "./lib/components/Sidebar.svelte";
	import TabBar from "./lib/components/TabBar.svelte";
	import QueryView from "./lib/components/QueryView.svelte";
	import ConnectionDialog from "./lib/components/ConnectionDialog.svelte";
	import TnsImportDialog from "./lib/components/TnsImportDialog.svelte";
	import ImportDialog from "./lib/components/ImportDialog.svelte";
	import AiDialog from "./lib/components/AiDialog.svelte";
	import UpdateDialog from "./lib/components/UpdateDialog.svelte";
	import { updates } from "./lib/state/updates.svelte";
	import PasswordDialog from "./lib/components/PasswordDialog.svelte";
	import ConfirmDialog from "./lib/components/ConfirmDialog.svelte";
	import SqlActionDialog from "./lib/components/SqlActionDialog.svelte";
	import TableDesigner from "./lib/components/TableDesigner.svelte";
	import ExportDialog from "./lib/components/ExportDialog.svelte";
	import DataImportDialog from "./lib/components/DataImportDialog.svelte";
	import RunFileDialog from "./lib/components/RunFileDialog.svelte";
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

	const REPO = "https://github.com/LowBloat/LowQuery";
	const SEPARATOR = { separator: true } as const;

	async function checkForUpdates() {
		await updates.check(true);
		if (updates.error) toast.error(`Could not check for updates: ${updates.error}`);
		else if (!updates.available) toast.success("LowQuery is up to date.");
	}

	async function showAbout() {
		const version = await getVersion().catch(() => "");
		toast.info(version ? `LowQuery ${version} · AGPL-3.0-only` : "LowQuery · AGPL-3.0-only");
	}

	// Application menus: in the title bar on Windows and Linux, in the global
	// menu bar on macOS.
	const menus = $derived.by(() => {
		const tab = workspace.activeTab;
		const profile = workspace.profile(tab?.profileId ?? null);
		const connected = !!tab?.session;
		return [
			appMenu("file", "File", [
				{ label: "New query", shortcut: "Ctrl+T", action: () => workspace.newTab(tab?.profileId ?? null) },
				{ label: "New connection…", action: () => (workspace.editing = emptyProfile()) },
				{ label: "Import connections…", action: () => importer.show() },
				SEPARATOR,
				{ label: "Run SQL file…", disabled: !profile, action: () => profile && transfer.chooseSqlFile(profile, null) },
				SEPARATOR,
				{ label: "Close tab", shortcut: "Ctrl+W", disabled: !tab, action: () => tab && workspace.closeTab(tab.id) },
				!IS_MAC && SEPARATOR,
				!IS_MAC && { label: "Quit LowQuery", action: () => exit(0) },
			]),
			appMenu("query", "Query", [
				{ label: "Run statement", shortcut: "Ctrl+Enter", disabled: !tab || !profile || tab.running, action: () => tab && workspace.run(tab, "statement") },
				{ label: "Run script", shortcut: "F5", disabled: !tab || !profile || tab.running, action: () => tab && workspace.run(tab, "script") },
				{ label: "Stop", disabled: !tab?.running, action: () => tab && workspace.cancel(tab) },
				SEPARATOR,
				{ label: "Commit", disabled: !tab?.inTransaction, action: () => tab && workspace.endTransaction(tab, true) },
				{ label: "Rollback", disabled: !tab?.inTransaction, action: () => tab && workspace.endTransaction(tab, false) },
				{ label: "Auto-commit", checked: tab?.autocommit ?? true, disabled: !connected, action: () => tab && workspace.setAutocommit(tab, !tab.autocommit) },
			]),
			appMenu("view", "View", [
				{ label: "Dark theme", checked: colorScheme.isDark, action: () => colorScheme.toggle() },
				SEPARATOR,
				{ label: "AI agents…", action: () => ai.show() },
			]),
			appMenu("help", "Help", [
				{ label: "Documentation", action: () => openUrl(`${REPO}#readme`) },
				{ label: "Release notes", action: () => openUrl(`${REPO}/releases`) },
				{ label: "Report a problem", action: () => openUrl(`${REPO}/issues/new/choose`) },
				SEPARATOR,
				{ label: "Check for updates…", action: () => checkForUpdates() },
				!IS_MAC && SEPARATOR,
				!IS_MAC && { label: "About LowQuery", action: () => showAbout() },
			]),
		];
	});

	const activeProfile = $derived(workspace.profile(workspace.activeTab?.profileId ?? null));

	$effect(() => {
		if (!IS_MAC) return;
		void setNativeAppMenu(menus, { label: "About LowQuery", action: () => showAbout() }).catch((e) => console.error("Failed to set the menu bar:", e));
	});

	// The window starts hidden; it appears once the page has painted, and
	// follows the theme's background so resizing never shows white.
	$effect(() => {
		void colorScheme.isDark;
		windowFrame.painted();
	});

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
		updates.start();
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

<div class="lq-shell">
<TitleBar {menus} title={workspace.activeTab?.title ?? "LowQuery"} detail={activeProfile?.name ?? ""} />
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
</div>

{#if narrow}
	<Drawer bind:open={drawer} side="left" title="Connections" hideTitle width="min(88vw, 340px)" class="lq-drawer">
		<Sidebar oninsert={insertIntoEditor} onnavigate={() => (drawer = false)} />
	</Drawer>
{/if}

<ConnectionDialog />
<ImportDialog />
<AiDialog />
<TnsImportDialog />
<UpdateDialog />
<PasswordDialog />
<ConfirmDialog />
<SqlActionDialog />
<TableDesigner />
<ExportDialog />
<DataImportDialog />
<RunFileDialog />
<Toaster />

<style>
	.lq-shell {
		display: flex;
		flex-direction: column;
		height: var(--ca-vh, 100vh);
		overflow: hidden;
	}
	.lq-app {
		display: grid;
		grid-template-columns: var(--side-w) 4px minmax(0, 1fr);
		flex: 1;
		min-height: 0;
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
