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
	import OracleClientPicker from "./OracleClientPicker.svelte";
	import { tns, describeEntry } from "../state/tns.svelte";
	import {
		api,
		asAppError,
		errorText,
		DEFAULT_PORTS,
		type ConnectionProfile,
		type Engine,
	} from "../api";
	import { workspace } from "../state/workspace.svelte";
	import { explorer } from "../state/explorer.svelte";

	let form = $state<ConnectionProfile | null>(null);
	let password = $state("");
	let passwordTouched = $state(false);
	let testing = $state(false);
	let saving = $state(false);
	let status = $state<{ ok: boolean; text: string } | null>(null);
	let error = $state<string | null>(null);

	// Copy the profile being edited when the dialog opens.
	$effect(() => {
		const p = workspace.editing;
		form = p ? structuredClone($state.snapshot(p)) : null;
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
	const open = $derived(form !== null);

	const engineItems = [
		{ value: "postgres", label: "PostgreSQL" },
		{ value: "oracle", label: "Oracle" },
	];
	const oracleTypeItems = [
		{ value: "service", label: "Service name" },
		{ value: "sid", label: "SID" },
		{ value: "tns", label: "TNS alias" },
		{ value: "custom", label: "Descriptor" },
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
	const sslOptions = [
		{ value: "disable", label: "Disabled" },
		{ value: "prefer", label: "Prefer (default)" },
		{ value: "require", label: "Require, skip certificate check" },
		{ value: "verify-full", label: "Require and verify certificate" },
	];
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
	// Identification colors; red for production, by convention.
	const colorItems = [
		{ value: "", label: "No color", color: "transparent" },
		{ value: "#b8f238", label: "Green", color: "#b8f238" },
		{ value: "#3b82f6", label: "Blue", color: "#3b82f6" },
		{ value: "#f59e0b", label: "Amber", color: "#f59e0b" },
		{ value: "#a855f7", label: "Violet", color: "#a855f7" },
		{ value: "#dc2626", label: "Red (production)", color: "#dc2626" },
	];

	function setEngine(v: string) {
		if (!form) return;
		const engine = v as Engine;
		if (form.engine === engine) return;
		if (form.port === DEFAULT_PORTS[form.engine]) form.port = DEFAULT_PORTS[engine];
		if (engine === "oracle" && form.database === "postgres") form.database = "FREEPDB1";
		if (engine === "postgres" && !form.database) form.database = "postgres";
		form.engine = engine;
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
			const info = await api.testConnection($state.snapshot(form), pw());
			status = { ok: true, text: `Connection succeeded · ${info.serverVersion}` };
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
				snapshot.name = `${snapshot.user || "user"}@${snapshot.host || "server"}`;
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

</script>

{#if form}
	<Dialog
		{open}
		size="lg"
		title={isNew ? "New connection" : `Edit ${form.name}`}
		description="Passwords are stored in the operating system keychain, never in a file."
		onOpenChange={(o) => {
			if (!o) close();
		}}
	>
		<form
			class="lq-conn-form"
			onsubmit={(e) => {
				e.preventDefault();
				save();
			}}
		>
			<div class="lq-conn-form__engine">
				<SegmentedControl
					aria-label="Database engine"
					items={engineItems}
					value={form.engine}
					onchange={setEngine}
				/>
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
					<TextField label="Host" bind:value={form.host} required autocomplete="off" />
					<NumberField label="Port" bind:value={form.port} min={1} max={65535} />
					<TextField
						label={form.engine === "postgres" ? "Database" : form.oracleConnectType === "sid" ? "SID" : "Service name"}
						placeholder={form.engine === "postgres" ? "postgres" : "FREEPDB1"}
						bind:value={form.database}
					/>
				{/if}
				{#if form.engine === "postgres"}
					<Select label="SSL" options={sslOptions} bind:value={form.sslMode} />
				{/if}
			</FormSection>

			<FormSection title="Authentication">
				<TextField label="User" bind:value={form.user} autocomplete="off" />
				<TextField
					label="Password"
					type="password"
					autocomplete="new-password"
					placeholder={isNew ? "" : "Kept if left blank"}
					bind:value={password}
					oninput={() => (passwordTouched = true)}
				/>
				{#if form.engine === "oracle"}
					<Select label="Privilege" options={privilegeOptions} bind:value={form.oraclePrivilege} />
				{/if}
				<div class="lq-conn-form__wide">
					<Switch label="Save password in the system keychain" bind:checked={form.savePassword} />
				</div>
			</FormSection>

			{#if status}
				<Alert severity={status.ok ? "success" : "error"}>{status.text}</Alert>
			{/if}
			{#if error}
				<Alert severity="error">{error}</Alert>
			{/if}
			<button type="submit" hidden aria-hidden="true" tabindex="-1"></button>
		</form>

		{#snippet actions()}
			<Button variant="ghost" onclick={test} loading={testing}>Test connection</Button>
			<span class="ca-toolbar-spacer"></span>
			<Button variant="outlined" onclick={close}>Cancel</Button>
			<Button variant="filled" onclick={save} loading={saving}>{isNew ? "Create connection" : "Save"}</Button>
		{/snippet}
	</Dialog>
{/if}

<style>
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
	}
	.lq-conn-form__wide {
		grid-column: 1 / -1;
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
