import tempfile
import unittest
from pathlib import Path

from update_manifest import build_manifest


class ManifestTest(unittest.TestCase):
    def test_signed_assets_become_platforms_with_one_key_per_linux_format(self):
        with tempfile.TemporaryDirectory() as tmp:
            assets = Path(tmp)
            for name in [
                "LowQuery-1.2.3-windows-x64-setup.exe",
                "LowQuery-1.2.3-windows-x64.msi",
                "LowQuery-1.2.3-macos-universal.app.tar.gz",
                "LowQuery-1.2.3-linux-x64.AppImage",
                "LowQuery-1.2.3-linux-x64.deb",
                "LowQuery-1.2.3-linux-x64.rpm",
                "LowQuery-1.2.3-linux-x86_64.pkg.tar.zst",
            ]:
                (assets / name).write_bytes(b"x")
                if not name.endswith(".zst"):
                    (assets / f"{name}.sig").write_text(f"sig-{name}\n")
            doc = build_manifest("1.2.3", "LowBloat/LowQuery", assets, "notes")
        self.assertEqual(
            sorted(doc["platforms"]),
            ["darwin-aarch64", "darwin-x86_64", "linux-x86_64-appimage", "linux-x86_64-deb",
             "linux-x86_64-rpm", "windows-x86_64", "windows-x86_64-msi", "windows-x86_64-nsis"],
        )
        self.assertNotIn("linux-x86_64", doc["platforms"])
        self.assertTrue(doc["platforms"]["linux-x86_64-deb"]["url"].endswith("/LowQuery-1.2.3-linux-x64.deb"))
        self.assertTrue(doc["platforms"]["linux-x86_64-rpm"]["url"].endswith("/LowQuery-1.2.3-linux-x64.rpm"))
        self.assertEqual(doc["platforms"]["windows-x86_64-msi"]["signature"], "sig-LowQuery-1.2.3-windows-x64.msi")
        self.assertTrue(doc["platforms"]["windows-x86_64"]["url"].endswith("/v1.2.3/LowQuery-1.2.3-windows-x64-setup.exe"))

    def test_unsigned_assets_are_skipped(self):
        with tempfile.TemporaryDirectory() as tmp:
            assets = Path(tmp)
            (assets / "LowQuery-1.2.3-windows-x64-setup.exe").write_bytes(b"x")
            doc = build_manifest("1.2.3", "LowBloat/LowQuery", assets, "notes")
        self.assertEqual(doc["platforms"], {})


if __name__ == "__main__":
    unittest.main()
