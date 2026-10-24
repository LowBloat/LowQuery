import { expect, test } from "bun:test";
import { accelerator, appMenu, shortcutLabel } from "./app-menu";
import base from "../../src-tauri/tauri.conf.json";
import mac from "../../src-tauri/tauri.macos.conf.json";

const item = (label: string) => ({ label, action: () => {} });
const separator = { separator: true } as const;

test("menus drop omitted entries and stray separators", () => {
	const menu = appMenu("file", "File", [separator, item("New query"), false, separator, null, separator, item("Quit"), separator]);
	expect(menu.entries.map((entry) => ("label" in entry ? entry.label : "-"))).toEqual(["New query", "-", "Quit"]);
});

test("shortcuts follow the platform", () => {
	expect(shortcutLabel("Ctrl+Enter", false)).toBe("Ctrl+Enter");
	expect(shortcutLabel("Ctrl+Shift+Enter", true)).toBe("⌘⇧Enter");
	expect(accelerator("Ctrl+T")).toBe("CmdOrCtrl+T");
	expect(accelerator("F5")).toBe("F5");
});

// tauri.macos.conf.json replaces the whole windows array (JSON merge patch),
// so it must repeat every other window setting.
test("the macOS window differs only in its title bar", () => {
	const { decorations, titleBarStyle, hiddenTitle, ...macWindow } = mac.app.windows[0];
	const { decorations: baseDecorations, ...baseWindow } = base.app.windows[0];
	expect(macWindow).toEqual(baseWindow);
	expect(baseDecorations).toBe(false);
	expect(base.app.windows[0].visible).toBe(false);
	expect({ decorations, titleBarStyle, hiddenTitle }).toEqual({ decorations: true, titleBarStyle: "Overlay", hiddenTitle: true });
});
