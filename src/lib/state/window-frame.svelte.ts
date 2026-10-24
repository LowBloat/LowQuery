import { invoke } from "@tauri-apps/api/core";
import { getCurrentWindow } from "@tauri-apps/api/window";
import type { UnlistenFn } from "@tauri-apps/api/event";

// Outside the desktop app (the browser dev mock) there is no window to drive.
function current() {
	try {
		return getCurrentWindow();
	} catch {
		return null;
	}
}

/** The page background as `#rrggbb`, whatever color syntax the theme uses. */
function backgroundHex(): string {
	const canvas = document.createElement("canvas");
	canvas.width = canvas.height = 1;
	const context = canvas.getContext("2d");
	if (!context) return "";
	context.fillStyle = getComputedStyle(document.body).backgroundColor;
	context.fillRect(0, 0, 1, 1);
	const [r, g, b] = context.getImageData(0, 0, 1, 1).data;
	return `#${[r, g, b].map((v) => v.toString(16).padStart(2, "0")).join("")}`;
}

/** The window frame drawn by the title bar: maximized and focus state, and the
 * actions behind its buttons. */
class WindowFrame {
	maximized = $state(false);
	focused = $state(true);
	private unlisteners: UnlistenFn[] = [];

	/** The window starts hidden so the webview's white background never shows;
	 * this gives it the page background and shows it the first time. Theme
	 * changes call it again. */
	painted() {
		const background = backgroundHex();
		if (background && "__TAURI_INTERNALS__" in window) void invoke("window_painted", { background }).catch(() => {});
	}

	async start() {
		const win = current();
		if (!win) return;
		try {
			this.unlisteners.push(
				await win.onResized(() => void this.refresh()),
				await win.onFocusChanged(({ payload }) => (this.focused = payload)),
			);
		} catch {
			// Without events the buttons still work; only the icons may lag.
		}
		await this.refresh();
	}

	stop() {
		this.unlisteners.splice(0).forEach((unlisten) => unlisten());
	}

	private async refresh() {
		try {
			this.maximized = (await current()?.isMaximized()) ?? false;
		} catch {
			// Keep the last known state.
		}
	}

	minimize = () => void current()?.minimize().catch(() => {});
	toggleMaximize = () => void current()?.toggleMaximize().then(() => this.refresh(), () => {});
	close = () => void current()?.close().catch(() => {});
}

export const windowFrame = new WindowFrame();
