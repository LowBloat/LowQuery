<script lang="ts">
	import {
		IconButton,
		Tooltip,
		AlertDialog,
		ThemeToggle,
		Spinner,
		EmptyState,
		Button,
		toast,
	} from "@codeatlasdev/ui";
	import {
		ChevronRight,
		Plus,
		Bot,
		EllipsisVertical,
		RefreshCw,
		Database,
		FolderTree,
		Table2,
		Eye,
		Layers,
		Hash,
		FunctionSquare,
		Cog,
		Package,
		Shapes,
		Braces,
		List,
		ListOrdered,
		Boxes,
		Waves,
		Link2,
		Columns3,
		KeyRound,
		ArrowDownWideNarrow,
		Pin,
		Sigma,
		ScanSearch,
		Play,
		FileInput,
		ArrowUpCircle,
	} from "@lucide/svelte";
	import { updates } from "../state/updates.svelte";
	import { importer } from "../state/importer.svelte";
	import { ai } from "../state/ai.svelte";
	import EngineLogo from "./EngineLogo.svelte";
	import TreeMenu from "./TreeMenu.svelte";
	import { actions as act } from "../state/actions.svelte";
	import { connectionSummary } from "../summary";
	import { ENGINE_LABEL, emptyProfile, type ConnectionProfile } from "../api";
	import { workspace } from "../state/workspace.svelte";
	import { explorer, RELATION_KINDS, REDIS_KINDS, type SchemaNode, type ObjectNode } from "../state/explorer.svelte";
	import { qualified, quoteIdent, redisArg } from "../sql/ident";

	interface Props {
		oninsert?: (text: string) => void;
		onnavigate?: () => void;
	}

	let { oninsert, onnavigate }: Props = $props();

	const KIND_ORDER = [
		"table", "view", "matview", "foreign", "sequence", "function", "procedure", "package", "type", "synonym", "aggregate", "index",
		"string", "hash", "list", "set", "zset", "stream", "json",
	];
	const KIND_LABEL: Record<string, string> = {
		table: "Tables",
		view: "Views",
		matview: "Materialized views",
		foreign: "Foreign tables",
		sequence: "Sequences",
		function: "Functions",
		procedure: "Procedures",
		package: "Packages",
		type: "Types",
		synonym: "Synonyms",
		aggregate: "Aggregates",
		index: "Indexes",
		string: "Strings",
		hash: "Hashes",
		list: "Lists",
		set: "Sets",
		zset: "Sorted sets",
		stream: "Streams",
		json: "JSON",
	};
	const KIND_ICON = {
		table: Table2,
		view: Eye,
		matview: Layers,
		foreign: Link2,
		sequence: Hash,
		function: FunctionSquare,
		procedure: Cog,
		package: Package,
		type: Shapes,
		synonym: Link2,
		aggregate: Sigma,
		index: ScanSearch,
		string: KeyRound,
		hash: Braces,
		list: List,
		set: Boxes,
		zset: ListOrdered,
		stream: Waves,
		json: Braces,
	} as const;

	/** Cassandra key roles: icon and text for the column list. */
	const KEY_ROLE = {
		partition: { icon: KeyRound, short: "PK", label: "Partition key" },
		clustering: { icon: ArrowDownWideNarrow, short: "CK", label: "Clustering column" },
		static: { icon: Pin, short: "S", label: "Static column" },
	} as const;

	function groups(objects: ObjectNode[]) {
		const map = new Map<string, ObjectNode[]>();
		for (const o of objects) {
			if (!map.has(o.kind)) map.set(o.kind, []);
			map.get(o.kind)!.push(o);
		}
		return [...map.entries()].sort((a, b) => KIND_ORDER.indexOf(a[0]) - KIND_ORDER.indexOf(b[0]));
	}

	function newQuery(p: ConnectionProfile) {
		workspace.newTab(p.id);
		onnavigate?.();
	}

	function preview(p: ConnectionProfile, schema: SchemaNode, obj: ObjectNode) {
		act.preview(p, schema, obj);
		onnavigate?.();
	}

	/** Objects with a ▶ preview: tables and views, or any Redis key. */
	const previewable = (kind: string) => RELATION_KINDS.has(kind) || REDIS_KINDS.has(kind);

	function insertName(p: ConnectionProfile, schema: SchemaNode, obj: ObjectNode) {
		const tree = explorer.tree(p.id);
		if (p.engine === "redis") {
			oninsert?.(redisArg(obj.name));
			return;
		}
		const name = schema.name === tree?.currentSchema ? quoteIdent(obj.name, p.engine) : qualified(schema.name, obj.name, p.engine);
		oninsert?.(name.replace(/\(.*\)$/, ""));
	}
