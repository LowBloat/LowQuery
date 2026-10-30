use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Engine {
    Postgres,
    Oracle,
    Mysql,
    Mariadb,
    Sqlserver,
    Sqlite,
    Redis,
    Cassandra,
}

/// libpq `sslmode` values that make sense for a desktop client.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "kebab-case")]
pub enum SslMode {
    Disable,
    #[default]
    Prefer,
    Require,
    VerifyFull,
}

/// How the Oracle address is built.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "lowercase")]
pub enum OracleConnectType {
    /// host:port/service_name (Easy Connect).
    #[default]
    Service,
    /// Classic SID, built as a descriptor.
    Sid,
    /// Alias from a tnsnames.ora file.
    Tns,
    /// Full descriptor or Easy Connect Plus string.
    Custom,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "lowercase")]
pub enum OraclePrivilege {
    #[default]
    None,
    Sysdba,
    Sysoper,
    Sysbackup,
    Sysdg,
    Syskm,
    Sysrac,
    Sysasm,
}

impl OraclePrivilege {
    /// Protocol `AUTH_MODE_*` values.
    pub fn auth_mode(self) -> u8 {
        match self {
            Self::None => 0x00,
            Self::Sysasm => 0x01,
            Self::Sysbackup => 0x02,
            Self::Sysdba => 0x04,
            Self::Sysdg => 0x08,
            Self::Syskm => 0x10,
            Self::Sysoper => 0x20,
            Self::Sysrac => 0x40,
        }
    }
}

/// What AI agents connected through the MCP server may do with a connection.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "lowercase")]
pub enum AiAccess {
    /// Not listed to agents.
    #[default]
    Off,
    /// Queries run read-only; changes are refused.
    Read,
    /// Agents may also change data and schema.
    Write,
}

/// Saved connection profile. The password is never part of the profile: it
/// lives in the OS credential store when `save_password` is on.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct ConnectionProfile {
    pub id: String,
    pub name: String,
    pub engine: Engine,
    /// Identification color (for example, red for production).
    pub color: Option<String>,
    pub host: String,
    pub port: u16,
    /// PostgreSQL, MySQL, SQL Server: database. Oracle: service name or SID.
    /// SQLite: path of the database file. Redis: logical database index.
    /// Cassandra: default keyspace (optional).
    pub database: String,
    pub user: String,
    pub save_password: bool,
    pub ssl_mode: SslMode,
    pub oracle_connect_type: OracleConnectType,
    pub connect_string: String,
    /// Alias looked up in tnsnames.ora when `oracle_connect_type` is `Tns`.
    pub tns_alias: String,
    /// Directory with tnsnames.ora / sqlnet.ora (TNS_ADMIN). Empty means
    /// automatic: TNS_ADMIN, ORACLE_HOME, registered Oracle homes and clients
    /// on PATH.
    pub config_dir: Option<String>,
    pub oracle_privilege: OraclePrivilege,
    /// SQL Server named instance (resolved through the SQL Browser service).
    pub instance: String,
    /// SQL Server: sign in with the current Windows account instead of a
    /// SQL login.
    pub windows_auth: bool,
    /// SQLite: create the file when it does not exist.
    pub create_file: bool,
    /// Cassandra: local datacenter. Requests go to its nodes only; empty
    /// means every node the cluster reports.
    pub datacenter: String,
    /// Access for AI agents through `lowquery mcp`.
    pub ai_access: AiAccess,
}

impl Default for ConnectionProfile {
    fn default() -> Self {
        Self {
            id: String::new(),
            name: String::new(),
            engine: Engine::Postgres,
            color: None,
            host: "localhost".into(),
            port: 5432,
            database: String::new(),
            user: String::new(),
            save_password: true,
            ssl_mode: SslMode::default(),
            oracle_connect_type: OracleConnectType::default(),
            connect_string: String::new(),
            tns_alias: String::new(),
            config_dir: None,
            oracle_privilege: OraclePrivilege::default(),
            instance: String::new(),
            windows_auth: false,
            create_file: false,
            datacenter: String::new(),
            ai_access: AiAccess::Off,
        }
    }
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionInfo {
    pub session_id: String,
    pub profile_id: String,
    pub engine: Engine,
    pub server_version: String,
    pub current_user: Option<String>,
    pub current_schema: Option<String>,
    pub autocommit: bool,
}

/// Column category, used by the UI for alignment and formatting.
#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum ValueKind {
    Number,
    Text,
    Bool,
    Temporal,
    Binary,
    Json,
    Other,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ColumnInfo {
    pub name: String,
    /// Type name as the database describes it (`int4`, `VARCHAR2(30)`...).
    pub type_name: String,
    pub kind: ValueKind,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ExecRequest {
    pub sql: String,
    /// 0 = no limit.
    #[serde(default)]
    pub max_rows: usize,
}

/// Result of a statement. Values arrive as text already formatted by the
/// driver: this preserves NUMERIC/NUMBER and date precision without relying
/// on JavaScript number conversion.
#[derive(Debug, Clone, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ExecResult {
    pub columns: Option<Vec<ColumnInfo>>,
    pub rows: Vec<Vec<Option<String>>>,
    pub truncated: bool,
    pub rows_affected: Option<u64>,
    pub elapsed_ms: u64,
    pub notices: Vec<String>,
    pub in_transaction: bool,
    /// Set when the rows come from one table with its key in the result, so
    /// the grid can edit them.
    pub editable: Option<EditTarget>,
    /// Why a row-returning result cannot be edited (shown in the grid).
    pub read_only_reason: Option<String>,
}

/// The table behind an editable result.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EditTarget {
    pub schema: Option<String>,
    pub table: String,
    /// One entry per result column: the table column it shows, or `None` for
    /// expressions, which stay read-only.
    pub columns: Vec<Option<EditColumn>>,
    /// Result column indexes that form the table's primary (or unique) key.
    pub key: Vec<usize>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EditColumn {
    pub name: String,
    /// Type as the engine names it in DDL, used to convert the text value.
    pub type_name: String,
}

/// A value for one result column.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CellValue {
    pub column: usize,
    pub value: Option<String>,
}

/// One pending change from the grid. `key` holds the original values of the
/// key columns, in the order of `EditTarget::key`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "lowercase")]
pub enum RowChange {
    Update {
        key: Vec<Option<String>>,
        values: Vec<CellValue>,
    },
    Delete {
        key: Vec<Option<String>>,
    },
    Insert {
        values: Vec<CellValue>,
    },
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ApplyRequest {
    pub target: EditTarget,
    pub changes: Vec<RowChange>,
}

#[derive(Debug, Clone, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ApplyResult {
    pub updated: u64,
    pub deleted: u64,
    pub inserted: u64,
    pub elapsed_ms: u64,
    pub in_transaction: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DbObject {
    pub name: String,
    /// table, view, matview, foreign, sequence, function, procedure, package,
    /// type, synonym.
    pub kind: String,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DbColumn {
    pub name: String,
    pub type_name: String,
    pub nullable: bool,
    pub primary_key: bool,
    /// Default as an SQL expression, as the table definition writes it.
    pub default_value: Option<String>,
    /// Cassandra: `partition`, `clustering`, `static` or `regular`. `None`
    /// on every other engine.
    pub key_role: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ServerInfo {
    pub server_version: String,
}
