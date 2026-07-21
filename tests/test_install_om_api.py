from pathlib import Path
import subprocess
import unittest


REPOSITORY_ROOT = Path(__file__).resolve().parents[1]
INSTALL_SCRIPT = REPOSITORY_ROOT / "scripts" / "install_om_api.sh"


class InstallOmApiTests(unittest.TestCase):
    @classmethod
    def setUpClass(cls) -> None:
        cls.content = INSTALL_SCRIPT.read_text(encoding="utf-8")

    def test_pins_both_official_gfs_static_assets(self) -> None:
        expected_values = (
            "https://openmeteo.s3.amazonaws.com/data/ncep_gfs013/static/HSURF.om",
            "203745df4dfa10069e1a39206350e006818a0eea644bb19c1668c0f32f7475e0",
            "https://openmeteo.s3.amazonaws.com/data/ncep_gfs025/static/HSURF.om",
            "fdd9587e606e64d6d85474c703b9898669d230aac1574fc460cc3087227e868d",
        )
        for expected in expected_values:
            with self.subTest(expected=expected):
                self.assertIn(expected, self.content)

        self.assertIn(
            '"GFS013" "$GFS013_STATIC_URL" "$GFS013_STATIC_SHA256" '
            '"$GFS013_STATIC_PATH"',
            self.content,
        )
        self.assertIn(
            '"GFS025" "$GFS025_STATIC_URL" "$GFS025_STATIC_SHA256" '
            '"$GFS025_STATIC_PATH"',
            self.content,
        )

    def test_shared_installer_verifies_staging_before_atomic_publish(self) -> None:
        self.assertEqual(self.content.count("install_verified_static_asset() ("), 1)
        self.assertIn(
            'staged_path="$(run_privileged mktemp '
            '"${target_path}.tmp.XXXXXX")"',
            self.content,
        )
        self.assertIn(
            'actual_sha256="$(run_privileged sha256sum -- "$staged_path"',
            self.content,
        )
        self.assertIn(
            'run_privileged mv -f -- "$staged_path" "$target_path"',
            self.content,
        )
        self.assertIn("remove_invalid_target=1", self.content)
        self.assertIn(
            'run_privileged rm -f -- "$target_path"',
            self.content,
        )

    def test_installer_rejects_bad_download_without_leaving_a_target(self) -> None:
        functions_start = self.content.index("run_privileged() {")
        functions_end = self.content.index(
            "\ninstall_verified_static_asset \\\n", functions_start
        )
        functions = self.content[functions_start:functions_end]
        harness = f"""
set -euo pipefail
SUDO=""
{functions}

test_root="$(mktemp -d)"
trap 'rm -rf -- "$test_root"' EXIT
source_path="$test_root/official.om"
bad_source_path="$test_root/bad.om"
target_path="$test_root/static/ncep_test/HSURF.om"
printf 'official-static-elevation' > "$source_path"
printf 'wrong-static-elevation' > "$bad_source_path"
expected_sha256="$(sha256sum -- "$source_path" | awk '{{print $1}}')"

install_verified_static_asset \\
  TEST "file://$source_path" "$expected_sha256" "$target_path"
test "$(sha256sum -- "$target_path" | awk '{{print $1}}')" = "$expected_sha256"

printf 'already-invalid' > "$target_path"
if install_verified_static_asset \\
  TEST "file://$bad_source_path" "$expected_sha256" "$target_path"; then
  echo 'bad static asset unexpectedly installed' >&2
  exit 1
fi
test ! -e "$target_path"
test -z "$(find "$(dirname -- "$target_path")" -maxdepth 1 -name 'HSURF.om.tmp.*' -print -quit)"
"""
        result = subprocess.run(
            ["bash", "-s"],
            input=harness.encode("utf-8"),
            capture_output=True,
            check=False,
        )
        self.assertEqual(
            result.returncode,
            0,
            msg=(
                f"stdout:\n{result.stdout.decode('utf-8', errors='replace')}\n"
                f"stderr:\n{result.stderr.decode('utf-8', errors='replace')}"
            ),
        )


if __name__ == "__main__":
    unittest.main()
