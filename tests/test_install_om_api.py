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
CODEC_BUILD_SCRIPT = REPOSITORY_ROOT / "scripts" / "build_omfileformat_decoder.sh"
PINNED_CODEC_REVISION = "71f422b2706d8a81f1cecf52ae3073990de1ddbe"
TEST_SOURCE_REVISION = "0123456789abcdef0123456789abcdef01234567"
POSIX_DEPLOY_TESTS_AVAILABLE = os.name == "posix" and all(
    shutil.which(command) is not None for command in ("bash", "id", "install")
)
REQUIRED_DEM_LATITUDES = range(0, 59)


class InstallOmApiTests(unittest.TestCase):
    @classmethod
    def setUpClass(cls) -> None:
        cls.content = INSTALL_SCRIPT.read_text(encoding="utf-8")

    def test_pins_all_official_model_static_assets(self) -> None:
        expected_values = (
            "https://openmeteo.s3.amazonaws.com/data/ncep_gfs013/static/HSURF.om",
            "203745df4dfa10069e1a39206350e006818a0eea644bb19c1668c0f32f7475e0",
            "https://openmeteo.s3.amazonaws.com/data/ncep_gfs025/static/HSURF.om",
            "fdd9587e606e64d6d85474c703b9898669d230aac1574fc460cc3087227e868d",
            "https://openmeteo.s3.amazonaws.com/data/ecmwf_ifs025/static/HSURF.om",
            "935d56ba000b438b61504fbc271bfaa8f70db2acb541d58d5b466a24d294a9fb",
        )
        for expected in expected_values:
            with self.subTest(expected=expected):
                self.assertIn(expected, self.content)

        self.assertIn(PINNED_CODEC_REVISION, self.content)

        self.assertIn(
            'API_DEM_ROOT="${OM_API_DEM_ROOT:-$INSTALL_DIR/static}"',
            self.content,
        )
        self.assertIn(
            'MODEL_STATIC_ROOT="${OM_API_MODEL_STATIC_ROOT:-$INSTALL_DIR}"',
            self.content,
        )
        self.assertNotIn("/data/om_static", self.content)
        self.assertNotIn('$DATA_ROOT/static/', self.content)

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
        self.assertIn(
            '"ECMWF025" "$ECMWF025_STATIC_URL" "$ECMWF025_STATIC_SHA256" '
            '"$ECMWF025_STATIC_PATH"',
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
        self.assertIn(
            'run_privileged install -m 0644 -- "$ENV_FILE_TMP" "$ENV_FILE"',
            self.content,
        )

    def test_service_file_limit_covers_native_forecast_inventory(self) -> None:
        self.assertIn("LimitNOFILE=65536", self.content)

    def test_installer_rejects_bad_download_without_leaving_a_target(self) -> None:
        functions_start = self.content.index("run_privileged() {")
        functions_end = self.content.index(
            "\ninstall_verified_static_asset \\\n", functions_start
        )
        functions = self.content[functions_start:functions_end]
        harness = f"""
set -euo pipefail
SUDO=""
test_root="$(mktemp -d)"
trap 'rm -rf -- "$test_root"' EXIT
MODEL_STATIC_ROOT="$test_root/model-static"
API_DEM_ROOT="$test_root/dem"
mkdir -p "$API_DEM_ROOT"
{functions}

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
            self._write_executable(
                script_dir / CODEC_BUILD_SCRIPT.name,
                """
                #!/usr/bin/env bash
                set -euo pipefail
                test "${1:-}" = "--verify"
                test -s "${2:-}"
                printf 'verified=%s\n' "$2"
                """,
            )
            (manifest_dir / "Cargo.toml").write_text(
                '[package]\nname = "om-api"\nversion = "0.0.0"\n',
                encoding="utf-8",
            )
            (nginx_dir / "om_client_api.conf").write_text("", encoding="utf-8")
            (install_root / "native" / "libomfileformat.so").write_bytes(b"decoder")
            (
                install_root / "native" / "om-file-format.source-revision"
            ).write_text(f"{PINNED_CODEC_REVISION}\n", encoding="utf-8")
            self._write_dem_chunks(dem_root)

            static_source = test_root / "HSURF.om"
            static_source.write_bytes(b"official-static-elevation")
            static_sha256 = hashlib.sha256(static_source.read_bytes()).hexdigest()
            source_archive = test_root / "corresponding-source.tar.gz"
            source_archive.write_bytes(b"tested-corresponding-source")
            source_archive_sha256 = hashlib.sha256(source_archive.read_bytes()).hexdigest()
            cargo_arguments_log = test_root / "cargo-arguments.log"
            cargo_target_log = test_root / "cargo-target.log"
            build_revision_log = test_root / "build-revision.log"

            self._write_executable(
                fake_bin / "cargo",
                r"""
                #!/usr/bin/env bash
                set -euo pipefail
                printf '%s\n' "$@" > "$FAKE_CARGO_ARGUMENTS_LOG"
                printf '%s\n' "${OM_BUILD_REVISION:-}" > "$FAKE_BUILD_REVISION_LOG"
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
                printf 'workspace-materializer-binary\n' > "$target_dir/release/om-native-materialize"
                chmod 0755 "$target_dir/release/om-native-materialize"
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
                    "FAKE_BUILD_REVISION_LOG": str(build_revision_log),
                    "REAL_ID": str(real_id),
                    "REAL_INSTALL": str(real_install),
                    "OM_DATA_ROOT": str(data_root),
                    "OM_API_DEM_ROOT": str(dem_root),
                    "OM_GFS013_STATIC_URL": static_source.resolve().as_uri(),
                    "OM_GFS013_STATIC_SHA256": static_sha256,
                    "OM_GFS025_STATIC_URL": static_source.resolve().as_uri(),
                    "OM_GFS025_STATIC_SHA256": static_sha256,
                    "OM_ECMWF025_STATIC_URL": static_source.resolve().as_uri(),
                    "OM_ECMWF025_STATIC_SHA256": static_sha256,
                    "OM_API_SOURCE_ARCHIVE": str(source_archive),
                    "OM_API_SOURCE_ARCHIVE_SHA256": source_archive_sha256,
                    "OM_API_SERVICE_NAME": "weather-om-api-test",
                    "OM_API_SOURCE_REVISION": TEST_SOURCE_REVISION,
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
            requested_binaries = [
                cargo_arguments[index + 1]
                for index, argument in enumerate(cargo_arguments)
                if argument == "--bin"
            ]
            self.assertEqual(
                requested_binaries,
                ["om-api", "om-native-materialize"],
            )
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
            self.assertEqual(
                (
                    install_root / "bin" / "om-native-materialize"
                ).read_text(encoding="utf-8"),
                "workspace-materializer-binary\n",
            )
            self.assertEqual(
                (install_root / "source-revision").read_text(encoding="utf-8"),
                f"{TEST_SOURCE_REVISION}\n",
            )
            self.assertEqual(
                build_revision_log.read_text(encoding="utf-8"),
                f"{TEST_SOURCE_REVISION}\n",
            )
            installed_source_archive = (
                install_root
                / "source-archives"
                / f"om_weather_server-{TEST_SOURCE_REVISION}.tar.gz"
            )
            self.assertEqual(installed_source_archive.read_bytes(), source_archive.read_bytes())
            self.assertEqual(
                (
                    installed_source_archive.parent
                    / f"{installed_source_archive.name}.sha256"
                ).read_text(encoding="utf-8"),
                f"{source_archive_sha256}  {installed_source_archive.name}\n",
            )
            service_environment = (
                install_root / "weather-om-api-test.env"
            ).read_text(encoding="utf-8")
            self.assertIn(f"OM_DEM_ROOT={dem_root}\n", service_environment)
            self.assertIn(
                f"OM_MODEL_STATIC_ROOT={install_root}\n",
                service_environment,
            )
            self.assertNotIn("OM_API_DEM_ROOT=", service_environment)
            self.assertFalse((data_root / "static").exists())
            for model in ("ncep_gfs013", "ncep_gfs025", "ecmwf_ifs025"):
                self.assertEqual(
                    (install_root / "static" / model / "HSURF.om").read_bytes(),
                    b"official-static-elevation",
                )
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
            source_root = test_root / "source"
            script_dir = source_root / "scripts"
            script_dir.mkdir(parents=True)
            copied_installer = script_dir / INSTALL_SCRIPT.name
            shutil.copyfile(INSTALL_SCRIPT, copied_installer)
            copied_installer.chmod(0o755)
            data_root.mkdir()
            self._write_dem_chunks(dem_root, missing_latitude=31)

            environment = os.environ.copy()
            environment.update(
                {
                    "OM_DATA_ROOT": str(data_root),
                    "OM_API_DEM_ROOT": str(dem_root),
                    "OM_API_CARGO_TARGET_DIR": str(test_root / "target"),
                    "OM_API_SOURCE_REVISION": TEST_SOURCE_REVISION,
                }
            )
            result = subprocess.run(
                ["bash", str(copied_installer), str(install_root)],
                cwd=source_root,
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

    @unittest.skipUnless(
        POSIX_DEPLOY_TESTS_AVAILABLE,
        "requires a POSIX deployment shell",
    )
    def test_installer_rejects_an_invalid_explicit_source_revision(self) -> None:
        with tempfile.TemporaryDirectory() as temporary_directory:
            test_root = Path(temporary_directory)
            source_root = test_root / "source"
            script_dir = source_root / "scripts"
            script_dir.mkdir(parents=True)
            copied_installer = script_dir / INSTALL_SCRIPT.name
            shutil.copyfile(INSTALL_SCRIPT, copied_installer)
            copied_installer.chmod(0o755)

            environment = os.environ.copy()
            environment["OM_API_SOURCE_REVISION"] = "not-a-full-git-sha"
            result = subprocess.run(
                ["bash", str(copied_installer), str(test_root / "install")],
                cwd=source_root,
                env=environment,
                text=True,
                capture_output=True,
                check=False,
            )

            self.assertEqual(result.returncode, 1)
            self.assertIn(
                "source revision must be a full lowercase 40-character Git SHA",
                result.stderr,
            )

    @unittest.skipUnless(
        POSIX_DEPLOY_TESTS_AVAILABLE and shutil.which("git") is not None,
        "requires a POSIX deployment shell and Git",
    )
    def test_installer_refuses_a_dirty_source_worktree(self) -> None:
        with tempfile.TemporaryDirectory() as temporary_directory:
            source_root = Path(temporary_directory) / "source"
            script_dir = source_root / "scripts"
            script_dir.mkdir(parents=True)
            copied_installer = script_dir / INSTALL_SCRIPT.name
            shutil.copyfile(INSTALL_SCRIPT, copied_installer)
            copied_installer.chmod(0o755)
            tracked_file = source_root / "tracked.txt"
            tracked_file.write_text("clean\n", encoding="utf-8")
            subprocess.run(
                ["git", "init", "--quiet"],
                cwd=source_root,
                check=True,
            )
            subprocess.run(
                ["git", "add", "."],
                cwd=source_root,
                check=True,
            )
            subprocess.run(
                [
                    "git",
                    "-c",
                    "user.name=OM Installer Test",
                    "-c",
                    "user.email=om-installer-test@example.invalid",
                    "commit",
                    "--quiet",
                    "-m",
                    "test fixture",
                ],
                cwd=source_root,
                check=True,
            )
            revision = subprocess.check_output(
                ["git", "rev-parse", "HEAD"],
                cwd=source_root,
                text=True,
            ).strip()
            tracked_file.write_text("dirty\n", encoding="utf-8")

            environment = os.environ.copy()
            environment["OM_API_SOURCE_REVISION"] = revision
            result = subprocess.run(
                ["bash", str(copied_installer), str(source_root / "install")],
                cwd=source_root,
                env=environment,
                text=True,
                capture_output=True,
                check=False,
            )

            self.assertEqual(result.returncode, 1)
            self.assertIn(
                f"refusing to deploy from a dirty source worktree: {source_root}",
                result.stderr,
            )
            self.assertIn(" M tracked.txt", result.stderr)

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
