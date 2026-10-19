import type { ThemeOptions } from "@codeatlasdev/ui/vite";

/**
 * LowBloat identity: light paper, dark ink, lime-green signal, zero radius,
 * no shadows or blur. Same palette as LowNotes.
 */
export const theme: ThemeOptions = {
	presets: [
		{
			id: "lowbloat",
			color: "#b8f238",
			label: "LowBloat",
			colors: {
				light: {
					brand: "#4d7100",
					background: "#f1f3ee",
					sunken: "#e8ebe3",
					surface: "#fbfcf8",
					overlay: "#fbfcf8",
					text: "#171a17",
					textSecondary: "#4f584f",
					textHint: "#606b60",
					borderSubtle: "#d4d9d0",
					border: "#a9b1a5",
					borderStrong: "#4f584f",
					success: "#247a48",
					error: "#b42318",
				},
				dark: {
					brand: "#b8f238",
					background: "#171a17",
					sunken: "#111510",
					surface: "#1d221c",
					overlay: "#222821",
					text: "#fbfcf8",
					textSecondary: "#b8c0b0",
					textHint: "#a9b1a5",
					borderSubtle: "#2f362d",
					border: "#41473e",
					borderStrong: "#a9b1a5",
					success: "#7ac394",
					error: "#f28f84",
				},
			},
		},
	],
	defaultPreset: "lowbloat",
	defaultScheme: "system",
	defaultStyle: "flat",
	fonts: {
		body: '"Inter Variable", Inter, "Segoe UI", system-ui, sans-serif',
		mono: '"Cascadia Mono", "Cascadia Code", Consolas, "SF Mono", monospace',
	},
	radius: 0,
	density: "compact",
	elevation: "none",
	motion: { enter: "none" },
	overrides: {
		light: {
			"--ca-brand-fill": "#b8f238",
			"--ca-brand-fill-hover": "#a7e01f",
			"--ca-brand-contrast": "#1a2700",
			"--ca-brand-text": "#4d7100",
			"--ca-scrim-blur": "none",
		},
		dark: {
			"--ca-brand-fill": "#b8f238",
			"--ca-brand-fill-hover": "#cdfa6a",
			"--ca-brand-contrast": "#1a2700",
			"--ca-brand-text": "#d4fc8a",
			"--ca-scrim-blur": "none",
		},
	},
};
