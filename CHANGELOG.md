# Changelog

All notable changes to LowQuery are documented here. The format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/) and the project uses [Semantic Versioning](https://semver.org/).

## [Unreleased]

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

[Unreleased]: https://github.com/LowBloat/LowQuery/compare/v0.1.0...HEAD
[0.1.0]: https://github.com/LowBloat/LowQuery/releases/tag/v0.1.0
