<script lang="ts">
	import {
		Dialog,
		Button,
		CopyButton,
		Alert,
		Chip,
		Spinner,
		Tabs,
		TabsList,
		TabsTrigger,
		TabsContent,
		EmptyState,
	} from "@codeatlasdev/ui";
	import { Bot, CircleAlert, CircleCheck, Plug, RefreshCw, Trash2 } from "@lucide/svelte";
	import EngineLogo from "./EngineLogo.svelte";
	import { ai } from "../state/ai.svelte";
	import { workspace } from "../state/workspace.svelte";
	import type { AiClient } from "../api";

	const shared = $derived(workspace.profiles.filter((p) => p.aiAccess && p.aiAccess !== "off"));
	const commandLine = $derived(ai.setup ? `${quote(ai.setup.command)} mcp` : "");

	function quote(s: string) {
		return /^[\w/\\:.-]+$/.test(s) ? s : `"${s}"`;
	}

	function status(c: AiClient): { label: string; variant: "success" | "default" | "info" } {
		if (c.configured) return { label: "Connected", variant: "success" };
		if (c.installed) return { label: "Detected", variant: "info" };
		return { label: "Not detected", variant: "default" };
	}

	const time = (iso: string) => {
		const d = new Date(iso);
		return Number.isNaN(d.getTime()) ? iso : d.toLocaleString(undefined, { hour12: false });
	};

	let expanded = $state<string | null>(null);
</script>

