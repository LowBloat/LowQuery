import { api, asAppError, errorText, type TnsEntry, type TnsSource } from "../api";

/** Short description of an alias target: `host:port/service`. */
export function describeEntry(e: TnsEntry): string {
	const target = e.serviceName ?? (e.sid ? `SID ${e.sid}` : "");
	if (e.host) {
		const port = e.port ? `:${e.port}` : "";
		const sep = target ? (e.serviceName ? "/" : " · ") : "";
		return `${e.host}${port}${sep}${target}${e.protocol === "TCPS" ? " · TCPS" : ""}`;
	}
	return e.descriptor.length > 60 ? `${e.descriptor.slice(0, 60)}…` : e.descriptor;
}

/** Folder key for comparisons (Windows paths are case-insensitive). */
const key = (dir: string) => dir.replace(/[\\/]+$/, "").toLowerCase();

/**
 * tnsnames.ora files found on this machine (TNS_ADMIN, ORACLE_HOME,
 * registered Oracle homes, clients on PATH) plus folders the user picked.
 */
class TnsStore {
	sources = $state<TnsSource[]>([]);
	loaded = $state(false);
	loading = $state(false);
	error = $state<string | null>(null);
	/** Import dialog visibility. */
	importOpen = $state(false);

	async load(force = false) {
		if ((this.loaded && !force) || this.loading) return;
		this.loading = true;
		this.error = null;
		try {
			const found = await api.oracleTnsSources();
			const picked = this.sources.filter((s) => s.origin === "Selected folder");
			this.sources = [...found, ...picked.filter((p) => !found.some((f) => key(f.dir) === key(p.dir)))];
			this.loaded = true;
		} catch (e) {
			this.error = errorText(asAppError(e));
		} finally {
			this.loading = false;
		}
	}

	/** Reads a folder picked by the user and keeps it in the list. */
	async addFolder(path: string): Promise<TnsSource> {
		const source = await api.oracleTnsFolder(path);
		const existing = this.sources.find((s) => key(s.dir) === key(source.dir));
		if (existing) return existing;
		this.sources.push(source);
		return source;
	}

	source(dir: string | null): TnsSource | undefined {
		return dir ? this.sources.find((s) => key(s.dir) === key(dir)) : undefined;
	}

	/**
	 * Aliases visible for a profile: the chosen folder only, or every
	 * discovered file in search order (the first definition wins), as the
	 * connection does at runtime.
	 */
	entries(dir: string | null): TnsEntry[] {
		if (dir) return this.source(dir)?.entries ?? [];
		const seen = new Set<string>();
		const out: TnsEntry[] = [];
		for (const s of this.sources) {
			if (s.origin === "Selected folder") continue;
			for (const e of s.entries) {
				const k = e.alias.toLowerCase();
				if (!seen.has(k)) {
					seen.add(k);
					out.push(e);
				}
			}
		}
		return out;
	}

	/** Label of what "Automatic" resolves to right now. */
	get automaticLabel(): string {
		const first = this.sources.find((s) => s.origin !== "Selected folder");
		return first ? `Automatic · ${first.label} (${first.origin})` : "Automatic · no tnsnames.ora found";
	}
}

export const tns = new TnsStore();
