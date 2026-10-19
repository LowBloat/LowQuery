<script lang="ts">
	import { engineInfo, type Engine } from "../engines";

	interface Props {
		engine: Engine;
		size?: number;
	}

	let { engine, size = 20 }: Props = $props();
	const info = $derived(engineInfo(engine));
</script>

<svg
	class="lq-logo"
	width={size}
	height={size}
	viewBox="0 0 24 24"
	role="img"
	aria-label={info.label}
	style="--logo-color:{info.color}"
>
	{#if "path" in info.logo}
		<path d={info.logo.path} />
	{:else if info.logo.glyph === "oracle"}
		<rect x="1.8" y="6.6" width="20.4" height="10.8" rx="5.4" fill="none" stroke-width="2.8" />
	{:else}
		<!-- Generic database cylinder in the SQL Server red. -->
		<ellipse cx="12" cy="5.5" rx="8" ry="3" />
		<path d="M4 7.6v4.9c0 1.66 3.58 3 8 3s8-1.34 8-3V7.6c-1.7 1.3-4.7 2-8 2s-6.3-.7-8-2Z" />
		<path d="M4 14.6v3.9c0 1.66 3.58 3 8 3s8-1.34 8-3v-3.9c-1.7 1.3-4.7 2-8 2s-6.3-.7-8-2Z" />
	{/if}
</svg>

<style>
	.lq-logo {
		flex-shrink: 0;
		fill: var(--logo-color);
		stroke: var(--logo-color);
	}
	.lq-logo path,
	.lq-logo ellipse {
		stroke: none;
	}
	/* Dark brand colors (MariaDB, SQLite) get lighter in the dark theme. */
	:global(.dark) .lq-logo {
		fill: color-mix(in oklab, var(--logo-color) 55%, white);
		stroke: color-mix(in oklab, var(--logo-color) 55%, white);
	}
</style>
