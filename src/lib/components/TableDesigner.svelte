<script lang="ts">
	import { Dialog, Button, IconButton, TextField, toast } from "@codeatlasdev/ui";
	import { Plus, Trash2, ArrowUp, ArrowDown, FileCode, TriangleAlert, KeyRound } from "@lucide/svelte";
	import { untrack } from "svelte";
	import { actions as act, type DesignerRequest } from "../state/actions.svelte";
	import { explorer } from "../state/explorer.svelte";
	import { workspace } from "../state/workspace.svelte";
	import { alterTableSql, createTableSql, foldName, toScript, type ColumnDraft, type ColumnSpec } from "../sql/ddl";
	import type { Engine } from "../api";

	/** Common types per engine, offered as suggestions (any type can be typed). */
	const TYPES: Record<Engine, string[]> = {
		postgres: ["integer", "bigint", "smallint", "numeric(12,2)", "real", "double precision", "boolean", "text", "varchar(255)", "char(1)", "date", "timestamp", "timestamptz", "time", "interval", "uuid", "jsonb", "json", "bytea", "inet"],
		oracle: ["NUMBER", "NUMBER(10)", "NUMBER(12,2)", "BINARY_DOUBLE", "VARCHAR2(255)", "NVARCHAR2(255)", "CHAR(1)", "CLOB", "BLOB", "DATE", "TIMESTAMP", "TIMESTAMP WITH TIME ZONE", "RAW(16)", "BOOLEAN"],
		mysql: ["INT", "BIGINT", "SMALLINT", "TINYINT(1)", "DECIMAL(12,2)", "DOUBLE", "VARCHAR(255)", "CHAR(1)", "TEXT", "LONGTEXT", "DATE", "DATETIME", "TIMESTAMP", "TIME", "JSON", "BLOB", "BINARY(16)"],
		mariadb: ["INT", "BIGINT", "SMALLINT", "TINYINT(1)", "DECIMAL(12,2)", "DOUBLE", "VARCHAR(255)", "CHAR(1)", "TEXT", "LONGTEXT", "DATE", "DATETIME", "TIMESTAMP", "TIME", "JSON", "BLOB", "UUID"],
		sqlserver: ["int", "bigint", "smallint", "bit", "decimal(12,2)", "float", "nvarchar(255)", "nvarchar(max)", "varchar(255)", "nchar(1)", "date", "datetime2(3)", "datetimeoffset(3)", "time(0)", "uniqueidentifier", "varbinary(max)"],
		sqlite: ["INTEGER", "REAL", "NUMERIC", "TEXT", "BLOB"],
		redis: [],
	};
	const DEFAULT_TYPE: Record<Engine, string> = {
		postgres: "text",
		oracle: "VARCHAR2(255)",
		mysql: "VARCHAR(255)",
		mariadb: "VARCHAR(255)",
		sqlserver: "nvarchar(255)",
		sqlite: "TEXT",
		redis: "",
	};
	const ID_TYPE: Record<Engine, string> = {
		postgres: "bigint",
		oracle: "NUMBER(19)",
		mysql: "BIGINT",
		mariadb: "BIGINT",
		sqlserver: "bigint",
		sqlite: "INTEGER",
		redis: "",
	};

	const req = $derived(act.designer);
	const engine = $derived<Engine>(req?.profile.engine ?? "postgres");
	const creating = $derived(req?.table === null);

	let name = $state("");
	let rows = $state<(ColumnDraft & { key: number })[]>([]);
	let before = $state<ColumnSpec[]>([]);
	let running = $state(false);
	let error = $state<string | null>(null);
	let nameField = $state<HTMLInputElement | null>(null);
	let seq = 0;

	// Starts over whenever the designer opens for another table.
	$effect(() => {
		const r = act.designer;
		if (r) untrack(() => reset(r));
	});

	function reset(r: DesignerRequest) {
		error = null;
		running = false;
		name = r.table ?? "";
		const cols: ColumnSpec[] = r.columns.map((c) => ({
			name: c.name,
			type: c.typeName,
			nullable: c.nullable,
			defaultValue: c.defaultValue ?? "",
			primaryKey: c.primaryKey,
			identity: false,
		}));
		before = cols;
		rows =
			r.table === null
				? [
						{
							key: seq++,
							origin: null,
							name: foldName("id", r.profile.engine),
							type: ID_TYPE[r.profile.engine],
							nullable: false,
							defaultValue: "",
							primaryKey: true,
							identity: true,
						},
					]
				: cols.map((c) => ({ ...c, key: seq++, origin: c.name }));
		if (r.table === null) queueMicrotask(() => nameField?.focus());
	}

	// New and renamed columns are folded like the database folds unquoted names.
	const columns = $derived(
		rows.map(({ key: _, ...c }) => ({ ...c, name: c.name.trim() === c.origin ? c.origin : foldName(c.name, engine) })),
	);
	const plan = $derived(
		!req
			? { statements: [], warnings: [] }
			: creating
				? createTableSql(req.schema, foldName(name, engine), columns, engine)
				: alterTableSql(req.schema, req.table!, before, columns, engine, name.trim() === req.table ? req.table! : foldName(name, engine)),
	);
	const sql = $derived(toScript(plan.statements, engine));

	function add() {
		rows.push({ key: seq++, origin: null, name: "", type: DEFAULT_TYPE[engine], nullable: true, defaultValue: "", primaryKey: false, identity: false });
		queueMicrotask(() => {
			const inputs = document.querySelectorAll<HTMLInputElement>(".lq-design__name");
			inputs[inputs.length - 1]?.focus();
		});
	}

	function move(i: number, by: number) {
		const j = i + by;
		if (j < 0 || j >= rows.length) return;
		[rows[i], rows[j]] = [rows[j], rows[i]];
	}

	function close() {
		if (running) return;
		act.designer = null;
	}

	async function apply() {
		if (!req || plan.statements.length === 0 || running) return;
		running = true;
		error = await act.execute(req.profile, plan.statements);
		running = false;
		await explorer.refreshSchema(req.profile.id, req.schema);
		if (error) return;
		const table = creating ? foldName(name, engine) : name.trim() || req.table;
		toast.success(creating ? `Table ${table} created` : `Table ${table} changed`);
		act.designer = null;
	}

	function toEditor() {
		if (!req) return;
		workspace.openScript(req.profile.id, creating ? "New table" : `Alter ${req.table}`, sql);
		act.designer = null;
	}
