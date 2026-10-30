# AI agents (MCP)

LowQuery includes an [MCP](https://modelcontextprotocol.io) server, so Claude, Cursor, VS Code, Codex, Gemini CLI and other MCP clients can use your database connections: list them, read schemas, find tables, run queries, check execution plans and, when you allow it, change data.

The server is the LowQuery executable started as `lowquery mcp`. It speaks MCP over stdio, opens no window, and does not need the app to be running. It uses the same connection profiles and the same passwords saved in your system keychain.

## Set it up

1. **Open a connection to agents.** Edit the connection and choose an access level under **AI agents**:
   - **Off** (default): agents do not see the connection.
   - **Read only**: agents read schemas and run queries; changes are refused.
   - **Read and write**: agents may also change data and schema.

   Save the password in the keychain, because agents cannot type it. SQLite files and SQL Server Windows authentication need none.
2. **Add LowQuery to your AI client.** Open **AI agents** (the robot button in the sidebar), then:
   - **Claude Desktop, Cursor, VS Code, Windsurf, Gemini CLI**: choose **Add to …**. LowQuery adds a `lowquery` server to the client's configuration file, keeps everything else and saves the previous file as `<file>.lowquery-backup`. Restart the client.
   - **Claude Code**: run the `claude mcp add` command shown in the dialog.
   - **Codex**: paste the shown block into `~/.codex/config.toml`.
   - **Anything else**: start `<path to lowquery> mcp` with the stdio transport.
3. **Ask.** For example: "Which customers in the ERP connection have no orders this year?" The agent lists the connections, finds the tables, reads their columns and keys, and writes the SQL in the database's own dialect.

## Tools

| Tool | What it does |
| --- | --- |
| `list_connections` | Connections open to agents: id, name, engine, where it points (never passwords), access and dialect notes |
| `list_schemas` | Schemas of a connection (keyspaces on Cassandra) |
| `list_tables` | Tables and views of a schema (the current schema by default) |
| `describe_table` | Columns with types, nullability and primary key (Cassandra: partition, clustering and static roles), plus foreign keys in both directions |
| `search_objects` | Tables, views and columns whose name contains a text, across schemas (Cassandra: also materialized views and types, from `system_schema`) |
| `query` | One read statement; 100 rows by default, up to 1,000 |
| `explain` | The engine's execution plan, without running the statement (not available on Redis or Cassandra, which have none) |
| `execute` | One statement of any kind, only on **Read and write** connections |

Results come as a compact table for the model plus structured JSON for clients that use it. Long values are cut at 1,000 characters and a reply at about 60,000, with a note saying what was left out.

## How access is enforced

Read-only access does not rely on the agent's good behaviour. Every call goes through several independent checks:

1. **One statement per call.** A lexer that knows each dialect's strings, quoted names and comments refuses text with more than one statement (MySQL executable comments `/*! */` included). On Cassandra a `BEGIN [UNLOGGED|COUNTER] BATCH ... APPLY BATCH` block counts as one statement, and `$$...$$` strings and `//` comments are understood.
2. **A read classifier.** On read-only connections the statement must start with `SELECT`, `WITH`, `SHOW`, `DESCRIBE`, `EXPLAIN` (no `ANALYZE`), `VALUES`, `TABLE` or a reporting `PRAGMA`; on Cassandra only `SELECT` and `LIST` (`DESCRIBE` is a cqlsh command, not CQL: use the schema tools). Write keywords anywhere in the statement and functions with side effects (`nextval`, `set_config`, `load_extension`...) are refused, and so are `EXEC` and `SET` on SQL Server.
3. **A read-only session in the database itself**, separate from the app's sessions:

   | Database | Session | Each call |
   | --- | --- | --- |
   | PostgreSQL | `SET SESSION CHARACTERISTICS AS TRANSACTION READ ONLY` | read-only transaction, rolled back |
   | MySQL, MariaDB | `SET SESSION TRANSACTION READ ONLY` | `START TRANSACTION READ ONLY`, rolled back |
   | Oracle | manual commit | `SET TRANSACTION READ ONLY`, rolled back |
   | SQL Server | implicit transactions | always rolled back |
   | SQLite | file opened read-only (`mode=ro`) and `PRAGMA query_only` | rolled back |
   | Redis | | allowlist of read commands; `KEYS` refused in favour of `SCAN` |
   | Cassandra | | CQL has no read-only transaction: only `SELECT` and `LIST` pass the classifier, and `USE` is refused so no call changes the session keyspace (names are written `keyspace.table`) |

On **Read and write** connections, `execute` runs with autocommit. Transaction control (`BEGIN`, `COMMIT`, `SET autocommit`...) is refused (on Cassandra: `BEGIN TRANSACTION`, `COMMIT`, `ROLLBACK`; `BEGIN BATCH` is allowed), and a statement that leaves a transaction open is rolled back.

Every call has a time limit (30 seconds, or `LOWQUERY_MCP_TIMEOUT` in seconds), after which the statement is cancelled on the server.

The database still decides what its user may do. For production databases, give agents a connection whose database user has only the rights they need: a read-only role is stronger than any client-side check. Side effects the database runs outside the transaction (an Oracle autonomous transaction inside a function, for example) cannot be rolled back by LowQuery.

## Activity log

Every tool call is written to `ai-activity.jsonl` in the LowQuery configuration folder: the time, the client, the tool, the connection, the SQL, the outcome, rows and duration. Refused calls are logged too. **AI agents → Activity** shows it while it happens, and **Clear** empties it.
