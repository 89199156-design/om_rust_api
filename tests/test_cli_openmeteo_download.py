import json
import struct
import subprocess
import sys
import tempfile
import threading
import unittest
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path

from tests.test_om_format import OM_HEADER, OM_TRAILER_MAGIC, _pack_array, _pack_root


def _product_config(openmeteo_model="ncep_gfs025", required_variables=None):
    required_variables = required_variables or ["temperature_2m"]
    return {
        "download_product": "om_gfs025",
        "openmeteo_model": openmeteo_model,
        "forecast_hour_end": 4,
        "run_cadence_hours": 6,
        "timezone_anchors": [8, 6],
        "requested_bounds": {
            "lon_min": 70.0,
            "lat_min": 0.0,
            "lon_max": 70.1,
            "lat_max": 0.1,
        },
        "bounds_padding_degrees": 0.0,
        "required_variables": required_variables,
        "optional_variables": [],
        "requested_pressure_levels_hpa": [],
    }


def _write_config(path, required_variables=None):
    path.write_text(
        json.dumps(
            {
                "version": 1,
                "products": {
                    "gfs025": _product_config(required_variables=required_variables)
                },
            }
        ),
        encoding="utf-8",
    )


def _read_run_summary(output_root):
    records = []
    for path in sorted((output_root / "logs").glob("om_run_summary-*.jsonl")):
        records.extend(
            json.loads(line)
            for line in path.read_text(encoding="utf-8").splitlines()
            if line.strip()
        )
    return records


def _write_gfs_group_config(path):
    products = {
        "gfs013_surface": _product_config("ncep_gfs013"),
        "gfs025": _product_config("ncep_gfs025"),
        "gfs_pressure_profile": _product_config("ncep_gfs025"),
    }
    path.write_text(json.dumps({"version": 1, "products": products}), encoding="utf-8")


def _sample_object_with_plain_lut(dimensions=None):
    dimensions = dimensions or [721, 1440]
    lut_offset = 100
    data_offset = 2000
    lut_values = [data_offset, data_offset + 40]
    lut_payload = b"".join(struct.pack("<Q", item) for item in lut_values)
    array = _pack_array(
        "temperature_2m",
        dimensions=dimensions,
        chunks=dimensions,
        lut_offset=lut_offset,
        lut_size=len(lut_payload),
    )
    array_offset = 256
    root = _pack_root("root", [(array_offset, len(array))])
    root_offset = array_offset + len(array) + 64
    blob = bytearray(data_offset + 40 + 24)
    blob[0:3] = OM_HEADER
    blob[lut_offset : lut_offset + len(lut_payload)] = lut_payload
    blob[array_offset : array_offset + len(array)] = array
    blob[root_offset : root_offset + len(root)] = root
    blob[data_offset : data_offset + 40] = b"x" * 40
    blob[-24:] = struct.pack("<2sBBIQQ", OM_TRAILER_MAGIC, 3, 0, 0, root_offset, len(root))
    return bytes(blob)


def _sample_object_with_two_plain_lut_arrays(dimensions=None):
    return _sample_object_with_plain_lut_arrays(["temperature_2m", "pressure_msl"], dimensions=dimensions)


def _sample_object_with_plain_lut_arrays(names, dimensions=None):
    dimensions = dimensions or [721, 1440]
    refs = []
    payloads = []
    cursor = 256
    data_end = 0
    for index, name in enumerate(names):
        lut_offset = 100 + index * 40
        data_offset = 2000 + index * 1000
        data_size = 40 - index * 8
        lut_payload = b"".join(struct.pack("<Q", item) for item in [data_offset, data_offset + data_size])
        array = _pack_array(
            name,
            dimensions=dimensions,
            chunks=dimensions,
            lut_offset=lut_offset,
            lut_size=len(lut_payload),
        )
        refs.append((cursor, len(array)))
        payloads.append((lut_offset, lut_payload, data_offset, bytes([120 + index]) * data_size, cursor, array))
        cursor += len(array) + 32
        data_end = max(data_end, data_offset + data_size)
    root = _pack_root("root", refs)
    root_offset = cursor + 64
    blob = bytearray(data_end + 24)
    blob[0:3] = OM_HEADER
    for lut_offset, lut_payload, data_offset, data_payload, array_offset, array in payloads:
        blob[lut_offset : lut_offset + len(lut_payload)] = lut_payload
        blob[array_offset : array_offset + len(array)] = array
        blob[data_offset : data_offset + len(data_payload)] = data_payload
    blob[root_offset : root_offset + len(root)] = root
    blob[-24:] = struct.pack("<2sBBIQQ", OM_TRAILER_MAGIC, 3, 0, 0, root_offset, len(root))
    return bytes(blob)


