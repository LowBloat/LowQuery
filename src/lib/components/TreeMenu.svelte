<script lang="ts">
	import {
		DropdownMenu,
		DropdownMenuItem,
		DropdownMenuSeparator,
		DropdownMenuLabel,
		DropdownMenuSub,
		DropdownMenuSubTrigger,
	} from "@codeatlasdev/ui";
	import {
		FilePlus2,
		RefreshCw,
		Plug,
		Unplug,
		DatabaseZap,
		Activity,
		FolderOpen,
		SquarePen,
		Copy,
		ClipboardCopy,
		Trash2,
		Table2,
		TableProperties,
		Eye,
		SquareFunction,
		Cog,
		Hash,
		TextCursorInput,
		Play,
		Sigma,
		FileCode,
		CodeXml,
		ListPlus,
		CopyPlus,
		Eraser,
		RefreshCcw,
		Timer,
		TimerOff,
		Hourglass,
		Columns3,
		Shapes,
		FolderTree,
	} from "@lucide/svelte";
	import { actions as act, kindName, type MenuTarget } from "../state/actions.svelte";
	import { explorer, RELATION_KINDS, REDIS_KINDS } from "../state/explorer.svelte";
	import { canDrop, hasDdl, schemaWord, templateKinds } from "../sql/ddl";
	import { qualified, quoteIdent } from "../sql/ident";

	interface Props {
		oninsert?: (text: string) => void;
		/** Called after an action that shows a tab (closes the drawer on small windows). */
		onnavigate?: () => void;
	}

	let { oninsert, onnavigate }: Props = $props();

	const TEMPLATE_ICON = { view: Eye, function: SquareFunction, procedure: Cog, sequence: Hash } as const;

	function go(fn: () => unknown) {
		fn();
		onnavigate?.();
	}

	function insertName(t: Extract<MenuTarget, { type: "object" }>) {
		const { profile: p, schema: s, obj: o } = t;
		const bare = o.name.replace(/\(.*\)$/, "");
		const current = explorer.tree(p.id)?.currentSchema;
		oninsert?.(p.engine === "redis" ? o.name : s.name === current ? quoteIdent(bare, p.engine) : qualified(s.name, bare, p.engine));
	}
</script>

