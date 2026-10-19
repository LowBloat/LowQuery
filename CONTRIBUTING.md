# Contributing to LowQuery

LowQuery is an open-source SQL client built with Svelte, TypeScript, Tauri and Rust. Contributions can include bug fixes, database support, documentation, accessibility improvements and tests.

Keep repository documentation, issues and pull requests in English so contributors around the world can participate.

## Before you start

- Search [existing issues](https://github.com/LowBloat/LowQuery/issues) and [pull requests](https://github.com/LowBloat/LowQuery/pulls) for related work.
- For a larger feature or a new database engine, open an issue describing the problem and the proposed approach before implementing it. The [database support form](https://github.com/LowBloat/LowQuery/issues/new?template=database_support.yml) is the place to start for a new engine.
- Keep each pull request focused on one problem.
- Read [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md) for the code layout and the rules every driver follows. Participation follows the [code of conduct](CODE_OF_CONDUCT.md).

## Report a bug or suggest a feature

Use the [issue forms](https://github.com/LowBloat/LowQuery/issues/new/choose). Bug reports should include the LowQuery version, operating system, database engine and server version, the smallest SQL that reproduces the problem, and the expected and actual behavior. The server version is shown in the status bar after connecting.

Never include passwords, connection strings with credentials or real business data. Reproduce with a small synthetic table instead.

## Set up development

Install [Bun](https://bun.sh/), the stable [Rust toolchain](https://rustup.rs/) and the [Tauri prerequisites](https://v2.tauri.app/start/prerequisites/) for your operating system. No Oracle Client or Instant Client is needed.

Fork the repository, then clone your fork and create a branch:

```bash
git clone https://github.com/YOUR-USERNAME/LowQuery.git
cd LowQuery
git switch -c fix/short-description
bun install --frozen-lockfile
bun run tauri dev
```

To work on the interface without a database, run `bun run dev` and open http://localhost:1420. A development-only mock backend answers every command; add `?demo` to the URL to start with a sample query and result.

## Verify your changes

Run the checks relevant to your change:

```bash
bun run check
bun test
bun run build
cargo fmt --check --manifest-path src-tauri/Cargo.toml
cargo clippy --manifest-path src-tauri/Cargo.toml --lib --tests -- -D warnings
cargo test --locked --lib --manifest-path src-tauri/Cargo.toml
```

Driver changes need integration tests against a real server. Copy `.env.example` to `.env.test` (it is ignored by git), fill in a test database and run:

```bash
cd src-tauri
set -a; source ../.env.test; set +a
cargo test --lib -- --ignored --test-threads=1
```

CI runs the same tests against PostgreSQL 13 and 17 and Oracle Database Free in containers, so a pull request shows whether a change works on both engines. For a bug fix, add a regression test that fails without the fix.

## Open a pull request

Explain the problem, the resulting behavior and how you verified it, including the database versions you tested against. Link a related issue if one exists. Include screenshots for visible UI changes and disclose any checks you could not run.

Keep dependency updates and version changes separate unless they are needed for the fix. Follow the surrounding code style.

LowQuery uses the [AGPL-3.0-only license](LICENSE); contributions must be compatible with it. Be respectful and give specific, actionable feedback during review.
