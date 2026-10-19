use std::path::PathBuf;

use parking_lot::Mutex;

use crate::error::{AppError, AppResult};
use crate::model::ConnectionProfile;

const KEYRING_SERVICE: &str = "dev.lowbloat.lowquery";

/// Connection profiles in `connections.json` in the user's configuration
/// directory. Passwords live in the OS credential store (Windows Credential
/// Manager, Keychain, Secret Service).
pub struct ProfileStore {
    path: PathBuf,
    lock: Mutex<()>,
}

impl ProfileStore {
    pub fn new(config_dir: PathBuf) -> AppResult<Self> {
        std::fs::create_dir_all(&config_dir)?;
        Ok(Self {
            path: config_dir.join("connections.json"),
            lock: Mutex::new(()),
        })
    }

    pub fn list(&self) -> AppResult<Vec<ConnectionProfile>> {
        let _guard = self.lock.lock();
        self.read()
    }

    pub fn get(&self, id: &str) -> AppResult<ConnectionProfile> {
        self.list()?
            .into_iter()
            .find(|p| p.id == id)
            .ok_or_else(|| AppError::not_found("Connection not found."))
    }

    /// Creates or updates the profile. `password`: `Some` stores it (an empty
    /// string deletes it), `None` keeps the current password.
    pub fn save(
        &self,
        mut profile: ConnectionProfile,
        password: Option<String>,
    ) -> AppResult<ConnectionProfile> {
        validate(&profile)?;
        let _guard = self.lock.lock();
        let mut all = self.read()?;
        if profile.id.is_empty() {
            profile.id = uuid::Uuid::new_v4().to_string();
        }
        match all.iter_mut().find(|p| p.id == profile.id) {
            Some(existing) => *existing = profile.clone(),
            None => all.push(profile.clone()),
        }
        self.write(&all)?;

        if !profile.save_password {
            delete_password(&profile.id)?;
        } else if let Some(pw) = password {
            if pw.is_empty() {
                delete_password(&profile.id)?;
            } else {
                entry(&profile.id)?.set_password(&pw)?;
            }
        }
        Ok(profile)
    }

    pub fn delete(&self, id: &str) -> AppResult<()> {
        let _guard = self.lock.lock();
        let mut all = self.read()?;
        all.retain(|p| p.id != id);
        self.write(&all)?;
        delete_password(id)
    }

    fn read(&self) -> AppResult<Vec<ConnectionProfile>> {
        match std::fs::read_to_string(&self.path) {
            Ok(text) => Ok(serde_json::from_str(&text)?),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Vec::new()),
            Err(e) => Err(e.into()),
        }
    }

    /// Writes to a temporary file and renames it, so the JSON is not corrupted
    /// if the process dies mid-write.
    fn write(&self, all: &[ConnectionProfile]) -> AppResult<()> {
        let tmp = self.path.with_extension("json.tmp");
        std::fs::write(&tmp, serde_json::to_vec_pretty(all)?)?;
        std::fs::rename(&tmp, &self.path)?;
        Ok(())
    }
}

fn validate(p: &ConnectionProfile) -> AppResult<()> {
    if p.name.trim().is_empty() {
        return Err(AppError::invalid("Enter a name for the connection."));
    }
    let custom = p.engine == crate::model::Engine::Oracle
        && p.oracle_connect_type == crate::model::OracleConnectType::Custom;
    if custom {
        if p.connect_string.trim().is_empty() {
            return Err(AppError::invalid("Enter the descriptor or TNS alias."));
        }
    } else if p.host.trim().is_empty() {
        return Err(AppError::invalid("Enter the host."));
    }
    Ok(())
}

fn entry(id: &str) -> AppResult<keyring::Entry> {
    Ok(keyring::Entry::new(KEYRING_SERVICE, id)?)
}

pub fn stored_password(id: &str) -> AppResult<Option<String>> {
    if id.is_empty() {
        return Ok(None);
    }
    match entry(id)?.get_password() {
        Ok(pw) => Ok(Some(pw)),
        Err(keyring::Error::NoEntry) => Ok(None),
        Err(e) => Err(e.into()),
    }
}

fn delete_password(id: &str) -> AppResult<()> {
    match entry(id)?.delete_credential() {
        Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
        Err(e) => Err(e.into()),
    }
}
