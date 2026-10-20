# Architecture

LowQuery is a Tauri 2 desktop app. The interface is Svelte 5 with the [CodeAtlas UI](https://ui.codeatlas.com.br/) components; everything that touches a database runs in the Rust process.

```
┌──────────────── WebView (Svelte) ────────────────┐      ┌──────────── Rust (Tauri) ────────────┐
│ SqlEditor (CodeMirror)   split.ts (per dialect)  │      │ lib.rs        Tauri commands         │
│ QueryView → workspace.svelte.ts (tabs, sessions) │ IPC  │ profiles.rs   JSON + OS keychain     │
│ ResultGrid (virtualized)  explorer.svelte.ts     │ ───► │ drivers/mod.rs  Session enum         │
│ api.ts  (typed invoke wrappers)                  │      │   postgres.rs + pg_value.rs          │
└──────────────────────────────────────────────────┘      │   oracle.rs                          │
                                                          └──────────────────────────────────────┘
```

## Principles

1. **Each database is used the way it is meant to be used.** There is no generic ODBC/JDBC layer. Every engine gets its own driver module that speaks its native protocol, formats its own types and follows its own transaction and script conventions.
2. **Values cross the IPC boundary as text.** Drivers format each value exactly (NUMERIC and NUMBER keep every digit, dates keep their precision) and send `string | null` plus a column kind (`number`, `text`, `temporal`, ...) the grid uses for alignment. JavaScript numbers never touch database values.
3. **Row limits are applied by the server.** A `SELECT` on a large table never streams everything just to show 500 rows.
4. **Secrets stay in the native process.** Passwords go from the connection dialog to the OS credential store and are read back only by Rust.
5. **One session per tab.** Transactions never leak between tabs. The schema browser opens its own session per connection so it does not wait behind a long query.

## Rust side (`src-tauri/src`)

| File | Responsibility |
| --- | --- |
| `lib.rs` | Tauri commands, session registry, password resolution |
| `profiles.rs` | `connections.json` in the user config folder; passwords in the keychain (`keyring`) |
| `model.rs` | Serializable types shared with the interface |
| `error.rs` | `AppError` with kind, native code, position, detail and hint |
| `drivers/mod.rs` | `Session` enum that dispatches to each engine |
| `drivers/postgres.rs` | PostgreSQL session on `tokio-postgres` |
| `drivers/pg_value.rs` | Binary decoding of PostgreSQL types into server-style text |
| `drivers/oracle.rs` | Oracle session on the official `oracledb` thin driver |
| `drivers/mysql.rs` | MySQL and MariaDB session on `mysql_async` |
| `drivers/sqlserver.rs` | SQL Server and Azure SQL session on `tiberius` (TDS) |
| `drivers/redis_db.rs` | Redis and Valkey session on `redis-rs` (RESP3) |
| `drivers/sqlite.rs` | SQLite session on `rusqlite` (bundled engine) |
| `mcp/` | `lowquery mcp`: MCP server over stdio for AI agents (`server.rs` protocol and sessions, `tools.rs`, `guard.rs` statement checks, `catalog.rs` search/foreign keys/plans per engine, `audit.rs` activity log); see [AI.md](AI.md) |
| `ai_setup.rs` | The MCP command and the configuration of each AI client |
| `import/` | Reads connection files of DBeaver, DataGrip, pgAdmin 4 and SQL Developer (`jdbc.rs` parses JDBC URLs); passwords go from the file to the keychain without reaching the interface |
| `drivers/tns.rs` | Finds and parses `tnsnames.ora` (TNS_ADMIN, ORACLE_HOME, Windows registry, clients on PATH) |
| `drivers/integration_tests.rs` | Tests against real servers (`#[ignore]`, see CONTRIBUTING) |

### PostgreSQL

- Each statement is prepared first, which gives column types without executing it.
- Statements that return rows run inside a short transaction through a portal with `max_rows`, then the transaction commits. In manual-commit mode the portal runs inside the user's transaction.
- An error inside a manual transaction rolls it back explicitly, since PostgreSQL rejects every later statement in that transaction anyway.
- Utility commands that cannot run in a transaction block (`VACUUM`, `CREATE DATABASE`, `... CONCURRENTLY`) always run outside one.
- The connection task collects NOTICE/WARNING messages; cancel uses the server's cancel protocol over a separate connection.

### Oracle

- The `oracledb` crate is Oracle's pure-Rust thin driver, so no Oracle Client is installed. It is synchronous; every call runs in `spawn_blocking`.
- `prefetch_rows` and `fetch_array_size` are sized from the row limit, and reading stops at the limit.
- SQL statements lose their trailing `;`, PL/SQL keeps it, and a `/` on its own line ends a block, as in SQL*Plus.
- `DBMS_OUTPUT` is enabled when the session opens and read after each non-query statement.
- The driver cannot interrupt a running call yet; a script stops before the next statement instead.
- TNS aliases are resolved by `tns.rs`, not by the driver: it searches the same places an Oracle client does plus clients on `PATH` and the Windows registry, reads Latin-1 files, follows `IFILE` and applies `NAMES.DEFAULT_DOMAIN`. The driver receives the full descriptor and the folder, which it still uses for `sqlnet.ora`.

### MySQL and MariaDB

- Statements use the text protocol, so the server formats every value; binary-charset columns are shown as hex.
- Row-returning statements run with `sql_select_limit` set to the limit and restored afterwards.
- Transaction state comes from the server status flags of the last OK packet.
- Warnings are read with `SHOW WARNINGS`; cancel sends `KILL QUERY` from a short-lived second connection.

### SQL Server

- Each call is one batch (`GO` splits them in the interface), so `#temp` tables, `USE` and `SET` persist.
- Plain `SELECT`/`WITH` batches run with `SET ROWCOUNT`; others stop reading at the limit.
- Manual commit mode uses `SET IMPLICIT_TRANSACTIONS ON`, and `@@TRANCOUNT`/`XACT_STATE()` after each call drive the transaction indicator.
- Cancel sends a TDS attention. The driver does not expose `PRINT` messages yet.

### SQLite

- The connection lives behind a mutex and every call runs in `spawn_blocking`; cancel uses `sqlite3_interrupt`.
- Manual commit mode opens `BEGIN` before the first statement; the indicator follows `sqlite3_get_autocommit`.

### Redis

- Redis is not SQL: each editor line is one redis-cli command, split into arguments with redis-cli quoting rules and sent as is.
- Replies keep their RESP3 types and are laid out as tables: scalars as one value, maps as field/value, arrays of pairs as columns, `INFO` as section/key/value rows.
- The tree lists databases (`db0`...) and up to 1000 keys per database with `SCAN` and `TYPE`; ▶ runs the read command that fits the key type.
- There is no autocommit: `MULTI` opens a queue that the toolbar's EXEC and DISCARD close. Cancel only works for blocking commands, through `CLIENT UNBLOCK` from a second connection.

## Interface side (`src`)

| Path | Responsibility |
| --- | --- |
| `lib/engines/` | Engine registry (name, logo, default port, dialect) used by the picker and the sidebar |
| `theme.ts` | LowBloat theme for CodeAtlas: palette, zero radius, no shadows |
| `lib/api.ts` | Types and `invoke` wrappers for every command |
| `lib/sql/split.ts` | Script splitting per dialect: strings, quoted identifiers, nested comments, dollar quotes, `q'[...]'`, PL/SQL blocks |
| `lib/sql/ident.ts` | Identifier quoting and preview queries per engine |
| `lib/state/workspace.svelte.ts` | Tabs, sessions, execution, transactions, drafts, saving grid edits |
| `lib/state/edits.ts` | Pending grid edits per result and the changes sent to `apply_changes` |
| `lib/state/explorer.svelte.ts` | Schema browser and autocompletion namespace |
| `lib/state/ai.svelte.ts` | AI agents dialog: client setup and activity |
| `lib/state/importer.svelte.ts` | Import dialog: detected tool files, preview, duplicate check |
| `lib/state/tns.svelte.ts` | Detected `tnsnames.ora` sources and aliases for the connection and import dialogs |
| `lib/components/` | Editor, grid, results, sidebar, dialogs |
| `lib/dev-mock.ts` | Development-only backend for `bun run dev` in a browser |

Scripts are split in the interface so the editor knows where each statement starts. Each statement is a separate command call: results appear as they finish, and an error position maps back to the document.

## Editing results

A result is editable only when the driver can prove which table row each grid row is. Every driver answers that in `execute` by setting `ExecResult.editable` (table, the table column behind each result column or `null` for expressions, and the result columns that hold the key) or `readOnlyReason`. The key is the primary key, otherwise a unique key whose columns are all NOT NULL. Detection never fails a query and costs at most one extra round trip, usually cached per table on the session.

| Database | Where the source table comes from | Why a result is read-only |
| --- | --- | --- |
| PostgreSQL | `table_oid`/`column_id` of the prepared statement; key and types from `pg_index`/`format_type` | Several tables, any join (a self-join reports one OID), UNION, views |
| Oracle | The SQL is read by a small parser that accepts `SELECT columns FROM table [alias] [WHERE/ORDER BY/FETCH]` only; the names are checked against the result metadata and resolved in the dictionary like Oracle does (schema, then public synonym) | Joins, subqueries, WITH, GROUP BY, set operators, views, database links |
| MySQL, MariaDB | `org_table`/`org_name`/`schema` of each column plus the alias; key from `information_schema` | Several tables or aliases, set operators, views, temporary tables |
| SQL Server | `sys.dm_exec_describe_first_result_set` in browse mode, with the keys from `sys.indexes` in the same batch | Several tables or the same table twice, computed and rowversion columns, temp tables |
| SQLite | Column metadata (`sqlite3_column_table_name`) checked against `EXPLAIN QUERY PLAN`; `rowid` works as the key of a table without one | Compound selects, more than one table scan, results without the key |

`apply_changes` receives the target and a list of updates (old key plus new values), deletes and inserts. Each driver builds one statement per change with quoted, schema-qualified names and binds every value, converting the text the grid shows back into the column type the engine's own way (typed binds or explicit conversions that do not depend on session date or number formats). Every UPDATE and DELETE must touch exactly one row. With autocommit on the whole save is one transaction; inside an open transaction it runs behind a savepoint, so a failed save is undone without losing earlier work. After saving, the interface runs the query again to show what the database now holds.

## Adding a database engine

1. Open a [database support issue](https://github.com/LowBloat/LowQuery/issues/new?template=database_support.yml) describing the driver and the engine conventions to follow.
2. Add a module in `src-tauri/src/drivers/` that implements connect, execute (with a server-side row limit), transactions, cancel when the protocol allows it, the metadata queries, and result editing (or a read-only reason for every result); add a variant to `Session`.
3. Add the engine to `Engine` in `model.rs` and `api.ts`, its connection fields to the dialog, its dialect to `split.ts` and the editor, and its quoting rules to `ident.ts`.
4. Add integration tests and a CI job with the engine in a container.