{#if act.menu}
	{@const m = act.menu}
	{@const t = m.target}
	{@const p = t.profile}
	{@const e = p.engine}
	<DropdownMenu
		open={true}
		anchor={{ x: m.x, y: m.y }}
		onOpenChange={(o) => {
			if (!o) act.menu = null;
		}}
	>
		{#if t.type === "connection"}
			{@const connected = !!explorer.tree(p.id)?.sessionId}
			<DropdownMenuLabel class="lq-menu-label">{p.name}</DropdownMenuLabel>
			<DropdownMenuItem onclick={() => go(() => act.newQuery(p))}>
				{#snippet icon()}<FilePlus2 size={15} />{/snippet}
				New query
			</DropdownMenuItem>
			{#if connected}
				<DropdownMenuItem onclick={() => explorer.refresh(p)}>
					{#snippet icon()}<RefreshCw size={15} />{/snippet}
					Refresh
				</DropdownMenuItem>
				<DropdownMenuItem onclick={() => act.disconnect(p)}>
					{#snippet icon()}<Unplug size={15} />{/snippet}
					Disconnect
				</DropdownMenuItem>
			{:else}
				<DropdownMenuItem onclick={() => explorer.load(p)}>
					{#snippet icon()}<Plug size={15} />{/snippet}
					Connect
				</DropdownMenuItem>
			{/if}
			{#if e !== "sqlite" && e !== "redis"}
				<DropdownMenuSeparator />
				<DropdownMenuItem onclick={() => go(() => act.createSchema(p))}>
					{#snippet icon()}<FolderTree size={15} />{/snippet}
					{e === "oracle" ? "Create schema (user)…" : `Create ${schemaWord(e)}…`}
				</DropdownMenuItem>
				{#if e === "postgres" || e === "sqlserver"}
					<DropdownMenuItem onclick={() => act.createSchema(p, true)}>
						{#snippet icon()}<DatabaseZap size={15} />{/snippet}
						Create database…
					</DropdownMenuItem>
				{/if}
			{/if}
			{#if e !== "sqlite"}
				<DropdownMenuItem onclick={() => go(() => act.showSessions(p))}>
					{#snippet icon()}<Activity size={15} />{/snippet}
					{e === "redis" ? "Connected clients" : "Server sessions"}
				</DropdownMenuItem>
			{:else}
				<DropdownMenuItem onclick={() => act.revealFile(p)}>
					{#snippet icon()}<FolderOpen size={15} />{/snippet}
					Show file in folder
				</DropdownMenuItem>
			{/if}
			<DropdownMenuSeparator />
			<DropdownMenuItem onclick={() => act.editProfile(p)}>
				{#snippet icon()}<SquarePen size={15} />{/snippet}
				Edit connection…
			</DropdownMenuItem>
			<DropdownMenuItem onclick={() => act.duplicateProfile(p)}>
				{#snippet icon()}<Copy size={15} />{/snippet}
				Duplicate
			</DropdownMenuItem>
			<DropdownMenuItem onclick={() => act.copyConnection(p)}>
				{#snippet icon()}<ClipboardCopy size={15} />{/snippet}
				Copy connection details
			</DropdownMenuItem>
			<DropdownMenuSeparator />
			<DropdownMenuItem variant="destructive" onclick={() => (act.deleting = p)}>
				{#snippet icon()}<Trash2 size={15} />{/snippet}
				Delete connection…
			</DropdownMenuItem>
		{:else if t.type === "schema"}
			{@const s = t.schema}
			{@const word = schemaWord(e)}
			{@const builtIn = e === "sqlite" && (s.name === "main" || s.name === "temp")}
			<DropdownMenuLabel class="lq-menu-label">{s.name}</DropdownMenuLabel>
			<DropdownMenuItem onclick={() => go(() => act.newQueryIn(p, s))}>
				{#snippet icon()}<FilePlus2 size={15} />{/snippet}
				{e === "sqlserver" || e === "sqlite" ? "New query" : `New query in this ${word}`}
			</DropdownMenuItem>
			<DropdownMenuItem onclick={() => explorer.refreshSchema(p.id, s.name)}>
				{#snippet icon()}<RefreshCw size={15} />{/snippet}
				Refresh
			</DropdownMenuItem>
			{#if e !== "redis"}
				<DropdownMenuSeparator />
				<DropdownMenuItem onclick={() => act.createTable(p, s)}>
					{#snippet icon()}<TableProperties size={15} />{/snippet}
					Create table…
				</DropdownMenuItem>
				<DropdownMenuSub>
					{#snippet trigger()}
						<DropdownMenuSubTrigger>
							{#snippet icon()}<Shapes size={15} />{/snippet}
							New object
						</DropdownMenuSubTrigger>
					{/snippet}
					{#each templateKinds(e) as kind (kind)}
						{@const Icon = TEMPLATE_ICON[kind]}
						<DropdownMenuItem onclick={() => go(() => act.template(p, s, kind))}>
							{#snippet icon()}<Icon size={15} />{/snippet}
							{kind.charAt(0).toUpperCase() + kind.slice(1)}
						</DropdownMenuItem>
					{/each}
				</DropdownMenuSub>
			{/if}
			<DropdownMenuSeparator />
			{#if e === "postgres"}
				<DropdownMenuItem onclick={() => act.renameSchema(p, s)}>
					{#snippet icon()}<TextCursorInput size={15} />{/snippet}
					Rename…
				</DropdownMenuItem>
			{/if}
			<DropdownMenuItem onclick={() => act.copyName(p, null, s.name, false)}>
				{#snippet icon()}<ClipboardCopy size={15} />{/snippet}
				Copy name
			</DropdownMenuItem>
			{#if !builtIn}
				<DropdownMenuSeparator />
				<DropdownMenuItem variant="destructive" onclick={() => act.dropSchema(p, s)}>
					{#snippet icon()}<Trash2 size={15} />{/snippet}
					{e === "redis" ? "Flush database…" : e === "sqlite" ? "Detach…" : `Drop ${word}…`}
				</DropdownMenuItem>
			{/if}
		{:else}
			{@const s = t.schema}
			{@const o = t.obj}
			{@const relation = RELATION_KINDS.has(o.kind)}
			{@const table = o.kind === "table"}
			<DropdownMenuLabel class="lq-menu-label">{o.name}</DropdownMenuLabel>
			{#if REDIS_KINDS.has(o.kind)}
				<DropdownMenuItem onclick={() => go(() => act.preview(p, s, o))}>
					{#snippet icon()}<Play size={15} />{/snippet}
					Show value
				</DropdownMenuItem>
				<DropdownMenuItem onclick={() => go(() => act.redisTtl(p, s, o))}>
					{#snippet icon()}<Hourglass size={15} />{/snippet}
					Time to live
				</DropdownMenuItem>
				<DropdownMenuSeparator />
				<DropdownMenuItem onclick={() => act.redisKey(p, s, o, "rename")}>
					{#snippet icon()}<TextCursorInput size={15} />{/snippet}
					Rename…
				</DropdownMenuItem>
				<DropdownMenuItem onclick={() => act.redisKey(p, s, o, "expire")}>
					{#snippet icon()}<Timer size={15} />{/snippet}
					Set expiry…
				</DropdownMenuItem>
				<DropdownMenuItem onclick={() => act.redisKey(p, s, o, "persist")}>
					{#snippet icon()}<TimerOff size={15} />{/snippet}
					Remove expiry…
				</DropdownMenuItem>
				<DropdownMenuItem onclick={() => insertName(t)}>
					{#snippet icon()}<CodeXml size={15} />{/snippet}
					Insert into editor
				</DropdownMenuItem>
				<DropdownMenuItem onclick={() => act.copy(o.name, "Key name")}>
					{#snippet icon()}<ClipboardCopy size={15} />{/snippet}
					Copy key name
				</DropdownMenuItem>
				<DropdownMenuSeparator />
				<DropdownMenuItem variant="destructive" onclick={() => act.redisKey(p, s, o, "delete")}>
					{#snippet icon()}<Trash2 size={15} />{/snippet}
					Delete key…
				</DropdownMenuItem>
			{:else}
				{#if relation}
					<DropdownMenuItem onclick={() => go(() => act.preview(p, s, o))}>
						{#snippet icon()}<Play size={15} />{/snippet}
						Select first 200 rows
					</DropdownMenuItem>
					<DropdownMenuItem onclick={() => go(() => act.countRows(p, s, o))}>
						{#snippet icon()}<Sigma size={15} />{/snippet}
						Count rows
					</DropdownMenuItem>
				{/if}
				{#if hasDdl(o.kind, e)}
					<DropdownMenuItem onclick={() => go(() => act.showDdl(p, s, o))}>
						{#snippet icon()}<FileCode size={15} />{/snippet}
						Show DDL
					</DropdownMenuItem>
				{/if}
				{#if relation}
					<DropdownMenuSub>
						{#snippet trigger()}
							<DropdownMenuSubTrigger>
								{#snippet icon()}<CodeXml size={15} />{/snippet}
								Generate SQL
							</DropdownMenuSubTrigger>
						{/snippet}
						<DropdownMenuItem onclick={() => go(() => act.generate(p, s, o, "select"))}>SELECT</DropdownMenuItem>
						{#if o.kind !== "matview"}
							<DropdownMenuItem onclick={() => go(() => act.generate(p, s, o, "insert"))}>INSERT</DropdownMenuItem>
							<DropdownMenuItem onclick={() => go(() => act.generate(p, s, o, "update"))}>UPDATE</DropdownMenuItem>
							<DropdownMenuItem onclick={() => go(() => act.generate(p, s, o, "delete"))}>DELETE</DropdownMenuItem>
						{/if}
						{#if table}
							<DropdownMenuSeparator />
							<DropdownMenuItem onclick={() => go(() => act.createIndex(p, s, o))}>
								{#snippet icon()}<ListPlus size={15} />{/snippet}
								CREATE INDEX
							</DropdownMenuItem>
						{/if}
					</DropdownMenuSub>
				{/if}
				{#if table}
					<DropdownMenuSeparator />
					<DropdownMenuItem onclick={() => act.editTable(p, s, o)}>
						{#snippet icon()}<Table2 size={15} />{/snippet}
						Edit table…
					</DropdownMenuItem>
					<DropdownMenuItem onclick={() => act.renameTable(p, s, o)}>
						{#snippet icon()}<TextCursorInput size={15} />{/snippet}
						Rename…
					</DropdownMenuItem>
					<DropdownMenuItem onclick={() => act.duplicateTable(p, s, o)}>
						{#snippet icon()}<CopyPlus size={15} />{/snippet}
						Duplicate…
					</DropdownMenuItem>
					<DropdownMenuItem onclick={() => act.truncate(p, s, o)}>
						{#snippet icon()}<Eraser size={15} />{/snippet}
						Empty table…
					</DropdownMenuItem>
				{/if}
				{#if o.kind === "matview" && (e === "postgres" || e === "oracle")}
					<DropdownMenuItem onclick={() => act.refreshMatview(p, s, o)}>
						{#snippet icon()}<RefreshCcw size={15} />{/snippet}
						Refresh data…
					</DropdownMenuItem>
				{/if}
				<DropdownMenuSeparator />
				<DropdownMenuItem onclick={() => insertName(t)}>
					{#snippet icon()}<CodeXml size={15} />{/snippet}
					Insert name into editor
				</DropdownMenuItem>
				<DropdownMenuItem onclick={() => act.copyName(p, s, o.name, false)}>
					{#snippet icon()}<ClipboardCopy size={15} />{/snippet}
					Copy name
				</DropdownMenuItem>
				<DropdownMenuItem onclick={() => act.copyName(p, s, o.name, true)}>
					{#snippet icon()}<ClipboardCopy size={15} />{/snippet}
					Copy qualified name
				</DropdownMenuItem>
				{#if relation}
					<DropdownMenuItem onclick={() => act.copyColumns(p, s, o)}>
						{#snippet icon()}<Columns3 size={15} />{/snippet}
						Copy column list
					</DropdownMenuItem>
				{/if}
				{#if canDrop(o.kind, e)}
					<DropdownMenuSeparator />
					<DropdownMenuItem variant="destructive" onclick={() => act.drop(p, s, o)}>
						{#snippet icon()}<Trash2 size={15} />{/snippet}
						Drop {kindName(o.kind)}…
					</DropdownMenuItem>
				{/if}
			{/if}
		{/if}
	</DropdownMenu>
{/if}

<style>
	/* Object names keep their case: in most databases it is part of the name. */
	:global(.lq-menu-label) {
		max-width: 280px;
		overflow: hidden;
		text-transform: none;
		white-space: nowrap;
		text-overflow: ellipsis;
	}
</style>