{#if ai.open}
	<Dialog
		open={true}
		size="lg"
		title="AI agents"
		description="Claude, Cursor, VS Code and other MCP clients can use the connections you open to them, with the access you choose for each one."
		onOpenChange={(o) => {
			if (!o) ai.close();
		}}
	>
		<Tabs bind:value={ai.tab} variant="line">
			<TabsList>
				<TabsTrigger value="setup">
					{#snippet icon()}<Plug size={14} />{/snippet}
					Setup
				</TabsTrigger>
				<TabsTrigger value="activity">
					{#snippet icon()}<Bot size={14} />{/snippet}
					Activity{ai.activity.length ? ` (${ai.activity.length})` : ""}
				</TabsTrigger>
			</TabsList>

			<TabsContent value="setup">
				<div class="lq-ai__panel">
				{#if shared.length === 0}
					<Alert severity="info">
						No connection is open to agents yet. Edit a connection and choose <strong>Read only</strong> under AI agents.
					</Alert>
				{:else}
					<ul class="lq-ai__shared" aria-label="Connections open to agents">
						{#each shared as p (p.id)}
							<li>
								<EngineLogo engine={p.engine} size={16} />
								<span class="lq-ai__shared-name">{p.name}</span>
								<Chip
									size="sm"
									label={p.aiAccess === "write" ? "Read and write" : "Read only"}
									variant={p.aiAccess === "write" ? "warning" : "default"}
								/>
								<Button size="sm" variant="ghost" onclick={() => {
										ai.close();
										workspace.editing = $state.snapshot(p);
									}}>Edit</Button>
							</li>
						{/each}
					</ul>
				{/if}

				{#if !ai.setup}
					<div class="lq-ai__center"><Spinner size={20} label="Loading" /></div>
				{:else}
					<h3 class="lq-ai__h">Clients</h3>
					<ul class="lq-ai__clients">
						{#each ai.setup.clients as c (c.id)}
							{@const st = status(c)}
							<li class="lq-ai__client">
								<div class="lq-ai__client-row">
									<span class="lq-ai__client-name">{c.name}</span>
									<Chip size="sm" label={st.label} variant={st.variant} />
									<span class="ca-toolbar-spacer"></span>
									{#if c.configPath}
										<Button
											size="sm"
											variant={c.configured ? "ghost" : c.installed ? "filled" : "outlined"}
											loading={ai.connecting === c.id}
											onclick={() => ai.connect(c.id, c.name)}
										>
											{c.configured ? "Update" : `Add to ${c.name}`}
										</Button>
									{/if}
									<Button size="sm" variant="ghost" onclick={() => (expanded = expanded === c.id ? null : c.id)}>
										{expanded === c.id ? "Hide" : c.configPath ? "Show config" : "How to"}
									</Button>
								</div>
								{#if expanded === c.id || (!c.configPath && c.installed)}
									<div class="lq-ai__snippet">
										<p>
											{#if c.snippetKind === "shell"}
												Run in a terminal:
											{:else if c.snippetKind === "toml"}
												Add to <code>~/.codex/config.toml</code>:
											{:else}
												Add to <code>{c.configPath}</code>:
											{/if}
										</p>
										<pre>{c.snippet}</pre>
										<CopyButton value={c.snippet} label="Copy" size="sm" variant="outlined" toast="Copied" />
									</div>
								{/if}
							</li>
						{/each}
					</ul>
					<h3 class="lq-ai__h">Any other MCP client</h3>
					<div class="lq-ai__snippet">
						<p>Start this command with the stdio transport:</p>
						<pre>{commandLine}</pre>
						<CopyButton value={commandLine} label="Copy" size="sm" variant="outlined" toast="Copied" />
					</div>
					<p class="lq-ai__fine">
						Agents use the passwords saved in the system keychain and the database user of each connection.
						For production, give agents a database user with only the rights they need.
					</p>
				{/if}
				</div>
			</TabsContent>

			<TabsContent value="activity">
				<div class="lq-ai__panel">
				<div class="lq-ai__bar">
					<span>Every tool call made by an agent, newest first.</span>
					<span class="ca-toolbar-spacer"></span>
					<Button size="sm" variant="ghost" onclick={() => ai.loadActivity()}>
						{#snippet leading()}<RefreshCw size={14} />{/snippet}
						Refresh
					</Button>
					<Button size="sm" variant="ghost" disabled={ai.activity.length === 0} onclick={() => ai.clearActivity()}>
						{#snippet leading()}<Trash2 size={14} />{/snippet}
						Clear
					</Button>
				</div>
				{#if ai.activity.length === 0}
					<EmptyState icon={Bot} title="No activity yet" description="Calls from AI agents appear here as they happen." />
				{:else}
					<ol class="lq-ai__log">
						{#each ai.activity as a, i (i)}
							<li class="lq-ai__entry" class:lq-ai__entry--error={!a.ok}>
								<span class="lq-ai__icon" aria-hidden="true">
									{#if a.ok}<CircleCheck size={14} />{:else}<CircleAlert size={14} />{/if}
								</span>
								<div class="lq-ai__body">
									<p class="lq-ai__head">
										<strong>{a.tool}</strong>
										{#if a.connectionName}<span>· {a.connectionName}</span>{/if}
										<span class="lq-ai__meta">
											{time(a.at)} · {a.client || "unknown client"}{a.rows != null ? ` · ${a.rows.toLocaleString()} ${a.rows === 1 ? "row" : "rows"}` : ""} · {a.elapsedMs} ms
										</span>
									</p>
									{#if a.detail}<code class="lq-ai__detail">{a.detail}</code>{/if}
									{#if a.error}<p class="lq-ai__error">{a.error}</p>{/if}
								</div>
							</li>
						{/each}
					</ol>
				{/if}
				</div>
			</TabsContent>
		</Tabs>
		{#snippet actions()}
			<Button variant="filled" onclick={() => ai.close()}>Done</Button>
		{/snippet}
	</Dialog>
{/if}

<style>
	.lq-ai__panel {
		display: flex;
		flex-direction: column;
		gap: var(--ca-space-3);
		max-height: min(62vh, 560px);
		padding: var(--ca-space-3) 0 0;
		overflow: auto;
	}
	.lq-ai__center {
		display: grid;
		place-items: center;
		padding: var(--ca-space-4);
	}
	.lq-ai__h {
		margin: var(--ca-space-2) 0 0;
		font-size: 13px;
		font-weight: 650;
	}
	.lq-ai__shared,
	.lq-ai__clients,
	.lq-ai__log {
		margin: 0;
		padding: 0;
		border: 1px solid var(--ca-outline-subtle);
		list-style: none;
	}
	.lq-ai__shared li {
		display: flex;
		align-items: center;
		gap: var(--ca-space-2);
		padding: 4px var(--ca-space-3);
		border-bottom: 1px solid var(--ca-outline-subtle);
		font-size: 13px;
	}
	.lq-ai__shared li:last-child,
	.lq-ai__client:last-child,
	.lq-ai__entry:last-child {
		border-bottom: 0;
	}
	.lq-ai__shared-name {
		flex: 1;
		min-width: 0;
		overflow: hidden;
		font-weight: 600;
		white-space: nowrap;
		text-overflow: ellipsis;
	}
	.lq-ai__client {
		border-bottom: 1px solid var(--ca-outline-subtle);
	}
	.lq-ai__client-row {
		display: flex;
		flex-wrap: wrap;
		align-items: center;
		gap: var(--ca-space-2);
		padding: 6px var(--ca-space-3);
	}
	.lq-ai__client-name {
		font-size: 13px;
		font-weight: 600;
	}
	.lq-ai__snippet {
		display: flex;
		flex-direction: column;
		align-items: flex-start;
		gap: var(--ca-space-2);
		padding: 0 var(--ca-space-3) var(--ca-space-3);
	}
	.lq-ai__snippet p {
		margin: 0;
		color: var(--ca-text-secondary);
		font-size: 12px;
	}
	.lq-ai__snippet code {
		font-family: var(--ca-font-mono);
		overflow-wrap: anywhere;
	}
	.lq-ai__snippet pre {
		width: 100%;
		margin: 0;
		padding: var(--ca-space-2) var(--ca-space-3);
		overflow: auto;
		background: var(--ca-surface-sunken);
		border: 1px solid var(--ca-outline-subtle);
		font-family: var(--ca-font-mono);
		font-size: 12px;
		white-space: pre;
	}
	.lq-ai__fine {
		margin: 0;
		color: var(--ca-text-hint);
		font-size: 12px;
	}
	.lq-ai__bar {
		display: flex;
		flex-wrap: wrap;
		align-items: center;
		gap: var(--ca-space-2);
		color: var(--ca-text-secondary);
		font-size: 13px;
	}
	.lq-ai__entry {
		display: grid;
		grid-template-columns: 18px minmax(0, 1fr);
		gap: var(--ca-space-2);
		padding: 6px var(--ca-space-3);
		border-bottom: 1px solid var(--ca-outline-subtle);
		font-size: 13px;
	}
	.lq-ai__icon {
		padding-top: 2px;
		color: var(--ca-semantic-success);
	}
	.lq-ai__entry--error .lq-ai__icon,
	.lq-ai__error {
		color: var(--ca-semantic-error);
	}
	.lq-ai__body {
		min-width: 0;
	}
	.lq-ai__head {
		display: flex;
		flex-wrap: wrap;
		gap: 0 var(--ca-space-2);
		margin: 0;
		line-height: 20px;
	}
	.lq-ai__meta {
		color: var(--ca-text-hint);
		font-size: 12px;
	}
	.lq-ai__detail {
		display: -webkit-box;
		overflow: hidden;
		color: var(--ca-text-secondary);
		font-family: var(--ca-font-mono);
		font-size: 12px;
		white-space: pre-wrap;
		word-break: break-word;
		-webkit-line-clamp: 3;
		line-clamp: 3;
		-webkit-box-orient: vertical;
	}
	.lq-ai__error {
		margin: 2px 0 0;
		font-size: 12px;
		white-space: pre-wrap;
	}
</style>
