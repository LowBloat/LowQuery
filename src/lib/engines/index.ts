import { CASSANDRA_PATH, MARIADB_PATH, MYSQL_PATH, POSTGRESQL_PATH, REDIS_PATH, SQLITE_PATH } from "./logos";

export type Engine = "postgres" | "oracle" | "mysql" | "mariadb" | "sqlserver" | "sqlite" | "redis" | "cassandra";

/** Lexical rules used to split scripts. */
export type { Dialect } from "../sql/split";
import type { Dialect } from "../sql/split";

export interface EngineInfo {
	id: Engine;
	label: string;
	/** One line under the name in the picker: how LowQuery connects. */
	driver: string;
	/** Brand color used for the logo. */
	color: string;
	/** Simple Icons path (24×24), or a glyph drawn by EngineLogo. */
	logo: { path: string } | { glyph: "oracle" | "sqlserver" };
	defaultPort: number | null;
	/** File-based engine: the profile points at a file instead of a server. */
	file: boolean;
	dialect: Dialect;
	/** Short badge text shown where a logo does not fit. */
	short: string;
	/** Key-value store: commands instead of SQL, keys instead of tables. */
	keyValue?: boolean;
	/** No transactions: the editor hides autocommit, Commit and Rollback. */
	noTransactions?: boolean;
}

export const ENGINES: EngineInfo[] = [
	{
		id: "postgres",
		label: "PostgreSQL",
		driver: "Native protocol (tokio-postgres)",
		color: "#4169E1",
		logo: { path: POSTGRESQL_PATH },
		defaultPort: 5432,
		file: false,
		dialect: "postgres",
		short: "PG",
	},
	{
		id: "oracle",
		label: "Oracle",
		driver: "Official thin driver, no Oracle Client needed",
		color: "#C74634",
		logo: { glyph: "oracle" },
		defaultPort: 1521,
		file: false,
		dialect: "oracle",
		short: "ORA",
	},
	{
		id: "mysql",
		label: "MySQL",
		driver: "Native protocol (mysql_async)",
		color: "#4479A1",
		logo: { path: MYSQL_PATH },
		defaultPort: 3306,
		file: false,
		dialect: "mysql",
		short: "MY",
	},
	{
		id: "mariadb",
		label: "MariaDB",
		driver: "Native protocol (mysql_async)",
		color: "#003545",
		logo: { path: MARIADB_PATH },
		defaultPort: 3306,
		file: false,
		dialect: "mysql",
		short: "MDB",
	},
	{
		id: "sqlserver",
		label: "SQL Server",
		driver: "Native TDS protocol (Tiberius), Azure SQL included",
		color: "#CC2927",
		logo: { glyph: "sqlserver" },
		defaultPort: 1433,
		file: false,
		dialect: "sqlserver",
		short: "MS",
	},
	{
		id: "sqlite",
		label: "SQLite",
		driver: "Embedded engine, opens a local database file",
		color: "#003B57",
		logo: { path: SQLITE_PATH },
		defaultPort: null,
		file: true,
		dialect: "sqlite",
		short: "LITE",
	},
	{
		id: "redis",
		label: "Redis",
		driver: "RESP3 protocol (redis-rs), Valkey included",
		color: "#FF4438",
		logo: { path: REDIS_PATH },
		defaultPort: 6379,
		file: false,
		dialect: "redis",
		short: "RDS",
		keyValue: true,
	},
	{
		id: "cassandra",
		label: "Apache Cassandra",
		driver: "Native CQL protocol (scylla), Cassandra 3.x to 5.x",
		color: "#1287B1",
		logo: { path: CASSANDRA_PATH },
		defaultPort: 9042,
		file: false,
		dialect: "cassandra",
		short: "CASS",
		noTransactions: true,
	},
];

const BY_ID = new Map(ENGINES.map((e) => [e.id, e]));

export function engineInfo(id: Engine): EngineInfo {
	return BY_ID.get(id) ?? ENGINES[0];
}
