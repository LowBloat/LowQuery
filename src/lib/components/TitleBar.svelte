<script lang="ts">
	import { onMount } from "svelte";
	import { DropdownMenu, DropdownMenuCheckboxItem, DropdownMenuItem, DropdownMenuSeparator } from "@codeatlasdev/ui";
	import { IS_MAC, isSeparator, shortcutLabel, type AppMenu } from "../app-menu";
	import { windowFrame as frame } from "../state/window-frame.svelte";

	interface Props {
		menus: AppMenu[];
		title: string;
		/** Shown dimmed after the title, such as the connection name. */
		detail?: string;
	}

	let { menus, title, detail = "" }: Props = $props();

	let openIndex = $state<number | null>(null);
	let triggers: HTMLElement[] = $state([]);
	// Alt pressed and released alone moves the focus to the menu bar, as on Windows.
	let altAlone = false;
	let focusBefore: HTMLElement | null = null;

	function modalOpen() {
		return !!document.querySelector('[role="dialog"][aria-modal="true"], dialog[open]');
	}

	function setOpen(index: number, open: boolean) {
		if (open && modalOpen()) return;
		if (open) openIndex = index;
		else if (openIndex === index) openIndex = null;
	}

	function focusMenuBar() {
		const active = document.activeElement;
		if (active instanceof HTMLElement && !triggers.includes(active)) focusBefore = active;
		triggers[0]?.focus();
	}

	function leaveMenuBar() {
		const target = focusBefore?.isConnected ? focusBefore : null;
		focusBefore = null;
		target?.focus();
	}

	function onTriggerKeydown(event: KeyboardEvent, index: number) {
		if (event.key !== "ArrowLeft" && event.key !== "ArrowRight") {
			if (event.key === "Escape" && openIndex === null) leaveMenuBar();
			return;
		}
		event.preventDefault();
		const next = (index + (event.key === "ArrowRight" ? 1 : -1) + menus.length) % menus.length;
		triggers[next]?.focus();
	}

	function onWindowKeydown(event: KeyboardEvent) {
		altAlone = event.key === "Alt" && !event.repeat && !event.ctrlKey && !event.shiftKey && !event.metaKey;
		if (IS_MAC) return;
		if (event.key === "F10" && !event.shiftKey && !event.ctrlKey && !event.altKey && !modalOpen()) {
			event.preventDefault();
			focusMenuBar();
		} else if (openIndex !== null && (event.key === "ArrowLeft" || event.key === "ArrowRight")
			&& !(event.target as Element | null)?.closest?.('[aria-haspopup="menu"][aria-expanded="true"]:not([data-titlebar-menu])')) {
			// Left and right move between the menus of the bar, as in native menu bars.
			event.preventDefault();
			event.stopPropagation();
			const next = (openIndex + (event.key === "ArrowRight" ? 1 : -1) + menus.length) % menus.length;
			openIndex = next;
		}
	}

	function onWindowKeyup(event: KeyboardEvent) {
		if (event.key !== "Alt" || !altAlone || IS_MAC || modalOpen()) return;
		altAlone = false;
		event.preventDefault();
		if (triggers.includes(document.activeElement as HTMLElement)) leaveMenuBar();
		else focusMenuBar();
	}

	onMount(() => {
		void frame.start();
		return () => frame.stop();
	});
</script>

<svelte:window onkeydowncapture={onWindowKeydown} onkeyup={onWindowKeyup} />

