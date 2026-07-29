import hashlib
import io
import tempfile
import unittest
from pathlib import Path, PurePosixPath
from unittest.mock import patch

from om_downloader import cli as cli_module
from om_downloader import mirror_sync
from om_downloader.static_assets import (
    ECMWF_IFS025_HSURF,
    ECMWF_IFS025_ENSEMBLE_HSURF,
    NCEP_GEFS025_HSURF,
    NCEP_GEFS05_HSURF,
    OPENMETEO_STATIC_ASSETS,
    StaticAssetSpec,
    ensure_static_asset,
    static_asset_manifest_record,
    static_asset_path,
    verify_static_asset,
)


def _fixture_spec(payload: bytes) -> StaticAssetSpec:
    return StaticAssetSpec(
        model="ecmwf_ifs025",
        relative_path=PurePosixPath("static/ecmwf_ifs025/HSURF.om"),
        bucket_key=PurePosixPath("data/ecmwf_ifs025/static/HSURF.om"),
        bytes=len(payload),
        sha256=hashlib.sha256(payload).hexdigest(),
    )


class StaticAssetTests(unittest.TestCase):
    def test_probability_product_group_membership_is_consistent(self):
        expected = {
            "gfs": (
                "gfs013_surface",
                "gfs025",
                "gfs_pressure_profile",
                "ncep_gefs025",
                "ncep_gefs05",
            ),
            "ecmwf": ("ecmwf_ifs025", "ecmwf_ifs025_ensemble"),
        }

        for group, products in expected.items():
            with self.subTest(group=group):
                self.assertEqual(cli_module.OPENMETEO_GROUP_PRODUCTS[group], products)
                self.assertEqual(mirror_sync.OPENMETEO_GROUP_PRODUCTS[group], products)
                self.assertEqual(mirror_sync.MINIMUM_GROUP_PRODUCTS[group], products)

    def test_probability_static_asset_specs_are_pinned(self):
        expected = {
            "ncep_gefs025": (
                NCEP_GEFS025_HSURF,
                408_440,
                "09f589bd34aad27f538a5c0cb01c5dbd4114acc27d6b6f24524fd9451ce56f62",
            ),
            "ncep_gefs05": (
                NCEP_GEFS05_HSURF,
                118_088,
                "aeb5313b8f402b9098a268b476b480ed065593eb718c358ad01fada0f0f6ddcb",
            ),
            "ecmwf_ifs025_ensemble": (
                ECMWF_IFS025_ENSEMBLE_HSURF,
                433_744,
                "c137997ccae161da2f79bd4ae3e00e03aa0a94cf351d5fd69b88a03191c1e2be",
            ),
        }

        for model, (spec, expected_bytes, expected_sha256) in expected.items():
            with self.subTest(model=model):
                self.assertIs(OPENMETEO_STATIC_ASSETS[model], spec)
                self.assertEqual(spec.model, model)
                self.assertEqual(
                    spec.relative_path,
                    PurePosixPath(f"static/{model}/HSURF.om"),
                )
                self.assertEqual(
                    spec.bucket_key,
                    PurePosixPath(f"data/{model}/static/HSURF.om"),
                )
                self.assertEqual(spec.bytes, expected_bytes)
                self.assertEqual(spec.sha256, expected_sha256)

    def test_ecmwf_group_manifest_records_static_asset_identity(self):
        deterministic_record = static_asset_manifest_record(
            ECMWF_IFS025_HSURF,
            bucket_url="https://openmeteo.s3.amazonaws.com",
        )
        ensemble_record = static_asset_manifest_record(
            ECMWF_IFS025_ENSEMBLE_HSURF,
            bucket_url="https://openmeteo.s3.amazonaws.com",
        )
        product_manifest = {
            "status": "complete",
            "coverage_id": "ecmwf_ifs025_2026071818_86h",
            "latest_complete_run": "2026071818",
            "required_start_utc": "2026-07-18T10:00:00Z",
            "public_start_utc": "2026-07-18T16:00:00Z",
            "required_end_utc": "2026-08-02T12:00:00Z",
            "valid_time_count": 86,
            "files": [{"path": "coverages/example/ecmwf_ifs025.omranges"}],
            "bytes": 123,
            "downloaded_bytes": 123,
        }

        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            group = cli_module._write_group_manifest(
                root,
                "ecmwf",
                {
                    "ecmwf_ifs025": product_manifest,
                    "ecmwf_ifs025_ensemble": {
                        **product_manifest,
                        "coverage_id": "ecmwf_ifs025_ensemble_2026071818_85h",
                        "valid_time_count": 85,
                        "files": [
                            {
                                "path": (
                                    "coverages/example/"
                                    "ecmwf_ifs025_ensemble.omranges"
                                )
                            }
                        ],
                    },
                },
                {
                    "ecmwf_ifs025": deterministic_record,
                    "ecmwf_ifs025_ensemble": ensemble_record,
                },
            )
            release_id = mirror_sync.group_release_id(group)

            self.assertEqual(group["status"], "complete")
            self.assertEqual(
                group["static_assets"],
                {
                    "ecmwf_ifs025": deterministic_record,
                    "ecmwf_ifs025_ensemble": ensemble_record,
                },
            )
            self.assertEqual(group["static_asset_files"], 2)
            self.assertEqual(group["static_asset_bytes"], 433_648 + 433_744)
            self.assertTrue(
                (
                    root
                    / "published"
                    / "groups"
                    / "ecmwf"
                    / "releases"
                    / f"{release_id}.json"
                ).is_file()
            )

    def test_downloads_verifies_and_reuses_static_asset_atomically(self):
        payload = b"test ECMWF HSURF OM payload"
        spec = _fixture_spec(payload)
        requests = []

        def opener(url, *, timeout):
            requests.append((url, timeout))
            return io.BytesIO(payload)

        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            first = ensure_static_asset(
                root,
                spec,
                bucket_url="https://openmeteo.example",
                opener=opener,
            )
            second = ensure_static_asset(
                root,
                spec,
                bucket_url="https://openmeteo.example",
                opener=opener,
            )
            target = static_asset_path(root, spec)
            target.write_bytes(b"corrupt")
            third = ensure_static_asset(
                root,
                spec,
                bucket_url="https://openmeteo.example",
                opener=opener,
            )

            self.assertEqual(first["status"], "downloaded")
            self.assertEqual(second["status"], "reused")
            self.assertEqual(third["status"], "downloaded")
            self.assertEqual(target.read_bytes(), payload)
            self.assertTrue(verify_static_asset(target, spec))
            self.assertFalse(target.with_name("HSURF.om.download.tmp").exists())
            self.assertEqual(
                requests,
                [
                    ("https://openmeteo.example/data/ecmwf_ifs025/static/HSURF.om", 60),
                    ("https://openmeteo.example/data/ecmwf_ifs025/static/HSURF.om", 60),
                ],
            )

    def test_failed_verification_keeps_existing_file_and_removes_temporary(self):
        payload = b"expected"
        spec = _fixture_spec(payload)

        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            target = static_asset_path(root, spec)
            target.parent.mkdir(parents=True)
            target.write_bytes(b"previous-corrupt-file")

            with self.assertRaises(ValueError):
                ensure_static_asset(
                    root,
                    spec,
                    bucket_url="https://openmeteo.example",
                    opener=lambda *_args, **_kwargs: io.BytesIO(b"wrong"),
                )

            self.assertEqual(target.read_bytes(), b"previous-corrupt-file")
            self.assertFalse(target.with_name("HSURF.om.download.tmp").exists())

    def test_mirror_uses_external_system_static_asset_without_data_disk_copy(self):
        payload = b"mirrored ECMWF static payload"
        spec = _fixture_spec(payload)
        record = static_asset_manifest_record(
            spec,
            bucket_url="https://openmeteo.example",
        )
        group_manifest = {
            "group": "ecmwf",
            "status": "complete",
            "latest_complete_run": "2026071818",
            "product_manifests": {
                "ecmwf_ifs025": {
                    "status": "complete",
                    "latest_complete_run": "2026071818",
                },
                "ecmwf_ifs025_ensemble": {
                    "status": "complete",
                    "latest_complete_run": "2026071812",
                },
            },
            "static_assets": {"ecmwf_ifs025": record},
        }

        with tempfile.TemporaryDirectory() as mirror_tmp, tempfile.TemporaryDirectory() as out_tmp:
            mirror_root = Path(mirror_tmp)
            output_root = Path(out_tmp)
            with patch.dict(
                mirror_sync.OPENMETEO_STATIC_ASSETS,
                {"ecmwf_ifs025": spec},
                clear=True,
            ):
                self.assertTrue(mirror_sync._group_manifest_is_complete(group_manifest, "ecmwf"))
                stages = mirror_sync._prepare_static_asset_stages(
                    group_manifest,
                    mirror_root,
                    output_root,
                    "ecmwf",
                )
                self.assertFalse(static_asset_path(output_root, spec).exists())
                result = mirror_sync._promote_static_asset_stages(stages)
                self.assertTrue(
                    mirror_sync._local_static_assets_match(
                        group_manifest,
                        output_root,
                        "ecmwf",
                    )
                )

            self.assertEqual(stages, [])
            self.assertEqual(result, [])
            self.assertFalse(static_asset_path(mirror_root, spec).exists())
            self.assertFalse(static_asset_path(output_root, spec).exists())

    def test_cli_group_prepare_records_external_static_without_data_disk_copy(self):
        payload = b"layout"
        spec = _fixture_spec(payload)
        record = static_asset_manifest_record(spec, bucket_url="https://openmeteo.example")

        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            with patch.dict(
                cli_module.OPENMETEO_STATIC_ASSETS,
                {"ecmwf_ifs025": spec},
                clear=True,
            ):
                records, results = cli_module._prepare_group_static_assets(
                    "ecmwf",
                    output_root=root,
                    bucket_url="https://openmeteo.example",
                )

            self.assertEqual(records, {"ecmwf_ifs025": record})
            self.assertEqual([item["status"] for item in results], ["external"])
            self.assertFalse(static_asset_path(root, spec).exists())
            self.assertFalse(static_asset_path(root / "published", spec).exists())


if __name__ == "__main__":
    unittest.main()
