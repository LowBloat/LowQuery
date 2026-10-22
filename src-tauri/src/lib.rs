mod ai_setup;
mod drivers;
mod error;
mod import;
pub mod mcp;
mod model;
mod profiles;
mod updates;

use std::collections::HashMap;
use std::sync::Arc;

use parking_lot::RwLock;
use tauri::State;

use drivers::{Session, TxState};
use error::{AppError, AppResult, ErrorKind};
use model::{
    ConnectionProfile, DbColumn, DbObject, ExecRequest, ExecResult, ServerInfo, SessionInfo,
};
use profiles::ProfileStore;

struct AppState {
    profiles: ProfileStore,
    sessions: RwLock<HashMap<String, Arc<Session>>>,
}

impl AppState {
    fn session(&self, id: &str) -> AppResult<Arc<Session>> {
        self.sessions
            .read()
            .get(id)
            .cloned()
            .ok_or_else(|| AppError::not_found("Session closed. Connect again."))
    }
}

fn is_auth_failure(e: &AppError) -> bool {
    matches!(
        e.code.as_deref(),
        Some(
            "28P01"
                | "28000"
                | "ORA-01017"
                | "ORA-01005"
                | "1045 (28000)"
                | "1044 (42000)"
                | "1698 (28000)"
                | "NOAUTH"
                | "WRONGPASS"
        )
    ) || e
        .code
        .as_deref()
        .is_some_and(|c| c.starts_with("Msg 18456,"))
        || e.message.contains("password")
}

/// Resolves the password: the one provided now, the one in the credential
/// store, or none (`trust` authentication, for example). Asks for it when the
/// user chose not to save it.
async fn open_with_password(
    profile: &ConnectionProfile,
    password: Option<String>,
) -> AppResult<Session> {
    // SQLite files and Windows-authenticated SQL Server need no password.
    let passwordless = profile.engine == model::Engine::Sqlite
        || (profile.engine == model::Engine::Sqlserver && profile.windows_auth);
    if passwordless {
        return Session::open(profile, None).await;
    }
    let provided = password.is_some();
    let password = match password {
        Some(pw) => Some(pw),
        None => profiles::stored_password(&profile.id).unwrap_or(None),
    };
    if password.is_none() && !profile.save_password {
        return Err(AppError::new(
            ErrorKind::PasswordRequired,
            "Enter the password.",
        ));
    }
    let had_password = password.is_some();
    match Session::open(profile, password.as_deref()).await {
        Err(e) if !provided && is_auth_failure(&e) => {
            let mut err = AppError::new(
                ErrorKind::PasswordRequired,
                if had_password {
                    "The saved password was rejected."
                } else {
                    "Enter the password."
                },
            );
            err.detail = Some(e.to_string());
            Err(err)
        }
        other => other,
    }
}

#[tauri::command]
fn list_profiles(state: State<'_, AppState>) -> AppResult<Vec<ConnectionProfile>> {
    state.profiles.list()
}

#[tauri::command]
fn save_profile(
    state: State<'_, AppState>,
    profile: ConnectionProfile,
    password: Option<String>,
) -> AppResult<ConnectionProfile> {
    state.profiles.save(profile, password)
}

#[tauri::command]
fn delete_profile(state: State<'_, AppState>, id: String) -> AppResult<()> {
    state.profiles.delete(&id)
}

#[tauri::command]
async fn test_connection(
    profile: ConnectionProfile,
    password: Option<String>,
) -> AppResult<ServerInfo> {
    let session = open_with_password(&profile, password).await?;
    Ok(ServerInfo {
        server_version: session.server_version().to_string(),
    })
}

#[tauri::command]
async fn open_session(
    state: State<'_, AppState>,
    profile_id: String,
    password: Option<String>,
) -> AppResult<SessionInfo> {
    let profile = state.profiles.get(&profile_id)?;
    let typed = password.clone();
    let session = open_with_password(&profile, password).await?;
    // A password typed at connect time is kept when the profile saves
    // passwords, so the next connection does not ask again.
    if let Some(pw) = typed.filter(|_| profile.save_password) {
        let _ = profiles::remember_password(&profile.id, &pw);
    }
    let id = uuid::Uuid::new_v4().to_string();
    let info = SessionInfo {
        session_id: id.clone(),
        profile_id,
        engine: session.engine(),
        server_version: session.server_version().to_string(),
        current_user: session.current_user(),
        current_schema: session.current_schema(),
        autocommit: session.tx_state().autocommit,
    };
    state.sessions.write().insert(id, Arc::new(session));
    Ok(info)
}

#[tauri::command]
fn close_session(state: State<'_, AppState>, session_id: String) {
    // The connection closes when the last reference (a running execution,
    // for example) is released.
    state.sessions.write().remove(&session_id);
}

#[tauri::command]
async fn execute(
    state: State<'_, AppState>,
    session_id: String,
    request: ExecRequest,
) -> AppResult<ExecResult> {
    let session = state.session(&session_id)?;
    session.execute(&request).await
}

#[tauri::command]
async fn cancel_query(state: State<'_, AppState>, session_id: String) -> AppResult<()> {
    state.session(&session_id)?.cancel().await
}

#[tauri::command]
async fn set_autocommit(
    state: State<'_, AppState>,
    session_id: String,
    on: bool,
) -> AppResult<TxState> {
    state.session(&session_id)?.set_autocommit(on).await
}

#[tauri::command]
async fn commit(state: State<'_, AppState>, session_id: String) -> AppResult<TxState> {
    state.session(&session_id)?.end_transaction(true).await
}

#[tauri::command]
async fn rollback(state: State<'_, AppState>, session_id: String) -> AppResult<TxState> {
    state.session(&session_id)?.end_transaction(false).await
}

