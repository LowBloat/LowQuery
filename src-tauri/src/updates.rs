//! Updates from GitHub Releases, installed the way this copy was installed.
//!
//! | Channel | Installs the signed update with |
//! | --- | --- |
//! | `installer` | Windows setup/MSI or macOS app bundle (updater plugin) |
//! | `appimage` | a new AppImage in place of this one (updater plugin) |
//! | `deb`, `rpm` | `dpkg -i` / `rpm -U` through pkexec (updater plugin) |
//! | `pacman` | `pacman -U` through pkexec: the Arch package and the AUR's `lowquery-bin` |
//! | `portable` | the new executable in place of this one (Linux and Windows) |
//! | `package` | nothing: installed under /usr or /opt by another tool, download link only |
//!
//! Every download is checked against LowQuery's update key before it is
//! installed. `latest.json` has one entry per channel, so a package is never
//! replaced by another format.

use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use parking_lot::Mutex;
use serde::Serialize;
use tauri::ipc::Channel;
use tauri::AppHandle;
use tauri_plugin_updater::{Update, UpdaterExt};

use crate::error::{AppError, ErrorKind};

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct UpdatePolicy {
    /// `installer`, `appimage`, `deb`, `rpm`, `pacman`, `portable` or `package`.
    pub channel: &'static str,
    pub can_install: bool,
    /// `latest.json` platform key; `None` uses the plugin's key for the bundle.
    #[serde(skip)]
    pub target: Option<String>,
}

impl UpdatePolicy {
    pub fn detect() -> Self {
        use tauri::utils::config::BundleType;
        let bundle = tauri::utils::platform::bundle_type();
        let exe = tauri::utils::platform::current_exe().ok();
        let arch = std::env::consts::ARCH;
        // Development builds never replace themselves with a release.
        let release = !cfg!(debug_assertions);
        let folder_writable = |path: &Path| path.parent().is_some_and(dir_is_writable);

        if cfg!(target_os = "linux") {
            let policy = |channel, can_install, kind: &str| Self {
                channel,
                can_install,
                target: Some(format!("linux-{arch}-{kind}")),
            };
            if let Some(appimage) = std::env::var_os("APPIMAGE").filter(|p| !p.is_empty()) {
                return policy(
                    "appimage",
                    folder_writable(Path::new(&appimage)),
                    "appimage",
                );
            }
            return match (bundle, exe) {
                (Some(BundleType::Deb), _) => policy("deb", true, "deb"),
                (Some(BundleType::Rpm), _) => policy("rpm", true, "rpm"),
                (_, Some(exe)) if exe.starts_with("/usr") || exe.starts_with("/opt") => {
                    if owned_by_pacman(&exe) {
                        policy("pacman", true, "pacman")
                    } else {
                        // The AppImage entry still tells which version is out.
                        policy("package", false, "appimage")
                    }
                }
                (_, exe) => policy(
                    "portable",
                    release && exe.as_deref().is_some_and(folder_writable),
                    "portable",
                ),
            };
        }

        // macOS reports `App` for every bundle.
        if matches!(
            bundle,
            Some(BundleType::Nsis | BundleType::Msi | BundleType::App)
        ) {
            return Self {
                channel: "installer",
                can_install: true,
                target: None,
            };
        }
        let windows = cfg!(windows);
        Self {
            channel: "portable",
            can_install: windows && release && exe.as_deref().is_some_and(folder_writable),
            target: windows.then(|| format!("windows-{arch}-portable")),
        }
    }
}

fn dir_is_writable(dir: &Path) -> bool {
    let probe = dir.join(format!(".lowquery-update-check-{}", std::process::id()));
    let ok = std::fs::write(&probe, b"").is_ok();
    let _ = std::fs::remove_file(&probe);
    ok
}

/// Whether pacman installed this executable (the release's Arch package or
/// the AUR's `lowquery-bin`).
fn owned_by_pacman(exe: &Path) -> bool {
    std::process::Command::new("pacman")
        .arg("-Qqo")
        .arg(exe)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
}

