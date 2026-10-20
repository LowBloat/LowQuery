import { toast } from "@codeatlasdev/ui";
import { api, asAppError, errorText, type AiActivity, type AiSetup } from "../api";

export type AiTab = "setup" | "activity";

/** "AI agents" dialog: MCP client setup and the activity log. */
class AiStore {
	open = $state(false);
	tab = $state<AiTab>("setup");
	setup = $state<AiSetup | null>(null);
	activity = $state<AiActivity[]>([]);
	loading = $state(false);
	connecting = $state<string | null>(null);
	private timer: ReturnType<typeof setInterval> | null = null;

	async show(tab: AiTab = "setup") {
		this.tab = tab;
		this.open = true;
		await this.refresh();
		this.timer ??= setInterval(() => {
			if (this.open && this.tab === "activity") void this.loadActivity();
		}, 3000);
	}

	close() {
		this.open = false;
		if (this.timer) clearInterval(this.timer);
		this.timer = null;
	}

	async refresh() {
		this.loading = true;
		try {
			const [setup, activity] = await Promise.all([api.aiSetup(), api.aiActivity()]);
			this.setup = setup;
			this.activity = activity;
		} catch (e) {
			toast.error(errorText(asAppError(e)));
		} finally {
			this.loading = false;
		}
	}

	async loadActivity() {
		try {
			this.activity = await api.aiActivity();
		} catch {
			// Kept as it was; the next poll tries again.
		}
	}

	async connect(id: string, name: string) {
		this.connecting = id;
		try {
			const path = await api.aiConnectClient(id);
			toast.success(`Added LowQuery to ${name} (${path}). Restart ${name} to load it.`);
			this.setup = await api.aiSetup();
		} catch (e) {
			toast.error(errorText(asAppError(e)));
		} finally {
			this.connecting = null;
		}
	}

	async clearActivity() {
		try {
			await api.aiActivityClear();
			this.activity = [];
		} catch (e) {
			toast.error(errorText(asAppError(e)));
		}
	}
}

export const ai = new AiStore();
