import tempfile
import unittest
import zipfile
from pathlib import Path

from om_downloader.deploy_package import create_deploy_zip, iter_deploy_files


class DeployPackageTests(unittest.TestCase):
    def test_iter_deploy_files_excludes_runtime_data_and_python_cache(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            (root / "om_downloader").mkdir()
            (root / "om_downloader" / "cli.py").write_text("print('ok')", encoding="utf-8")
            (root / "om_downloader" / "__pycache__").mkdir()
            (root / "om_downloader" / "__pycache__" / "cli.pyc").write_bytes(b"cache")
            (root / "config").mkdir()
            (root / "config" / "models.json").write_text("{}", encoding="utf-8")
            (root / "data" / "published").mkdir(parents=True)
            (root / "data" / "published" / "latest.json").write_text("{}", encoding="utf-8")
            (root / "tests").mkdir()
            (root / "tests" / "test_cli.py").write_text("import unittest", encoding="utf-8")
            (root / "README.md").write_text("readme", encoding="utf-8")

            files = [str(item).replace("\\", "/") for item in iter_deploy_files(root)]

        self.assertIn("om_downloader/cli.py", files)
        self.assertIn("config/models.json", files)
        self.assertIn("README.md", files)
        self.assertNotIn("om_downloader/__pycache__/cli.pyc", files)
        self.assertFalse(any(item.startswith("data/") for item in files))
        self.assertFalse(any(item.startswith("tests/") for item in files))

    def test_create_deploy_zip_writes_expected_files(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp) / "project"
            root.mkdir()
            (root / "om_downloader").mkdir()
            (root / "om_downloader" / "cli.py").write_text("print('ok')", encoding="utf-8")
            (root / "README.md").write_text("readme", encoding="utf-8")
            output = Path(tmp) / "deploy.zip"

            create_deploy_zip(root, output)
            with zipfile.ZipFile(output) as archive:
                names = sorted(archive.namelist())

        self.assertEqual(names, ["README.md", "om_downloader/cli.py"])


if __name__ == "__main__":
    unittest.main()