/// Update state: the policy is detected on first use (it may ask pacman), and
/// the update found by the last check is the one `install_update` installs.
#[derive(Default)]
pub struct Updates {
    policy: OnceLock<UpdatePolicy>,
    pending: Mutex<Option<Update>>,
}

impl Updates {
    fn policy(&self) -> &UpdatePolicy {
        self.policy.get_or_init(UpdatePolicy::detect)
    }
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AvailableUpdate {
    pub version: String,
    pub notes: String,
}

#[derive(Debug, Clone, Serialize)]
#[serde(
    tag = "event",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
pub enum InstallEvent {
    Progress {
        downloaded: u64,
        total: Option<u64>,
    },
    /// The download is verified and the installation started.
    Installing,
}

fn update_error(e: tauri_plugin_updater::Error) -> AppError {
    AppError::connection(e.to_string())
}

/// Async so the first call, which may ask pacman, stays off the main thread.
#[tauri::command]
pub async fn update_policy(updates: tauri::State<'_, Updates>) -> Result<UpdatePolicy, AppError> {
    Ok(updates.policy().clone())
}

#[tauri::command]
pub async fn check_update(
    app: AppHandle,
    updates: tauri::State<'_, Updates>,
) -> Result<Option<AvailableUpdate>, AppError> {
    let mut builder = app.updater_builder();
    if let Some(target) = &updates.policy().target {
        builder = builder.target(target);
    }
    let update = builder
        .build()
        .map_err(update_error)?
        .check()
        .await
        .map_err(update_error)?;
    let available = update.as_ref().map(|u| AvailableUpdate {
        version: u.version.clone(),
        notes: u.body.clone().unwrap_or_default(),
    });
    *updates.pending.lock() = update;
    Ok(available)
}

/// Downloads and verifies the update found by the last check, then installs
/// it. The interface restarts LowQuery afterwards; the Windows installer
/// closes it by itself.
#[tauri::command]
pub async fn install_update(
    updates: tauri::State<'_, Updates>,
    on_event: Channel<InstallEvent>,
) -> Result<(), AppError> {
    let policy = updates.policy().clone();
    if !policy.can_install {
        return Err(AppError::invalid(
            "This copy of LowQuery cannot install updates by itself.",
        ));
    }
    let update = updates
        .pending
        .lock()
        .clone()
        .ok_or_else(|| AppError::invalid("No update to install: check for updates first."))?;

    let mut downloaded = 0u64;
    let bytes = update
        .download(
            |chunk, total| {
                downloaded += chunk as u64;
                let _ = on_event.send(InstallEvent::Progress { downloaded, total });
            },
            || {},
        )
        .await
        .map_err(update_error)?;
    let _ = on_event.send(InstallEvent::Installing);

    tauri::async_runtime::spawn_blocking(move || match policy.channel {
        "pacman" => install_pacman(&bytes, &update.version),
        "portable" => install_portable(&bytes),
        _ => update.install(&bytes).map_err(update_error),
    })
    .await
    .map_err(|e| AppError::internal(e.to_string()))?
}

/// Installs a verified Arch package with `pkexec pacman -U`.
fn install_pacman(package: &[u8], version: &str) -> Result<(), AppError> {
    const ZSTD_MAGIC: [u8; 4] = [0x28, 0xb5, 0x2f, 0xfd];
    if !package.starts_with(&ZSTD_MAGIC) {
        return Err(AppError::invalid(
            "The update is not an Arch Linux package.",
        ));
    }
    let dir = PrivateDir::create()?;
    let path = dir.0.join(format!(
        "lowquery-{version}-{}.pkg.tar.zst",
        std::env::consts::ARCH
    ));
    std::fs::write(&path, package)?;

    let output = std::process::Command::new("pkexec")
        .args(["pacman", "-U", "--noconfirm"])
        .arg(&path)
        .output();
    match output {
        Ok(out) if out.status.success() => Ok(()),
        // pkexec: 126 when the password dialog is dismissed, 127 when not authorized.
        Ok(out) if matches!(out.status.code(), Some(126 | 127)) => Err(AppError::new(
            ErrorKind::Cancelled,
            "The update was not authorized, so LowQuery was not changed.",
        )),
        Ok(out) => {
            let stderr = String::from_utf8_lossy(&out.stderr);
            let reason = stderr.lines().rev().find(|l| !l.trim().is_empty());
            Err(AppError::internal(format!(
                "pacman could not install the update{}",
                reason.map(|r| format!(": {}", r.trim())).unwrap_or_default()
            )))
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Err(AppError::internal(
            "pkexec (polkit) is needed to install the update. Download the package and install it with pacman -U.",
        )),
        Err(e) => Err(e.into()),
    }
}

/// A folder only this user can write to, removed when dropped, so nobody can
/// swap the package between writing it and pacman reading it as root.
struct PrivateDir(PathBuf);

impl PrivateDir {
    fn create() -> std::io::Result<Self> {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.subsec_nanos())
            .unwrap_or_default();
        let dir =
            std::env::temp_dir().join(format!("lowquery-update-{}-{nanos}", std::process::id()));
        let mut builder = std::fs::DirBuilder::new();
        #[cfg(unix)]
        std::os::unix::fs::DirBuilderExt::mode(&mut builder, 0o700);
        builder.create(&dir)?;
        Ok(Self(dir))
    }
}

impl Drop for PrivateDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// Replaces the running executable with the one in the verified portable archive.
fn install_portable(archive: &[u8]) -> Result<(), AppError> {
    let exe = tauri::utils::platform::current_exe()?;
    let binary = portable_executable(archive)?;
    replace_executable(&exe, &binary)?;
    Ok(())
}

/// Upper bound for the executable read from an archive.
#[cfg(any(target_os = "linux", windows))]
const MAX_EXECUTABLE: u64 = 512 * 1024 * 1024;

/// `LowQuery` from the Linux portable `.tar.gz`.
#[cfg(target_os = "linux")]
fn portable_executable(archive: &[u8]) -> Result<Vec<u8>, AppError> {
    use std::io::Read;
    let mut tar = tar::Archive::new(flate2::read::GzDecoder::new(archive));
    for entry in tar.entries()? {
        let mut entry = entry?;
        let is_binary = entry.header().entry_type().is_file()
            && entry.path()?.file_name().is_some_and(|n| n == "LowQuery");
        if is_binary {
            let mut binary = Vec::new();
            entry
                .by_ref()
                .take(MAX_EXECUTABLE)
                .read_to_end(&mut binary)?;
            if !binary.starts_with(b"\x7fELF") {
                break;
            }
            return Ok(binary);
        }
    }
    Err(AppError::invalid(
        "The update archive does not contain the LowQuery executable.",
    ))
}

/// `LowQuery.exe` from the Windows portable `.zip`.
#[cfg(windows)]
fn portable_executable(archive: &[u8]) -> Result<Vec<u8>, AppError> {
    use std::io::Read;
    let missing = || AppError::invalid("The update archive does not contain LowQuery.exe.");
    let mut zip = zip::ZipArchive::new(std::io::Cursor::new(archive)).map_err(|_| missing())?;
    let entry = zip.by_name("LowQuery.exe").map_err(|_| missing())?;
    let mut binary = Vec::new();
    entry.take(MAX_EXECUTABLE).read_to_end(&mut binary)?;
    if !binary.starts_with(b"MZ") {
        return Err(missing());
    }
    Ok(binary)
}

#[cfg(not(any(target_os = "linux", windows)))]
fn portable_executable(_archive: &[u8]) -> Result<Vec<u8>, AppError> {
    Err(AppError::invalid(
        "Portable copies update themselves on Linux and Windows only.",
    ))
}

/// Writes the new executable next to the current one and renames it over,
/// keeping the file's permissions. The running process keeps the old inode.
#[cfg(unix)]
fn replace_executable(exe: &Path, binary: &[u8]) -> std::io::Result<()> {
    let staged = staged_path(exe);
    let permissions = std::fs::metadata(exe)?.permissions();
    std::fs::write(&staged, binary)?;
    let result =
        std::fs::set_permissions(&staged, permissions).and_then(|()| std::fs::rename(&staged, exe));
    if result.is_err() {
        let _ = std::fs::remove_file(&staged);
    }
    result
}

/// Windows cannot overwrite a running executable but can rename it: the old
/// one becomes `LowQuery.exe.old`, removed on the next start.
#[cfg(windows)]
fn replace_executable(exe: &Path, binary: &[u8]) -> std::io::Result<()> {
    let staged = staged_path(exe);
    let old = previous_executable(exe);
    std::fs::write(&staged, binary)?;
    let _ = std::fs::remove_file(&old);
    if let Err(e) = std::fs::rename(exe, &old) {
        let _ = std::fs::remove_file(&staged);
        return Err(e);
    }
    if let Err(e) = std::fs::rename(&staged, exe) {
        let _ = std::fs::rename(&old, exe);
        let _ = std::fs::remove_file(&staged);
        return Err(e);
    }
    Ok(())
}

fn staged_path(exe: &Path) -> PathBuf {
    let name = exe.file_name().unwrap_or_default().to_string_lossy();
    exe.with_file_name(format!(".{name}.update-{}", std::process::id()))
}

#[cfg(windows)]
fn previous_executable(exe: &Path) -> PathBuf {
    let mut name = exe.as_os_str().to_owned();
    name.push(".old");
    PathBuf::from(name)
}

/// Removes the executable a portable update replaced on Windows.
pub fn remove_replaced_executable() {
    #[cfg(windows)]
    if let Ok(exe) = tauri::utils::platform::current_exe() {
        let _ = std::fs::remove_file(previous_executable(&exe));
    }
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    fn portable_archive(entries: &[(&str, &[u8])]) -> Vec<u8> {
        let gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
        let mut tar = tar::Builder::new(gz);
        for (path, data) in entries {
            let mut header = tar::Header::new_gnu();
            header.set_size(data.len() as u64);
            header.set_mode(0o755);
            header.set_cksum();
            tar.append_data(&mut header, path, *data).unwrap();
        }
        tar.into_inner().unwrap().finish().unwrap()
    }

    #[test]
    fn portable_archive_yields_the_executable_not_the_docs() {
        let archive = portable_archive(&[
            ("./README.md", b"readme"),
            ("./LowQuery", b"\x7fELF new build"),
            ("./LICENSE", b"license"),
        ]);
        assert_eq!(portable_executable(&archive).unwrap(), b"\x7fELF new build");
    }

    #[test]
    fn portable_archive_without_an_executable_is_rejected() {
        let missing = portable_archive(&[("./README.md", b"readme")]);
        assert_eq!(
            portable_executable(&missing).unwrap_err().kind,
            ErrorKind::Invalid
        );
        let not_elf = portable_archive(&[("./LowQuery", b"#!/bin/sh")]);
        assert_eq!(
            portable_executable(&not_elf).unwrap_err().kind,
            ErrorKind::Invalid
        );
    }

    #[test]
    fn replacing_the_executable_keeps_its_permissions_and_leaves_nothing_behind() {
        let dir = std::env::temp_dir().join(format!("lq-replace-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let exe = dir.join("LowQuery");
        std::fs::write(&exe, b"old").unwrap();
        std::fs::set_permissions(&exe, std::fs::Permissions::from_mode(0o750)).unwrap();

        replace_executable(&exe, b"new").unwrap();

        assert_eq!(std::fs::read(&exe).unwrap(), b"new");
        let mode = std::fs::metadata(&exe).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o750);
        let files: Vec<_> = std::fs::read_dir(&dir)
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert_eq!(files, ["LowQuery"]);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn pacman_rejects_a_payload_that_is_not_a_package() {
        let err = install_pacman(b"\x7fELF", "1.0.0").unwrap_err();
        assert_eq!(err.kind, ErrorKind::Invalid);
    }
}
