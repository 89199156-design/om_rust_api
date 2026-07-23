from pathlib import Path
import tempfile
import unittest
from unittest import mock

from om_downloader.storage_guard import (
    DEFAULT_MINIMUM_FREE_BYTES,
    MINIMUM_ALLOWED_RESERVE_BYTES,
    configured_minimum_free_bytes,
    enforce_environment_storage_guard,
    require_strict_data_path,
)


class StorageGuardTests(unittest.TestCase):
    def test_default_and_explicit_reserve_are_validated(self) -> None:
        self.assertEqual(configured_minimum_free_bytes({}), DEFAULT_MINIMUM_FREE_BYTES)
        self.assertEqual(
            configured_minimum_free_bytes({"OM_DATA_MIN_FREE_BYTES": "2147483648"}),
            2_147_483_648,
        )
        with self.assertRaisesRegex(ValueError, "must be at least"):
            configured_minimum_free_bytes(
                {"OM_DATA_MIN_FREE_BYTES": str(MINIMUM_ALLOWED_RESERVE_BYTES - 1)}
            )

    def test_guard_is_disabled_without_an_explicit_strict_root(self) -> None:
        self.assertIsNone(
            enforce_environment_storage_guard(
                Path("/tmp/output"),
                environment={},
            )
        )

    def test_guard_rejects_a_path_outside_the_data_mount(self) -> None:
        with tempfile.TemporaryDirectory() as temporary_directory:
            root = Path(temporary_directory)
            data = root / "data"
            outside = root / "system"
            data.mkdir()
            outside.mkdir()
            with mock.patch("os.path.ismount", return_value=True):
                with self.assertRaisesRegex(ValueError, "escapes strict data root"):
                    require_strict_data_path(
                        outside,
                        required_root=data,
                        minimum_free_bytes=MINIMUM_ALLOWED_RESERVE_BYTES,
                    )

    def test_guard_rejects_a_data_root_on_the_system_device(self) -> None:
        with tempfile.TemporaryDirectory() as temporary_directory:
            data = Path(temporary_directory)
            with mock.patch("os.path.ismount", return_value=True):
                with self.assertRaisesRegex(ValueError, "shares the system filesystem"):
                    require_strict_data_path(
                        data,
                        required_root=data,
                        minimum_free_bytes=MINIMUM_ALLOWED_RESERVE_BYTES,
                    )

    def test_guard_rejects_capacity_below_write_plus_reserve(self) -> None:
        with tempfile.TemporaryDirectory() as temporary_directory:
            data = Path(temporary_directory)
            system_root = Path(data.resolve().anchor)
            with (
                mock.patch("os.path.ismount", return_value=True),
                mock.patch(
                    "om_downloader.storage_guard._device_id",
                    side_effect=lambda path: 100 if Path(path) == system_root else 200,
                ),
                mock.patch(
                    "om_downloader.storage_guard.shutil.disk_usage",
                    return_value=mock.Mock(free=1_000),
                ),
            ):
                with self.assertRaises(OSError) as raised:
                    require_strict_data_path(
                        data,
                        required_root=data,
                        minimum_free_bytes=800,
                        additional_bytes=201,
                    )
            self.assertEqual(raised.exception.errno, 28)


if __name__ == "__main__":
    unittest.main()