</script>

<nav class="lq-side" aria-label="Connections">
	<header class="lq-side__brand">
		<svg class="lq-side__logo" viewBox="0 0 40 40" aria-hidden="true">
			<rect width="40" height="40" fill="#171a17" />
			<path d="M10 9h5v20h15v5H10z" fill="#f1f3ee" />
			<path d="M21 9h9v5h-9zM21 17h6v5h-6z" fill="#b8f238" />
		</svg>
		<span class="lq-side__name">LowQuery</span>
		<span class="ca-toolbar-spacer"></span>
		<ThemeToggle label="Toggle light/dark theme" />
	</header>

	<div class="lq-side__section">
		<h2 class="lq-side__title">Connections</h2>
		<span class="ca-toolbar-spacer"></span>
		<Tooltip content="Import connections from DBeaver, DataGrip, pgAdmin, SQL Developer or tnsnames.ora">
			{#snippet triggerChild(props)}<IconButton {...props} size="sm" variant="text" aria-label="Import connections" onclick={() => importer.show()}>
				<FileInput size={16} />
			</IconButton>{/snippet}
		</Tooltip>
		<Tooltip content="AI agents: MCP setup and activity">
			{#snippet triggerChild(props)}<IconButton {...props} size="sm" variant="text" aria-label="AI agents" onclick={() => ai.show()}>
				<Bot size={16} />
			</IconButton>{/snippet}
		</Tooltip>
		<Tooltip content="New connection">
			{#snippet triggerChild(props)}<IconButton {...props} size="sm" variant="text" aria-label="New connection" onclick={() => (workspace.editing = emptyProfile())}>
				<Plus size={16} />
			</IconButton>{/snippet}
		</Tooltip>
	</div>

	<div class="lq-side__scroll">
		{#if workspace.profiles.length === 0}
			<div class="lq-side__empty">
				<EmptyState
					icon={Database}
					title="No connections"
					description="Add a connection to get started."
				>
					{#snippet actions()}
						<Button size="sm" variant="filled" onclick={() => (workspace.editing = emptyProfile())}>
							{#snippet leading()}<Plus size={14} />{/snippet}
							New connection
						</Button>
						<Button size="sm" variant="outlined" onclick={() => importer.show()}>
							{#snippet leading()}<FileInput size={14} />{/snippet}
							Import
						</Button>
					{/snippet}
				</EmptyState>
			</div>
		{/if}

		<ul class="lq-tree" role="list">
			{#each workspace.profiles as p (p.id)}
				{@const tree = explorer.tree(p.id)}
				<li class="lq-tree__conn" style="--conn-color:{p.color ?? 'transparent'}">
					<div class="lq-tree__row lq-tree__row--conn" role="group" oncontextmenu={(e) => act.openMenu(e, { type: "connection", profile: p })}>
						<button
							type="button"
							class="lq-tree__toggle"
							aria-expanded={!!tree?.expanded}
							onclick={() => explorer.toggleProfile(p)}
							ondblclick={() => newQuery(p)}
						>
							<ChevronRight size={14} class="lq-tree__chev {tree?.expanded ? 'lq-tree__chev--open' : ''}" aria-hidden="true" />
							<span class="lq-tree__engine" title={ENGINE_LABEL[p.engine]}>
								<EngineLogo engine={p.engine} size={18} />
							</span>
							<span class="lq-tree__conn-text">
								<span class="lq-tree__label"
									>{p.name}{#if p.aiAccess && p.aiAccess !== "off"}<span
											class="lq-tree__ai"
											class:lq-tree__ai--write={p.aiAccess === "write"}
											title={p.aiAccess === "write" ? "AI agents: read and write" : "AI agents: read only"}
											><Bot size={12} aria-label="Open to AI agents" /></span
										>{/if}</span
								>
								<span class="lq-tree__sub">{connectionSummary(p)}</span>
							</span>
						</button>
						{#if tree?.loading}
							<Spinner size={14} label="Loading" />
						{/if}
						<IconButton size="sm" variant="text" aria-label="Actions for {p.name}" onclick={(e) => act.openMenu(e, { type: "connection", profile: p })}>
							<EllipsisVertical size={15} />
						</IconButton>
					</div>

					{#if tree?.expanded}
						{#if tree.error}
							<p class="lq-tree__error">{tree.error}</p>
						{/if}
						{#if tree.schemas}
							<ul class="lq-tree__children" role="list">
								{#each tree.schemas as s (s.name)}
									<li>
										<div class="lq-tree__row lq-tree__row--node" role="group" oncontextmenu={(e) => act.openMenu(e, { type: "schema", profile: p, schema: s })}>
											<button type="button" class="lq-tree__toggle" aria-expanded={s.expanded} onclick={() => explorer.toggleSchema(p.id, s)}>
												<ChevronRight size={14} class="lq-tree__chev {s.expanded ? 'lq-tree__chev--open' : ''}" aria-hidden="true" />
												<FolderTree size={14} aria-hidden="true" />
												<span class="lq-tree__label" class:lq-tree__label--current={s.name === tree.currentSchema}>{s.name}</span>
											</button>
											{#if s.loading}<Spinner size={12} label="Loading" />{/if}
											<button type="button" class="lq-tree__action" aria-label="Actions for {s.name}" title="Actions" onclick={(e) => act.openMenu(e, { type: "schema", profile: p, schema: s })}>
												<EllipsisVertical size={13} />
											</button>
										</div>
										{#if s.expanded && s.objects}
											{#if s.objects.length === 0}
												<p class="lq-tree__hint">No visible objects</p>
											{/if}
											{#if p.engine === "redis" && s.objects.length >= 1000}
												<p class="lq-tree__hint">First 1000 keys. Use SCAN with MATCH in the editor to find others.</p>
											{/if}
											<ul class="lq-tree__children" role="list">
												{#each groups(s.objects) as [kind, items] (kind)}
													<li class="lq-tree__group">{KIND_LABEL[kind] ?? kind} <span>{items.length}</span></li>
													{#each items as o (o.kind + o.name)}
														{@const Icon = KIND_ICON[o.kind as keyof typeof KIND_ICON] ?? Table2}
														<li>
															<div class="lq-tree__row lq-tree__row--node" role="group" oncontextmenu={(e) => act.openMenu(e, { type: "object", profile: p, schema: s, obj: o })}>
																<button
																	type="button"
																	class="lq-tree__toggle"
																	aria-expanded={RELATION_KINDS.has(o.kind) ? o.expanded : undefined}
																	onclick={() => explorer.toggleObject(p.id, s, o)}
																	ondblclick={() => insertName(p, s, o)}
																	title="Double-click to insert the name into the editor"
																>
																	{#if RELATION_KINDS.has(o.kind)}
																		<ChevronRight size={14} class="lq-tree__chev {o.expanded ? 'lq-tree__chev--open' : ''}" aria-hidden="true" />
																	{:else}
																		<span class="lq-tree__chev-space"></span>
																	{/if}
																	<Icon size={14} aria-hidden="true" />
																	<span class="lq-tree__label">{o.name}</span>
																</button>
																{#if previewable(o.kind)}
																	<button type="button" class="lq-tree__action" aria-label="Query {o.name}" title="Query 200 rows" onclick={() => preview(p, s, o)}>
																		<Play size={13} />
																	</button>
																{/if}
																<button type="button" class="lq-tree__action" aria-label="Actions for {o.name}" title="Actions" onclick={(e) => act.openMenu(e, { type: "object", profile: p, schema: s, obj: o })}>
																	<EllipsisVertical size={13} />
																</button>
															</div>
															{#if o.expanded && o.columns}
																<ul class="lq-tree__children lq-tree__cols" role="list">
																	{#each o.columns as c (c.name)}
																		<li class="lq-tree__col">
																			{#if c.keyRole && c.keyRole in KEY_ROLE}
																				{@const role = KEY_ROLE[c.keyRole as keyof typeof KEY_ROLE]}
																				<role.icon size={12} aria-label={role.label} />
																			{:else if c.primaryKey}<KeyRound size={12} aria-label="Primary key" />{:else}<Columns3 size={12} aria-hidden="true" />{/if}
																			<button type="button" class="lq-tree__col-name" ondblclick={() => oninsert?.(quoteIdent(c.name, p.engine))} title="Double-click to insert into the editor">
																				{c.name}
																			</button>
																			<span class="lq-tree__type">{c.typeName}{c.nullable || c.keyRole ? "" : " not null"}</span>
																			{#if c.keyRole && c.keyRole in KEY_ROLE}
																				{@const role = KEY_ROLE[c.keyRole as keyof typeof KEY_ROLE]}
																				<span class="lq-tree__role" title={role.label}>{role.short}</span>
																			{/if}
																		</li>
																	{/each}
																</ul>
															{/if}
														</li>
													{/each}
												{/each}
											</ul>
										{/if}
									</li>
								{/each}
							</ul>
						{/if}
					{/if}
				</li>
			{/each}
		</ul>
	</div>

	<footer class="lq-side__footer">
		{#if updates.available}
			<button type="button" class="lq-side__update" onclick={() => (updates.dialogOpen = true)}>
				<ArrowUpCircle size={15} aria-hidden="true" />
				Update to {updates.available.version}
			</button>
		{:else}
			<span class="lq-side__version">{updates.currentVersion ? `LowQuery ${updates.currentVersion}` : "LowQuery"}</span>
			<span class="ca-toolbar-spacer"></span>
			<Tooltip content={updates.error ?? "Check for updates"}>
				{#snippet triggerChild(props)}<IconButton
						{...props}
						size="sm"
						variant="text"
						aria-label="Check for updates"
						loading={updates.checking}
						onclick={async () => {
							await updates.check(true);
							if (!updates.available && !updates.error) toast.success("LowQuery is up to date");
							if (updates.error) toast.error(`Could not check for updates: ${updates.error}`);
						}}
					>
						<RefreshCw size={14} />
					</IconButton>{/snippet}
			</Tooltip>
		{/if}
	</footer>
</nav>

<TreeMenu {oninsert} {onnavigate} />

{#if act.deleting}
	{@const confirmDelete = act.deleting}
	<AlertDialog
		open={true}
		title="Delete {confirmDelete.name}?"
		description="The profile and its saved password will be removed from this computer. The database is not changed."
		confirmLabel="Delete"
		cancelLabel="Cancel"
		destructive
		onconfirm={async () => {
			act.deleting = null;
			await explorer.close(confirmDelete.id);
			await workspace.deleteProfile(confirmDelete.id);
		}}
		oncancel={() => (act.deleting = null)}
	/>
{/if}

<style>
	.lq-tree__ai {
		display: inline-flex;
		margin-left: 6px;
		color: var(--ca-text-hint);
		vertical-align: -1px;
	}
	.lq-tree__ai--write {
		color: var(--ca-semantic-warning);
	}
	.lq-side {
		display: flex;
		flex-direction: column;
		height: 100%;
		min-height: 0;
		background: var(--ca-surface-sunken);
	}
	.lq-side__brand {
		display: flex;
		align-items: center;
		gap: var(--ca-space-2);
		height: 44px;
		padding: 0 var(--ca-space-2) 0 var(--ca-space-3);
		border-bottom: 1px solid var(--ca-outline-subtle);
	}
	.lq-side__logo {
		width: 22px;
		height: 22px;
		flex-shrink: 0;
	}
	.lq-side__name {
		font-size: 15px;
		font-weight: 700;
		letter-spacing: -0.02em;
	}
	.lq-side__footer {
		display: flex;
		align-items: center;
		gap: var(--ca-space-2);
		min-height: 36px;
		padding: 0 var(--ca-space-2) 0 var(--ca-space-3);
		border-top: 1px solid var(--ca-outline-subtle);
	}
	.lq-side__version {
		color: var(--ca-text-hint);
		font-family: var(--ca-font-mono);
		font-size: 11px;
	}
	.lq-side__update {
		display: flex;
		flex: 1;
		align-items: center;
		gap: var(--ca-space-2);
		margin: var(--ca-space-1) 0;
		padding: 6px var(--ca-space-2);
		border: 0;
		background: var(--ca-brand-fill);
		color: var(--ca-brand-contrast);
		font-size: 13px;
		font-weight: 650;
		cursor: pointer;
	}
	.lq-side__update:hover {
		background: var(--ca-brand-fill-hover);
	}
	.lq-side__update:focus-visible {
		outline: 2px solid var(--ca-focus-ring);
		outline-offset: 2px;
	}
	.lq-side__section {
		display: flex;
		align-items: center;
		justify-content: space-between;
		padding: var(--ca-space-2) var(--ca-space-2) var(--ca-space-1) var(--ca-space-3);
	}
	.lq-side__title {
		margin: 0;
		color: var(--ca-text-secondary);
		font-family: var(--ca-font-mono);
		font-size: 11px;
		font-weight: 600;
		letter-spacing: 0.06em;
		text-transform: uppercase;
	}
	.lq-side__scroll {
		flex: 1;
		min-height: 0;
		overflow: auto;
		padding-bottom: var(--ca-space-4);
	}
	.lq-side__empty {
		padding: var(--ca-space-4) var(--ca-space-2);
	}
	.lq-tree,
	.lq-tree__children {
		margin: 0;
		padding: 0;
		list-style: none;
	}
	.lq-tree__children {
		padding-left: 14px;
	}
	.lq-tree__conn {
		border-left: 3px solid var(--conn-color);
	}
	.lq-tree__row {
		display: flex;
		align-items: center;
		gap: var(--ca-space-1);
		min-height: 26px;
		padding-right: var(--ca-space-1);
	}
	.lq-tree__row:hover {
		background: var(--ca-action-hover);
	}
	.lq-tree__row--conn {
		min-height: 40px;
	}
	.lq-tree__toggle {
		display: flex;
		flex: 1;
		align-items: center;
		gap: 6px;
		min-width: 0;
		padding: 3px 4px 3px 6px;
		border: 0;
		background: transparent;
		color: var(--ca-text-primary);
		font-size: 13px;
		text-align: left;
		cursor: pointer;
	}
	.lq-tree__toggle:focus-visible,
	.lq-tree__action:focus-visible,
	.lq-tree__col-name:focus-visible {
		outline: 2px solid var(--ca-focus-ring);
		outline-offset: -2px;
	}
	.lq-tree__toggle :global(.lq-tree__chev) {
		flex-shrink: 0;
		color: var(--ca-text-hint);
		transition: transform var(--ca-duration-fast) var(--ca-ease-standard);
	}
	.lq-tree__toggle :global(.lq-tree__chev--open) {
		transform: rotate(90deg);
	}
	.lq-tree__toggle :global(svg) {
		flex-shrink: 0;
	}
	.lq-tree__chev-space {
		width: 14px;
		flex-shrink: 0;
	}
	.lq-tree__engine {
		display: grid;
		flex-shrink: 0;
		place-items: center;
		width: 28px;
		height: 28px;
		border: 1px solid var(--ca-outline-subtle);
		background: var(--ca-surface-raised);
	}
	.lq-tree__conn-text {
		display: flex;
		flex-direction: column;
		min-width: 0;
	}
	.lq-tree__label {
		overflow: hidden;
		white-space: nowrap;
		text-overflow: ellipsis;
	}
	.lq-tree__row--conn .lq-tree__label {
		font-weight: 600;
	}
	.lq-tree__label--current {
		font-weight: 650;
	}
	.lq-tree__label--current::after {
		content: " ·";
		color: var(--ca-brand-main);
	}
	.lq-tree__sub {
		overflow: hidden;
		color: var(--ca-text-hint);
		font-family: var(--ca-font-mono);
		font-size: 11px;
		white-space: nowrap;
		text-overflow: ellipsis;
	}
	.lq-tree__action {
		display: grid;
		place-items: center;
		width: 24px;
		height: 22px;
		padding: 0;
		border: 0;
		background: transparent;
		color: var(--ca-text-secondary);
		cursor: pointer;
		opacity: 0;
	}
	.lq-tree__row--node:hover .lq-tree__action,
	.lq-tree__action:focus-visible {
		opacity: 1;
	}
	.lq-tree__action:hover {
		background: var(--ca-brand-fill);
		color: var(--ca-brand-contrast);
	}
	.lq-tree__group {
		padding: 6px 0 2px 26px;
		color: var(--ca-text-hint);
		font-family: var(--ca-font-mono);
		font-size: 10px;
		letter-spacing: 0.05em;
		text-transform: uppercase;
	}
	.lq-tree__group span {
		margin-left: 4px;
	}
	.lq-tree__cols {
		padding-left: 34px;
	}
	.lq-tree__col {
		display: flex;
		align-items: center;
		gap: 6px;
		min-height: 22px;
		color: var(--ca-text-hint);
		font-size: 12px;
	}
	.lq-tree__col-name {
		padding: 0;
		border: 0;
		background: transparent;
		color: var(--ca-text-primary);
		font: inherit;
		cursor: default;
	}
	.lq-tree__type {
		overflow: hidden;
		font-family: var(--ca-font-mono);
		font-size: 11px;
		white-space: nowrap;
		text-overflow: ellipsis;
	}
	.lq-tree__role {
		flex-shrink: 0;
		padding: 0 4px;
		border: 1px solid var(--ca-outline-subtle);
		color: var(--ca-text-secondary);
		font-family: var(--ca-font-mono);
		font-size: 10px;
		font-weight: 600;
		letter-spacing: 0.04em;
	}
	.lq-tree__error,
	.lq-tree__hint {
		margin: 4px var(--ca-space-3) 6px 26px;
		font-size: 12px;
	}
	.lq-tree__error {
		color: var(--ca-semantic-error);
	}
	.lq-tree__hint {
		color: var(--ca-text-hint);
	}
	@media (prefers-reduced-motion: reduce) {
		.lq-tree__toggle :global(.lq-tree__chev) {
			transition: none;
		}
	}
</style>
