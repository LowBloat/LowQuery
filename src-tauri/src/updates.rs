//! Update policy: which installations can update themselves.
//!
//! Windows installers (NSIS/MSI), macOS app bundles and writable AppImages
//! download the signed update, install it and restart. Linux packages
//! (.deb/.rpm) belong to the system package manager and portable copies have
//! no installer, so they are only told a new version exists.

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
            let channel = match bundle {
                Some(BundleType::Deb) => "deb",
                Some(BundleType::Rpm) => "rpm",
                _ if std::env::current_exe()
                    .is_ok_and(|p| p.starts_with("/usr") || p.starts_with("/opt")) =>
                {
                    "package"
                }
                _ => "portable",
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