</script>

{#if req}
	<Dialog
		open={true}
		size="xl"
		title={creating ? `New table in ${req.schema}` : `Edit table ${req.table}`}
		description={creating ? undefined : "Changes are written as ALTER TABLE statements; review them below before applying."}
		onOpenChange={(o) => {
			if (!o) close();
		}}
	>
		<div class="lq-design">
			<TextField label="Table name" bind:value={name} bind:ref={nameField} placeholder="new_table" />

			<div class="lq-design__grid" role="table" aria-label="Columns">
				<div class="lq-design__head" role="row">
					<span role="columnheader">Name</span>
					<span role="columnheader">Type</span>
					<span role="columnheader" title="Primary key"><KeyRound size={13} aria-label="Primary key" /></span>
					<span role="columnheader">Not null</span>
					<span role="columnheader" title="Values generated by the database">Auto</span>
					<span role="columnheader">Default</span>
					<span></span>
				</div>
				{#each rows as row, i (row.key)}
					<div class="lq-design__row" role="row" class:lq-design__row--new={!creating && row.origin === null}>
						<input class="lq-design__name" aria-label="Column name" bind:value={row.name} spellcheck="false" />
						<input aria-label="Type of {row.name}" list="lq-types-{engine}" bind:value={row.type} spellcheck="false" />
						<input type="checkbox" aria-label="{row.name} is in the primary key" bind:checked={row.primaryKey} />
						<input type="checkbox" aria-label="{row.name} is NOT NULL" checked={!row.nullable || row.primaryKey} disabled={row.primaryKey} onchange={(e) => (row.nullable = !e.currentTarget.checked)} />
						<input
							type="checkbox"
							aria-label="{row.name} values generated by the database"
							bind:checked={row.identity}
							disabled={row.origin !== null}
							title={row.origin !== null ? "Only new columns can be made identity columns here" : "Identity / AUTO_INCREMENT"}
						/>
						<input aria-label="Default of {row.name}" placeholder="none" bind:value={row.defaultValue} disabled={row.identity} spellcheck="false" />
						<span class="lq-design__tools">
							<IconButton size="sm" variant="text" aria-label="Move {row.name} up" disabled={i === 0} onclick={() => move(i, -1)}><ArrowUp size={14} /></IconButton>
							<IconButton size="sm" variant="text" aria-label="Move {row.name} down" disabled={i === rows.length - 1} onclick={() => move(i, 1)}><ArrowDown size={14} /></IconButton>
							<IconButton size="sm" variant="text" aria-label="Remove {row.name}" onclick={() => rows.splice(i, 1)}><Trash2 size={14} /></IconButton>
						</span>
					</div>
				{/each}
				<datalist id="lq-types-{engine}">
					{#each TYPES[engine] as t (t)}<option value={t}></option>{/each}
				</datalist>
			</div>
			<div>
				<Button size="sm" variant="outlined" onclick={add}>
					{#snippet leading()}<Plus size={14} />{/snippet}
					Add column
				</Button>
			</div>
			{#if !creating}
				<p class="lq-design__hint">Defaults are SQL expressions: quote text values ('active'). Moving columns does not reorder an existing table.</p>
			{/if}

			<div class="lq-design__sql">
				<span class="lq-design__label">SQL</span>
				{#if sql}<pre>{sql}</pre>{:else}<p class="lq-design__hint">Nothing to run yet.</p>{/if}
			</div>
			{#each plan.warnings as w (w)}
				<p class="lq-design__warn"><TriangleAlert size={14} aria-hidden="true" />{w}</p>
			{/each}
			{#if error}<p class="lq-design__error" role="alert">{error}</p>{/if}
		</div>
		{#snippet actions()}
			<Button variant="ghost" disabled={running} onclick={close}>Cancel</Button>
			<Button variant="outlined" disabled={plan.statements.length === 0 || running} onclick={toEditor}>
				{#snippet leading()}<FileCode size={15} />{/snippet}
				Open in editor
			</Button>
			<Button variant="filled" disabled={plan.statements.length === 0} loading={running} onclick={apply}>
				{creating ? "Create table" : "Apply changes"}
			</Button>
		{/snippet}
	</Dialog>
{/if}

<style>
	.lq-design {
		display: flex;
		flex-direction: column;
		gap: var(--ca-space-3);
	}
	.lq-design__grid {
		display: flex;
		flex-direction: column;
		max-height: 300px;
		overflow: auto;
		border: 1px solid var(--ca-outline-subtle);
	}
	.lq-design__head,
	.lq-design__row {
		display: grid;
		grid-template-columns: minmax(120px, 1.3fr) minmax(120px, 1.2fr) 36px 60px 44px minmax(90px, 1fr) 96px;
		align-items: center;
		gap: 6px;
		padding: 3px 6px;
	}
	.lq-design__head {
		position: sticky;
		top: 0;
		z-index: 1;
		border-bottom: 1px solid var(--ca-outline-subtle);
		background: var(--ca-surface-sunken);
		color: var(--ca-text-secondary);
		font-family: var(--ca-font-mono);
		font-size: 11px;
		font-weight: 600;
		letter-spacing: 0.04em;
		text-transform: uppercase;
	}
	.lq-design__row + .lq-design__row {
		border-top: 1px solid var(--ca-outline-subtle);
	}
	.lq-design__row--new {
		background: color-mix(in srgb, var(--ca-brand-main) 8%, transparent);
	}
	.lq-design__row input:not([type="checkbox"]) {
		min-width: 0;
		height: 28px;
		padding: 0 6px;
		border: 1px solid var(--ca-outline-subtle);
		background: var(--ca-surface-raised);
		color: var(--ca-text-primary);
		font-family: var(--ca-font-mono);
		font-size: 12px;
	}
	.lq-design__row input:disabled {
		opacity: 0.5;
	}
	.lq-design__row input:focus-visible {
		outline: 2px solid var(--ca-focus-ring);
		outline-offset: -1px;
	}
	.lq-design__row input[type="checkbox"] {
		justify-self: center;
		width: 15px;
		height: 15px;
		accent-color: var(--ca-brand-fill);
	}
	.lq-design__tools {
		display: flex;
		justify-content: flex-end;
	}
	.lq-design__label {
		color: var(--ca-text-secondary);
		font-family: var(--ca-font-mono);
		font-size: 11px;
		font-weight: 600;
		letter-spacing: 0.06em;
		text-transform: uppercase;
	}
	.lq-design__sql {
		display: flex;
		flex-direction: column;
		gap: var(--ca-space-1);
	}
	.lq-design__sql pre {
		max-height: 200px;
		margin: 0;
		padding: var(--ca-space-2) var(--ca-space-3);
		overflow: auto;
		border: 1px solid var(--ca-outline-subtle);
		background: var(--ca-surface-sunken);
		font-family: var(--ca-font-mono);
		font-size: 12px;
		line-height: 1.5;
		white-space: pre-wrap;
		overflow-wrap: anywhere;
	}
	.lq-design__hint {
		margin: 0;
		color: var(--ca-text-hint);
		font-size: 12px;
	}
	.lq-design__warn,
	.lq-design__error {
		display: flex;
		gap: var(--ca-space-2);
		align-items: flex-start;
		margin: 0;
		font-size: 13px;
	}
	.lq-design__warn {
		color: var(--ca-text-secondary);
	}
	.lq-design__warn :global(svg) {
		flex-shrink: 0;
		margin-top: 2px;
		color: var(--ca-semantic-warning);
	}
	.lq-design__error {
		color: var(--ca-semantic-error);
		white-space: pre-wrap;
	}
</style>