class _OpenMeteoDownloadHandler(BaseHTTPRequestHandler):
    object_content = _sample_object_with_plain_lut()
    object_content_by_model = {
        "ncep_gfs013": _sample_object_with_plain_lut([1536, 3072]),
        "ncep_gfs025": _sample_object_with_plain_lut([721, 1440]),
    }
    object_content_by_path = {}
    catalog_variables = ["temperature_2m"]
    catalog_valid_times = ["2026-07-07T16:00Z"]
    catalog_reference_times = {
        "ncep_gfs013": "2026-07-07T12:00:00Z",
        "ncep_gfs025": "2026-07-07T12:00:00Z",
    }
    range_headers = []
    plain_object_get_count = 0
    fail_ranges = set()

    def log_message(self, _format, *_args):
        return

    def do_HEAD(self):
        if not self.path.endswith(".om"):
            self.send_response(404)
            self.end_headers()
            return
        model = self.path.split("/")[2]
        object_content = self.object_content_by_path.get(
            self.path,
            self.object_content_by_model.get(model, self.object_content),
        )
        self.send_response(200)
        self.send_header("Content-Length", str(len(object_content)))
        self.send_header("Accept-Ranges", "bytes")
        self.end_headers()

    def do_GET(self):
        if self.path.endswith("/latest.json") or self.path.endswith("/meta.json"):
            parts = self.path.strip("/").split("/")
            if self.path.endswith("/meta.json"):
                model = parts[1]
                date_text = "".join(parts[2:5])
                time_text = parts[5]
                reference_time = (
                    f"{date_text[:4]}-{date_text[4:6]}-{date_text[6:8]}T{time_text[:2]}:00:00Z"
                )
            else:
                model = parts[1]
                reference_time = self.catalog_reference_times.get(model, "2026-07-07T12:00:00Z")
            payload = json.dumps(
                [
                    {
                        "completed": True,
                        "reference_time": reference_time,
                        "valid_times": self.catalog_valid_times,
                        "variables": self.catalog_variables,
                    }
                ]
            ).encode("utf-8")
            self.send_response(200)
            self.send_header("Content-Length", str(len(payload)))
            self.end_headers()
            self.wfile.write(payload)
            return

        if not self.path.endswith(".om"):
            self.send_response(404)
            self.end_headers()
            return
        model = self.path.split("/")[2]
        object_content = self.object_content_by_path.get(
            self.path,
            self.object_content_by_model.get(model, self.object_content),
        )

        range_header = self.headers.get("Range")
        if range_header is None:
            self.plain_object_get_count += 1
            self.send_response(200)
            self.send_header("Content-Length", str(len(object_content)))
            self.end_headers()
            self.wfile.write(object_content)
            return

        self.range_headers.append(range_header)
        if range_header in self.fail_ranges:
            self.close_connection = True
            return
        start_text, end_text = range_header.replace("bytes=", "").split("-", 1)
        start = int(start_text)
        end = int(end_text)
        payload = object_content[start : end + 1]
        self.send_response(206)
        self.send_header("Content-Length", str(len(payload)))
        self.send_header("Content-Range", f"bytes {start}-{end}/{len(object_content)}")
        self.end_headers()
        self.wfile.write(payload)