<header class="lq-titlebar" class:lq-titlebar--mac={IS_MAC} class:lq-titlebar--inactive={!frame.focused} data-tauri-drag-region>
	{#if !IS_MAC}
		<img src="/favicon.svg" alt="" class="lq-titlebar__logo" draggable="false" data-tauri-drag-region />
		<div class="lq-titlebar__menus" role="menubar" aria-label="Application menu">
			{#each menus as menu, index (menu.id)}
				<DropdownMenu open={openIndex === index} onOpenChange={(open) => setOpen(index, open)} align="start" sideOffset={2}>
					{#snippet triggerChild(props)}
						<button
							{...props}
							bind:this={triggers[index]}
							type="button"
							class="lq-titlebar__menu"
							data-titlebar-menu
							tabindex={index === 0 ? 0 : -1}
							onpointerenter={() => { if (openIndex !== null && openIndex !== index) openIndex = index; }}
							onkeydown={(event) => onTriggerKeydown(event, index)}
						>{menu.label}</button>
					{/snippet}
					{#each menu.entries as entry, i (i)}
						{#if isSeparator(entry)}
							<DropdownMenuSeparator />
						{:else if entry.checked !== undefined}
							<DropdownMenuCheckboxItem checked={entry.checked} disabled={entry.disabled} closeOnSelect onCheckedChange={() => void entry.action()}>
								{entry.label}
							</DropdownMenuCheckboxItem>
						{:else}
							<DropdownMenuItem disabled={entry.disabled} onclick={() => void entry.action()}>
								<span class="lq-titlebar__item">{entry.label}{#if entry.shortcut}<span class="lq-titlebar__key">{shortcutLabel(entry.shortcut)}</span>{/if}</span>
							</DropdownMenuItem>
						{/if}
					{/each}
				</DropdownMenu>
			{/each}
		</div>
	{/if}

	<!-- Centered on the window, not on the space between menus and buttons. -->
	<div class="lq-titlebar__title">
		<span class="lq-titlebar__name">{title}</span>
		{#if detail}<span class="lq-titlebar__detail">— {detail}</span>{/if}
	</div>

	<div class="lq-titlebar__drag" data-tauri-drag-region></div>

	{#if !IS_MAC}
		<div class="lq-titlebar__buttons">
			<button type="button" class="lq-titlebar__button" onclick={frame.minimize} aria-label="Minimize" title="Minimize">
				<svg viewBox="0 0 10 10" aria-hidden="true"><path d="M0 5h10" /></svg>
			</button>
			<button type="button" class="lq-titlebar__button" onclick={frame.toggleMaximize}
				aria-label={frame.maximized ? "Restore" : "Maximize"} title={frame.maximized ? "Restore" : "Maximize"}>
				{#if frame.maximized}
					<svg viewBox="0 0 10 10" aria-hidden="true"><path d="M2.5 2.5V.5h7v7h-2M.5 2.5h7v7h-7z" /></svg>
				{:else}
					<svg viewBox="0 0 10 10" aria-hidden="true"><path d="M.5.5h9v9h-9z" /></svg>
				{/if}
			</button>
			<button type="button" class="lq-titlebar__button lq-titlebar__button--close" onclick={frame.close} aria-label="Close" title="Close">
				<svg viewBox="0 0 10 10" aria-hidden="true"><path d="M0 0l10 10M10 0L0 10" /></svg>
			</button>
		</div>
	{/if}
</header>

<style>
	.lq-titlebar {
		position: relative;
		display: flex;
		align-items: stretch;
		flex-shrink: 0;
		height: 34px;
		border-bottom: 1px solid var(--ca-outline-subtle);
		background: var(--ca-surface-sunken);
		color: var(--ca-text-secondary);
		font-size: 13px;
		user-select: none;
	}
	/* Room for the native traffic lights drawn over the bar. */
	.lq-titlebar--mac {
		padding-left: 78px;
	}
	.lq-titlebar__logo {
		width: 16px;
		height: 16px;
		align-self: center;
		margin: 0 10px 0 12px;
	}
	.lq-titlebar__menus {
		display: flex;
		align-items: stretch;
	}
	.lq-titlebar__menu {
		margin: 4px 0;
		padding: 0 10px;
		border: 0;
		background: transparent;
		color: inherit;
		font: inherit;
		cursor: default;
		outline: none;
	}
	.lq-titlebar__menu:hover,
	.lq-titlebar__menu:focus-visible,
	.lq-titlebar__menu[aria-expanded="true"] {
		background: var(--ca-surface-raised, var(--ca-surface-base));
		color: var(--ca-text-primary);
	}
	.lq-titlebar__title {
		position: absolute;
		inset: 0 auto 0 50%;
		transform: translateX(-50%);
		display: flex;
		align-items: center;
		gap: 6px;
		max-width: 45%;
		min-width: 0;
		pointer-events: none;
	}
	.lq-titlebar__name {
		overflow: hidden;
		text-overflow: ellipsis;
		white-space: nowrap;
		font-weight: 600;
		color: var(--ca-text-primary);
	}
	.lq-titlebar--inactive .lq-titlebar__name {
		color: var(--ca-text-secondary);
	}
	.lq-titlebar__detail {
		overflow: hidden;
		text-overflow: ellipsis;
		white-space: nowrap;
		color: var(--ca-text-tertiary, var(--ca-text-secondary));
	}
	.lq-titlebar__drag {
		flex: 1;
	}
	.lq-titlebar__buttons {
		display: flex;
	}
	.lq-titlebar__button {
		width: 46px;
		display: grid;
		place-items: center;
		border: 0;
		background: transparent;
		color: var(--ca-text-secondary);
		outline: none;
	}
	.lq-titlebar__button:hover,
	.lq-titlebar__button:focus-visible {
		background: var(--ca-surface-raised, var(--ca-surface-base));
		color: var(--ca-text-primary);
	}
	.lq-titlebar__button--close:hover,
	.lq-titlebar__button--close:focus-visible {
		background: #c42b1c;
		color: #fff;
	}
	.lq-titlebar__button svg {
		width: 10px;
		height: 10px;
		fill: none;
		stroke: currentColor;
		stroke-width: 1;
		shape-rendering: crispEdges;
	}
	.lq-titlebar__item {
		display: flex;
		align-items: center;
		gap: 24px;
		width: 100%;
	}
	.lq-titlebar__key {
		margin-left: auto;
		font-size: 11px;
		color: var(--ca-text-tertiary, var(--ca-text-secondary));
	}
</style>
