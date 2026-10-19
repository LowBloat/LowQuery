<script lang="ts">
	import {
		IconButton,
		Tooltip,
		DropdownMenu,
		DropdownMenuItem,
		DropdownMenuSeparator,
		AlertDialog,
		ThemeToggle,
		Spinner,
		EmptyState,
		Button,
	} from "@codeatlasdev/ui";
	import {
		ChevronRight,
		Plus,
		EllipsisVertical,
		SquarePen,
		Copy,
		Trash2,
		RefreshCw,
		FilePlus2,
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
		Link2,
		Columns3,
		KeyRound,
		Play,
	} from "@lucide/svelte";
	import { ENGINE_LABEL, emptyProfile, type ConnectionProfile } from "../api";
	import { workspace } from "../state/workspace.svelte";
	import { explorer, RELATION_KINDS, type SchemaNode, type ObjectNode } from "../state/explorer.svelte";
	import { previewQuery, qualified, quoteIdent } from "../sql/ident";

	interface Props {
		oninsert?: (text: string) => void;
		onnavigate?: () => void;
	}

	let { oninsert, onnavigate }: Props = $props();

	let confirmDelete = $state<ConnectionProfile | null>(null);

	const KIND_ORDER = ["table", "view", "matview", "foreign", "sequence", "function", "procedure", "package", "type", "synonym"];
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

	function duplicate(p: ConnectionProfile) {
		workspace.editing = { ...$state.snapshot(p), id: "", name: `${p.name} (copy)` };
	}

	function preview(p: ConnectionProfile, schema: SchemaNode, obj: ObjectNode) {
		workspace.openQuery(p.id, previewQuery(schema.name, obj.name, p.engine));
		onnavigate?.();
	}

	function insertName(p: ConnectionProfile, schema: SchemaNode, obj: ObjectNode) {
		const tree = explorer.tree(p.id);
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
					description="Add a PostgreSQL or Oracle connection to get started."
				>
					{#snippet actions()}
						<Button size="sm" variant="filled" onclick={() => (workspace.editing = emptyProfile())}>
							{#snippet leading()}<Plus size={14} />{/snippet}
							New connection
						</Button>
					{/snippet}
				</EmptyState>
			</div>
		{/if}

		<ul class="lq-tree" role="list">
			{#each workspace.profiles as p (p.id)}
				{@const tree = explorer.tree(p.id)}
				<li class="lq-tree__conn" style="--conn-color:{p.color ?? 'transparent'}">
					<div class="lq-tree__row lq-tree__row--conn">
						<button
							type="button"
							class="lq-tree__toggle"
							aria-expanded={!!tree?.expanded}
							onclick={() => explorer.toggleProfile(p)}
							ondblclick={() => newQuery(p)}
						>
							<ChevronRight size={14} class="lq-tree__chev {tree?.expanded ? 'lq-tree__chev--open' : ''}" aria-hidden="true" />
							<span class="lq-tree__engine lq-tree__engine--{p.engine}" title={ENGINE_LABEL[p.engine]}>
								{p.engine === "postgres" ? "PG" : "ORA"}
							</span>
							<span class="lq-tree__conn-text">
								<span class="lq-tree__label">{p.name}</span>
								<span class="lq-tree__sub">{p.user}@{p.engine === "oracle" && p.oracleConnectType === "custom" ? "descriptor" : p.host}</span>
							</span>
						</button>
						{#if tree?.loading}
							<Spinner size={14} label="Loading" />
						{/if}
						<DropdownMenu align="end">
							{#snippet triggerChild(props)}
								<IconButton {...props} size="sm" variant="text" aria-label="Actions for {p.name}">
									<EllipsisVertical size={15} />
								</IconButton>
							{/snippet}
							<DropdownMenuItem onclick={() => newQuery(p)}>
								{#snippet icon()}<FilePlus2 size={15} />{/snippet}
								New query
							</DropdownMenuItem>
							<DropdownMenuItem onclick={() => explorer.refresh(p)}>
								{#snippet icon()}<RefreshCw size={15} />{/snippet}
								Refresh schemas
							</DropdownMenuItem>
							<DropdownMenuSeparator />
							<DropdownMenuItem onclick={() => (workspace.editing = $state.snapshot(p))}>
								{#snippet icon()}<SquarePen size={15} />{/snippet}
								Edit
							</DropdownMenuItem>
							<DropdownMenuItem onclick={() => duplicate(p)}>
								{#snippet icon()}<Copy size={15} />{/snippet}
								Duplicate
							</DropdownMenuItem>
							<DropdownMenuItem variant="destructive" onclick={() => (confirmDelete = p)}>
								{#snippet icon()}<Trash2 size={15} />{/snippet}
								Delete
							</DropdownMenuItem>
						</DropdownMenu>
					</div>

					{#if tree?.expanded}
						{#if tree.error}
							<p class="lq-tree__error">{tree.error}</p>
						{/if}
						{#if tree.schemas}
							<ul class="lq-tree__children" role="list">
								{#each tree.schemas as s (s.name)}
									<li>
										<div class="lq-tree__row">
											<button type="button" class="lq-tree__toggle" aria-expanded={s.expanded} onclick={() => explorer.toggleSchema(p.id, s)}>
												<ChevronRight size={14} class="lq-tree__chev {s.expanded ? 'lq-tree__chev--open' : ''}" aria-hidden="true" />
												<FolderTree size={14} aria-hidden="true" />
												<span class="lq-tree__label" class:lq-tree__label--current={s.name === tree.currentSchema}>{s.name}</span>
											</button>
											{#if s.loading}<Spinner size={12} label="Loading" />{/if}
										</div>
										{#if s.expanded && s.objects}
											{#if s.objects.length === 0}
												<p class="lq-tree__hint">No visible objects</p>
											{/if}
											<ul class="lq-tree__children" role="list">
												{#each groups(s.objects) as [kind, items] (kind)}
													<li class="lq-tree__group">{KIND_LABEL[kind] ?? kind} <span>{items.length}</span></li>
													{#each items as o (o.kind + o.name)}
														{@const Icon = KIND_ICON[o.kind as keyof typeof KIND_ICON] ?? Table2}
														<li>
															<div class="lq-tree__row lq-tree__row--obj">
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
																{#if RELATION_KINDS.has(o.kind)}
																	<button type="button" class="lq-tree__action" aria-label="Query {o.name}" title="Query 200 rows" onclick={() => preview(p, s, o)}>
																		<Play size={13} />
																	</button>
																{/if}
															</div>
															{#if o.expanded && o.columns}
																<ul class="lq-tree__children lq-tree__cols" role="list">
																	{#each o.columns as c (c.name)}
																		<li class="lq-tree__col">
																			{#if c.primaryKey}<KeyRound size={12} aria-label="Primary key" />{:else}<Columns3 size={12} aria-hidden="true" />{/if}
																			<button type="button" class="lq-tree__col-name" ondblclick={() => oninsert?.(quoteIdent(c.name, p.engine))} title="Double-click to insert into the editor">
																				{c.name}
																			</button>
																			<span class="lq-tree__type">{c.typeName}{c.nullable ? "" : " not null"}</span>
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
</nav>

{#if confirmDelete}
	<AlertDialog
		open={true}
		title="Delete {confirmDelete.name}?"
		description="The profile and its saved password will be removed from this computer. The database is not changed."
		confirmLabel="Delete"
		cancelLabel="Cancel"
		destructive
		onconfirm={async () => {
			if (confirmDelete) {
				await explorer.close(confirmDelete.id);
				await workspace.deleteProfile(confirmDelete.id);
			}
			confirmDelete = null;
		}}
		oncancel={() => (confirmDelete = null)}
	/>
{/if}

<style>
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
		flex-shrink: 0;
		min-width: 30px;
		padding: 1px 4px;
		border: 1px solid var(--ca-outline-default);
		color: var(--ca-text-secondary);
		font-family: var(--ca-font-mono);
		font-size: 10px;
		font-weight: 700;
		text-align: center;
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
	.lq-tree__more {
		display: grid;
		place-items: center;
		width: 26px;
		height: 26px;
		color: var(--ca-text-secondary);
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
	.lq-tree__row--obj:hover .lq-tree__action,
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