class CliOpenMeteoDownloadTests(unittest.TestCase):
    def setUp(self):
        _OpenMeteoDownloadHandler.range_headers = []
        _OpenMeteoDownloadHandler.plain_object_get_count = 0
        _OpenMeteoDownloadHandler.fail_ranges = set()
        _OpenMeteoDownloadHandler.catalog_variables = ["temperature_2m"]
        _OpenMeteoDownloadHandler.catalog_reference_times = {
            "ncep_gfs013": "2026-07-07T12:00:00Z",
            "ncep_gfs025": "2026-07-07T12:00:00Z",
        }
        _OpenMeteoDownloadHandler.object_content_by_model = {
            "ncep_gfs013": _sample_object_with_plain_lut([1536, 3072]),
            "ncep_gfs025": _sample_object_with_plain_lut([721, 1440]),
        }
        _OpenMeteoDownloadHandler.object_content_by_path = {}
        _OpenMeteoDownloadHandler.catalog_valid_times = ["2026-07-07T16:00Z"]
        self.server = ThreadingHTTPServer(("127.0.0.1", 0), _OpenMeteoDownloadHandler)
        self.thread = threading.Thread(target=self.server.serve_forever, daemon=True)
        self.thread.start()
        self.bucket_url = f"http://127.0.0.1:{self.server.server_address[1]}"

    def tearDown(self):
        self.server.shutdown()
        self.server.server_close()
        self.thread.join(timeout=5)

    def test_cli_downloads_openmeteo_product_range_bundle(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            config = root / "models.json"
            output = root / "out"
            _write_config(config)

            result = subprocess.run(
                [
                    sys.executable,
                    "-m",
                    "om_downloader.cli",
                    "--download-openmeteo-product",
                    "gfs025",
                    "--config",
                    str(config),
                    "--openmeteo-bucket-url",
                    self.bucket_url,
                    "--output",
                    str(output),
                    "--now",
                    "2026-07-08T14:00:00Z",
                    "--lut-codec",
                    "plain",
                    "--planning-workers",
                    "2",
                    "--range-workers",
                    "3",
                ],
                cwd=Path.cwd(),
                text=True,
                capture_output=True,
                check=True,
            )

            payload = json.loads(result.stdout)
            manifest = json.loads(
                (output / "published" / "gfs025" / "latest.json").read_text(encoding="utf-8")
            )
            run_summary = _read_run_summary(output)

        self.assertEqual(payload["status"], "complete")
        self.assertEqual(manifest["status"], "complete")
        self.assertEqual(len(manifest["files"]), 1)
        file_record = manifest["files"][0]
        self.assertEqual(file_record["kind"], "om_coverage_bundle")
        self.assertEqual(len(file_record["entries"]), 1)
        entry = file_record["entries"][0]
        self.assertEqual(entry["variable"], "temperature_2m")
        self.assertEqual(entry["valid_time_utc"], "2026-07-07T16:00:00Z")
        self.assertEqual(entry["byte_ranges"], [[100, 115], [2000, 2039]])
        self.assertEqual(entry["bundle_offset"], 0)
        self.assertEqual(entry["bundle_bytes"], 56)
        self.assertEqual(file_record["downloaded_bytes"], 56)
        self.assertEqual(_OpenMeteoDownloadHandler.plain_object_get_count, 0)
        self.assertIn("bytes=100-115", _OpenMeteoDownloadHandler.range_headers)
        self.assertIn("bytes=2000-2039", _OpenMeteoDownloadHandler.range_headers)
        product_runs = [
            record
            for record in run_summary
            if record["kind"] == "product" and record["product"] == "gfs025"
        ]
        self.assertEqual(len(product_runs), 1)
        product_run = product_runs[0]
        self.assertEqual(product_run["status"], "complete")
        self.assertEqual(product_run["coverage_id"], "gfs025_2026070712_1h")
        self.assertEqual(product_run["entries"], 1)
        self.assertEqual(product_run["bytes"], 56)
        self.assertEqual(product_run["downloaded_bytes"], 56)
        self.assertGreaterEqual(product_run["duration_seconds"], 0)
        self.assertFalse(product_run["reused_existing"])

    def test_cli_downloads_one_coverage_bundle_for_multiple_variables(self):
        _OpenMeteoDownloadHandler.catalog_variables = ["temperature_2m", "pressure_msl"]
        _OpenMeteoDownloadHandler.object_content_by_model["ncep_gfs025"] = (
            _sample_object_with_two_plain_lut_arrays()
        )
        try:
            with tempfile.TemporaryDirectory() as tmp:
                root = Path(tmp)
                config = root / "models.json"
                output = root / "out"
                _write_config(config, required_variables=["temperature_2m", "pressure_msl"])

                subprocess.run(
                    [
                        sys.executable,
                        "-m",
                        "om_downloader.cli",
                        "--download-openmeteo-product",
                        "gfs025",
                        "--config",
                        str(config),
                        "--openmeteo-bucket-url",
                        self.bucket_url,
                        "--output",
                        str(output),
                        "--now",
                        "2026-07-08T14:00:00Z",
                        "--lut-codec",
                        "plain",
                    ],
                    cwd=Path.cwd(),
                    text=True,
                    capture_output=True,
                    check=True,
                )
                manifest = json.loads(
                    (output / "published" / "gfs025" / "latest.json").read_text(
                        encoding="utf-8"
                    )
                )
        finally:
            _OpenMeteoDownloadHandler.object_content_by_model["ncep_gfs025"] = (
                _sample_object_with_plain_lut([721, 1440])
            )

        self.assertEqual(manifest["status"], "complete")
        self.assertEqual(len(manifest["files"]), 1)
        file_record = manifest["files"][0]
        self.assertEqual(file_record["kind"], "om_coverage_bundle")
        self.assertEqual(file_record["path"], "coverages/gfs025_2026070712_1h/gfs025.omranges")
        self.assertEqual(file_record["downloaded_bytes"], 104)
        self.assertEqual(
            [entry["variable"] for entry in file_record["entries"]],
            ["temperature_2m", "pressure_msl"],
        )
        self.assertEqual(file_record["entries"][0]["bundle_offset"], 0)
        self.assertEqual(file_record["entries"][0]["bundle_bytes"], 56)
        self.assertEqual(file_record["entries"][1]["bundle_offset"], 56)
        self.assertEqual(file_record["entries"][1]["bundle_bytes"], 48)

    def test_cli_reuses_existing_range_bundle_data_on_rerun(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            config = root / "models.json"
            output = root / "out"
            _write_config(config)
            command = [
                sys.executable,
                "-m",
                "om_downloader.cli",
                "--download-openmeteo-product",
                "gfs025",
                "--config",
                str(config),
                "--openmeteo-bucket-url",
                self.bucket_url,
                "--output",
                str(output),
                "--now",
                "2026-07-08T14:00:00Z",
                "--lut-codec",
                "plain",
            ]
            subprocess.run(command, cwd=Path.cwd(), text=True, capture_output=True, check=True)
            _OpenMeteoDownloadHandler.range_headers = []

            subprocess.run(command, cwd=Path.cwd(), text=True, capture_output=True, check=True)
            manifest = json.loads(
                (output / "published" / "gfs025" / "latest.json").read_text(encoding="utf-8")
            )

        self.assertEqual(manifest["status"], "complete")
        self.assertEqual(manifest["files"][0]["reused_existing"], True)
        self.assertEqual(_OpenMeteoDownloadHandler.range_headers, [])
        self.assertNotIn("bytes=2000-2039", _OpenMeteoDownloadHandler.range_headers)

    def test_cli_redownloads_when_existing_bundle_sha256_mismatches_manifest(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            config = root / "models.json"
            output = root / "out"
            _write_config(config)
            command = [
                sys.executable,
                "-m",
                "om_downloader.cli",
                "--download-openmeteo-product",
                "gfs025",
                "--config",
                str(config),
                "--openmeteo-bucket-url",
                self.bucket_url,
                "--output",
                str(output),
                "--now",
                "2026-07-08T14:00:00Z",
                "--lut-codec",
                "plain",
            ]
            subprocess.run(command, cwd=Path.cwd(), text=True, capture_output=True, check=True)
            manifest = json.loads(
                (output / "published" / "gfs025" / "latest.json").read_text(encoding="utf-8")
            )
            bundle = output / "published" / "gfs025" / manifest["files"][0]["path"]
            bundle.write_bytes(b"z" * manifest["files"][0]["bytes"])
            _OpenMeteoDownloadHandler.range_headers = []

            subprocess.run(command, cwd=Path.cwd(), text=True, capture_output=True, check=True)
            manifest = json.loads(
                (output / "published" / "gfs025" / "latest.json").read_text(encoding="utf-8")
            )

        self.assertEqual(manifest["status"], "complete")
        self.assertEqual(manifest["files"][0]["reused_existing"], False)
        self.assertIn("bytes=2000-2039", _OpenMeteoDownloadHandler.range_headers)

    def test_cli_download_failure_does_not_publish_complete_latest_json(self):
        _OpenMeteoDownloadHandler.fail_ranges = {"bytes=2000-2039"}
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            config = root / "models.json"
            output = root / "out"
            _write_config(config)

            with self.assertRaises(subprocess.CalledProcessError):
                subprocess.run(
                    [
                        sys.executable,
                        "-m",
                        "om_downloader.cli",
                        "--download-openmeteo-product",
                        "gfs025",
                        "--config",
                        str(config),
                        "--openmeteo-bucket-url",
                        self.bucket_url,
                        "--output",
                        str(output),
                        "--now",
                        "2026-07-08T14:00:00Z",
                        "--lut-codec",
                        "plain",
                    ],
                    cwd=Path.cwd(),
                    text=True,
                    capture_output=True,
                    check=True,
                )

            self.assertFalse((output / "published" / "gfs025" / "latest.json").exists())
            self.assertFalse(list((output / "published" / "gfs025").glob("**/*.omranges")))

    def test_cli_download_failure_writes_run_summary_error(self):
        _OpenMeteoDownloadHandler.fail_ranges = {"bytes=2000-2039"}
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            config = root / "models.json"
            output = root / "out"
            _write_config(config)

            with self.assertRaises(subprocess.CalledProcessError):
                subprocess.run(
                    [
                        sys.executable,
                        "-m",
                        "om_downloader.cli",
                        "--download-openmeteo-product",
                        "gfs025",
                        "--config",
                        str(config),
                        "--openmeteo-bucket-url",
                        self.bucket_url,
                        "--output",
                        str(output),
                        "--now",
                        "2026-07-08T14:00:00Z",
                        "--lut-codec",
                        "plain",
                    ],
                    cwd=Path.cwd(),
                    text=True,
                    capture_output=True,
                    check=True,
                )

            run_summary = _read_run_summary(output)

        failed_runs = [
            record
            for record in run_summary
            if record.get("kind") == "product"
            and record.get("product") == "gfs025"
            and record.get("status") == "failed"
        ]
        self.assertEqual(len(failed_runs), 1)
        failed_run = failed_runs[0]
        self.assertEqual(failed_run["error_type"], "ValueError")
        self.assertIn("range request failed", failed_run["error_message"])
        self.assertTrue(failed_run["remote_error"])
        self.assertGreaterEqual(failed_run["duration_seconds"], 0)

    def test_cli_marks_manifest_incomplete_when_object_required_variable_is_missing(self):
        _OpenMeteoDownloadHandler.catalog_variables = ["temperature_2m", "pressure_msl"]
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            config = root / "models.json"
            output = root / "out"
            _write_config(config, required_variables=["temperature_2m", "pressure_msl"])

            result = subprocess.run(
                [
                    sys.executable,
                    "-m",
                    "om_downloader.cli",
                    "--download-openmeteo-product",
                    "gfs025",
                    "--config",
                    str(config),
                    "--openmeteo-bucket-url",
                    self.bucket_url,
                    "--output",
                    str(output),
                    "--now",
                    "2026-07-08T14:00:00Z",
                    "--lut-codec",
                    "plain",
                ],
                cwd=Path.cwd(),
                text=True,
                capture_output=True,
                check=True,
            )

            payload = json.loads(result.stdout)
            manifest = json.loads(
                (output / "published" / "gfs025" / "latest.json").read_text(encoding="utf-8")
            )

        self.assertEqual(payload["status"], "incomplete")
        self.assertEqual(manifest["status"], "incomplete")
        self.assertEqual(
            manifest["missing_object_required_variables"],
            [
                {
                    "valid_time_utc": "2026-07-07T16:00:00Z",
                    "source_run": "2026070712",
                    "missing_required_variables": ["pressure_msl"],
                }
            ],
        )

    def test_cli_keeps_manifest_complete_when_required_variable_is_sparse(self):
        _OpenMeteoDownloadHandler.catalog_variables = ["temperature_2m", "pressure_msl"]
        _OpenMeteoDownloadHandler.catalog_valid_times = [
            "2026-07-07T15:00Z",
            "2026-07-07T16:00Z",
        ]
        first_path = "/data_spatial/ncep_gfs025/2026/07/07/1200Z/2026-07-07T1500.om"
        second_path = "/data_spatial/ncep_gfs025/2026/07/07/1200Z/2026-07-07T1600.om"
        _OpenMeteoDownloadHandler.object_content_by_path = {
            first_path: _sample_object_with_plain_lut_arrays(["temperature_2m"]),
            second_path: _sample_object_with_plain_lut_arrays(["temperature_2m", "pressure_msl"]),
        }
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            config = root / "models.json"
            output = root / "out"
            _write_config(config, required_variables=["temperature_2m", "pressure_msl"])

            result = subprocess.run(
                [
                    sys.executable,
                    "-m",
                    "om_downloader.cli",
                    "--download-openmeteo-product",
                    "gfs025",
                    "--config",
                    str(config),
                    "--openmeteo-bucket-url",
                    self.bucket_url,
                    "--output",
                    str(output),
                    "--now",
                    "2026-07-07T15:00:00Z",
                    "--lut-codec",
                    "plain",
                ],
                cwd=Path.cwd(),
                text=True,
                capture_output=True,
                check=True,
            )

            payload = json.loads(result.stdout)
            manifest = json.loads(
                (output / "published" / "gfs025" / "latest.json").read_text(encoding="utf-8")
            )

        self.assertEqual(payload["status"], "complete")
        self.assertEqual(manifest["status"], "complete")
        self.assertEqual(
            manifest["missing_object_required_variables"],
            [
                {
                    "valid_time_utc": "2026-07-07T15:00:00Z",
                    "source_run": "2026070712",
                    "missing_required_variables": ["pressure_msl"],
                }
            ],
        )
        self.assertEqual(manifest["missing_bundle_required_variables"], [])
        entries = manifest["files"][0]["entries"]
        self.assertIn("pressure_msl", [entry["variable"] for entry in entries])

    def test_cli_falls_back_to_older_run_for_required_variables_missing_from_latest_object(self):
        _OpenMeteoDownloadHandler.catalog_variables = ["temperature_2m", "pressure_msl"]
        _OpenMeteoDownloadHandler.catalog_reference_times = {
            "ncep_gfs013": "2026-07-07T12:00:00Z",
            "ncep_gfs025": "2026-07-07T18:00:00Z",
        }
        _OpenMeteoDownloadHandler.catalog_valid_times = ["2026-07-07T18:00Z"]
        latest_path = "/data_spatial/ncep_gfs025/2026/07/07/1800Z/2026-07-07T1800.om"
        fallback_path = "/data_spatial/ncep_gfs025/2026/07/07/1200Z/2026-07-07T1800.om"
        _OpenMeteoDownloadHandler.object_content_by_path = {
            latest_path: _sample_object_with_plain_lut_arrays(["temperature_2m"]),
            fallback_path: _sample_object_with_plain_lut_arrays(["pressure_msl"]),
        }
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            config = root / "models.json"
            output = root / "out"
            _write_config(config, required_variables=["temperature_2m", "pressure_msl"])

            result = subprocess.run(
                [
                    sys.executable,
                    "-m",
                    "om_downloader.cli",
                    "--download-openmeteo-product",
                    "gfs025",
                    "--config",
                    str(config),
                    "--openmeteo-bucket-url",
                    self.bucket_url,
                    "--output",
                    str(output),
                    "--now",
                    "2026-07-08T14:00:00Z",
                    "--lut-codec",
                    "plain",
                ],
                cwd=Path.cwd(),
                text=True,
                capture_output=True,
                check=True,
            )
            payload = json.loads(result.stdout)
            manifest = json.loads(
                (output / "published" / "gfs025" / "latest.json").read_text(encoding="utf-8")
            )

        self.assertEqual(payload["status"], "complete")
        self.assertEqual(manifest["status"], "complete")
        entries = manifest["files"][0]["entries"]
        self.assertEqual([entry["variable"] for entry in entries], ["temperature_2m", "pressure_msl"])
        self.assertEqual(entries[0]["source_run"], "2026070718")
        self.assertEqual(entries[0]["forecast_hour"], 0)
        self.assertEqual(entries[1]["source_run"], "2026070712")
        self.assertEqual(entries[1]["forecast_hour"], 6)
        self.assertIn(fallback_path, entries[1]["source_url"])

    def test_cli_downloads_gfs_group_only_when_all_products_share_run(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            config = root / "models.json"
            output = root / "out"
            _write_gfs_group_config(config)

            result = subprocess.run(
                [
                    sys.executable,
                    "-m",
                    "om_downloader.cli",
                    "--download-openmeteo-group",
                    "gfs",
                    "--config",
                    str(config),
                    "--openmeteo-bucket-url",
                    self.bucket_url,
                    "--output",
                    str(output),
                    "--now",
                    "2026-07-08T14:00:00Z",
                    "--lut-codec",
                    "plain",
                ],
                cwd=Path.cwd(),
                text=True,
                capture_output=True,
                check=True,
            )

            payload = json.loads(result.stdout)
            group_manifest = json.loads(
                (output / "published" / "groups" / "gfs" / "latest.json").read_text(encoding="utf-8")
            )
            product_latest_exists = {
                product_name: (output / "published" / product_name / "latest.json").exists()
                for product_name in group_manifest["products"]
            }
            run_summary = _read_run_summary(output)

        self.assertEqual(payload["status"], "complete")
        self.assertEqual(group_manifest["status"], "complete")
        self.assertEqual(group_manifest["latest_complete_run"], "2026070712")
        self.assertEqual(group_manifest["files"], 3)
        self.assertEqual(group_manifest["bytes"], 168)
        self.assertEqual(
            sorted(group_manifest["products"]),
            ["gfs013_surface", "gfs025", "gfs_pressure_profile"],
        )
        for product_name in group_manifest["products"]:
            self.assertTrue(product_latest_exists[product_name])
            self.assertEqual(group_manifest["product_manifests"][product_name]["files"], 1)
            self.assertEqual(group_manifest["product_manifests"][product_name]["bytes"], 56)
        group_runs = [
            record
            for record in run_summary
            if record["kind"] == "group" and record["group"] == "gfs"
        ]
        self.assertEqual(len(group_runs), 1)
        group_run = group_runs[0]
        self.assertEqual(group_run["status"], "complete")
        self.assertEqual(group_run["latest_complete_run"], "2026070712")
        self.assertEqual(group_run["files"], 3)
        self.assertEqual(group_run["bytes"], 168)
        self.assertEqual(
            sorted(
                record["product"]
                for record in run_summary
                if record["kind"] == "product"
            ),
            ["gfs013_surface", "gfs025", "gfs_pressure_profile"],
        )

    def test_cli_group_publish_to_api_root_clears_download_payloads_and_skips_next_probe(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            config = root / "models.json"
            output = root / "out"
            api_root = root / "api"
            _write_gfs_group_config(config)
            command = [
                sys.executable,
                "-m",
                "om_downloader.cli",
                "--download-openmeteo-group",
                "gfs",
                "--config",
                str(config),
                "--openmeteo-bucket-url",
                self.bucket_url,
                "--output",
                str(output),
                "--publish-openmeteo-group-to",
                str(api_root),
                "--now",
                "2026-07-08T14:00:00Z",
                "--lut-codec",
                "plain",
            ]

            first = subprocess.run(
                command,
                cwd=Path.cwd(),
                text=True,
                capture_output=True,
                check=True,
            )
            first_payload = json.loads(first.stdout)
            group_manifest_path = output / "published" / "groups" / "gfs" / "latest.json"
            api_ready_path = api_root / "groups" / "gfs" / "current" / "ready_for_processing.json"
            group_manifest = json.loads(group_manifest_path.read_text(encoding="utf-8"))
            api_ready = json.loads(api_ready_path.read_text(encoding="utf-8"))
            _OpenMeteoDownloadHandler.range_headers = []

            second = subprocess.run(
                command,
                cwd=Path.cwd(),
                text=True,
                capture_output=True,
                check=True,
            )
            second_payload = json.loads(second.stdout)

            self.assertEqual(first_payload["status"], "complete")
            self.assertEqual(api_ready["status"], "complete")
            self.assertEqual(api_ready["latest_complete_run"], group_manifest["latest_complete_run"])
            self.assertTrue((output / "published" / "gfs025" / "latest.json").exists())
            self.assertFalse((output / "published" / "gfs025" / "coverages").exists())
            self.assertFalse((output / "published" / "gfs013_surface" / "coverages").exists())
            self.assertFalse((output / "published" / "gfs_pressure_profile" / "coverages").exists())
            self.assertTrue((api_root / "gfs025" / "coverages").exists())
            self.assertEqual(second_payload["status"], "skipped")
            self.assertEqual(second_payload["reason"], "api group already current")
            self.assertFalse(
                any(header == "bytes=2000-2039" for header in _OpenMeteoDownloadHandler.range_headers)
            )

    def test_cli_mixed_group_reuses_api_current_product_when_download_payload_was_cleared(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            config = root / "models.json"
            output = root / "out"
            api_root = root / "api"
            _write_gfs_group_config(config)
            command = [
                sys.executable,
                "-m",
                "om_downloader.cli",
                "--download-openmeteo-group",
                "gfs",
                "--config",
                str(config),
                "--openmeteo-bucket-url",
                self.bucket_url,
                "--output",
                str(output),
                "--publish-openmeteo-group-to",
                str(api_root),
                "--now",
                "2026-07-08T14:00:00Z",
                "--lut-codec",
                "plain",
            ]
            subprocess.run(
                command,
                cwd=Path.cwd(),
                text=True,
                capture_output=True,
                check=True,
            )
            self.assertFalse((output / "published" / "gfs025" / "coverages").exists())
            before_count = len(_read_run_summary(output))
            _OpenMeteoDownloadHandler.catalog_reference_times = {
                "ncep_gfs013": "2026-07-07T18:00:00Z",
                "ncep_gfs025": "2026-07-07T12:00:00Z",
            }

            second = subprocess.run(
                command,
                cwd=Path.cwd(),
                text=True,
                capture_output=True,
                check=True,
            )
            second_payload = json.loads(second.stdout)
            new_records = _read_run_summary(output)[before_count:]
            new_product_records = [
                record["product"] for record in new_records if record.get("kind") == "product"
            ]
            api_ready = json.loads(
                (api_root / "groups" / "gfs" / "current" / "ready_for_processing.json").read_text(
                    encoding="utf-8"
                )
            )

        self.assertEqual(second_payload["status"], "complete")
        self.assertIsNone(api_ready["latest_complete_run"])
        self.assertEqual(new_product_records, ["gfs013_surface"])
        self.assertEqual(
            {
                product: summary["latest_complete_run"]
                for product, summary in api_ready["product_manifests"].items()
            },
            {
                "gfs013_surface": "2026070718",
                "gfs025": "2026070712",
                "gfs_pressure_profile": "2026070712",
            },
        )

    def test_cli_group_download_failure_writes_product_and_group_errors(self):
        _OpenMeteoDownloadHandler.fail_ranges = {"bytes=2000-2039"}
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            config = root / "models.json"
            output = root / "out"
            _write_gfs_group_config(config)

            with self.assertRaises(subprocess.CalledProcessError):
                subprocess.run(
                    [
                        sys.executable,
                        "-m",
                        "om_downloader.cli",
                        "--download-openmeteo-group",
                        "gfs",
                        "--config",
                        str(config),
                        "--openmeteo-bucket-url",
                        self.bucket_url,
                        "--output",
                        str(output),
                        "--now",
                        "2026-07-08T14:00:00Z",
                        "--lut-codec",
                        "plain",
                    ],
                    cwd=Path.cwd(),
                    text=True,
                    capture_output=True,
                    check=True,
                )

            run_summary = _read_run_summary(output)

        product_failures = [
            record
            for record in run_summary
            if record.get("kind") == "product" and record.get("status") == "failed"
        ]
        group_failures = [
            record
            for record in run_summary
            if record.get("kind") == "group"
            and record.get("group") == "gfs"
            and record.get("status") == "failed"
        ]
        self.assertEqual(len(product_failures), 1)
        self.assertEqual(len(group_failures), 1)
        self.assertTrue(product_failures[0]["remote_error"])
        self.assertTrue(group_failures[0]["remote_error"])
        self.assertIn("range request failed", product_failures[0]["error_message"])
        self.assertGreaterEqual(group_failures[0]["duration_seconds"], 0)

    def test_cli_clears_group_published_data_before_downloading_new_run(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            config = root / "models.json"
            output = root / "out"
            _write_gfs_group_config(config)
            stale_group_file = output / "published" / "groups" / "gfs" / "old.marker"
            stale_group_file.parent.mkdir(parents=True)
            stale_group_file.write_text("old group", encoding="utf-8")
            stale_product_file = (
                output
                / "published"
                / "gfs025"
                / "coverages"
                / "gfs025_older"
                / "old.omranges"
            )
            stale_product_file.parent.mkdir(parents=True)
            stale_product_file.write_bytes(b"old")
            unrelated_file = (
                output
                / "published"
                / "cams_global"
                / "coverages"
                / "cams_global_keep"
                / "keep.omranges"
            )
            unrelated_file.parent.mkdir(parents=True)
            unrelated_file.write_bytes(b"keep")

            subprocess.run(
                [
                    sys.executable,
                    "-m",
                    "om_downloader.cli",
                    "--download-openmeteo-group",
                    "gfs",
                    "--config",
                    str(config),
                    "--openmeteo-bucket-url",
                    self.bucket_url,
                    "--output",
                    str(output),
                    "--now",
                    "2026-07-08T14:00:00Z",
                    "--lut-codec",
                    "plain",
                ],
                cwd=Path.cwd(),
                text=True,
                capture_output=True,
                check=True,
            )

            self.assertFalse(stale_group_file.exists())
            self.assertFalse(stale_product_file.exists())
            self.assertTrue(unrelated_file.exists())

    def test_cli_gfs_group_downloads_when_product_runs_do_not_match(self):
        _OpenMeteoDownloadHandler.catalog_reference_times = {
            "ncep_gfs013": "2026-07-07T12:00:00Z",
            "ncep_gfs025": "2026-07-07T18:00:00Z",
        }
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            config = root / "models.json"
            output = root / "out"
            _write_gfs_group_config(config)

            result = subprocess.run(
                [
                    sys.executable,
                    "-m",
                    "om_downloader.cli",
                    "--download-openmeteo-group",
                    "gfs",
                    "--config",
                    str(config),
                    "--openmeteo-bucket-url",
                    self.bucket_url,
                    "--output",
                    str(output),
                    "--now",
                    "2026-07-08T14:00:00Z",
                    "--lut-codec",
                    "plain",
                ],
                cwd=Path.cwd(),
                text=True,
                capture_output=True,
                check=True,
            )

            payload = json.loads(result.stdout)
            group_manifest = json.loads(
                (output / "published" / "groups" / "gfs" / "latest.json").read_text(
                    encoding="utf-8"
                )
            )

        self.assertEqual(payload["status"], "complete")
        self.assertIsNone(payload["latest_complete_run"])
        self.assertEqual(group_manifest["status"], "complete")
        self.assertIsNone(group_manifest["latest_complete_run"])
        self.assertEqual(
            {
                product: summary["latest_complete_run"]
                for product, summary in group_manifest["product_manifests"].items()
            },
            {
                "gfs013_surface": "2026070712",
                "gfs025": "2026070718",
                "gfs_pressure_profile": "2026070718",
            },
        )

    def test_cli_replaces_old_changed_product_when_product_runs_do_not_match(self):
        _OpenMeteoDownloadHandler.catalog_reference_times = {
            "ncep_gfs013": "2026-07-07T12:00:00Z",
            "ncep_gfs025": "2026-07-07T18:00:00Z",
        }
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            config = root / "models.json"
            output = root / "out"
            _write_gfs_group_config(config)
            stale_product_file = (
                output
                / "published"
                / "gfs025"
                / "coverages"
                / "gfs025_older"
                / "old.omranges"
            )
            stale_product_file.parent.mkdir(parents=True)
            stale_product_file.write_bytes(b"old")

            subprocess.run(
                [
                    sys.executable,
                    "-m",
                    "om_downloader.cli",
                    "--download-openmeteo-group",
                    "gfs",
                    "--config",
                    str(config),
                    "--openmeteo-bucket-url",
                    self.bucket_url,
                    "--output",
                    str(output),
                    "--now",
                    "2026-07-08T14:00:00Z",
                    "--lut-codec",
                    "plain",
                ],
                cwd=Path.cwd(),
                text=True,
                capture_output=True,
                check=True,
            )

            self.assertFalse(stale_product_file.exists())
            self.assertTrue((output / "published" / "gfs025" / "latest.json").exists())

    def test_cli_group_lock_skips_duplicate_probe_without_error(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            config = root / "models.json"
            output = root / "out"
            _write_gfs_group_config(config)
            (output / "locks").mkdir(parents=True)
            (output / "locks" / "gfs.lock").write_text("pid=1\n", encoding="utf-8")

            result = subprocess.run(
                [
                    sys.executable,
                    "-m",
                    "om_downloader.cli",
                    "--download-openmeteo-group",
                    "gfs",
                    "--config",
                    str(config),
                    "--openmeteo-bucket-url",
                    self.bucket_url,
                    "--output",
                    str(output),
                    "--now",
                    "2026-07-08T14:00:00Z",
                    "--lut-codec",
                    "plain",
                ],
                cwd=Path.cwd(),
                text=True,
                capture_output=True,
                check=True,
            )

            payload = json.loads(result.stdout)

        self.assertEqual(payload["status"], "skipped")
        self.assertEqual(payload["reason"], "group already running")


if __name__ == "__main__":
    unittest.main()
