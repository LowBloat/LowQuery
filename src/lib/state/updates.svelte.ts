import { invoke } from "@tauri-apps/api/core";
import { getVersion } from "@tauri-apps/api/app";
import { check, type Update } from "@tauri-apps/plugin-updater";
import { relaunch } from "@tauri-apps/plugin-process";
import { openUrl } from "@tauri-apps/plugin-opener";

export interface UpdatePolicy {
	/** installer, appimage, deb, rpm, package or portable. */
	channel: string;
	canInstall: boolean;
	target: string | null;
}

const RELEASES_URL = "https://github.com/LowBloat/LowQuery/releases/latest";
const FIRST_CHECK_MS = 5_000;
/** The manifest is a small file on GitHub; checking every minute means a new
 * release reaches open windows within a minute of being published. */
const CHECK_EVERY_MS = 60 * 1000;
/** Focus comes and goes quickly; at most one extra check per this long. */
const FOCUS_GAP_MS = 15 * 1000;
/** A check that hangs (a network that changed under it, a sleeping laptop)
 * would otherwise keep every later check waiting. */
const CHECK_TIMEOUT_MS = 30 * 1000;
/** The same for a stalled background download; installing from the dialog
 * downloads again without a limit. */
const DOWNLOAD_TIMEOUT_MS = 15 * 60 * 1000;

/** Each check holds the update (and its download) on the Rust side until
 * closed; checks run every minute, so the ones not kept are released. */
function discard(update: Update | null) {
	update?.close().catch(() => {});
}

/** Why an installation cannot update itself, shown next to the download link. */
const MANUAL_HINT: Record<string, string> = {
	package: "Installed by the system package manager: update it there or download the new package.",
	appimage: "This AppImage is in a read-only folder: download the new AppImage to replace it.",
	portable: "Portable copy: download the new version and replace this one, or use the installer for automatic updates.",
};

/**
 * Automatic updates from GitHub Releases. The app checks shortly after it
 * opens, every minute and whenever the window comes back. A copy that
 * can update itself (Windows installer, macOS app, writable AppImage, .deb or
 * .rpm) downloads the signed update in the background; once it is verified and
 * ready, the sidebar offers "restart to update", which installs it right away.
 * Other formats (Arch package, portable copy) get a download link.
 */
class Updates {
	currentVersion = $state("");
	policy = $state<UpdatePolicy | null>(null);
	available = $state<{ version: string; notes: string } | null>(null);
	checking = $state(false);
	/** Downloading in the background, before anyone asked. */
	downloading = $state(false);
	/** Downloaded and verified: a restart installs it. */
	ready = $state(false);
	installing = $state(false);
	/** Download progress from 0 to 1, or null when the size is unknown. */
	progress = $state<number | null>(null);
	error = $state<string | null>(null);
	dialogOpen = $state(false);
	private update: Update | null = null;
	private timer: ReturnType<typeof setInterval> | undefined;
	private lastCheck = 0;
	private prefetching: Promise<void> | null = null;

	/** The sidebar offers the update once it is ready (or when it is manual). */
	get offered(): boolean {
		return this.available !== null && !this.downloading;
	}

	/** "Restart to update" once downloaded, "Update to" otherwise. */
	get label(): string {
		if (!this.available) return "";
		return this.ready ? `Restart to update to ${this.available.version}` : `Update to ${this.available.version}`;
	}

	get manualHint(): string | null {
		if (!this.policy || this.policy.canInstall) return null;
		return MANUAL_HINT[this.policy.channel] ?? MANUAL_HINT.portable;
	}

	/** Starts background checks; safe to call once at startup. */
	start() {
		if (!("__TAURI_INTERNALS__" in window) || this.timer) return;
		getVersion().then((v) => (this.currentVersion = v)).catch(() => {});
		setTimeout(() => this.check(false), FIRST_CHECK_MS);
		this.timer = setInterval(() => this.check(false), CHECK_EVERY_MS);
		const checkSoon = () => {
			if (Date.now() - this.lastCheck > FOCUS_GAP_MS) void this.check(false);
		};
		window.addEventListener("focus", checkSoon);
		// The window can come back from the tray or another desktop without focus.
		document.addEventListener("visibilitychange", () => {
			if (document.visibilityState === "visible") checkSoon();
		});
	}

	/** `interactive`: the user asked, so report "up to date" and errors. */
	async check(interactive: boolean): Promise<void> {
		if (this.checking || this.downloading || this.installing) {
			// Still downloading what was found: show it rather than nothing.
			if (interactive && this.available) this.dialogOpen = true;
			return;
		}
		this.checking = true;
		this.lastCheck = Date.now();
		if (interactive) this.error = null;
		try {
			this.policy ??= await invoke<UpdatePolicy>("update_policy");
			// Copies that cannot install still read the AppImage entry to learn
			// the version and notes.
			const target = this.policy.target ?? (this.policy.canInstall ? undefined : "linux-x86_64-appimage");
			const update = await check({ timeout: CHECK_TIMEOUT_MS, ...(target ? { target } : {}) });
			if (!update) {
				discard(this.update);
				this.update = null;
				this.available = null;
				this.ready = false;
				return;
			}
			// Already found this one: nothing to do until the restart. A background
			// download that failed is not repeated every minute; installing from
			// the dialog downloads it again.
			if (this.update && this.available?.version === update.version) {
				discard(update);
				if (interactive) this.dialogOpen = true;
				return;
			}
			discard(this.update);
			this.update = update;
			this.ready = false;
			this.available = { version: update.version, notes: update.body ?? "" };
			if (interactive) this.dialogOpen = true;
			if (this.policy.canInstall) this.prefetching = this.prefetch(update);
		} catch (e) {
			if (interactive) this.error = e instanceof Error ? e.message : String(e);
		} finally {
			this.checking = false;
		}
	}

	/** Downloads and verifies in the background; the sidebar changes once it is ready. */
	private async prefetch(update: Update): Promise<void> {
		this.downloading = true;
		this.progress = null;
		let total = 0;
		let received = 0;
		try {
			await update.download(
				(event) => {
					if (event.event === "Started") total = event.data.contentLength ?? 0;
					else if (event.event === "Progress") {
						received += event.data.chunkLength;
						this.progress = total > 0 ? Math.min(1, received / total) : null;
					}
				},
				{ timeout: DOWNLOAD_TIMEOUT_MS },
			);
			if (this.update === update) this.ready = true;
		} catch {
			// The dialog falls back to download-and-install in one go.
		} finally {
			this.downloading = false;
			this.prefetching = null;
			if (!this.installing) this.progress = null;
		}
	}

	async install(): Promise<void> {
		const update = this.update;
		if (!update || !this.policy?.canInstall || this.installing) return;
		this.installing = true;
		this.error = null;
		try {
			// Clicked while the background download runs: let it finish first.
			if (this.prefetching) await this.prefetching;
			if (this.ready) {
				this.progress = 1;
				await update.install();
			} else {
				this.progress = null;
				let total = 0;
				let received = 0;
				await update.downloadAndInstall((event) => {
					if (event.event === "Started") total = event.data.contentLength ?? 0;
					else if (event.event === "Progress") {
						received += event.data.chunkLength;
						this.progress = total > 0 ? Math.min(1, received / total) : null;
					} else if (event.event === "Finished") this.progress = 1;
				});
			}
			await relaunch();
		} catch (e) {
			this.error = e instanceof Error ? e.message : String(e);
			this.installing = false;
		}
	}

	async download(): Promise<void> {
		await openUrl(RELEASES_URL);
	}
}

export const updates = new Updates();
