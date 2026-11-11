//! Update policy: which installations can update themselves.
//!
//! Windows installers (NSIS/MSI), macOS app bundles, writable AppImages and
//! .deb/.rpm installs download the signed update, install it and restart. For
//! .deb and .rpm the updater hands the new package to dpkg/rpm through pkexec,
//! which asks for the administrator password. The Arch package (pacman owns
//! it) and portable copies have no installer, so they are only told a new
//! version exists.

use serde::Serialize;

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct UpdatePolicy {
    /// `installer`, `appimage`, `deb`, `rpm`, `package` or `portable`.
    pub channel: &'static str,
    pub can_install: bool,
    /// Manifest platform key when the default one does not apply.
    pub target: Option<String>,
}

impl UpdatePolicy {
    pub fn detect() -> Self {
        use tauri::utils::config::BundleType;
        let bundle = tauri::utils::platform::bundle_type();

        if cfg!(target_os = "linux") {
            if let Some(appimage) = std::env::var_os("APPIMAGE").filter(|p| !p.is_empty()) {
                let writable = std::path::Path::new(&appimage)
                    .parent()
                    .is_some_and(dir_is_writable);
                return Self {
                    channel: "appimage",
                    can_install: writable,
                    target: Some(format!("linux-{}-appimage", std::env::consts::ARCH)),
                };
            }
            let exe = std::env::current_exe().ok();
            // The updater picks dpkg or rpm from the bundle type built into the
            // binary; only trust it when that package manager really owns this
            // file (a binary copied into the Arch package keeps a marker too).
            let package = match bundle {
                Some(BundleType::Deb)
                    if exe
                        .as_deref()
                        .is_some_and(|p| owned_by(p, "dpkg-query", "-S")) =>
                {
                    Some("deb")
                }
                Some(BundleType::Rpm)
                    if exe.as_deref().is_some_and(|p| owned_by(p, "rpm", "-qf")) =>
                {
                    Some("rpm")
                }
                _ => None,
            };
            if let Some(channel) = package {
                return Self {
                    channel,
                    can_install: true,
                    target: Some(format!("linux-{}-{channel}", std::env::consts::ARCH)),
                };
            }
            let channel = if exe.is_some_and(|p| p.starts_with("/usr") || p.starts_with("/opt")) {
                "package"
            } else {
                "portable"
            };
            return Self {
                channel,
                can_install: false,
                target: None,
            };
        }

        // Windows: only an installed copy can be replaced by the installer.
        // macOS reports `App` for every bundle.
        let installed = matches!(
            bundle,
            Some(BundleType::Nsis | BundleType::Msi | BundleType::App)
        );
        Self {
            channel: if installed { "installer" } else { "portable" },
            can_install: installed,
            target: None,
        }
    }
}

/// Whether `program` (dpkg-query -S, rpm -qf) names a package that installed `path`.
fn owned_by(path: &std::path::Path, program: &str, flag: &str) -> bool {
    std::process::Command::new(program)
        .arg(flag)
        .arg(path)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
}

fn dir_is_writable(dir: &std::path::Path) -> bool {
    let probe = dir.join(format!(".lowquery-update-check-{}", std::process::id()));
    let ok = std::fs::write(&probe, b"").is_ok();
    let _ = std::fs::remove_file(&probe);
    ok
}

#[tauri::command]
pub fn update_policy(policy: tauri::State<'_, UpdatePolicy>) -> UpdatePolicy {
    policy.inner().clone()
}
