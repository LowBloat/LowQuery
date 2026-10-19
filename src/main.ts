import "@fontsource-variable/inter";
import "@codeatlasdev/ui/styles/core.css";
import "./app.css";
import { colorScheme } from "@codeatlasdev/ui";
import { mount } from "svelte";
import App from "./App.svelte";

colorScheme.init();

// Outside the app (a plain browser during development), use a mock backend
// so the UI can be viewed and tested. `?demo` preloads a sample workspace.
if (import.meta.env.DEV && !("__TAURI_INTERNALS__" in window)) {
	const { installDevMock, setupDemo } = await import("./lib/dev-mock");
	installDevMock();
	if (new URLSearchParams(location.search).has("demo")) setupDemo();
}

mount(App, { target: document.getElementById("app")! });