#[tauri::command]
async fn list_schemas(state: State<'_, AppState>, session_id: String) -> AppResult<Vec<String>> {
    state.session(&session_id)?.list_schemas().await
}

#[tauri::command]
async fn list_objects(
    state: State<'_, AppState>,
    session_id: String,
    schema: String,
) -> AppResult<Vec<DbObject>> {
    state.session(&session_id)?.list_objects(&schema).await
}

#[tauri::command]
async fn list_columns(
    state: State<'_, AppState>,
    session_id: String,
    schema: String,
    table: String,
) -> AppResult<Vec<DbColumn>> {
    state
        .session(&session_id)?
        .list_columns(&schema, &table)
        .await
}

/// The statements that create a table, view, routine or other object.
#[tauri::command]
async fn object_ddl(
    state: State<'_, AppState>,
    session_id: String,
    schema: String,
    name: String,
    kind: String,
) -> AppResult<String> {
    state
        .session(&session_id)?
        .object_ddl(&schema, &name, &kind)
        .await
}

/// Every `tnsnames.ora` found on this machine (TNS_ADMIN, ORACLE_HOME,
/// registered Oracle homes, clients on PATH).
#[tauri::command]
async fn oracle_tns_sources() -> AppResult<Vec<drivers::tns::TnsSource>> {
    tauri::async_runtime::spawn_blocking(drivers::tns::discover)
        .await
        .map_err(|e| AppError::internal(e.to_string()))
}

/// The MCP server command and the configuration of each AI client.
#[tauri::command]
fn ai_setup() -> AppResult<ai_setup::AiSetup> {
    ai_setup::setup()
}

/// Adds LowQuery to an AI client's configuration file; returns its path.
#[tauri::command]
fn ai_connect_client(id: String) -> AppResult<String> {
    ai_setup::connect_client(&id)
}

/// What AI agents did, newest first.
#[tauri::command]
fn ai_activity(limit: Option<usize>) -> AppResult<Vec<mcp::audit::AuditEntry>> {
    mcp::audit::read_recent(&mcp::config_dir(), limit.unwrap_or(200).min(2000))
}

#[tauri::command]
fn ai_activity_clear() -> AppResult<()> {
    mcp::audit::clear(&mcp::config_dir())
}

/// Connection files of other database tools found on this machine.
#[tauri::command]
async fn import_detect() -> AppResult<Vec<import::ImportSource>> {
    tauri::async_runtime::spawn_blocking(import::detect)
        .await
        .map_err(|e| AppError::internal(e.to_string()))
}

/// Reads one tool file and lists what can be imported (without passwords).
#[tauri::command]
async fn import_preview(
    tool: import::ImportTool,
    path: String,
) -> AppResult<import::ImportPreview> {
    tauri::async_runtime::spawn_blocking(move || import::preview(tool, &path))
        .await
        .map_err(|e| AppError::internal(e.to_string()))?
}

/// Creates profiles for the chosen connections of a tool file.
#[tauri::command]
fn import_apply(
    state: State<'_, AppState>,
    tool: import::ImportTool,
    path: String,
    keys: Vec<String>,
    with_passwords: bool,
) -> AppResult<import::ImportResult> {
    import::apply(&state.profiles, tool, &path, &keys, with_passwords)
}

/// Reads the `tnsnames.ora` in a folder picked by the user.
#[tauri::command]
async fn oracle_tns_folder(path: String) -> AppResult<drivers::tns::TnsSource> {
    tauri::async_runtime::spawn_blocking(move || drivers::tns::read_folder(&path))
        .await
        .map_err(|e| AppError::internal(e.to_string()))?
}

/// Applies the grid's pending changes on the tab's session.
#[tauri::command]
async fn apply_changes(
    state: State<'_, AppState>,
    session_id: String,
    request: model::ApplyRequest,
) -> AppResult<model::ApplyResult> {
    state.session(&session_id)?.apply_changes(&request).await
}

/// Writes a file the user picked in the "Save as" dialog.
#[tauri::command]
fn write_text_file(path: String, content: String) -> AppResult<()> {
    std::fs::write(path, content)?;
    Ok(())
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    // Both drivers use rustls; an explicit default provider avoids ambiguity
    // when more than one crypto backend is compiled in.
    let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();

    let dirs = directories::ProjectDirs::from("dev", "LowBloat", "LowQuery")
        .expect("user configuration directory unavailable");
    let profiles = ProfileStore::new(dirs.config_dir().to_path_buf())
        .expect("could not create the configuration directory");

    let update_policy = updates::UpdatePolicy::detect();
    let mut updater = tauri_plugin_updater::Builder::new();
    if let Some(target) = &update_policy.target {
        updater = updater.target(target);
    }

    tauri::Builder::default()
        .plugin(tauri_plugin_dialog::init())
        .plugin(updater.build())
        .plugin(tauri_plugin_process::init())
        .plugin(tauri_plugin_opener::init())
        .manage(update_policy)
        .manage(AppState {
            profiles,
            sessions: RwLock::new(HashMap::new()),
        })
        .invoke_handler(tauri::generate_handler![
            list_profiles,
            save_profile,
            delete_profile,
            test_connection,
            open_session,
            close_session,
            execute,
            apply_changes,
            cancel_query,
            set_autocommit,
            commit,
            rollback,
            list_schemas,
            list_objects,
            list_columns,
            object_ddl,
            write_text_file,
            oracle_tns_sources,
            oracle_tns_folder,
            import_detect,
            import_preview,
            import_apply,
            ai_setup,
            ai_connect_client,
            ai_activity,
            ai_activity_clear,
            updates::update_policy,
        ])
        .run(tauri::generate_context!())
        .expect("error while starting LowQuery");
}
