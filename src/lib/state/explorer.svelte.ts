import { toast } from "@codeatlasdev/ui";
import { api, asAppError, errorText, type ConnectionProfile, type DbColumn } from "../api";
import { workspace } from "./workspace.svelte";

export interface ObjectNode {
	name: string;
	kind: string;
	expanded: boolean;
	loading: boolean;
	columns: DbColumn[] | null;
}

export interface SchemaNode {
	name: string;
	expanded: boolean;
	loading: boolean;
	objects: ObjectNode[] | null;
}

export interface ProfileTree {
	sessionId: string | null;
	currentSchema: string | null;
	expanded: boolean;
	loading: boolean;
	error: string | null;
	schemas: SchemaNode[] | null;
}

/** Objects with columns to expand. */
export const RELATION_KINDS = new Set(["table", "view", "matview", "foreign"]);

/**
 * Schema browser. Each expanded connection uses its own session, separate
 * from the tabs, so it does not compete with running queries.
 */
class Explorer {
	trees = $state<Record<string, ProfileTree>>({});

	tree(profileId: string): ProfileTree | undefined {
		return this.trees[profileId];
	}

	async toggleProfile(profile: ConnectionProfile) {
		const t = this.trees[profile.id];
		if (t?.expanded) {
			t.expanded = false;
			return;
		}
		if (t?.schemas) {
			t.expanded = true;
			return;
		}
		await this.load(profile);
	}

	async load(profile: ConnectionProfile) {
		const tree: ProfileTree = this.trees[profile.id] ?? {
			sessionId: null,
			currentSchema: null,
			expanded: true,
			loading: false,
			error: null,
			schemas: null,
		};
		this.trees[profile.id] = tree;
		const t = this.trees[profile.id];
		t.expanded = true;
		t.loading = true;
		t.error = null;
		try {
			if (!t.sessionId) {
				const session = await workspace.openSession(profile);
				if (!session) {
					t.expanded = false;
					return;
				}
				t.sessionId = session.sessionId;
				t.currentSchema = session.currentSchema;
			}
			const names = await api.listSchemas(t.sessionId);
			t.schemas = names.map((name) => ({ name, expanded: false, loading: false, objects: null }));
			const current = t.schemas.find((s) => s.name === t.currentSchema);
			if (current) await this.toggleSchema(profile.id, current);
		} catch (e) {
			const err = asAppError(e);
			t.error = errorText(err);
			if (err.kind === "connection" || err.kind === "not_found") t.sessionId = null;
		} finally {
			t.loading = false;
		}
	}

	async refresh(profile: ConnectionProfile) {
		const t = this.trees[profile.id];
		if (t) t.schemas = null;
		await this.load(profile);
	}

	async close(profileId: string) {
		const t = this.trees[profileId];
		if (t?.sessionId) await api.closeSession(t.sessionId).catch(() => {});
		delete this.trees[profileId];
	}

	async toggleSchema(profileId: string, schema: SchemaNode) {
		const t = this.trees[profileId];
		if (!t?.sessionId) return;
		if (schema.expanded) {
			schema.expanded = false;
			return;
		}
		schema.expanded = true;
		if (schema.objects) return;
		schema.loading = true;
		try {
			const list = await api.listObjects(t.sessionId, schema.name);
			schema.objects = list.map((o) => ({ ...o, expanded: false, loading: false, columns: null }));
		} catch (e) {
			toast.error(errorText(asAppError(e)));
			schema.expanded = false;
		} finally {
			schema.loading = false;
		}
	}

	async toggleObject(profileId: string, schema: SchemaNode, obj: ObjectNode) {
		const t = this.trees[profileId];
		if (!t?.sessionId || !RELATION_KINDS.has(obj.kind)) return;
		if (obj.expanded) {
			obj.expanded = false;
			return;
		}
		obj.expanded = true;
		if (obj.columns) return;
		obj.loading = true;
		try {
			obj.columns = await api.listColumns(t.sessionId, schema.name, obj.name);
		} catch (e) {
			toast.error(errorText(asAppError(e)));
			obj.expanded = false;
		} finally {
			obj.loading = false;
		}
	}

	/**
	 * Schema for editor autocompletion (SQLNamespace format from
	 * @codemirror/lang-sql): `{ schema: { table: [columns] } }`.
	 */
	completion(profileId: string | null): {
		schema: Record<string, Record<string, string[]>>;
		defaultSchema?: string;
	} {
		const t = profileId ? this.trees[profileId] : undefined;
		const schema: Record<string, Record<string, string[]>> = {};
		for (const s of t?.schemas ?? []) {
			if (!s.objects) continue;
			const tables: Record<string, string[]> = {};
			for (const o of s.objects) {
				if (RELATION_KINDS.has(o.kind)) tables[o.name] = o.columns?.map((c) => c.name) ?? [];
			}
			schema[s.name] = tables;
		}
		return { schema, defaultSchema: t?.currentSchema ?? undefined };
	}
}

export const explorer = new Explorer();
