<script lang="ts">
	import {
		Dialog,
		Button,
		TextField,
		TextArea,
		NumberField,
		Select,
		SegmentedControl,
		SwatchGroup,
		Switch,
		Alert,
		FormSection,
		Combobox,
	} from "@codeatlasdev/ui";
	import { FolderOpen, FilePlus2, ArrowLeft, Search } from "@lucide/svelte";
	import { open as openFile, save as saveFile } from "@tauri-apps/plugin-dialog";
	import OracleClientPicker from "./OracleClientPicker.svelte";
	import EngineLogo from "./EngineLogo.svelte";
	import { tns, describeEntry } from "../state/tns.svelte";
	import { api, asAppError, errorText, emptyProfile, type ConnectionProfile } from "../api";
	import { ENGINES, engineInfo, type Engine } from "../engines";
	import { workspace } from "../state/workspace.svelte";
	import { explorer } from "../state/explorer.svelte";
	import { ai } from "../state/ai.svelte";

	let form = $state<ConnectionProfile | null>(null);
	/** "pick": choose the database; "form": connection details. */
	let step = $state<"pick" | "form">("form");
	let filter = $state("");
	let password = $state("");
	let passwordTouched = $state(false);
	let testing = $state(false);
	let saving = $state(false);
	let status = $state<{ ok: boolean; text: string } | null>(null);
	let error = $state<string | null>(null);

	// Copy the profile being edited when the dialog opens. A blank new profile
	// starts at the database picker; edits and duplicates go to the form.
	$effect(() => {
		const p = workspace.editing;
		form = p ? structuredClone($state.snapshot(p)) : null;
		step = p && !p.id && !p.name ? "pick" : "form";
		filter = "";
		password = "";
		passwordTouched = false;
		status = null;
		error = null;
	});

	// tnsnames.ora is read once, the first time an Oracle profile is edited.
	$effect(() => {
		if (form?.engine === "oracle") tns.load();
	});

	const isNew = $derived(!form?.id);
	const info = $derived(form ? engineInfo(form.engine) : null);
	const visibleEngines = $derived(
		ENGINES.filter((e) => {
			const q = filter.trim().toLowerCase();
			return !q || e.label.toLowerCase().includes(q) || e.driver.toLowerCase().includes(q);
		}),
	);

	const oracleTypeItems = [
		{ value: "service", label: "Service name" },
		{ value: "sid", label: "SID" },
		{ value: "tns", label: "TNS alias" },
		{ value: "custom", label: "Descriptor" },
	];
	const sqlServerAuthItems = [
		{ value: "sql", label: "SQL Server login" },
		{ value: "windows", label: "Windows account" },
	];

	// Aliases from the chosen Oracle client, or from every discovered one.
	const tnsEntries = $derived(form?.engine === "oracle" ? tns.entries(form.configDir) : []);
	const aliasOptions = $derived.by(() => {
		const opts = tnsEntries.map((e) => ({ value: e.alias, label: `${e.alias} — ${describeEntry(e)}` }));
		const current = form?.tnsAlias;
		if (current && !tnsEntries.some((e) => e.alias.toLowerCase() === current.toLowerCase())) {
			opts.unshift({ value: current, label: `${current} (not in this tnsnames.ora)` });
		}
		return opts;
	});
	const selectedEntry = $derived(
		tnsEntries.find((e) => e.alias.toLowerCase() === form?.tnsAlias.toLowerCase()) ?? null,
	);
	// Service/SID mode: entries that can fill host, port and service.
	const fillOptions = $derived(
		tnsEntries.filter((e) => e.host).map((e) => ({ value: e.alias, label: `${e.alias} — ${describeEntry(e)}` })),
	);

	/** TLS options; SQL Server calls the same choices "encryption". */
	const sslOptions = $derived(
		form?.engine === "sqlserver"
			? [
					{ value: "disable", label: "Login only" },
					{ value: "prefer", label: "Encrypt, trust the server certificate" },
					{ value: "require", label: "Required, trust the server certificate" },
					{ value: "verify-full", label: "Required and verify the certificate" },
				]
			: [
					{ value: "disable", label: "Disabled" },
					{ value: "prefer", label: "Prefer (default)" },
					{ value: "require", label: "Require, skip certificate check" },
					{ value: "verify-full", label: "Require and verify certificate" },
				],
	);
	const privilegeOptions = [
		{ value: "none", label: "Normal" },
		{ value: "sysdba", label: "SYSDBA" },
		{ value: "sysoper", label: "SYSOPER" },
		{ value: "sysbackup", label: "SYSBACKUP" },
		{ value: "sysdg", label: "SYSDG" },
		{ value: "syskm", label: "SYSKM" },
		{ value: "sysrac", label: "SYSRAC" },
		{ value: "sysasm", label: "SYSASM" },
	];
	const aiItems = [
		{ value: "off", label: "Off" },
		{ value: "read", label: "Read only" },
		{ value: "write", label: "Read and write" },
	];

	// Identification colors; red for production, by convention.
	const colorItems = [
		{ value: "", label: "No color", color: "transparent" },
		{ value: "#b8f238", label: "Green", color: "#b8f238" },
		{ value: "#3b82f6", label: "Blue", color: "#3b82f6" },
		{ value: "#f59e0b", label: "Amber", color: "#f59e0b" },
		{ value: "#a855f7", label: "Violet", color: "#a855f7" },
		{ value: "#dc2626", label: "Red (production)", color: "#dc2626" },
	];

	/** Picks the database and moves to its connection form with its defaults. */
	function choose(engine: Engine) {
		if (!form) return;
		const keep = { name: form.name, color: form.color, savePassword: form.savePassword, aiAccess: form.aiAccess };
		form = { ...emptyProfile(engine), ...keep };
		step = "form";
		status = null;
		error = null;
	}

	function pickAlias(v: string | string[]) {
		if (!form || typeof v !== "string") return;
		form.tnsAlias = v;
		if (!form.name.trim()) form.name = v;
	}

	/** Copies host, port and service (or SID) from a tnsnames.ora entry. */
	function fillFrom(v: string | string[]) {
		const e = tnsEntries.find((x) => x.alias === v);
		if (!form || !e) return;
		form.host = e.host ?? form.host;
		form.port = e.port ?? form.port;
		if (e.serviceName) {
			form.oracleConnectType = "service";
			form.database = e.serviceName;
		} else if (e.sid) {
			form.oracleConnectType = "sid";
			form.database = e.sid;
		}
		if (!form.name.trim()) form.name = e.alias;
	}

	const SQLITE_FILTERS = [
		{ name: "SQLite database", extensions: ["db", "sqlite", "sqlite3", "db3"] },
		{ name: "All files", extensions: ["*"] },
	];

	async function browseSqlite() {
		const path = await openFile({ title: "Open SQLite database", filters: SQLITE_FILTERS });
		if (!form || typeof path !== "string") return;
		form.database = path;
		form.createFile = false;
		if (!form.name.trim()) form.name = fileName(path);
	}

	async function newSqlite() {
		const path = await saveFile({ title: "New SQLite database", defaultPath: "database.db", filters: SQLITE_FILTERS });
		if (!form || !path) return;
		form.database = path;
		form.createFile = true;
		if (!form.name.trim()) form.name = fileName(path);
	}

	function fileName(path: string): string {
		return path.split(/[\\/]/).pop()?.replace(/\.(db|sqlite3?|db3)$/i, "") ?? path;
	}

	function close() {
		workspace.editing = null;
	}

	function pw(): string | null {
		// Untouched empty password: keep the saved one (existing profiles).
		if (!passwordTouched && !isNew) return null;
		return password;
	}

	async function test() {
		if (!form) return;
		testing = true;
		status = null;
		error = null;
		try {
			const result = await api.testConnection($state.snapshot(form), pw());
			status = { ok: true, text: `Connection succeeded · ${result.serverVersion}` };
		} catch (e) {
			const err = asAppError(e);
			status = {
				ok: false,
				text: err.kind === "password_required" ? "Enter the password to test." : errorText(err),
			};
		} finally {
			testing = false;
		}
	}

	async function save() {
		if (!form) return;
		saving = true;
		error = null;
		try {
			const snapshot = $state.snapshot(form);
			if (!snapshot.name.trim()) {
				snapshot.name = engineInfo(snapshot.engine).file
					? fileName(snapshot.database)
					: `${snapshot.user || "user"}@${snapshot.host || "server"}`;
			}
			const saved = await workspace.saveProfile(snapshot, snapshot.savePassword ? pw() : null);
			// Settings changed: the tree reconnects on the next expand.
			await explorer.close(saved.id);
			close();
		} catch (e) {
			error = errorText(asAppError(e));
		} finally {
			saving = false;
		}
	}

	const needsCredentials = $derived(
		!!form && !engineInfo(form.engine).file && !(form.engine === "sqlserver" && form.windowsAuth),
	);
