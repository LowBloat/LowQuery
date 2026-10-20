<script lang="ts">
	import { AlertDialog } from "@codeatlasdev/ui";
	import { workspace } from "../state/workspace.svelte";

	const prompt = workspace.confirm;
</script>

{#if prompt.request}
	<AlertDialog
		open={true}
		title={prompt.request.title}
		description={prompt.request.message}
		confirmLabel={prompt.request.confirmLabel}
		cancelLabel="Cancel"
		destructive={prompt.request.destructive}
		onconfirm={() => prompt.answer(true)}
		oncancel={() => prompt.answer(false)}
	/>
{/if}
