import tempfile
import unittest
from pathlib import Path

from update_manifest import build_manifest


class ManifestTest(unittest.TestCase):
    def test_each_format_gets_its_own_platform_key(self):
        with tempfile.TemporaryDirectory() as tmp:
            assets = Path(tmp)
            for name in [
                "LowQuery-1.2.3-windows-x64-setup.exe",
                "LowQuery-1.2.3-windows-x64.msi",
                "LowQuery-1.2.3-windows-x64-portable.zip",
                "LowQuery-1.2.3-macos-universal.app.tar.gz",
                "LowQuery-1.2.3-linux-x64.AppImage",
                "LowQuery-1.2.3-linux-x64.deb",
                "LowQuery-1.2.3-linux-x64.rpm",
                "LowQuery-1.2.3-linux-x86_64.pkg.tar.zst",
                "LowQuery-1.2.3-linux-x64-portable.tar.gz",
            ]:
                (assets / name).write_bytes(b"x")
                (assets / f"{name}.sig").write_text(f"sig-{name}\n")
            doc = build_manifest("1.2.3", "LowBloat/LowQuery", assets, "notes")
        files = {key: entry["url"].rsplit("/", 1)[1] for key, entry in doc["platforms"].items()}
        self.assertEqual(files, {
            "windows-x86_64": "LowQuery-1.2.3-windows-x64-setup.exe",
            "windows-x86_64-nsis": "LowQuery-1.2.3-windows-x64-setup.exe",
            "windows-x86_64-msi": "LowQuery-1.2.3-windows-x64.msi",
            "windows-x86_64-portable": "LowQuery-1.2.3-windows-x64-portable.zip",
            "darwin-aarch64": "LowQuery-1.2.3-macos-universal.app.tar.gz",
            "darwin-x86_64": "LowQuery-1.2.3-macos-universal.app.tar.gz",
            "linux-x86_64-appimage": "LowQuery-1.2.3-linux-x64.AppImage",
            "linux-x86_64-deb": "LowQuery-1.2.3-linux-x64.deb",
            "linux-x86_64-rpm": "LowQuery-1.2.3-linux-x64.rpm",
            "linux-x86_64-pacman": "LowQuery-1.2.3-linux-x86_64.pkg.tar.zst",
            "linux-x86_64-portable": "LowQuery-1.2.3-linux-x64-portable.tar.gz",
        })
        self.assertEqual(doc["platforms"]["linux-x86_64-deb"]["signature"], "sig-LowQuery-1.2.3-linux-x64.deb")
        self.assertTrue(doc["platforms"]["windows-x86_64"]["url"].startswith(
            "https://github.com/LowBloat/LowQuery/releases/download/v1.2.3/"))

    def test_unsigned_assets_are_skipped(self):
        with tempfile.TemporaryDirectory() as tmp:
            assets = Path(tmp)
            (assets / "LowQuery-1.2.3-windows-x64-setup.exe").write_bytes(b"x")
            doc = build_manifest("1.2.3", "LowBloat/LowQuery", assets, "notes")
        self.assertEqual(doc["platforms"], {})


if __name__ == "__main__":
    unittest.main()
