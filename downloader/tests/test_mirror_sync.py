import hashlib
import json
import subprocess
import sys
import tempfile
import threading
import unittest
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path

from om_downloader.mirror_sync import (
    group_release_id,
    prune_expired_group_releases,
    sync_from_manifest_path,
    sync_from_manifest_url,
    sync_group_from_mirror,
)


class _SyncHandler(BaseHTTPRequestHandler):
    model = "gfs025"
    coverage_id = "gfs025_2026070718_1h"
    latest_complete_run = "2026070718"
    payload = b"range-bytes"
    manifest_status = "complete"
    requests = []

    def log_message(self, _format, *_args):
        return

    def do_GET(self):
        self.requests.append(self.path)
        if self.path == f"/published/{self.model}/latest.json":
            file_path = f"coverages/{self.coverage_id}/objects/2026070712/20260707T160000Z/temperature_2m.omranges"
            manifest = {
                "model": self.model,
                "coverage_id": self.coverage_id,
                "status": self.manifest_status,
                "latest_complete_run": self.latest_complete_run,
                "bytes": len(self.payload),
                "files": [
                    {
                        "path": file_path,
                        "bytes": len(self.payload),
                        "sha256": hashlib.sha256(self.payload).hexdigest(),
                    }
                ],
            }
            payload = json.dumps(manifest).encode("utf-8")
        elif self.path.endswith("temperature_2m.omranges"):
            payload = self.payload
        else:
            self.send_response(404)
            self.end_headers()
            return
        self.send_response(200)
        self.send_header("Content-Length", str(len(payload)))
        self.end_headers()
        self.wfile.write(payload)


