<script lang="ts">
	import { Dialog, Button, TextField } from "@codeatlasdev/ui";
	import { workspace } from "../state/workspace.svelte";

	const prompt = workspace.password;
	let value = $state("");
	let input = $state<HTMLInputElement | null>(null);

	$effect(() => {
		if (prompt.profile) {
			value = "";
			queueMicrotask(() => input?.focus());
		}
	});

	function submit() {
		prompt.answer(value);
	}
</script>

{#if prompt.profile}
	<Dialog
		open={true}
		size="sm"
		title="Password for {prompt.profile.name}"
		description="{prompt.profile.user}@{prompt.profile.host || 'server'}"
		onOpenChange={(o) => {
			if (!o) prompt.answer(null);
		}}
	>
		<form
			onsubmit={(e) => {
				e.preventDefault();
				submit();
			}}
		>
			<TextField
				label="Password"
				type="password"
				autocomplete="current-password"
				description={prompt.message}
				bind:value
				bind:ref={input}
			/>
		</form>
		{#snippet actions()}
			<Button variant="outlined" onclick={() => prompt.answer(null)}>Cancel</Button>
			<Button variant="filled" onclick={submit}>Connect</Button>
		{/snippet}
	</Dialog>
{/if}
