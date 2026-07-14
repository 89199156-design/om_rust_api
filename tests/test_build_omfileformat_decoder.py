import hashlib
import json
import os
from pathlib import Path
import subprocess
import tempfile
import unittest


ROOT = Path(__file__).resolve().parents[1]
SCRIPT = ROOT / "scripts" / "build_omfileformat_decoder.sh"


def run(*args: str, cwd: Path, check: bool = True, env: dict[str, str] | None = None):
    return subprocess.run(
        list(args),
        cwd=cwd,
        env=env,
        text=True,
        capture_output=True,
        check=check,
    )


def make_source(root: Path, source_text: str = "int om_fixture(void) { return 7; }\n") -> tuple[Path, str]:
    source = root / "source"
    (source / "c/include").mkdir(parents=True)
    (source / "c/src").mkdir(parents=True)
    (source / "c/include/fixture.h").write_text("int om_fixture(void);\n", encoding="utf-8")
    (source / "c/src/fixture.c").write_text(source_text, encoding="utf-8")
    run("git", "init", "--quiet", cwd=source)
    run("git", "add", ".", cwd=source)
    run(
        "git",
        "-c",
        "user.name=Decoder Test",
        "-c",
        "user.email=decoder-test@example.invalid",
        "commit",
        "--quiet",
        "-m",
        "fixture",
        cwd=source,
    )
    revision = run("git", "rev-parse", "HEAD", cwd=source).stdout.strip()
    return source, revision


class BuildOmFileFormatDecoderTests(unittest.TestCase):
    def test_builds_pinned_clean_source_and_writes_matching_provenance(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            source, revision = make_source(root)
            output = root / "output"
            env = os.environ.copy()
            env.update(
                {
                    "OM_FILE_FORMAT_SRC": str(source),
                    "OM_FILE_FORMAT_REF": revision,
                    "OM_FILE_FORMAT_BUILD_JOBS": "2",
                }
            )

            run("bash", str(SCRIPT), str(output), cwd=ROOT, env=env)

            artifact = output / "libomfileformat.so"
            manifest = json.loads(
                (output / "libomfileformat.build.json").read_text(encoding="utf-8")
            )
            self.assertEqual(manifest["source_revision"], revision)
            self.assertEqual(
                manifest["artifact_sha256"], hashlib.sha256(artifact.read_bytes()).hexdigest()
            )
            self.assertTrue(os.access(artifact, os.X_OK))

    def test_rejects_dirty_source_without_replacing_existing_artifact(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            source, revision = make_source(root)
            (source / "c/src/fixture.c").write_text("not valid C\n", encoding="utf-8")
            output = root / "output"
            output.mkdir()
            artifact = output / "libomfileformat.so"
            artifact.write_bytes(b"existing-good-decoder")
            env = os.environ.copy()
            env.update(
                {
                    "OM_FILE_FORMAT_SRC": str(source),
                    "OM_FILE_FORMAT_REF": revision,
                }
            )

            completed = run(
                "bash", str(SCRIPT), str(output), cwd=ROOT, env=env, check=False
            )

            self.assertNotEqual(completed.returncode, 0)
            self.assertIn("clean Git worktree", completed.stderr)
            self.assertEqual(artifact.read_bytes(), b"existing-good-decoder")


if __name__ == "__main__":
    unittest.main()
