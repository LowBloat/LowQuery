/** Application menus (File, Query, View, Help). Windows and Linux show them in
 * the title bar; macOS puts them in the global menu bar. */

export const IS_MAC = typeof navigator !== "undefined" && /Mac/.test(navigator.platform);

export interface MenuItem {
	label: string;
	/** Written for Windows ("Ctrl+T") and shown in the platform's notation. The
	 * shortcut itself lives elsewhere. */
	shortcut?: string;
	disabled?: boolean;
	/** Shown as a check box when defined. */
	checked?: boolean;
	action: () => unknown;
}

export type MenuEntry = MenuItem | { separator: true };

export interface AppMenu {
	id: string;
	label: string;
	entries: MenuEntry[];
}

export function isSeparator(entry: MenuEntry): entry is { separator: true } {
	return "separator" in entry;
}

/** Drops omitted entries (`false`, `null`) and separators at the edges or next to each other. */
export function appMenu(id: string, label: string, entries: Array<MenuEntry | false | null | undefined>): AppMenu {
	const out: MenuEntry[] = [];
	for (const entry of entries) {
		if (!entry) continue;
		if (isSeparator(entry) && (!out.length || isSeparator(out[out.length - 1]))) continue;
		out.push(entry);
	}
	while (out.length && isSeparator(out[out.length - 1])) out.pop();
	return { id, label, entries: out };
}

/** "Ctrl+Shift+Enter" reads "⌘⇧Enter" on macOS. */
export function shortcutLabel(keys: string, mac = IS_MAC): string {
	return mac ? keys.replace("Ctrl+", "⌘").replace("Shift+", "⇧").replace("Alt+", "⌥") : keys;
}

/** Native accelerator for a shortcut hint: Ctrl is Command on macOS. */
export function accelerator(shortcut: string | undefined): string | undefined {
	return shortcut?.replace(/^Ctrl\+/, "CmdOrCtrl+");
}

/** Replaces the macOS menu bar: an application menu (About, Hide, Quit), the
 * standard Edit menu so Command+C and friends keep working in text fields,
 * `menus`, and the standard Window menu. */
export async function setNativeAppMenu(menus: AppMenu[], about: MenuItem): Promise<void> {
	const { Menu, Submenu, MenuItem, CheckMenuItem, PredefinedMenuItem } = await import("@tauri-apps/api/menu");
	const predefined = (...items: Parameters<typeof PredefinedMenuItem.new>[0][]) => Promise.all(items.map((item) => PredefinedMenuItem.new(item)));
	const item = (entry: MenuEntry) => {
		if (isSeparator(entry)) return PredefinedMenuItem.new({ item: "Separator" });
		const options = { text: entry.label, enabled: !entry.disabled, accelerator: accelerator(entry.shortcut), action: () => void entry.action() };
		return entry.checked === undefined ? MenuItem.new(options) : CheckMenuItem.new({ ...options, checked: entry.checked });
	};
	const menu = await Menu.new({
		items: [
			await Submenu.new({
				text: "LowQuery",
				items: [
					await MenuItem.new({ text: about.label, action: () => void about.action() }),
					...(await predefined({ item: "Separator" }, { item: "Services" }, { item: "Separator" }, { item: "Hide" }, { item: "HideOthers" }, { item: "ShowAll" }, { item: "Separator" }, { item: "Quit" })),
				],
			}),
			await Submenu.new({
				text: "Edit",
				items: await predefined({ item: "Undo" }, { item: "Redo" }, { item: "Separator" }, { item: "Cut" }, { item: "Copy" }, { item: "Paste" }, { item: "SelectAll" }),
			}),
			...(await Promise.all(menus.map(async (m) => Submenu.new({ text: m.label, items: await Promise.all(m.entries.map(item)) })))),
			await Submenu.new({
				text: "Window",
				items: await predefined({ item: "Minimize" }, { item: "Maximize" }, { item: "Fullscreen" }, { item: "Separator" }, { item: "BringAllToFront" }),
			}),
		],
	});
	const previous = await menu.setAsAppMenu();
	await previous?.close();
}
