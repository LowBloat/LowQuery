import { Channel, invoke } from "@tauri-apps/api/core";
import { getVersion } from "@tauri-apps/api/app";
import { relaunch } from "@tauri-apps/plugin-process";
import { openUrl } from "@tauri-apps/plugin-opener";
import { asAppError } from "../api";

export interface UpdatePolicy {
	/** installer, appimage, deb, rpm, pacman, portable or package. */
	channel: string;
	canInstall: boolean;
}

type InstallEvent = { event: "progress"; downloaded: number; total: number | null } | { event: "installing" };

const RELEASES_URL = "https://github.com/LowBloat/LowQuery/releases/latest";
const FIRST_CHECK_MS = 5_000;
const CHECK_EVERY_MS = 6 * 60 * 60 * 1000;

/** Why an installation cannot update itself, shown next to the download link. */
const MANUAL_HINT: Record<string, string> = {
	package: "Installed by the system package manager: update it there or download the new package.",
	appimage: "This AppImage is in a read-only folder: download the new AppImage to replace it.",
	portable: "This portable copy is in a read-only folder: download the new version and replace this one.",
};

/** Channels whose update runs the system package manager through pkexec. */
const SYSTEM_PACKAGE: Record<string, true> = { deb: true, rpm: true, pacman: true };

/**
 * Checks GitHub Releases for a signed update. Every copy that can replace
 * itself (installers, AppImage, .deb, .rpm, Arch package, portable) downloads,
 * verifies, installs and restarts; the rest get a download link.
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
	private timer: ReturnType<typeof setInterval> | undefined;

	get manualHint(): string | null {
		if (!this.policy || this.policy.canInstall) return null;
		return MANUAL_HINT[this.policy.channel] ?? MANUAL_HINT.package;
	}

	get passwordHint(): string | null {
		if (!this.policy?.canInstall || !SYSTEM_PACKAGE[this.policy.channel]) return null;
		return "The system asks for your password to install the new package.";
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
			this.available = await invoke<{ version: string; notes: string } | null>("check_update");
			if (this.available && interactive) this.dialogOpen = true;
		} catch (e) {
			if (interactive) this.error = asAppError(e).message;
		} finally {
			this.checking = false;
		}
	}

	async install(): Promise<void> {
		if (!this.available || !this.policy?.canInstall) return;
		this.installing = true;
		this.error = null;
		this.progress = null;
		const onEvent = new Channel<InstallEvent>();
		onEvent.onmessage = (e) => {
			if (e.event === "installing") this.progress = 1;
			else this.progress = e.total ? Math.min(1, e.downloaded / e.total) : null;
		};
		try {
			await invoke("install_update", { onEvent });
			await relaunch();
		} catch (e) {
			this.error = asAppError(e).message;
			this.installing = false;
		}
	}

	async download(): Promise<void> {
		await openUrl(RELEASES_URL);
	}
}

export const updates = new Updates();