</script>

{#if form && info}
	<Dialog
		open={true}
		size="lg"
		title={step === "pick" ? "New connection" : isNew ? `New ${info.label} connection` : `Edit ${form.name}`}
		description={step === "pick"
			? "Choose the database. Each one connects through its own native driver."
			: info.file
				? "The database file stays where it is; LowQuery only keeps its path."
				: "Passwords are stored in the operating system keychain, never in a file."}
		onOpenChange={(o) => {
			if (!o) close();
		}}
	>
		{#if step === "pick"}
			<div class="lq-pick">
				<TextField label="Search" hideLabel placeholder="Search databases" bind:value={filter} autocomplete="off">
					{#snippet leading()}<Search size={16} />{/snippet}
				</TextField>
				<ul class="lq-pick__grid" role="list">
					{#each visibleEngines as e (e.id)}
						<li>
							<button type="button" class="lq-pick__card" onclick={() => choose(e.id)}>
								<EngineLogo engine={e.id} size={36} />
								<span class="lq-pick__name">{e.label}</span>
								<span class="lq-pick__driver">{e.driver}</span>
							</button>
						</li>
					{/each}
				</ul>
				{#if visibleEngines.length === 0}
					<p class="lq-pick__empty">No database matches “{filter}”.</p>
				{/if}
			</div>
		{:else}
			<form
				class="lq-conn-form"
				onsubmit={(e) => {
					e.preventDefault();
					save();
				}}
			>
				<div class="lq-conn-form__engine">
					<EngineLogo engine={form.engine} size={28} />
					<div>
						<p class="lq-conn-form__engine-name">{info.label}</p>
						<p class="lq-conn-form__engine-driver">{info.driver}</p>
					</div>
					{#if isNew}
						<span class="ca-toolbar-spacer"></span>
						<Button variant="ghost" size="sm" onclick={() => (step = "pick")}>
							{#snippet leading()}<ArrowLeft size={14} />{/snippet}
							Change database
						</Button>
					{/if}
				</div>

				<FormSection title="Identification">
					<TextField label="Name" placeholder="Production · ERP" bind:value={form.name} required />
					<div class="lq-conn-form__color">
						<span class="lq-conn-form__label">Color</span>
						<SwatchGroup
							aria-label="Identification color"
							items={colorItems}
							value={form.color ?? ""}
							onchange={(v) => form && (form.color = v || null)}
						/>
					</div>
				</FormSection>

				{#if form.engine === "sqlite"}
					<FormSection title="Database file">
						<div class="lq-conn-form__wide lq-conn-form__file">
							<TextField label="File" placeholder="C:\data\app.db" bind:value={form.database} required />
							<Button variant="outlined" onclick={browseSqlite}>
								{#snippet leading()}<FolderOpen size={15} />{/snippet}
								Open…
							</Button>
							<Button variant="outlined" onclick={newSqlite}>
								{#snippet leading()}<FilePlus2 size={15} />{/snippet}
								New…
							</Button>
						</div>
						<div class="lq-conn-form__wide">
							<Switch label="Create the file if it does not exist" bind:checked={form.createFile} />
						</div>
					</FormSection>
				{:else}
					<FormSection title="Server">
						{#if form.engine === "oracle"}
							<div class="lq-conn-form__wide">
								<SegmentedControl
									aria-label="Oracle connection type"
									size="sm"
									items={oracleTypeItems}
									bind:value={form.oracleConnectType}
								/>
							</div>
						{/if}

						{#if form.engine === "oracle" && form.oracleConnectType === "tns"}
							<div class="lq-conn-form__wide">
								<OracleClientPicker value={form.configDir} onchange={(d) => form && (form.configDir = d)} />
							</div>
							<div class="lq-conn-form__wide">
								<Combobox
									label="TNS alias"
									placeholder={tns.loading ? "Reading tnsnames.ora…" : "Choose an alias"}
									searchPlaceholder="Search aliases"
									emptyText="No alias found"
									options={aliasOptions}
									value={form.tnsAlias}
									onchange={pickAlias}
									description={selectedEntry ? describeEntry(selectedEntry) : undefined}
								/>
							</div>
							{#if tns.loaded && tnsEntries.length === 0}
								<div class="lq-conn-form__wide">
									<Alert severity="info">
										No tnsnames.ora found automatically. Set TNS_ADMIN, install an Oracle client or choose its folder with Browse.
									</Alert>
								</div>
							{/if}
						{:else if form.engine === "oracle" && form.oracleConnectType === "custom"}
							<div class="lq-conn-form__wide">
								<TextArea
									label="Descriptor or Easy Connect"
									rows={3}
									placeholder="(DESCRIPTION=(ADDRESS=(PROTOCOL=TCPS)(HOST=db.example.com)(PORT=2484))(CONNECT_DATA=(SERVICE_NAME=ORCL)))"
									bind:value={form.connectString}
								/>
							</div>
							<div class="lq-conn-form__wide">
								<OracleClientPicker value={form.configDir} onchange={(d) => form && (form.configDir = d)} />
							</div>
						{:else}
							{#if form.engine === "oracle" && fillOptions.length > 0}
								<div class="lq-conn-form__wide">
									<Combobox
										label="Fill from tnsnames.ora"
										placeholder="Optional: pick an alias to copy its host, port and service"
										searchPlaceholder="Search aliases"
										emptyText="No alias found"
										options={fillOptions}
										onchange={fillFrom}
									/>
								</div>
							{/if}
							{#if form.engine === "cassandra"}
								<TextField
									label="Contact points"
									placeholder="10.0.0.1,10.0.0.2"
									description="One or more hosts separated by commas; the driver discovers the rest of the cluster."
									bind:value={form.host}
									required
									autocomplete="off"
								/>
							{:else}
								<TextField label="Host" bind:value={form.host} required autocomplete="off" />
							{/if}
							<NumberField label="Port" bind:value={form.port} min={1} max={65535} />
							{#if form.engine === "sqlserver"}
								<TextField
									label="Instance"
									placeholder="Optional, e.g. SQLEXPRESS"
									description="Named instances are found through the SQL Server Browser service."
									bind:value={form.instance}
									autocomplete="off"
								/>
							{/if}
							<TextField
								label={form.engine === "oracle"
									? form.oracleConnectType === "sid"
										? "SID"
										: "Service name"
									: form.engine === "redis"
										? "Database index"
										: form.engine === "cassandra"
											? "Default keyspace"
											: "Database"}
								placeholder={form.engine === "oracle"
									? "FREEPDB1"
									: form.engine === "mysql" || form.engine === "mariadb" || form.engine === "cassandra"
										? "Optional"
										: form.engine === "redis"
											? "0"
										: form.engine === "sqlserver"
											? "master"
											: "postgres"}
								bind:value={form.database}
							/>
							{#if form.engine === "cassandra"}
								<TextField
									label="Local datacenter"
									placeholder="Optional, e.g. datacenter1"
									description="Requests go to this datacenter's nodes only. Leave empty to use every node."
									bind:value={form.datacenter}
									autocomplete="off"
								/>
							{/if}
						{/if}
						{#if form.engine !== "oracle"}
							<Select
								label={form.engine === "sqlserver" ? "Encryption" : form.engine === "redis" || form.engine === "cassandra" ? "TLS" : "SSL"}
								options={sslOptions}
								bind:value={form.sslMode}
							/>
						{/if}
					</FormSection>

					<FormSection title="Authentication">
						{#if form.engine === "sqlserver"}
							<div class="lq-conn-form__wide">
								<SegmentedControl
									aria-label="SQL Server authentication"
									size="sm"
									items={sqlServerAuthItems}
									value={form.windowsAuth ? "windows" : "sql"}
									onchange={(v) => form && (form.windowsAuth = v === "windows")}
								/>
							</div>
						{/if}
						{#if needsCredentials}
							<TextField
								label={form.engine === "redis" ? "User (ACL)" : "User"}
								placeholder={form.engine === "redis" ? "Optional, default user" : form.engine === "cassandra" ? "Optional, when authentication is enabled" : ""}
								bind:value={form.user}
								autocomplete="off"
							/>
							<TextField
								label="Password"
								type="password"
								autocomplete="new-password"
								placeholder={isNew ? "" : "Kept if left blank"}
								bind:value={password}
								oninput={() => (passwordTouched = true)}
							/>
						{:else}
							<p class="lq-conn-form__wide lq-conn-form__note">
								LowQuery signs in as the Windows user running it, like SQL Server Management Studio.
							</p>
						{/if}
						{#if form.engine === "oracle"}
							<Select label="Privilege" options={privilegeOptions} bind:value={form.oraclePrivilege} />
						{/if}
						{#if needsCredentials}
							<div class="lq-conn-form__wide">
								<Switch label="Save password in the system keychain" bind:checked={form.savePassword} />
							</div>
						{/if}
					</FormSection>
				{/if}

				<FormSection title="AI agents">
					<div class="lq-conn-form__wide">
						<SegmentedControl aria-label="AI access" size="sm" items={aiItems} bind:value={form.aiAccess} />
					</div>
					<p class="lq-conn-form__wide lq-conn-form__note">
						{#if form.aiAccess === "off"}
							Agents connected to LowQuery (Claude, Cursor, VS Code…) do not see this connection.
						{:else if form.aiAccess === "read"}
							Agents can read schemas and run queries. Their sessions are read-only and every call is rolled back.
						{:else}
							Agents can also change data and schema, as this database user. Every call is logged in AI activity.
						{/if}
						{#if form.aiAccess !== "off" && needsCredentials && !form.savePassword}
							Save the password so agents can connect.
						{/if}
						<button type="button" class="lq-conn-form__link" onclick={() => ai.show("setup")}>Set up AI clients</button>
					</p>
				</FormSection>

				{#if status}
					<Alert severity={status.ok ? "success" : "error"}>{status.text}</Alert>
				{/if}
				{#if error}
					<Alert severity="error">{error}</Alert>
				{/if}
				<button type="submit" hidden aria-hidden="true" tabindex="-1"></button>
			</form>
		{/if}

		{#snippet actions()}
			{#if step === "pick"}
				<Button variant="outlined" onclick={close}>Cancel</Button>
			{:else}
				<Button variant="ghost" onclick={test} loading={testing}>Test connection</Button>
				<span class="ca-toolbar-spacer"></span>
				<Button variant="outlined" onclick={close}>Cancel</Button>
				<Button variant="filled" onclick={save} loading={saving}>{isNew ? "Create connection" : "Save"}</Button>
			{/if}
		{/snippet}
	</Dialog>
{/if}

<style>
	.lq-pick {
		display: flex;
		flex-direction: column;
		gap: var(--ca-space-4);
	}
	.lq-pick__grid {
		display: grid;
		grid-template-columns: repeat(auto-fill, minmax(min(100%, 180px), 1fr));
		gap: var(--ca-space-3);
		margin: 0;
		padding: 0;
		list-style: none;
	}
	.lq-pick__card {
		display: grid;
		grid-template-columns: auto minmax(0, 1fr);
		grid-template-rows: auto auto;
		align-items: center;
		column-gap: var(--ca-space-3);
		width: 100%;
		height: 100%;
		padding: var(--ca-space-3) var(--ca-space-4);
		border: 1px solid var(--ca-outline-default);
		background: var(--ca-surface-raised);
		color: var(--ca-text-primary);
		text-align: left;
		cursor: pointer;
	}
	.lq-pick__card :global(.lq-logo) {
		grid-row: 1 / span 2;
	}
	.lq-pick__card:hover {
		border-color: var(--ca-brand-main);
		background: var(--ca-action-hover);
	}
	.lq-pick__card:focus-visible {
		outline: 2px solid var(--ca-focus-ring);
		outline-offset: 2px;
	}
	.lq-pick__name {
		font-size: 15px;
		font-weight: 650;
	}
	.lq-pick__driver {
		color: var(--ca-text-secondary);
		font-size: 12px;
		line-height: 1.35;
	}
	.lq-pick__empty {
		margin: 0;
		color: var(--ca-text-secondary);
	}
	.lq-conn-form {
		display: flex;
		flex-direction: column;
		gap: var(--ca-space-5);
	}
	/* Two columns even with the dialog scrollbar visible. */
	.lq-conn-form :global(.ca-form-section--grid .ca-form-section__fields) {
		grid-template-columns: repeat(auto-fill, minmax(min(100%, 200px), 1fr));
	}
	.lq-conn-form__engine {
		display: flex;
		align-items: center;
		gap: var(--ca-space-3);
	}
	.lq-conn-form__engine p {
		margin: 0;
	}
	.lq-conn-form__engine-name {
		font-size: 15px;
		font-weight: 650;
	}
	.lq-conn-form__engine-driver {
		color: var(--ca-text-secondary);
		font-size: 12px;
	}
	.lq-conn-form__wide {
		grid-column: 1 / -1;
	}
	.lq-conn-form__file {
		display: grid;
		grid-template-columns: minmax(0, 1fr) auto auto;
		align-items: end;
		gap: var(--ca-space-2);
	}
	.lq-conn-form__link {
		margin-left: var(--ca-space-1);
		padding: 0;
		border: 0;
		background: none;
		color: var(--ca-text-link, var(--ca-brand-main));
		font: inherit;
		text-decoration: underline;
		cursor: pointer;
	}
	.lq-conn-form__note {
		margin: 0;
		color: var(--ca-text-secondary);
		font-size: 13px;
	}
	.lq-conn-form__color {
		display: flex;
		flex-direction: column;
		gap: var(--ca-space-2);
	}
	.lq-conn-form__label {
		color: var(--ca-text-secondary);
		font-size: var(--ca-font-size-label);
		font-weight: 500;
	}
</style>
