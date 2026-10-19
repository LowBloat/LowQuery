#[cfg(test)]
mod integration_tests;
pub mod oracle;
pub mod pg_value;
pub mod postgres;

use crate::error::{AppError, AppResult};
use crate::model::{ConnectionProfile, DbColumn, DbObject, Engine, ExecRequest, ExecResult};

#[derive(Debug, Clone, Copy, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TxState {
    pub autocommit: bool,
    pub in_transaction: bool,
}

/// An open connection. Each query tab has its own, so transactions do not
/// mix across tabs.
#[allow(
    clippy::large_enum_variant,
    reason = "one per tab, created once and kept behind an Arc; boxing only adds indirection"
)]
pub enum Session {
    Postgres(postgres::PgSession),
    Oracle(oracle::OraSession),
}

impl Session {
    pub async fn open(profile: &ConnectionProfile, password: Option<&str>) -> AppResult<Self> {
        Ok(match profile.engine {
            Engine::Postgres => Session::Postgres(postgres::connect(profile, password).await?),
            Engine::Oracle => Session::Oracle(oracle::connect(profile, password).await?),
        })
    }

    pub fn engine(&self) -> Engine {
        match self {
            Session::Postgres(_) => Engine::Postgres,
            Session::Oracle(_) => Engine::Oracle,
        }
    }

    pub fn server_version(&self) -> &str {
        match self {
            Session::Postgres(s) => &s.server_version,
            Session::Oracle(s) => &s.server_version,
        }
    }

    pub fn current_user(&self) -> Option<String> {
        match self {
            Session::Postgres(s) => s.current_user.clone(),
            Session::Oracle(s) => s.current_user.clone(),
        }
    }

    pub fn current_schema(&self) -> Option<String> {
        match self {
            Session::Postgres(s) => s.current_schema.clone(),
            Session::Oracle(s) => s.current_schema.clone(),
        }
    }

    pub fn tx_state(&self) -> TxState {
        match self {
            Session::Postgres(s) => s.tx_state(),
            Session::Oracle(s) => s.tx_state(),
        }
    }

    pub async fn execute(&self, req: &ExecRequest) -> AppResult<ExecResult> {
        if req.sql.trim().is_empty() {
            return Err(AppError::invalid("Nothing to execute."));
        }
        match self {
            Session::Postgres(s) => s.execute(req).await,
            Session::Oracle(s) => s.execute(req).await,
        }
    }

    pub async fn cancel(&self) -> AppResult<()> {
        match self {
            Session::Postgres(s) => s.cancel().await,
            Session::Oracle(_) => Err(AppError::invalid(
                "The Oracle driver cannot interrupt a running call yet.",
            )),
        }
    }

    pub async fn set_autocommit(&self, on: bool) -> AppResult<TxState> {
        match self {
            Session::Postgres(s) => s.set_autocommit(on).await,
            Session::Oracle(s) => s.set_autocommit(on).await,
        }
    }

    pub async fn end_transaction(&self, commit: bool) -> AppResult<TxState> {
        match self {
            Session::Postgres(s) => s.end_transaction(commit).await,
            Session::Oracle(s) => s.end_transaction(commit).await,
        }
    }

    pub async fn list_schemas(&self) -> AppResult<Vec<String>> {
        match self {
            Session::Postgres(s) => s.list_schemas().await,
            Session::Oracle(s) => s.list_schemas().await,
        }
    }

    pub async fn list_objects(&self, schema: &str) -> AppResult<Vec<DbObject>> {
        match self {
            Session::Postgres(s) => s.list_objects(schema).await,
            Session::Oracle(s) => s.list_objects(schema).await,
        }
    }

    pub async fn list_columns(&self, schema: &str, table: &str) -> AppResult<Vec<DbColumn>> {
        match self {
            Session::Postgres(s) => s.list_columns(schema, table).await,
            Session::Oracle(s) => s.list_columns(schema, table).await,
        }
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
