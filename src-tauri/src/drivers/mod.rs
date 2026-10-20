#[cfg(test)]
mod integration_tests;
pub mod mysql;
pub mod oracle;
pub mod pg_value;
pub mod postgres;
pub mod redis_db;
pub mod sqlite;
pub mod sqlserver;
pub mod tns;

use crate::error::{AppError, AppResult};
use crate::model::{
    ApplyRequest, ApplyResult, ConnectionProfile, DbColumn, DbObject, Engine, ExecRequest,
    ExecResult,
};

#[derive(Debug, Clone, Copy, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TxState {
    pub autocommit: bool,
    pub in_transaction: bool,
}

/// An open connection. Each query tab has its own, so transactions do not
/// mix across tabs.
///
/// Every driver session exposes the same surface, used through [`dispatch!`]:
/// `server_version`, `current_user` and `current_schema` fields, and
/// `tx_state`, `execute`, `cancel`, `set_autocommit`, `end_transaction`,
/// `list_schemas`, `list_objects` and `list_columns` methods with the
/// signatures used below. Rules every driver follows are in
/// docs/ARCHITECTURE.md.
#[allow(
    clippy::large_enum_variant,
    reason = "one per tab, created once and kept behind an Arc; boxing only adds indirection"
)]
pub enum Session {
    Postgres(postgres::PgSession),
    Oracle(oracle::OraSession),
    /// MySQL and MariaDB share the protocol and the driver.
    Mysql(mysql::MySession),
    SqlServer(sqlserver::MsSession),
    Sqlite(sqlite::LiteSession),
    Redis(redis_db::RedisSession),
}

macro_rules! dispatch {
    ($session:expr, $s:ident => $body:expr) => {
        match $session {
            Session::Postgres($s) => $body,
            Session::Oracle($s) => $body,
            Session::Mysql($s) => $body,
            Session::SqlServer($s) => $body,
            Session::Sqlite($s) => $body,
            Session::Redis($s) => $body,
        }
    };
}

impl Session {
    pub async fn open(profile: &ConnectionProfile, password: Option<&str>) -> AppResult<Self> {
        Ok(match profile.engine {
            Engine::Postgres => Session::Postgres(postgres::connect(profile, password).await?),
            Engine::Oracle => Session::Oracle(oracle::connect(profile, password).await?),
            Engine::Mysql | Engine::Mariadb => {
                Session::Mysql(mysql::connect(profile, password).await?)
            }
            Engine::Sqlserver => Session::SqlServer(sqlserver::connect(profile, password).await?),
            Engine::Sqlite => Session::Sqlite(sqlite::connect(profile).await?),
            Engine::Redis => Session::Redis(redis_db::connect(profile, password).await?),
        })
    }

    pub fn engine(&self) -> Engine {
        match self {
            Session::Postgres(_) => Engine::Postgres,
            Session::Oracle(_) => Engine::Oracle,
            Session::Mysql(s) => s.engine,
            Session::SqlServer(_) => Engine::Sqlserver,
            Session::Sqlite(_) => Engine::Sqlite,
            Session::Redis(_) => Engine::Redis,
        }
    }

    pub fn server_version(&self) -> &str {
        dispatch!(self, s => &s.server_version)
    }

    pub fn current_user(&self) -> Option<String> {
        dispatch!(self, s => s.current_user.clone())
    }

    pub fn current_schema(&self) -> Option<String> {
        dispatch!(self, s => s.current_schema.clone())
    }

    pub fn tx_state(&self) -> TxState {
        dispatch!(self, s => s.tx_state())
    }

    pub async fn execute(&self, req: &ExecRequest) -> AppResult<ExecResult> {
        if req.sql.trim().is_empty() {
            return Err(AppError::invalid("Nothing to execute."));
        }
        dispatch!(self, s => s.execute(req).await)
    }

    pub async fn cancel(&self) -> AppResult<()> {
        dispatch!(self, s => s.cancel().await)
    }

    pub async fn set_autocommit(&self, on: bool) -> AppResult<TxState> {
        dispatch!(self, s => s.set_autocommit(on).await)
    }

    pub async fn end_transaction(&self, commit: bool) -> AppResult<TxState> {
        dispatch!(self, s => s.end_transaction(commit).await)
    }

    pub async fn list_schemas(&self) -> AppResult<Vec<String>> {
        dispatch!(self, s => s.list_schemas().await)
    }

    /// Applies grid edits as one unit: with autocommit on they commit
    /// together or not at all; with autocommit off they join the open
    /// transaction.
    pub async fn apply_changes(&self, req: &ApplyRequest) -> AppResult<ApplyResult> {
        if req.changes.is_empty() {
            return Ok(ApplyResult {
                in_transaction: self.tx_state().in_transaction,
                ..Default::default()
            });
        }
        if req.target.key.is_empty() {
            return Err(AppError::invalid(
                "This result has no key to identify its rows.",
            ));
        }
        match self {
            Session::Postgres(s) => s.apply_changes(req).await,
            Session::Oracle(s) => s.apply_changes(req).await,
            Session::Mysql(s) => s.apply_changes(req).await,
            Session::SqlServer(s) => s.apply_changes(req).await,
            Session::Sqlite(s) => s.apply_changes(req).await,
            Session::Redis(_) => Err(AppError::invalid(
                "Redis replies cannot be edited in the grid.",
            )),
        }
    }

    pub async fn list_objects(&self, schema: &str) -> AppResult<Vec<DbObject>> {
        dispatch!(self, s => s.list_objects(schema).await)
    }

    pub async fn list_columns(&self, schema: &str, table: &str) -> AppResult<Vec<DbColumn>> {
        dispatch!(self, s => s.list_columns(schema, table).await)
    }
}

/// First words of the statement, uppercased, skipping whitespace and
/// comments. Stops at the first punctuation.
pub fn first_keywords(sql: &str, n: usize) -> Vec<String> {
    let bytes = sql.as_bytes();
    let mut i = 0;
    let mut words = Vec::new();
    while i < bytes.len() && words.len() < n {
        let c = bytes[i];
        if c.is_ascii_whitespace() {
            i += 1;
        } else if c == b'-' && bytes.get(i + 1) == Some(&b'-') {
            while i < bytes.len() && bytes[i] != b'\n' {
                i += 1;
            }
        } else if c == b'/' && bytes.get(i + 1) == Some(&b'*') {
            i += 2;
            while i + 1 < bytes.len() && !(bytes[i] == b'*' && bytes[i + 1] == b'/') {
                i += 1;
            }
            i += 2;
        } else if c.is_ascii_alphabetic() || c == b'_' {
            let start = i;
            while i < bytes.len() && (bytes[i].is_ascii_alphanumeric() || bytes[i] == b'_') {
                i += 1;
            }
            words.push(sql[start..i].to_ascii_uppercase());
        } else {
            break;
        }
    }
    words
}

#[cfg(test)]
mod tests {
    use super::first_keywords;

    #[test]
    fn keywords_skip_comments() {
        assert_eq!(
            first_keywords("-- x\n /* y */ begin transaction", 2),
            ["BEGIN", "TRANSACTION"]
        );
        assert_eq!(first_keywords("  select(1)", 3), ["SELECT"]);
        assert!(first_keywords("", 2).is_empty());
    }
}
