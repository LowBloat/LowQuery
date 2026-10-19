"""Render the Arch PKGBUILD templates with the release version and real checksums.

Usage:
  python render_pkgbuild.py release VERSION ASSETS_DIR   -> ASSETS_DIR/PKGBUILD (downloads the portable build)
  python render_pkgbuild.py local VERSION BUILD_DIR      -> BUILD_DIR/PKGBUILD (packages files already in BUILD_DIR)
"""

import hashlib
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]


def sha256(path):
    digest = hashlib.sha256()
    with open(path, "rb") as handle:
        for block in iter(lambda: handle.read(1 << 20), b""):
            digest.update(block)
    return digest.hexdigest()


def fill(template, values):
    template = template.replace("\r\n", "\n")
    for placeholder, value in values.items():
        if placeholder not in template:
            raise ValueError(f"PKGBUILD template lacks {placeholder}")
        template = template.replace(placeholder, value)
    if "SKIP" in template or "__" in template.split("package()")[0]:
        raise ValueError("PKGBUILD still contains unchecked sources")
    return template


def render_release(version, assets, root=ROOT):
    return fill(
        (root / "packaging/arch/PKGBUILD").read_text(encoding="utf-8"),
        {
            "__VERSION__": version,
            "__SHA256_PORTABLE__": sha256(assets / f"LowQuery-{version}-linux-x64-portable.tar.gz"),
            "__SHA256_DESKTOP__": sha256(root / "packaging/arch/lowquery.desktop"),
            "__SHA256_ICON__": sha256(root / "src-tauri/icons/128x128.png"),
        },
    )


def render_local(version, build, root=ROOT):
    return fill(
        (root / "packaging/arch/PKGBUILD.ci").read_text(encoding="utf-8"),
        {
            "__VERSION__": version,
            "__SHA256_BIN__": sha256(build / "lowquery"),
            "__SHA256_DESKTOP__": sha256(build / "lowquery.desktop"),
            "__SHA256_ICON__": sha256(build / "lowquery.png"),
        },
    )


if __name__ == "__main__":
    mode, version, folder = sys.argv[1], sys.argv[2], Path(sys.argv[3])
    text = render_release(version, folder) if mode == "release" else render_local(version, folder)
    (folder / "PKGBUILD").write_text(text, encoding="utf-8", newline="\n")
    print(f"Rendered {folder / 'PKGBUILD'}")
