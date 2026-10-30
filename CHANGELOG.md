# Changelog

All notable changes to LowQuery are documented here. The format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/) and the project uses [Semantic Versioning](https://semver.org/).

## [Unreleased]

### Changed
- Every Linux format now updates itself, not only the AppImage: `.deb` and `.rpm` installs update through `dpkg`/`rpm`, the Arch package (and the AUR's `lowquery-bin`) through `pacman`, after the system asks for the password, and portable copies on Linux and Windows replace their own executable. Each install receives the signed file of its own format; releases sign the `.deb`, `.rpm`, `.pkg.tar.zst` and portable archives and list them in `latest.json`. Copies installed before this version still show the download link once.

## [0.5.3] - 2026-10-24

### Added
- A title bar in the LowBloat style on Windows and Linux, with File, Query, View and Help menus and themed window buttons. macOS keeps its traffic lights and gets the same menus in the menu bar. Alt or F10 moves to the menus; Help → Check for updates reports when LowQuery is up to date ([#20](https://github.com/LowBloat/LowQuery/issues/20)).

### Fixed
- The window no longer flashes white or jumps in size on startup: it stays hidden until the interface has painted ([#20](https://github.com/LowBloat/LowQuery/issues/20)).

## [0.5.2] - 2026-10-23

### Added
- **Export…** on tables, views, other objects and schemas ([#10](https://github.com/LowBloat/LowQuery/issues/10), [#11](https://github.com/LowBloat/LowQuery/issues/11), [#13](https://github.com/LowBloat/LowQuery/issues/13)): pick SQL, CSV, CSV for Excel or JSON (and for SQL: structure and data, structure, or data) and where to save. Nothing else to set.
  - SQL scripts recreate the objects on the same kind of database: structure read from the database's catalog, tables in foreign key order, and INSERTs that keep every value exact (binary, dates with time zones, long text, JSON), skip generated columns and keep identity values (`OVERRIDING SYSTEM VALUE` and `setval`, `IDENTITY_INSERT`, `START WITH LIMIT VALUE`).
  - A schema exports every table, view, sequence and routine to one `.sql` file, or one CSV/JSON file per table into a folder.
  - Rows are streamed from the server to the file, so large tables do not fill the memory; progress shows the object and row count, and **Stop** cancels on the server and leaves no half file.
- **Import data…** on a table and **Import file as a new table…** on a schema ([#12](https://github.com/LowBloat/LowQuery/issues/12)): CSV (any delimiter) or JSON (an array or one object per line). The encoding (UTF-8, UTF-16, Windows-1252), delimiter and header row are detected, columns are matched by name, values like `1.234,56`, `31/12/2024` and `sim` are converted for the column, and everything is inserted in one transaction. The only options: delimiter, header row, and replacing the table's rows. A new table gets column types from the file.
- **Run SQL file…** on connections and schemas ([#14](https://github.com/LowBloat/LowQuery/issues/14)): runs a script (an export, a dump from another tool) statement by statement on its own connection, with progress, stopping at the first error unless unticked, and lists the statements that failed.

### Fixed
- Context menus (tree and grid) threw an error in the console while closing.
- Oracle: running a query that returns a CLOB or BLOB again in the same session could fail with ORA-01002 (fetch out of sequence) ([#17](https://github.com/LowBloat/LowQuery/issues/17)). It happened when the server parsed the query again, which on Oracle 23ai often came by the third run of a query such as `dbms_metadata.get_ddl`, and on every run after one that had failed, until reconnecting. Both causes were in the Oracle driver LowQuery ships and are fixed there; Show DDL no longer needs to bypass the statement cache.

## [0.5.1] - 2026-10-22

### Security
- `tokio` 1.54.1 fixes a race condition in `tokio::time::Interval` (RUSTSEC-2026-0384).

### Added
- Context menus in the connection tree ([#8](https://github.com/LowBloat/LowQuery/issues/8)): right-click a node, press the context-menu key, or use the ⋯ button that shows on hover.
  - **Connection:** new query, connect, refresh, disconnect; create a schema (a user in Oracle) or a database; server sessions (`pg_stat_activity`, `SHOW PROCESSLIST`, `v$session`, `sys.dm_exec_sessions`, `CLIENT LIST`); show a SQLite file in its folder; edit, duplicate, copy connection details and delete the profile.
  - **Schema or database:** new query working in it (`SET search_path`, `USE`, `ALTER SESSION SET CURRENT_SCHEMA`, `SELECT n`), refresh, create table, view, function, procedure and sequence templates, rename (PostgreSQL), copy name, and drop (type the name to confirm; CASCADE where the engine has it), flush a Redis database.
  - **Table and other objects:** select the first rows, count rows, show DDL, generate SELECT, INSERT, UPDATE, DELETE and CREATE INDEX, edit the table, rename, duplicate with or without rows, empty, refresh a materialized view, insert or copy the name, qualified name or column list, and drop tables, views, materialized views, sequences, functions, procedures, packages, types and synonyms.
  - **Redis keys:** show the value, time to live, rename, set or remove the expiry, delete.
- **Show DDL** opens the statements that create the object in a new tab, read from the database's own catalog: `DBMS_METADATA` and the stored PL/SQL source in Oracle, `SHOW CREATE` in MySQL and MariaDB, `pg_get_*def` and the catalog in PostgreSQL, `OBJECT_DEFINITION` and `sys.*` views in SQL Server, `sqlite_schema` in SQLite. Tables come with their keys, indexes, triggers and comments.
- Table designer for new and existing tables: columns with type suggestions per engine, NOT NULL, default, primary key and identity (`GENERATED BY DEFAULT AS IDENTITY`, `AUTO_INCREMENT`, `IDENTITY(1,1)`, `INTEGER PRIMARY KEY AUTOINCREMENT`). Changes to an existing table become the `ALTER TABLE` statements each engine needs (renames, type, NULL and default changes, added and dropped columns, primary key), and what an engine cannot do (SQLite type changes, for example) is explained instead of attempted.
- Every change shows its SQL before it runs, with **Open in editor** to review or adapt it in a new tab. Statements run on the tree's own session, so a transaction open in a query tab is not affected.
- Columns in the tree report their default value.

### Changed
- The connection's ⋯ button opens the same menu as a right-click.

## [0.5.0] - 2026-10-22

### Security
- TLS: `rustls-webpki` 0.103.16 and `webpki-roots` 1.0.10 no longer trust distrusted intermediates and emSign roots (RUSTSEC-2026-0381, RUSTSEC-2026-0382).
- The `zip` advisories RUSTSEC-2026-0367/0368/0369 are acknowledged in `deny.toml`: `zip` is used only by the updater, which extracts Windows update packages after checking their signature against LowQuery's update key.

### Added
- Result grid filters ([#4](https://github.com/LowBloat/LowQuery/issues/4)):
  - A filter button in each column header (shown on hover, highlighted while the column is filtered): *contains* (any case), *equals*, *not equal to* (keeps NULLs), *is NULL*, *is not NULL*.
  - Right-click on a column header: sort ascending or descending, clear sort, filter, show only NULL, hide NULL, clear the column's filter or all filters, copy the column name, fit the width to the values.
  - Right-click on a cell: **Filter by this value** and **Exclude this value**.
  - Active filters show as chips above the grid (click to change, × to remove, **Clear filters**), the row count shows how many rows match, and filters on several columns combine with AND.
  - Like sorting, a result that fit within the row limit is filtered in memory, and one that hit the limit runs again on the server with a `WHERE` written in each database's SQL, so matching rows beyond the limit are found.

### Fixed
- Result grid: sorting by a column of a result that hit the row limit only reordered the fetched rows, so the first rows of the whole result never showed up. Such a result now runs again on the server ordered by that column (the statement goes inside `SELECT * FROM (...) ORDER BY <position>`); a result that fit within the limit still sorts in memory. The result stays editable, unsaved edits ask first, an open PostgreSQL transaction is protected by a savepoint, and if the database rejects the sorted query the grid sorts the fetched rows and says why in Messages. Copy and CSV/JSON export follow the order shown. ([#3](https://github.com/LowBloat/LowQuery/issues/3))

## [0.4.0] - 2026-10-20

### Added
- MCP server for AI agents: `lowquery mcp` speaks the Model Context Protocol over stdio (revisions 2024-11-05 to 2025-11-25), without a window and without the app running. Tools: `list_connections`, `list_schemas`, `list_tables`, `describe_table` (columns, primary key and foreign keys both ways), `search_objects` (tables, views and columns by name across schemas), `query`, `explain` and `execute`, with structured output, row and size limits, and a 30-second time limit that cancels the statement on the server.
- **AI agents** access per connection: Off (default), Read only or Read and write. Read-only is enforced by a one-statement check, a read classifier, and a read-only session in the database itself (read-only transactions, `PRAGMA query_only` and a read-only file for SQLite, a read command allowlist for Redis), and every read call is rolled back.
- **AI agents** dialog in the sidebar: adds LowQuery to Claude Desktop, Cursor, VS Code, Windsurf and Gemini CLI in one click (keeping a backup of their configuration), shows the commands for Claude Code and Codex, and lists every call agents made in **Activity**, refused ones included.
- [docs/AI.md](docs/AI.md): setup, tools and the safety model.

## [0.3.2] - 2026-10-20

### Added
- **Import connections** in the sidebar (and on the empty sidebar) replaces the tnsnames.ora button. It finds the connection files of other tools on this computer, or opens one you choose, lists what can be imported with folders, colors and what was left out, marks connections you already have, and creates the profiles you select:
  - DBeaver (`data-sources*.json` of every project, Snap and Flatpak installs included), with user names and saved passwords decrypted from `credentials-config.json` and stored in the system keychain.
  - DataGrip and other JetBrains IDEs (global and project `dataSources.xml`, user names from `dataSources.local.xml`).
  - pgAdmin 4 (`pgadmin4.db`, read-only, or an exported `servers.json`).
  - Oracle SQL Developer (`connections.json` or `connections.xml`, every installed version).
  - Oracle `tnsnames.ora`, as before.
- PostgreSQL, Oracle (service, SID, TNS alias, descriptor, SYSDBA and other roles), MySQL, MariaDB, SQL Server (named instances, Windows authentication), SQLite and Redis connections are mapped, including TLS settings. Settings LowQuery does not support (SSH tunnels, client certificates, init SQL...) are listed per connection, and unsupported databases are shown with the reason.

## [0.3.1] - 2026-10-20

### Fixed
- Oracle: running a query again on wide tables with repeated values (ERP tables such as Protheus `SA1010`) failed with "internal error: unknown TTC message type" and left the connection unusable. LowQuery now ships the Oracle driver with that fixed in its parser.
- Oracle: if the driver ever loses its place in a server reply, the session reopens its connection instead of failing every later statement, and says so (and whether an open transaction was rolled back).

### Changed
- CI also runs the Oracle tests against Oracle XE 21c, which handles multi-packet replies like 19c does.

## [0.3.0] - 2026-10-19

### Added
- Edit query results in the grid: change cells, set NULL, add, duplicate and delete rows, then **Save** (Ctrl+S) or **Discard**. Pending changes are highlighted, new rows show `DEFAULT` for the columns left empty, and deleted rows are struck through. Saving writes all changes in one transaction with autocommit on, or inside the open transaction otherwise, and the result refreshes in place.
- A result is editable when it comes from one table and includes its primary key (or a unique key with NOT NULL columns). The table and key come from the database's own result metadata, and a **Read-only** chip explains why a result cannot be edited. Computed columns stay read-only.
- Running another query, switching connections or closing a tab with unsaved changes asks first.

### Changed
- The value viewer edits long and multi-line values when the result is editable.

### Fixed
- Oracle `TIMESTAMP WITH TIME ZONE` values showed the UTC time next to the original offset; they now show the local time of that offset.
- Oracle `RAW` columns appeared as `RAW(0)` in the sidebar.

## [0.2.1] - 2026-10-19

### Added
- Redis and Valkey: redis-cli style commands with quoting and escapes, RESP3 replies shown as tables (maps as field/value, sorted sets with scores, `INFO` as rows), databases and keys by type in the sidebar with a preview command per type, `MULTI`/`EXEC`/`DISCARD` from the toolbar, `CLIENT UNBLOCK` to stop blocking commands, ACL users and TLS.
- Native Arch Linux package (`.pkg.tar.zst`) and a `PKGBUILD` with real checksums in every release.

## [0.2.0] - 2026-10-19

### Added
- MySQL and MariaDB through `mysql_async`: server-side row limit with `sql_select_limit`, exact DECIMAL and unsigned values, warnings in the Messages tab, `KILL QUERY` cancel, TLS modes, and `DELIMITER` in scripts.
- SQL Server and Azure SQL through `tiberius` (TDS): SQL logins or the Windows account, named instances through SQL Server Browser, encryption modes, `GO` batches, `SET ROWCOUNT` limit for plain SELECT batches, attention cancel and implicit transactions for manual commit.
- SQLite through the bundled engine: open or create a database file, interrupt, extended error codes with positions and trigger bodies in scripts.
- The new connection dialog starts with a database picker showing each engine's logo, name and driver; connections in the sidebar show the engine logo.
- Per-engine identifier quoting and preview queries (`LIMIT`, `FETCH FIRST`, `TOP`), and editor dialects for every engine.

## [0.1.2] - 2026-10-19

### Added
- Automatic updates: installed copies on Windows (setup and MSI), macOS and AppImage check GitHub Releases, download the signed update, install it and restart. Linux packages and portable copies are told a new version exists and get a download link. **Check for updates** sits at the bottom of the sidebar.

### Fixed
- The SQL editor rendered as plain text in installed builds (line numbers and the placeholder appeared inside the text). Typing could then insert non-breaking spaces, which Oracle rejected with ORA-00911. Tauri's build-time CSP changes blocked the editor's runtime styles.

## [0.1.1] - 2026-10-19

### Added
- Oracle **TNS alias** connection type that reads the `tnsnames.ora` the machine already uses: `TNS_ADMIN`, `ORACLE_HOME/network/admin`, Oracle homes registered in the Windows registry, Oracle clients and Instant Clients on `PATH`, and `/etc` or `/var/opt/oracle` on Linux and macOS.
- Oracle client picker in the connection dialog listing the detected homes (such as `OraClient19Home1`) and a **Browse…** option for any folder, Oracle home or Instant Client.
- **Import from tnsnames.ora** in the sidebar: creates one connection per selected alias, with a shared user.
- **Fill from tnsnames.ora** in Service name and SID modes copies host, port and service from an alias.

### Changed
- A password typed at connect time is saved in the system keychain when the profile has **Save password** on.
- The custom Oracle mode is now **Descriptor** (descriptor or Easy Connect); a bare alias typed there is still resolved.

### Fixed
- `tnsnames.ora` files saved as Latin-1 (accented comments) and aliases that rely on `NAMES.DEFAULT_DOMAIN` from `sqlnet.ora` now connect; aliases are resolved by LowQuery and the driver receives the full descriptor.

## [0.1.0] - 2026-10-19

First public release, with PostgreSQL and Oracle.

### Added
- Connection profiles with a color tag, connection test and password prompt. Passwords live in the operating system's credential store.
- PostgreSQL through the native extended protocol: row-limited portals, binary decoding of the server types (exact NUMERIC, infinite dates, intervals, arrays, ranges, composites, inet, hstore and more), NOTICE messages, server-side cancel and SSL modes from `disable` to `verify-full`.
- Oracle through Oracle's official pure-Rust thin driver, with no Oracle Client or Instant Client: service name, SID, descriptor, Easy Connect and TNS aliases, SYSDBA and other privileges, PL/SQL blocks ended by `/`, and `DBMS_OUTPUT` in the Messages tab.
- SQL editor with each engine's dialect, schema-aware autocompletion and engine-aware script splitting.
- Per-tab sessions with autocommit, Commit and Rollback.
- Virtualized result grid with sorting, resizable columns, keyboard navigation, value viewer, copy and CSV/JSON export.
- Errors with native codes (SQLSTATE, ORA-NNNNN) and the reported position highlighted in the editor.
- Schema browser for tables, views, sequences, routines, packages and columns.
- Drafts restored on restart, light and dark themes, and a layout for narrow windows.

[Unreleased]: https://github.com/LowBloat/LowQuery/compare/v0.5.3...HEAD
[0.5.3]: https://github.com/LowBloat/LowQuery/compare/v0.5.2...v0.5.3
[0.5.2]: https://github.com/LowBloat/LowQuery/compare/v0.5.1...v0.5.2
[0.5.1]: https://github.com/LowBloat/LowQuery/compare/v0.5.0...v0.5.1
[0.5.0]: https://github.com/LowBloat/LowQuery/compare/v0.4.0...v0.5.0
[0.4.0]: https://github.com/LowBloat/LowQuery/compare/v0.3.2...v0.4.0
[0.3.2]: https://github.com/LowBloat/LowQuery/compare/v0.3.1...v0.3.2
[0.3.1]: https://github.com/LowBloat/LowQuery/compare/v0.3.0...v0.3.1
[0.3.0]: https://github.com/LowBloat/LowQuery/compare/v0.2.1...v0.3.0
[0.2.1]: https://github.com/LowBloat/LowQuery/compare/v0.2.0...v0.2.1
[0.2.0]: https://github.com/LowBloat/LowQuery/compare/v0.1.2...v0.2.0
[0.1.2]: https://github.com/LowBloat/LowQuery/compare/v0.1.1...v0.1.2
[0.1.1]: https://github.com/LowBloat/LowQuery/compare/v0.1.0...v0.1.1
[0.1.0]: https://github.com/LowBloat/LowQuery/releases/tag/v0.1.0
