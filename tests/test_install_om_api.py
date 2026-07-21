import hashlib
import os
from pathlib import Path
import shutil
import subprocess
import tempfile
import textwrap
import unittest


REPOSITORY_ROOT = Path(__file__).resolve().parents[1]
INSTALL_SCRIPT = REPOSITORY_ROOT / "scripts" / "install_om_api.sh"
POSIX_DEPLOY_TESTS_AVAILABLE = os.name == "posix" and all(
    shutil.which(command) is not None for command in ("bash", "id", "install")
)
REQUIRED_DEM_LATITUDES = range(0, 59)


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

    @unittest.skipUnless(
        POSIX_DEPLOY_TESTS_AVAILABLE,
        "requires a POSIX deployment shell with id and install",
    )
    def test_installer_builds_and_installs_the_workspace_target_binary(self) -> None:
        real_install = shutil.which("install")
        real_id = shutil.which("id")
        self.assertIsNotNone(real_install)
        self.assertIsNotNone(real_id)

        with tempfile.TemporaryDirectory() as temporary_directory:
            test_root = Path(temporary_directory)
            app_root = test_root / "source"
            script_dir = app_root / "scripts"
            manifest_dir = app_root / "om_api"
            nginx_dir = app_root / "nginx"
            fake_bin = test_root / "fake-bin"
            install_root = test_root / "install"
            data_root = test_root / "data"
            dem_root = test_root / "dem"
            for directory in (
                script_dir,
                manifest_dir,
                nginx_dir,
                fake_bin,
                install_root / "native",
                data_root,
            ):
                directory.mkdir(parents=True, exist_ok=True)

            copied_installer = script_dir / INSTALL_SCRIPT.name
            shutil.copyfile(INSTALL_SCRIPT, copied_installer)
            copied_installer.chmod(0o755)
            (manifest_dir / "Cargo.toml").write_text(
                '[package]\nname = "om-api"\nversion = "0.0.0"\n',
                encoding="utf-8",
            )
            (nginx_dir / "om_client_api.conf").write_text("", encoding="utf-8")
            (install_root / "native" / "libomfileformat.so").write_bytes(b"decoder")
            self._write_dem_chunks(dem_root)

            static_source = test_root / "HSURF.om"
            static_source.write_bytes(b"official-static-elevation")
            static_sha256 = hashlib.sha256(static_source.read_bytes()).hexdigest()
            cargo_arguments_log = test_root / "cargo-arguments.log"
            cargo_target_log = test_root / "cargo-target.log"

            self._write_executable(
                fake_bin / "cargo",
                r"""
                #!/usr/bin/env bash
                set -euo pipefail
                printf '%s\n' "$@" > "$FAKE_CARGO_ARGUMENTS_LOG"
                manifest_path=""
                target_dir=""
                while [ "$#" -gt 0 ]; do
                  case "$1" in
                    --manifest-path)
                      manifest_path="$2"
                      shift 2
                      ;;
                    --target-dir)
                      target_dir="$2"
                      shift 2
                      ;;
                    *)
                      shift
                      ;;
                  esac
                done
                if [ -z "$manifest_path" ]; then
                  echo "fake cargo did not receive --manifest-path" >&2
                  exit 2
                fi
                if [ -z "$target_dir" ]; then
                  target_dir="$(cd "$(dirname "$manifest_path")/.." && pwd -P)/target"
                fi
                if [[ "$target_dir" != /* ]]; then
                  echo "fake cargo received a relative target directory: $target_dir" >&2
                  exit 2
                fi
                printf '%s\n' "$target_dir" > "$FAKE_CARGO_TARGET_LOG"
                mkdir -p "$target_dir/release"
                printf 'workspace-target-binary\n' > "$target_dir/release/om-api"
                chmod 0755 "$target_dir/release/om-api"
                """,
            )
            self._write_executable(
                fake_bin / "install",
                r"""
                #!/usr/bin/env bash
                set -euo pipefail
                destination="${!#}"
                if [[ "$destination" == /etc/* ]]; then
                  exit 0
                fi
                exec "$REAL_INSTALL" "$@"
                """,
            )
            self._write_executable(
                fake_bin / "id",
                r"""
                #!/usr/bin/env bash
                set -euo pipefail
                if [ "${1:-}" = "-u" ]; then
                  printf '0\n'
                  exit 0
                fi
                exec "$REAL_ID" "$@"
                """,
            )
            self._write_executable(
                fake_bin / "tee",
                "#!/usr/bin/env bash\nset -euo pipefail\ncat >/dev/null\n",
            )
            for command in ("ln", "nginx", "systemctl"):
                self._write_executable(
                    fake_bin / command,
                    "#!/usr/bin/env bash\nset -euo pipefail\nexit 0\n",
                )

            environment = os.environ.copy()
            environment.update(
                {
                    "PATH": f"{fake_bin}{os.pathsep}{environment['PATH']}",
                    "FAKE_CARGO_ARGUMENTS_LOG": str(cargo_arguments_log),
                    "FAKE_CARGO_TARGET_LOG": str(cargo_target_log),
                    "REAL_ID": str(real_id),
                    "REAL_INSTALL": str(real_install),
                    "OM_DATA_ROOT": str(data_root),
                    "OM_API_DEM_ROOT": str(dem_root),
                    "OM_GFS013_STATIC_URL": static_source.resolve().as_uri(),
                    "OM_GFS013_STATIC_SHA256": static_sha256,
                    "OM_GFS025_STATIC_URL": static_source.resolve().as_uri(),
                    "OM_GFS025_STATIC_SHA256": static_sha256,
                    "OM_API_SERVICE_NAME": "weather-om-api-test",
                }
            )
            result = subprocess.run(
                ["bash", str(copied_installer), str(install_root)],
                cwd=app_root,
                env=environment,
                text=True,
                capture_output=True,
                check=False,
            )

            self.assertEqual(
                result.returncode,
                0,
                msg=f"stdout:\n{result.stdout}\nstderr:\n{result.stderr}",
            )
            expected_target = (app_root / "target").resolve()
            self.assertEqual(
                Path(cargo_target_log.read_text(encoding="utf-8").strip()),
                expected_target,
            )
            cargo_arguments = cargo_arguments_log.read_text(
                encoding="utf-8"
            ).splitlines()
            self.assertIn("--release", cargo_arguments)
            binary_option = cargo_arguments.index("--bin")
            self.assertEqual(cargo_arguments[binary_option + 1], "om-api")
            manifest_option = cargo_arguments.index("--manifest-path")
            self.assertEqual(
                Path(cargo_arguments[manifest_option + 1]),
                manifest_dir / "Cargo.toml",
            )
            target_option = cargo_arguments.index("--target-dir")
            self.assertEqual(Path(cargo_arguments[target_option + 1]), expected_target)
            self.assertEqual(
                (install_root / "bin" / "om-api").read_text(encoding="utf-8"),
                "workspace-target-binary\n",
            )
            service_environment = (
                install_root / "weather-om-api-test.env"
            ).read_text(encoding="utf-8")
            self.assertIn(f"OM_DEM_ROOT={dem_root}\n", service_environment)
            self.assertNotIn("OM_API_DEM_ROOT=", service_environment)
            self.assertFalse((manifest_dir / "target").exists())

    @unittest.skipUnless(
        POSIX_DEPLOY_TESTS_AVAILABLE,
        "requires a POSIX deployment shell with id and install",
    )
    def test_installer_fails_closed_when_a_required_dem_chunk_is_missing(self) -> None:
        with tempfile.TemporaryDirectory() as temporary_directory:
            test_root = Path(temporary_directory)
            data_root = test_root / "data"
            dem_root = test_root / "dem"
            install_root = test_root / "install"
            data_root.mkdir()
            self._write_dem_chunks(dem_root, missing_latitude=31)

            environment = os.environ.copy()
            environment.update(
                {
                    "OM_DATA_ROOT": str(data_root),
                    "OM_API_DEM_ROOT": str(dem_root),
                    "OM_API_CARGO_TARGET_DIR": str(test_root / "target"),
                }
            )
            result = subprocess.run(
                ["bash", str(INSTALL_SCRIPT), str(install_root)],
                cwd=REPOSITORY_ROOT,
                env=environment,
                text=True,
                capture_output=True,
                check=False,
            )

            missing_chunk = (
                dem_root / "copernicus_dem90" / "static" / "lat_31.om"
            )
            self.assertEqual(result.returncode, 1)
            self.assertIn(
                f"required Copernicus DEM90 chunk is missing or empty: {missing_chunk}",
                result.stderr,
            )
            self.assertFalse(install_root.exists())

    @unittest.skipUnless(
        POSIX_DEPLOY_TESTS_AVAILABLE,
        "requires a POSIX deployment shell with id and install",
    )
    def test_installer_rejects_a_relative_cargo_target_directory(self) -> None:
        environment = os.environ.copy()
        environment["OM_API_CARGO_TARGET_DIR"] = "relative-target"
        result = subprocess.run(
            ["bash", str(INSTALL_SCRIPT), "/tmp/weather-om-api-unused"],
            cwd=REPOSITORY_ROOT,
            env=environment,
            text=True,
            capture_output=True,
            check=False,
        )

        self.assertEqual(result.returncode, 2)
        self.assertIn(
            "OM_API_CARGO_TARGET_DIR must be an absolute path: relative-target",
            result.stderr,
        )

    @staticmethod
    def _write_executable(path: Path, content: str) -> None:
        path.write_text(textwrap.dedent(content).lstrip(), encoding="utf-8")
        path.chmod(0o755)

    @staticmethod
    def _write_dem_chunks(root: Path, missing_latitude: int | None = None) -> None:
        static_directory = root / "copernicus_dem90" / "static"
        static_directory.mkdir(parents=True)
        for latitude in REQUIRED_DEM_LATITUDES:
            if latitude != missing_latitude:
                (static_directory / f"lat_{latitude}.om").write_bytes(b"OM")


if __name__ == "__main__":
    unittest.main()