class MirrorSyncTests(unittest.TestCase):
    def setUp(self):
        _SyncHandler.manifest_status = "complete"
        _SyncHandler.requests = []
        self.server = ThreadingHTTPServer(("127.0.0.1", 0), _SyncHandler)
        self.thread = threading.Thread(target=self.server.serve_forever, daemon=True)
        self.thread.start()
        self.manifest_url = f"http://127.0.0.1:{self.server.server_address[1]}/published/gfs025/latest.json"

    def tearDown(self):
        self.server.shutdown()
        self.server.server_close()
        self.thread.join(timeout=5)

    def test_sync_from_manifest_url_promotes_verified_complete_coverage(self):
        with tempfile.TemporaryDirectory() as tmp:
            result = sync_from_manifest_url(self.manifest_url, Path(tmp))
            ready_path = Path(tmp) / "gfs025" / "current" / "ready_for_processing.json"
            ready = json.loads(ready_path.read_text(encoding="utf-8"))
            downloaded = (
                Path(tmp)
                / "gfs025"
                / "coverages"
                / _SyncHandler.coverage_id
                / "objects"
                / "2026070712"
                / "20260707T160000Z"
                / "temperature_2m.omranges"
            )
            downloaded_bytes = downloaded.read_bytes()

        self.assertEqual(result["status"], "synced")
        self.assertEqual(ready["status"], "complete")
        self.assertEqual(ready["latest_complete_run"], _SyncHandler.latest_complete_run)
        self.assertEqual(ready["files"], 1)
        self.assertEqual(ready["bytes"], len(_SyncHandler.payload))
        self.assertEqual(ready["coverage_id"], _SyncHandler.coverage_id)
        self.assertEqual(ready["source_manifest_url"], self.manifest_url)
        self.assertEqual(downloaded_bytes, _SyncHandler.payload)
        self.assertIn("/published/gfs025/latest.json", _SyncHandler.requests)

    def test_sync_from_manifest_url_skips_incomplete_without_replacing_current(self):
        with tempfile.TemporaryDirectory() as tmp:
            current = Path(tmp) / "gfs025" / "current"
            current.mkdir(parents=True)
            ready_path = current / "ready_for_processing.json"
            ready_path.write_text(json.dumps({"coverage_id": "old"}), encoding="utf-8")
            _SyncHandler.manifest_status = "incomplete"

            result = sync_from_manifest_url(self.manifest_url, Path(tmp))
            ready = json.loads(ready_path.read_text(encoding="utf-8"))

        self.assertEqual(result["status"], "skipped")
        self.assertEqual(result["reason"], "manifest status is incomplete")
        self.assertEqual(ready["coverage_id"], "old")

    def test_cli_sync_from_manifest_url(self):
        with tempfile.TemporaryDirectory() as tmp:
            result = subprocess.run(
                [
                    sys.executable,
                    "-m",
                    "om_downloader.cli",
                    "--sync-from-manifest-url",
                    self.manifest_url,
                    "--output",
                    tmp,
                ],
                cwd=Path.cwd(),
                text=True,
                capture_output=True,
                check=True,
            )
            payload = json.loads(result.stdout)

        self.assertEqual(payload["status"], "synced")
        self.assertEqual(payload["coverage_id"], _SyncHandler.coverage_id)

    def test_sync_from_manifest_path_promotes_verified_complete_coverage(self):
        with tempfile.TemporaryDirectory() as tmp:
            source_root = Path(tmp) / "published" / "gfs025"
            file_path = (
                source_root
                / "coverages"
                / _SyncHandler.coverage_id
                / "objects"
                / "2026070712"
                / "20260707T160000Z"
                / "temperature_2m.omranges"
            )
            file_path.parent.mkdir(parents=True)
            file_path.write_bytes(_SyncHandler.payload)
            manifest = {
                "model": _SyncHandler.model,
                "coverage_id": _SyncHandler.coverage_id,
                "status": "complete",
                "latest_complete_run": _SyncHandler.latest_complete_run,
                "bytes": len(_SyncHandler.payload),
                "files": [
                    {
                        "path": str(file_path.relative_to(source_root)).replace("\\", "/"),
                        "bytes": len(_SyncHandler.payload),
                        "sha256": hashlib.sha256(_SyncHandler.payload).hexdigest(),
                    }
                ],
            }
            manifest_path = source_root / "latest.json"
            manifest_path.write_text(json.dumps(manifest), encoding="utf-8")
            output_root = Path(tmp) / "om_raw"

            result = sync_from_manifest_path(manifest_path, output_root)
            ready_path = output_root / "gfs025" / "current" / "ready_for_processing.json"
            ready = json.loads(ready_path.read_text(encoding="utf-8"))
            synced = (
                output_root
                / "gfs025"
                / "coverages"
                / _SyncHandler.coverage_id
                / "objects"
                / "2026070712"
                / "20260707T160000Z"
                / "temperature_2m.omranges"
            )
            synced_bytes = synced.read_bytes()

        self.assertEqual(result["status"], "synced")
        self.assertEqual(ready["status"], "complete")
        self.assertEqual(ready["latest_complete_run"], _SyncHandler.latest_complete_run)
        self.assertEqual(ready["files"], 1)
        self.assertEqual(ready["bytes"], len(_SyncHandler.payload))
        self.assertEqual(ready["source_manifest_path"], str(manifest_path.resolve()))
        self.assertEqual(synced_bytes, _SyncHandler.payload)

    def test_cli_sync_from_manifest_path(self):
        with tempfile.TemporaryDirectory() as tmp:
            source_root = Path(tmp) / "published" / "gfs025"
            file_path = source_root / "coverages" / _SyncHandler.coverage_id / "payload.omranges"
            file_path.parent.mkdir(parents=True)
            file_path.write_bytes(_SyncHandler.payload)
            manifest_path = source_root / "latest.json"
            manifest_path.write_text(
                json.dumps(
                    {
                        "model": _SyncHandler.model,
                        "coverage_id": _SyncHandler.coverage_id,
                        "status": "complete",
                        "files": [
                            {
                                "path": str(file_path.relative_to(source_root)).replace("\\", "/"),
                                "bytes": len(_SyncHandler.payload),
                                "sha256": hashlib.sha256(_SyncHandler.payload).hexdigest(),
                            }
                        ],
                    }
                ),
                encoding="utf-8",
            )
            result = subprocess.run(
                [
                    sys.executable,
                    "-m",
                    "om_downloader.cli",
                    "--sync-from-manifest-path",
                    str(manifest_path),
                    "--output",
                    str(Path(tmp) / "om_raw"),
                ],
                cwd=Path.cwd(),
                text=True,
                capture_output=True,
                check=True,
            )
            payload = json.loads(result.stdout)

        self.assertEqual(payload["status"], "synced")
        self.assertEqual(payload["coverage_id"], _SyncHandler.coverage_id)

    def test_sync_group_from_mirror_skips_when_local_group_run_matches(self):
        with tempfile.TemporaryDirectory() as tmp:
            mirror_root = Path(tmp) / "mirror"
            output_root = Path(tmp) / "raw"
            old_raw = output_root / "cams_global" / "coverages" / "cams_global_old"
            old_mirror = mirror_root / "cams_global" / "coverages" / "cams_global_old"
            keep_raw = output_root / "cams_global" / "coverages" / "cams_global_2026070800_1h"
            keep_mirror = mirror_root / "cams_global" / "coverages" / "cams_global_2026070800_1h"
            old_raw.mkdir(parents=True)
            old_mirror.mkdir(parents=True)
            keep_raw.mkdir(parents=True)
            keep_mirror.mkdir(parents=True)
            group_manifest = {
                "group": "cams",
                "status": "complete",
                "latest_complete_run": "2026070800",
                "products": ["cams_global", "cams_global_greenhouse_gases"],
                "product_manifests": {
                    "cams_global": {
                        "coverage_id": "cams_global_2026070800_1h",
                        "status": "complete",
                        "latest_complete_run": "2026070800",
                        "path": "../cams_global/latest.json",
                    },
                    "cams_global_greenhouse_gases": {
                        "coverage_id": "cams_global_greenhouse_gases_2026070800_1h",
                        "status": "complete",
                        "latest_complete_run": "2026070800",
                        "path": "../cams_global_greenhouse_gases/latest.json",
                    }
                },
                "files": 1,
                "bytes": 12,
                "downloaded_bytes": 12,
            }
            group_path = mirror_root / "groups" / "cams" / "latest.json"
            group_path.parent.mkdir(parents=True)
            group_path.write_text(json.dumps(group_manifest), encoding="utf-8")
            current_group = output_root / "groups" / "cams" / "current"
            current_group.mkdir(parents=True)
            (current_group / "ready_for_processing.json").write_text(
                json.dumps(
                    {
                        "group": "cams",
                        "status": "complete",
                        "latest_complete_run": "2026070800",
                        "product_manifests": group_manifest["product_manifests"],
                    }
                ),
                encoding="utf-8",
            )
            product_current = output_root / "cams_global" / "current"
            product_current.mkdir(parents=True)
            (product_current / "ready_for_processing.json").write_text(
                json.dumps({"coverage_id": "cams_global_2026070800_1h"}),
                encoding="utf-8",
            )
            greenhouse_current = output_root / "cams_global_greenhouse_gases" / "current"
            greenhouse_current.mkdir(parents=True)
            (greenhouse_current / "ready_for_processing.json").write_text(
                json.dumps({"coverage_id": "cams_global_greenhouse_gases_2026070800_1h"}),
                encoding="utf-8",
            )

            result = sync_group_from_mirror("cams", mirror_root, output_root)
            old_raw_exists = old_raw.exists()
            old_mirror_exists = old_mirror.exists()
            keep_raw_exists = keep_raw.exists()
            keep_mirror_exists = keep_mirror.exists()

        self.assertEqual(result["status"], "skipped")
        self.assertEqual(result["reason"], "local group already current")
        self.assertFalse(old_raw_exists)
        self.assertTrue(old_mirror_exists)
        self.assertTrue(keep_raw_exists)
        self.assertTrue(keep_mirror_exists)

    def test_sync_group_from_mirror_resyncs_when_supplement_window_summary_changes(self):
        with tempfile.TemporaryDirectory() as tmp:
            mirror_root = Path(tmp) / "mirror"
            output_root = Path(tmp) / "raw"
            group_manifest = {
                "group": "cams",
                "status": "complete",
                "latest_complete_run": "2026070800",
                "products": ["cams_global", "cams_global_greenhouse_gases"],
                "product_manifests": {
                    "cams_global": {
                        "coverage_id": "cams_global_2026070800_105h",
                        "status": "complete",
                        "latest_complete_run": "2026070800",
                        "required_start_utc": "2026-07-07T16:00:00Z",
                        "required_end_utc": "2026-07-12T00:00:00Z",
                        "valid_time_count": 105,
                        "files": 1,
                        "bytes": 12,
                        "downloaded_bytes": 12,
                        "path": "../cams_global/latest.json",
                    },
                    "cams_global_greenhouse_gases": {
                        "coverage_id": "cams_global_greenhouse_gases_2026070800_105h",
                        "status": "complete",
                        "latest_complete_run": "2026070800",
                        "required_start_utc": "2026-07-07T16:00:00Z",
                        "required_end_utc": "2026-07-12T00:00:00Z",
                        "valid_time_count": 105,
                        "files": 1,
                        "bytes": 15,
                        "downloaded_bytes": 15,
                        "path": "../cams_global_greenhouse_gases/latest.json",
                    }
                },
                "files": 2,
                "bytes": 27,
                "downloaded_bytes": 27,
            }
            group_path = mirror_root / "groups" / "cams" / "latest.json"
            group_path.parent.mkdir(parents=True)
            group_path.write_text(json.dumps(group_manifest), encoding="utf-8")
            current_group = output_root / "groups" / "cams" / "current"
            current_group.mkdir(parents=True)
            old_ready = json.loads(json.dumps(group_manifest))
            old_ready["product_manifests"]["cams_global"]["required_start_utc"] = "2026-07-08T00:00:00Z"
            (current_group / "ready_for_processing.json").write_text(
                json.dumps(old_ready),
                encoding="utf-8",
            )
            product_current = output_root / "cams_global" / "current"
            product_current.mkdir(parents=True)
            (product_current / "ready_for_processing.json").write_text(
                json.dumps({"coverage_id": "cams_global_2026070800_105h"}),
                encoding="utf-8",
            )
            greenhouse_current = output_root / "cams_global_greenhouse_gases" / "current"
            greenhouse_current.mkdir(parents=True)
            (greenhouse_current / "ready_for_processing.json").write_text(
                json.dumps({"coverage_id": "cams_global_greenhouse_gases_2026070800_105h"}),
                encoding="utf-8",
            )
            product_root = mirror_root / "cams_global"
            file_rel = "coverages/cams_global_2026070800_105h/cams_global.omranges"
            payload = b"cams-payload"
            file_path = product_root / file_rel
            file_path.parent.mkdir(parents=True)
            file_path.write_bytes(payload)
            manifest = {
                "model": "cams_global",
                "coverage_id": "cams_global_2026070800_105h",
                "status": "complete",
                "latest_complete_run": "2026070800",
                "required_start_utc": "2026-07-07T16:00:00Z",
                "required_end_utc": "2026-07-12T00:00:00Z",
                "valid_time_count": 105,
                "bytes": len(payload),
                "downloaded_bytes": len(payload),
                "files": [
                    {
                        "path": file_rel,
                        "bytes": len(payload),
                        "sha256": hashlib.sha256(payload).hexdigest(),
                    }
                ],
            }
            (product_root / "latest.json").write_text(json.dumps(manifest), encoding="utf-8")
            greenhouse_root = mirror_root / "cams_global_greenhouse_gases"
            greenhouse_file_rel = (
                "coverages/cams_global_greenhouse_gases_2026070800_105h/"
                "cams_global_greenhouse_gases.omranges"
            )
            greenhouse_payload = b"greenhouse-cams"
            greenhouse_file = greenhouse_root / greenhouse_file_rel
            greenhouse_file.parent.mkdir(parents=True)
            greenhouse_file.write_bytes(greenhouse_payload)
            greenhouse_manifest = {
                "model": "cams_global_greenhouse_gases",
                "coverage_id": "cams_global_greenhouse_gases_2026070800_105h",
                "status": "complete",
                "latest_complete_run": "2026070800",
                "required_start_utc": "2026-07-07T16:00:00Z",
                "required_end_utc": "2026-07-12T00:00:00Z",
                "valid_time_count": 105,
                "bytes": len(greenhouse_payload),
                "downloaded_bytes": len(greenhouse_payload),
                "files": [
                    {
                        "path": greenhouse_file_rel,
                        "bytes": len(greenhouse_payload),
                        "sha256": hashlib.sha256(greenhouse_payload).hexdigest(),
                    }
                ],
            }
            (greenhouse_root / "latest.json").write_text(
                json.dumps(greenhouse_manifest), encoding="utf-8"
            )

            result = sync_group_from_mirror("cams", mirror_root, output_root)

        self.assertEqual(result["status"], "synced")

    def test_sync_group_from_mirror_preserves_old_current_when_new_group_incomplete(self):
        with tempfile.TemporaryDirectory() as tmp:
            mirror_root = Path(tmp) / "mirror"
            output_root = Path(tmp) / "raw"
            old_group_ready = {
                "group": "gfs",
                "status": "complete",
                "latest_complete_run": "2026070718",
                "product_manifests": {
                    "gfs013_surface": {"coverage_id": "old013"},
                    "gfs025": {"coverage_id": "old025"},
                    "gfs_pressure_profile": {"coverage_id": "oldpressure"},
                },
            }
            group_current = output_root / "groups" / "gfs" / "current"
            group_current.mkdir(parents=True)
            (group_current / "ready_for_processing.json").write_text(
                json.dumps(old_group_ready),
                encoding="utf-8",
            )
            product_current = output_root / "gfs025" / "current"
            product_current.mkdir(parents=True)
            (product_current / "ready_for_processing.json").write_text(
                json.dumps({"coverage_id": "old025"}),
                encoding="utf-8",
            )

            new_run = "2026070800"
            product_payloads = {
                "gfs013_surface": b"gfs013",
                "gfs025": b"gfs025",
                "gfs_pressure_profile": b"pressure",
            }
            product_manifests = {}
            for product, payload in product_payloads.items():
                coverage_id = f"{product}_{new_run}_1h"
                product_root = mirror_root / product
                file_rel = f"coverages/{coverage_id}/{product}.omranges"
                product_manifests[product] = {
                    "model": product,
                    "coverage_id": coverage_id,
                    "status": "complete",
                    "latest_complete_run": new_run,
                    "bytes": len(payload),
                    "files": [
                        {
                            "path": file_rel,
                            "bytes": len(payload),
                            "sha256": hashlib.sha256(payload).hexdigest(),
                        }
                    ],
                }
                (product_root / "latest.json").parent.mkdir(parents=True, exist_ok=True)
                (product_root / "latest.json").write_text(
                    json.dumps(product_manifests[product]),
                    encoding="utf-8",
                )
                if product != "gfs_pressure_profile":
                    file_path = product_root / file_rel
                    file_path.parent.mkdir(parents=True)
                    file_path.write_bytes(payload)
            group_manifest = {
                "group": "gfs",
                "status": "complete",
                "latest_complete_run": new_run,
                "products": list(product_payloads),
                "product_manifests": {
                    product: {
                        "coverage_id": manifest["coverage_id"],
                        "status": "complete",
                        "latest_complete_run": new_run,
                        "path": f"../{product}/latest.json",
                    }
                    for product, manifest in product_manifests.items()
                },
                "files": 3,
                "bytes": sum(len(payload) for payload in product_payloads.values()),
                "downloaded_bytes": sum(len(payload) for payload in product_payloads.values()),
            }
            group_path = mirror_root / "groups" / "gfs" / "latest.json"
            group_path.parent.mkdir(parents=True)
            group_path.write_text(json.dumps(group_manifest), encoding="utf-8")

            with self.assertRaises(FileNotFoundError):
                sync_group_from_mirror("gfs", mirror_root, output_root)
            group_ready = json.loads(
                (group_current / "ready_for_processing.json").read_text(encoding="utf-8")
            )
            product_ready = json.loads(
                (product_current / "ready_for_processing.json").read_text(encoding="utf-8")
            )

        self.assertEqual(group_ready["latest_complete_run"], "2026070718")
        self.assertEqual(product_ready["coverage_id"], "old025")

    def test_sync_group_from_mirror_promotes_all_products_then_group_ready(self):
        with tempfile.TemporaryDirectory() as tmp:
            mirror_root = Path(tmp) / "mirror"
            output_root = Path(tmp) / "raw"
            new_run = "2026070800"
            product_payloads = {
                "gfs013_surface": b"gfs013",
                "gfs025": b"gfs025",
                "gfs_pressure_profile": b"pressure",
            }
            product_manifest_summary = {}
            for product, payload in product_payloads.items():
                coverage_id = f"{product}_{new_run}_1h"
                product_root = mirror_root / product
                file_rel = f"coverages/{coverage_id}/{product}.omranges"
                file_path = product_root / file_rel
                file_path.parent.mkdir(parents=True)
                file_path.write_bytes(payload)
                manifest = {
                    "model": product,
                    "coverage_id": coverage_id,
                    "status": "complete",
                    "latest_complete_run": new_run,
                    "bytes": len(payload),
                    "files": [
                        {
                            "path": file_rel,
                            "bytes": len(payload),
                            "sha256": hashlib.sha256(payload).hexdigest(),
                        }
                    ],
                }
                if product == "gfs_pressure_profile":
                    manifest["entries"] = [
                        {
                            "variable": "temperature_850hPa",
                            "source_run": "2026070718",
                            "valid_time_utc": "2026-07-08T00:00:00Z",
                            "forecast_hour": 6,
                        },
                        {
                            "variable": "temperature_850hPa",
                            "source_run": "2026070800",
                            "valid_time_utc": "2026-07-08T06:00:00Z",
                            "forecast_hour": 6,
                        },
                    ]
                (product_root / "latest.json").write_text(json.dumps(manifest), encoding="utf-8")
                product_manifest_summary[product] = {
                    "coverage_id": coverage_id,
                    "status": "complete",
                    "latest_complete_run": new_run,
                    "path": f"../{product}/latest.json",
                }
            group_manifest = {
                "group": "gfs",
                "status": "complete",
                "latest_complete_run": new_run,
                "products": list(product_payloads),
                "product_manifests": product_manifest_summary,
                "files": 3,
                "bytes": sum(len(payload) for payload in product_payloads.values()),
                "downloaded_bytes": sum(len(payload) for payload in product_payloads.values()),
            }
            group_path = mirror_root / "groups" / "gfs" / "latest.json"
            group_path.parent.mkdir(parents=True)
            group_path.write_text(json.dumps(group_manifest), encoding="utf-8")

            result = sync_group_from_mirror("gfs", mirror_root, output_root)
            group_ready = json.loads(
                (output_root / "groups" / "gfs" / "current" / "ready_for_processing.json").read_text(
                    encoding="utf-8"
                )
            )
            pressure_manifest = json.loads(
                (
                    output_root
                    / "gfs_pressure_profile"
                    / "current"
                    / "latest.json"
                ).read_text(encoding="utf-8")
            )
            product_ready_exists = {
                product: (output_root / product / "current" / "ready_for_processing.json").exists()
                for product in product_payloads
            }
            product_file_exists = {
                product: (
                    output_root
                    / product
                    / "coverages"
                    / f"{product}_{new_run}_1h"
                    / f"{product}.omranges"
                ).exists()
                for product in product_payloads
            }

            self.assertEqual(result["status"], "synced")
            self.assertEqual(result["latest_complete_run"], new_run)
            self.assertEqual(result["products"], 3)
            self.assertEqual(group_ready["latest_complete_run"], new_run)
            self.assertEqual(
                {entry["source_run"] for entry in pressure_manifest.get("entries", [])},
                {"2026070800", "2026070718"},
            )
            for product in product_payloads:
                self.assertTrue(product_ready_exists[product])
                self.assertTrue(product_file_exists[product])

    def test_sync_group_from_mirror_rejects_mixed_gfs_product_runs(self):
        with tempfile.TemporaryDirectory() as tmp:
            mirror_root = Path(tmp) / "mirror"
            output_root = Path(tmp) / "raw"
            product_runs = {
                "gfs013_surface": "2026071000",
                "gfs025": "2026070918",
                "gfs_pressure_profile": "2026070918",
            }
            product_manifest_summary = {}
            for product, run in product_runs.items():
                payload = product.encode("utf-8")
                coverage_id = f"{product}_{run}_1h"
                product_root = mirror_root / product
                file_rel = f"coverages/{coverage_id}/{product}.omranges"
                file_path = product_root / file_rel
                file_path.parent.mkdir(parents=True)
                file_path.write_bytes(payload)
                manifest = {
                    "model": product,
                    "coverage_id": coverage_id,
                    "status": "complete",
                    "latest_complete_run": run,
                    "bytes": len(payload),
                    "downloaded_bytes": len(payload),
                    "files": [
                        {
                            "path": file_rel,
                            "bytes": len(payload),
                            "sha256": hashlib.sha256(payload).hexdigest(),
                        }
                    ],
                }
                (product_root / "latest.json").write_text(json.dumps(manifest), encoding="utf-8")
                product_manifest_summary[product] = {
                    "coverage_id": coverage_id,
                    "status": "complete",
                    "latest_complete_run": run,
                    "path": f"../{product}/latest.json",
                    "files": 1,
                    "bytes": len(payload),
                    "downloaded_bytes": len(payload),
                }
            group_manifest = {
                "group": "gfs",
                "status": "complete",
                "latest_complete_run": None,
                "products": list(product_runs),
                "product_manifests": product_manifest_summary,
                "files": 3,
                "bytes": sum(summary["bytes"] for summary in product_manifest_summary.values()),
                "downloaded_bytes": sum(
                    summary["downloaded_bytes"] for summary in product_manifest_summary.values()
                ),
            }
            group_path = mirror_root / "groups" / "gfs" / "latest.json"
            group_path.parent.mkdir(parents=True)
            group_path.write_text(json.dumps(group_manifest), encoding="utf-8")

            result = sync_group_from_mirror("gfs", mirror_root, output_root)
            group_ready_exists = (
                output_root / "groups" / "gfs" / "current" / "ready_for_processing.json"
            ).exists()

        self.assertEqual(result["status"], "skipped")
        self.assertFalse(group_ready_exists)

    def test_sync_group_from_mirror_promotes_cams_independent_product_runs(self):
        with tempfile.TemporaryDirectory() as tmp:
            mirror_root = Path(tmp) / "mirror"
            output_root = Path(tmp) / "raw"
            product_runs = {
                "cams_global": "2026071500",
                "cams_global_greenhouse_gases": "2026071400",
            }
            product_manifest_summary = {}
            for product, run in product_runs.items():
                payload = product.encode("utf-8")
                coverage_id = f"{product}_{run}_1h"
                product_root = mirror_root / product
                file_rel = f"coverages/{coverage_id}/{product}.omranges"
                file_path = product_root / file_rel
                file_path.parent.mkdir(parents=True)
                file_path.write_bytes(payload)
                manifest = {
                    "model": product,
                    "coverage_id": coverage_id,
                    "status": "complete",
                    "latest_complete_run": run,
                    "bytes": len(payload),
                    "downloaded_bytes": len(payload),
                    "files": [
                        {
                            "path": file_rel,
                            "bytes": len(payload),
                            "sha256": hashlib.sha256(payload).hexdigest(),
                        }
                    ],
                }
                (product_root / "latest.json").write_text(
                    json.dumps(manifest), encoding="utf-8"
                )
                product_manifest_summary[product] = {
                    "coverage_id": coverage_id,
                    "status": "complete",
                    "latest_complete_run": run,
                    "path": f"../{product}/latest.json",
                    "files": 1,
                    "bytes": len(payload),
                    "downloaded_bytes": len(payload),
                }
            group_manifest = {
                "group": "cams",
                "status": "complete",
                "latest_complete_run": "2026071500",
                "products": list(product_runs),
                "product_manifests": product_manifest_summary,
                "files": 2,
                "bytes": sum(
                    summary["bytes"] for summary in product_manifest_summary.values()
                ),
                "downloaded_bytes": sum(
                    summary["downloaded_bytes"]
                    for summary in product_manifest_summary.values()
                ),
            }
            group_path = mirror_root / "groups" / "cams" / "latest.json"
            group_path.parent.mkdir(parents=True)
            group_path.write_text(json.dumps(group_manifest), encoding="utf-8")

            result = sync_group_from_mirror("cams", mirror_root, output_root)
            group_ready = json.loads(
                (
                    output_root
                    / "groups"
                    / "cams"
                    / "current"
                    / "ready_for_processing.json"
                ).read_text(encoding="utf-8")
            )

        self.assertEqual(result["status"], "synced")
        self.assertEqual(result["latest_complete_run"], "2026071500")
        self.assertEqual(group_ready["status"], "complete")
        self.assertEqual(
            {
                product: summary["latest_complete_run"]
                for product, summary in group_ready["product_manifests"].items()
            },
            product_runs,
        )

    def test_sync_group_from_mirror_preserves_previous_complete_release(self):
        with tempfile.TemporaryDirectory() as tmp:
            mirror_root = Path(tmp) / "mirror"
            output_root = Path(tmp) / "raw"
            product = "cams_global"
            greenhouse = "cams_global_greenhouse_gases"
            old_coverage = "cams_global_2026070712_1h"
            new_coverage = "cams_global_2026070800_1h"
            old_greenhouse_coverage = "cams_global_greenhouse_gases_2026070712_1h"
            new_greenhouse_coverage = "cams_global_greenhouse_gases_2026070800_1h"
            old_raw = output_root / product / "coverages" / old_coverage
            old_mirror = mirror_root / product / "coverages" / old_coverage
            old_greenhouse_raw = output_root / greenhouse / "coverages" / old_greenhouse_coverage
            old_raw.mkdir(parents=True)
            old_mirror.mkdir(parents=True)
            old_greenhouse_raw.mkdir(parents=True)
            (old_raw / "cams_global.omranges").write_bytes(b"old-raw")
            (old_mirror / "cams_global.omranges").write_bytes(b"old-mirror")
            current_group = output_root / "groups" / "cams" / "current"
            current_group.mkdir(parents=True)
            (current_group / "ready_for_processing.json").write_text(
                json.dumps(
                    {
                        "group": "cams",
                        "status": "complete",
                        "latest_complete_run": "2026070712",
                        "product_manifests": {
                            product: {
                                "coverage_id": old_coverage,
                                "status": "complete",
                                "latest_complete_run": "2026070712",
                            },
                            greenhouse: {
                                "coverage_id": old_greenhouse_coverage,
                                "status": "complete",
                                "latest_complete_run": "2026070712",
                            }
                        },
                    }
                ),
                encoding="utf-8",
            )

            payload = b"new-cams"
            file_rel = f"coverages/{new_coverage}/cams_global.omranges"
            new_file = mirror_root / product / file_rel
            new_file.parent.mkdir(parents=True)
            new_file.write_bytes(payload)
            manifest = {
                "model": product,
                "coverage_id": new_coverage,
                "status": "complete",
                "latest_complete_run": "2026070800",
                "bytes": len(payload),
                "downloaded_bytes": len(payload),
                "files": [
                    {
                        "path": file_rel,
                        "bytes": len(payload),
                        "sha256": hashlib.sha256(payload).hexdigest(),
                    }
                ],
            }
            (mirror_root / product / "latest.json").write_text(
                json.dumps(manifest),
                encoding="utf-8",
            )
            greenhouse_payload = b"new-greenhouse"
            greenhouse_file_rel = (
                f"coverages/{new_greenhouse_coverage}/cams_global_greenhouse_gases.omranges"
            )
            greenhouse_file = mirror_root / greenhouse / greenhouse_file_rel
            greenhouse_file.parent.mkdir(parents=True)
            greenhouse_file.write_bytes(greenhouse_payload)
            greenhouse_manifest = {
                "model": greenhouse,
                "coverage_id": new_greenhouse_coverage,
                "status": "complete",
                "latest_complete_run": "2026070800",
                "bytes": len(greenhouse_payload),
                "downloaded_bytes": len(greenhouse_payload),
                "files": [
                    {
                        "path": greenhouse_file_rel,
                        "bytes": len(greenhouse_payload),
                        "sha256": hashlib.sha256(greenhouse_payload).hexdigest(),
                    }
                ],
            }
            (mirror_root / greenhouse / "latest.json").write_text(
                json.dumps(greenhouse_manifest),
                encoding="utf-8",
            )
            group_manifest = {
                "group": "cams",
                "status": "complete",
                "latest_complete_run": "2026070800",
                "products": [product, greenhouse],
                "product_manifests": {
                    product: {
                        "coverage_id": new_coverage,
                        "status": "complete",
                        "latest_complete_run": "2026070800",
                        "path": f"../{product}/latest.json",
                    },
                    greenhouse: {
                        "coverage_id": new_greenhouse_coverage,
                        "status": "complete",
                        "latest_complete_run": "2026070800",
                        "path": f"../{greenhouse}/latest.json",
                    }
                },
                "files": 2,
                "bytes": len(payload) + len(greenhouse_payload),
                "downloaded_bytes": len(payload) + len(greenhouse_payload),
            }
            group_path = mirror_root / "groups" / "cams" / "latest.json"
            group_path.parent.mkdir(parents=True)
            group_path.write_text(json.dumps(group_manifest), encoding="utf-8")

            result = sync_group_from_mirror("cams", mirror_root, output_root)

            self.assertEqual(result["status"], "synced")
            self.assertTrue(old_raw.exists())
            self.assertTrue(old_mirror.exists())
            self.assertTrue(old_greenhouse_raw.exists())
            self.assertTrue((output_root / product / "coverages" / new_coverage).exists())
            self.assertTrue(
                (output_root / greenhouse / "coverages" / new_greenhouse_coverage).exists()
            )
            self.assertTrue((mirror_root / product / "coverages" / new_coverage).exists())

    def test_native_current_and_duplicate_do_not_consume_gfs_retention_slots(self):
        products = ("gfs013_surface", "gfs025", "gfs_pressure_profile")
        runs = ["2026072018", "2026072012", "2026072006", "2026072000", "2026071918"]

        def release(run: str) -> dict:
            payload = {
                "group": "gfs",
                "status": "complete",
                "latest_complete_run": run,
                "product_manifests": {
                    product: {
                        "coverage_id": f"{product}_{run}",
                        "status": "complete",
                        "latest_complete_run": run,
                    }
                    for product in products
                },
            }
            payload["release_id"] = group_release_id(payload)
            return payload

        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            releases_root = root / "groups" / "gfs" / "releases"
            releases_root.mkdir(parents=True)
            payloads = [release(run) for run in runs]
            for payload in payloads:
                (releases_root / f"{payload['release_id']}.json").write_text(
                    json.dumps(payload), encoding="utf-8"
                )

            native = dict(payloads[0])
            native.update(
                {
                    "runtime_format": "openmeteo-native-v1",
                    "coverage_id": "gfs_native_2026072018_old",
                    "coverage_path": "coverages/gfs/gfs_native_2026072018_old",
                }
            )
            current = root / "groups" / "gfs" / "current"
            current.mkdir(parents=True)
            (current / "ready_for_processing.json").write_text(
                json.dumps(native), encoding="utf-8"
            )
            duplicate = dict(native)
            duplicate["release_id"] = duplicate["coverage_id"]
            duplicate_path = releases_root / f"{duplicate['release_id']}.json"
            duplicate_path.write_text(json.dumps(duplicate), encoding="utf-8")

            for _ in range(2):
                prune_expired_group_releases(
                    root,
                    "gfs",
                    retain_complete_releases=5,
                    preserve_current=True,
                )

            retained = [
                json.loads(path.read_text(encoding="utf-8"))
                for path in releases_root.glob("*.json")
            ]
            duplicate_removed = not duplicate_path.exists()

        self.assertEqual(
            {payload["latest_complete_run"] for payload in retained}, set(runs)
        )
        self.assertEqual(len(retained), 5)
        self.assertTrue(duplicate_removed)


if __name__ == "__main__":
    unittest.main()
