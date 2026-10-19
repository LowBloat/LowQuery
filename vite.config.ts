import { svelte } from "@sveltejs/vite-plugin-svelte";
import { caThemePlugin } from "@codeatlasdev/ui/vite";
import { defineConfig } from "vite";
import { theme } from "./src/theme.ts";

const host = process.env.TAURI_DEV_HOST;

// https://v2.tauri.app/start/frontend/vite/
export default defineConfig({
	plugins: [caThemePlugin(theme), svelte()],
	// Mantém os erros do Rust visíveis no terminal do `tauri dev`.
	clearScreen: false,
	server: {
		port: 1420,
		strictPort: true,
		host: host || "127.0.0.1",
		hmr: host ? { protocol: "ws", host, port: 1421 } : undefined,
		watch: { ignored: ["**/src-tauri/**"] },
	},
	envPrefix: ["VITE_", "TAURI_ENV_*"],
	build: {
		// WebView2 (Windows) é Chromium; WebKit no macOS/Linux.
		target: process.env.TAURI_ENV_PLATFORM === "windows" ? "chrome111" : "safari15",
		minify: !process.env.TAURI_ENV_DEBUG,
		sourcemap: !!process.env.TAURI_ENV_DEBUG,
	},
});
