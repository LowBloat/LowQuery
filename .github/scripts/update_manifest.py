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
    signed(["darwin-aarch64", "darwin-x86_64"], f"LowQuery-{version}-macos-universal.app.tar.gz")
    # No generic Linux key: .deb and .rpm installs must never receive an AppImage.
    signed(["linux-x86_64-appimage"], f"LowQuery-{version}-linux-x64.AppImage")

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
    required = {"windows-x86_64", "darwin-aarch64", "linux-x86_64-appimage"}
    missing = required - doc["platforms"].keys()
    if missing:
        raise SystemExit(f"Signed updater artifacts missing for: {sorted(missing)}")
    (assets / "latest.json").write_text(json.dumps(doc, indent=2), encoding="utf-8")
    print("latest.json platforms:", sorted(doc["platforms"]))
