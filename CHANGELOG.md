# Changelog

All notable changes to LowQuery are documented here. The format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/) and the project uses [Semantic Versioning](https://semver.org/).

## [Unreleased]

### Added
- Edit query results in the grid: change cells, set NULL, add, duplicate and delete rows, then **Save** (Ctrl+S) or **Discard**. Pending changes are highlighted, new rows show `DEFAULT` for the columns left empty, and deleted rows are struck through. Saving writes all changes in one transaction with autocommit on, or inside the open transaction otherwise, and the result refreshes in place.
- A result is editable when it comes from one table and includes its primary key (or a unique key with NOT NULL columns). The table and key come from the database's own result metadata, and a **Read-only** chip explains why a result cannot be edited. Computed columns stay read-only.
- Running another query, switching connections or closing a tab with unsaved changes asks first.

### Changed
- The value viewer edits long and multi-line values when the result is editable.

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

[Unreleased]: https://github.com/LowBloat/LowQuery/compare/v0.2.1...HEAD
[0.2.1]: https://github.com/LowBloat/LowQuery/compare/v0.2.0...v0.2.1
[0.2.0]: https://github.com/LowBloat/LowQuery/compare/v0.1.2...v0.2.0
[0.1.2]: https://github.com/LowBloat/LowQuery/compare/v0.1.1...v0.1.2
[0.1.1]: https://github.com/LowBloat/LowQuery/compare/v0.1.0...v0.1.1
[0.1.0]: https://github.com/LowBloat/LowQuery/releases/tag/v0.1.0
