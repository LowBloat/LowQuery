"""Write latest.json for the in-app updater from the signed release assets."""

import datetime
import json
import os
import subprocess
from pathlib import Path


def build_manifest(version, repo, assets, notes):
    base = f"https://github.com/{repo}/releases/download/v{version}"
    platforms = {}

    def signed(keys, asset):
        signature = assets / f"{asset}.sig"
        if (assets / asset).is_file() and signature.is_file():
            for key in keys:
                platforms[key] = {"signature": signature.read_text().strip(), "url": f"{base}/{asset}"}

    # Installed Windows copies update with the installer they came from.
    signed(["windows-x86_64", "windows-x86_64-nsis"], f"LowQuery-{version}-windows-x64-setup.exe")
    signed(["windows-x86_64-msi"], f"LowQuery-{version}-windows-x64.msi")
    signed(["windows-x86_64-portable"], f"LowQuery-{version}-windows-x64-portable.zip")
    signed(["darwin-aarch64", "darwin-x86_64"], f"LowQuery-{version}-macos-universal.app.tar.gz")
    # One key per Linux format and no generic one: each installation must
    # receive the format it was installed from (see src-tauri/src/updates.rs).
    signed(["linux-x86_64-appimage"], f"LowQuery-{version}-linux-x64.AppImage")
    signed(["linux-x86_64-deb"], f"LowQuery-{version}-linux-x64.deb")
    signed(["linux-x86_64-rpm"], f"LowQuery-{version}-linux-x64.rpm")
    signed(["linux-x86_64-pacman"], f"LowQuery-{version}-linux-x86_64.pkg.tar.zst")
    signed(["linux-x86_64-portable"], f"LowQuery-{version}-linux-x64-portable.tar.gz")

    return {
        "version": version,
        "notes": notes,
        "pub_date": datetime.datetime.now(datetime.timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ"),
        "platforms": platforms,
    }


if __name__ == "__main__":
    version = os.environ["VERSION"]
    repo = os.environ["GITHUB_REPOSITORY"]
    tag = os.environ["RELEASE_TAG"]
    if tag != f"v{version}":
        raise ValueError("Release tag must match the application version")
    notes = f"LowQuery {version}"
    result = subprocess.run(
        ["gh", "release", "view", tag, "--repo", repo, "--json", "body", "-q", ".body"],
        capture_output=True, text=True, check=False,
    )
    if result.returncode == 0 and result.stdout.strip():
        notes = result.stdout.strip()
    assets = Path("release-assets")
    doc = build_manifest(version, repo, assets, notes)
    required = {
        "windows-x86_64", "windows-x86_64-portable", "darwin-aarch64",
        "linux-x86_64-appimage", "linux-x86_64-deb", "linux-x86_64-rpm", "linux-x86_64-pacman",
        "linux-x86_64-portable",
    }
    missing = required - doc["platforms"].keys()
    if missing:
        raise SystemExit(f"Signed updater artifacts missing for: {sorted(missing)}")
    (assets / "latest.json").write_text(json.dumps(doc, indent=2), encoding="utf-8")
    print("latest.json platforms:", sorted(doc["platforms"]))
