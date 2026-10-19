import tempfile
import unittest
from pathlib import Path

from render_pkgbuild import render_local, render_release, sha256


class RenderTest(unittest.TestCase):
    def test_release_recipe_has_real_checksums(self):
        with tempfile.TemporaryDirectory() as tmp:
            assets = Path(tmp)
            portable = assets / "LowQuery-1.2.3-linux-x64-portable.tar.gz"
            portable.write_bytes(b"portable")
            text = render_release("1.2.3", assets)
        self.assertIn("pkgver=1.2.3", text)
        self.assertIn(sha256_bytes(b"portable"), text)
        self.assertNotIn("__", text.split("package()")[0])

    def test_local_recipe_packages_the_built_files(self):
        with tempfile.TemporaryDirectory() as tmp:
            build = Path(tmp)
            for name in ["lowquery", "lowquery.desktop", "lowquery.png"]:
                (build / name).write_bytes(name.encode())
            text = render_local("1.2.3", build)
            expected = sha256(build / "lowquery")
        self.assertIn(expected, text)
        self.assertIn("source=('lowquery' 'lowquery.desktop' 'lowquery.png')", text)


def sha256_bytes(data):
    import hashlib

    return hashlib.sha256(data).hexdigest()


if __name__ == "__main__":
    unittest.main()
