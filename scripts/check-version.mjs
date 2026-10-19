// Usage: bun scripts/check-version.mjs [vX.Y.Z]
// Fails when the version-bearing files disagree, or differ from the given tag.
import { appendFileSync, readFileSync } from "node:fs";

const read = (path) => readFileSync(new URL(`../${path}`, import.meta.url), "utf8");

const versions = [
	{ path: "package.json", version: JSON.parse(read("package.json")).version },
	{ path: "src-tauri/tauri.conf.json", version: JSON.parse(read("src-tauri/tauri.conf.json")).version },
	{
		path: "src-tauri/Cargo.toml",
		version: /^\[package\][\s\S]*?^version\s*=\s*"([^"]+)"/m.exec(read("src-tauri/Cargo.toml"))?.[1],
	},
];

for (const { path, version } of versions) console.log(`${String(version).padEnd(10)} ${path}`);

const distinct = [...new Set(versions.map((v) => v.version))];
if (distinct.length !== 1 || !distinct[0]) {
	console.error("Version files disagree. Update package.json, src-tauri/tauri.conf.json and src-tauri/Cargo.toml together.");
	process.exit(1);
}

const tag = process.argv[2];
if (tag && tag !== `v${distinct[0]}`) {
	console.error(`Tag ${tag} does not match version ${distinct[0]}.`);
	process.exit(1);
}

if (process.env.GITHUB_OUTPUT) appendFileSync(process.env.GITHUB_OUTPUT, `version=${distinct[0]}\n`);
