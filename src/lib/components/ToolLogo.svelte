<script lang="ts">
	import { FileText } from "@lucide/svelte";
	import type { ImportTool } from "../api";
	import { POSTGRESQL_PATH } from "../engines/logos";
	import { DATAGRIP_PATH, DBEAVER_PATH } from "../importers/logos";

	interface Props {
		/** A tool, or "tnsnames" for Oracle's tnsnames.ora. */
		tool: ImportTool | "tnsnames";
		size?: number;
	}

	let { tool, size = 22 }: Props = $props();

	// pgAdmin's mark is PostgreSQL's elephant; SQL Developer uses Oracle's ring.
	const marks: Record<ImportTool, { path?: string; color: string }> = {
		dbeaver: { path: DBEAVER_PATH, color: "#382923" },
		datagrip: { path: DATAGRIP_PATH, color: "#000000" },
		pgadmin: { path: POSTGRESQL_PATH, color: "#4169E1" },
		"sql-developer": { color: "#C74634" },
	};
</script>

{#if tool === "tnsnames"}
	<span class="lq-tool lq-tool--file" style="--size:{size}px"><FileText size={size - 2} aria-hidden="true" /></span>
{:else}
	{@const mark = marks[tool]}
	<svg class="lq-tool" width={size} height={size} viewBox="0 0 24 24" aria-hidden="true" style="--logo-color:{mark.color}">
		{#if mark.path}
			<path d={mark.path} />
		{:else}
			<rect x="1.8" y="6.6" width="20.4" height="10.8" rx="5.4" fill="none" stroke-width="2.8" />
		{/if}
	</svg>
{/if}

<style>
	.lq-tool {
		flex-shrink: 0;
		fill: var(--logo-color);
		stroke: var(--logo-color);
	}
	.lq-tool path {
		stroke: none;
	}
	.lq-tool--file {
		display: inline-grid;
		place-items: center;
		width: var(--size);
		height: var(--size);
		color: var(--ca-text-secondary);
		fill: none;
		stroke: none;
	}
	/* Dark marks (DBeaver, DataGrip) get lighter in the dark theme. */
	:global(.dark) .lq-tool:not(.lq-tool--file) {
		fill: color-mix(in oklab, var(--logo-color) 45%, white);
		stroke: color-mix(in oklab, var(--logo-color) 45%, white);
	}
</style>
