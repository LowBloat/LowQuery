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
const CHECK_EVERY_MS = 6 * 60 * 60 * 1000;

/** Why an installation cannot update itself, shown next to the download link. */
const MANUAL_HINT: Record<string, string> = {
	deb: "Installed from a .deb package: download the new package or update it with your package manager.",
	rpm: "Installed from an .rpm package: download the new package or update it with your package manager.",
	package: "Installed by the system package manager: update it there or download the new package.",
	appimage: "This AppImage is in a read-only folder: download the new AppImage to replace it.",
	portable: "Portable copy: download the new version and replace this one, or use the installer for automatic updates.",
};

/**
 * Checks GitHub Releases for a signed update. Installed copies download,
 * verify, install and restart; other formats get a download link.
 */
class Updates {
	currentVersion = $state("");
	policy = $state<UpdatePolicy | null>(null);
	available = $state<{ version: string; notes: string } | null>(null);
	checking = $state(false);
	installing = $state(false);
	/** Download progress from 0 to 1, or null when the size is unknown. */
	progress = $state<number | null>(null);
	error = $state<string | null>(null);
	dialogOpen = $state(false);
	private update: Update | null = null;
	private timer: ReturnType<typeof setInterval> | undefined;

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
	}

	/** `interactive`: the user asked, so report "up to date" and errors. */
	async check(interactive: boolean): Promise<void> {
		if (this.checking || this.installing) return;
		this.checking = true;
		this.error = null;
		try {
			this.policy ??= await invoke<UpdatePolicy>("update_policy");
			// Packages that cannot install still read the AppImage entry to learn
			// the version and notes.
			const target = this.policy.target ?? (this.policy.canInstall ? undefined : "linux-x86_64-appimage");
			const update = await check(target ? { target } : undefined);
			this.update = update;
			this.available = update ? { version: update.version, notes: update.body ?? "" } : null;
			if (update && interactive) this.dialogOpen = true;
		} catch (e) {
			if (interactive) this.error = e instanceof Error ? e.message : String(e);
		} finally {
			this.checking = false;
		}
	}

	async install(): Promise<void> {
		if (!this.update || !this.policy?.canInstall) return;
		this.installing = true;
		this.error = null;
		this.progress = null;
		let total = 0;
		let received = 0;
		try {
			await this.update.downloadAndInstall((event) => {
				if (event.event === "Started") total = event.data.contentLength ?? 0;
				else if (event.event === "Progress") {
					received += event.data.chunkLength;
					this.progress = total > 0 ? Math.min(1, received / total) : null;
				} else if (event.event === "Finished") this.progress = 1;
			});
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
