import json
import struct
import subprocess
import sys
import tempfile
import threading
import unittest
from contextlib import redirect_stderr, redirect_stdout
from datetime import datetime, timedelta, timezone
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from io import StringIO
from pathlib import Path
from types import SimpleNamespace
from unittest.mock import patch

from om_downloader import cli as cli_module
from om_downloader.metadata import OmRun
from tests.test_om_format import OM_HEADER, OM_TRAILER_MAGIC, _pack_array, _pack_root


def _product_config(
    openmeteo_model="ncep_gfs025",
    required_variables=None,
    forecast_hour_end=4,
):
    required_variables = required_variables or ["temperature_2m"]
    return {
        "download_product": "om_gfs025",
        "openmeteo_model": openmeteo_model,
        "forecast_hour_end": forecast_hour_end,
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


def _write_gfs_group_config(path, *, forecast_hour_end=4):
    _OpenMeteoDownloadHandler.complete_run_catalogs = True
    products = {
        "gfs013_surface": _product_config(
            "ncep_gfs013", forecast_hour_end=forecast_hour_end
        ),
        "gfs025": _product_config(
            "ncep_gfs025", forecast_hour_end=forecast_hour_end
        ),
        "gfs_pressure_profile": _product_config(
            "ncep_gfs025", forecast_hour_end=forecast_hour_end
        ),
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
    complete_run_catalogs = False
    complete_run_forecast_hour_end = 4

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
            valid_times = self.catalog_valid_times
            if self.complete_run_catalogs:
                base = datetime.fromisoformat(reference_time.replace("Z", "+00:00"))
                valid_times = [
                    (base + timedelta(hours=hour)).strftime("%Y-%m-%dT%H:%MZ")
                    for hour in range(self.complete_run_forecast_hour_end + 1)
                ]
            payload = json.dumps(
                [
                    {
                        "completed": True,
                        "reference_time": reference_time,
                        "valid_times": valid_times,
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
        _OpenMeteoDownloadHandler.complete_run_catalogs = False
        _OpenMeteoDownloadHandler.complete_run_forecast_hour_end = 4
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

    def test_cams_retention_pairs_each_products_three_latest_complete_runs_by_rank(self):
        products = [
            SimpleNamespace(
                name="cams_global",
                openmeteo_model="cams_global",
                run_cadence_hours=12,
                forecast_hour_end=4,
            ),
            SimpleNamespace(
                name="cams_global_greenhouse_gases",
                openmeteo_model="cams_global_greenhouse_gases",
                run_cadence_hours=24,
                forecast_hour_end=4,
            ),
        ]
        latest_times = {
            "cams_global": datetime(2026, 7, 15, 0, tzinfo=timezone.utc),
            "cams_global_greenhouse_gases": datetime(2026, 7, 14, 0, tzinfo=timezone.utc),
        }

        def catalog(model, reference_time):
            return SimpleNamespace(
                completed=True,
                reference_time_utc=reference_time,
                max_forecast_hour=120,
                model=model,
            )

        def load_latest(model, *, bucket_url):
            self.assertEqual(bucket_url, "https://example.invalid")
            return catalog(model, latest_times[model])

        def load_run(model, reference_time, *, bucket_url):
            self.assertEqual(bucket_url, "https://example.invalid")
            return catalog(model, reference_time)

        def complete_plan(product, product_catalog):
            run_id = product_catalog.reference_time_utc.strftime("%Y%m%d%H")
            plan = SimpleNamespace(latest_complete_run=run_id)
            return product_catalog, [SimpleNamespace(reference_time_utc=product_catalog.reference_time_utc)], plan

        with (
            patch.object(cli_module, "load_openmeteo_spatial_latest", side_effect=load_latest),
            patch.object(cli_module, "load_openmeteo_spatial_run", side_effect=load_run),
            patch.object(cli_module, "_complete_run_plan_data", side_effect=complete_plan),
        ):
            ranked = cli_module._discover_recent_complete_cams_ranked_plans(
                products,
                bucket_url="https://example.invalid",
                count=3,
            )

        self.assertEqual([run for run, _plans in ranked], [
            "2026071500",
            "2026071412",
            "2026071400",
        ])
        self.assertEqual(
            [
                {
                    name: data[2].latest_complete_run
                    for name, data in plans.items()
                }
                for _run, plans in ranked
            ],
            [
                {
                    "cams_global": "2026071500",
                    "cams_global_greenhouse_gases": "2026071400",
                },
                {
                    "cams_global": "2026071412",
                    "cams_global_greenhouse_gases": "2026071300",
                },
                {
                    "cams_global": "2026071400",
                    "cams_global_greenhouse_gases": "2026071200",
                },
            ],
        )

    def test_gfs_retention_targets_two_full_and_three_zero_through_five_hour_runs(self):
        products = [
            SimpleNamespace(name=name, forecast_hour_end=12)
            for name in ("gfs013_surface", "gfs025", "gfs_pressure_profile")
        ]
        base = datetime(2026, 7, 15, 0, tzinfo=timezone.utc)
        discovered = []
        for rank in range(5):
            run_time = base - cli_module.timedelta(hours=rank * 6)
            run_id = run_time.strftime("%Y%m%d%H")
            plans = {}
            for product in products:
                run = OmRun(run_id, run_time, 12, ("temperature_2m",), ())
                plans[product.name] = (
                    SimpleNamespace(reference_time_utc=run_time),
                    [run],
                    SimpleNamespace(
                        latest_complete_run=run_id,
                        slots=tuple(range(13)),
                    ),
                )
            discovered.append((run_id, plans))

        with patch.object(
            cli_module,
            "_discover_recent_complete_group_plans",
            return_value=discovered,
        ):
            ranked = cli_module._discover_recent_gfs_retention_plans(
                products,
                bucket_url="https://example.invalid",
            )

        for _run, plans in ranked[:2]:
            self.assertEqual(
                {len(plan_data[2].slots) for plan_data in plans.values()},
                {13},
            )
        for _run, plans in ranked[2:]:
            self.assertEqual(
                {
                    tuple(slot.forecast_hour for slot in plan_data[2].slots)
                    for plan_data in plans.values()
                },
                {tuple(range(6))},
            )

    def test_exact_gfs_short_run_plan_loads_each_model_once_and_requires_f000_through_f005(self):
        products = [
            SimpleNamespace(
                name="gfs013_surface",
                openmeteo_model="ncep_gfs013",
                forecast_hour_end=384,
            ),
            SimpleNamespace(
                name="gfs025",
                openmeteo_model="ncep_gfs025",
                forecast_hour_end=384,
            ),
            SimpleNamespace(
                name="gfs_pressure_profile",
                openmeteo_model="ncep_gfs025",
                forecast_hour_end=384,
            ),
        ]
        reference_time = datetime(2026, 7, 19, 18, tzinfo=timezone.utc)
        calls = []

        def load_run(model, requested_time, *, bucket_url):
            calls.append((model, requested_time, bucket_url))
            return SimpleNamespace(
                completed=True,
                reference_time_utc=reference_time,
                valid_times_utc=tuple(
                    reference_time + timedelta(hours=hour) for hour in range(13)
                ),
                variables=("temperature_2m",),
                max_forecast_hour=12,
            )

        with patch.object(cli_module, "load_openmeteo_spatial_run", side_effect=load_run):
            plans = cli_module._build_exact_gfs_short_run_plans(
                products,
                run_id="2026071918",
                bucket_url="https://example.invalid",
            )

        self.assertEqual(
            [call[0] for call in calls],
            ["ncep_gfs013", "ncep_gfs025"],
        )
        for _catalog, runs, plan in plans.values():
            self.assertEqual(len(runs), 1)
            self.assertEqual(plan.latest_complete_run, "2026071918")
            self.assertEqual(
                tuple(slot.forecast_hour for slot in plan.slots),
                tuple(range(6)),
            )
            self.assertEqual({slot.source_run for slot in plan.slots}, {"2026071918"})

    def test_gfs_short_run_recovery_retains_without_changing_current_markers(self):
        products = [
            SimpleNamespace(name=name)
            for name in ("gfs013_surface", "gfs025", "gfs_pressure_profile")
        ]
        run_id = "2026071918"
        plans = {
            product.name: (None, [], SimpleNamespace(latest_complete_run=run_id))
            for product in products
        }
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            api_root = root / "raw"
            output = root / "downloader"
            marker = api_root / "groups" / "gfs" / "current" / "ready_for_processing.json"
            marker.parent.mkdir(parents=True)
            marker.write_text('{"latest_complete_run":"2026072018"}', encoding="utf-8")
            before = marker.read_bytes()
            args = SimpleNamespace(
                config="models.json",
                recover_openmeteo_gfs_short_run=run_id,
                retain_openmeteo_group_to=str(api_root),
                openmeteo_bucket_url="https://example.invalid",
                output=str(output),
            )
            seen_download_args = []

            def fake_download(download_args, _parser, *, plan_by_product_override, preserve_published):
                seen_download_args.append(download_args)
                self.assertIs(plan_by_product_override, plans)
                self.assertTrue(preserve_published)
                print(json.dumps({"group": "gfs", "status": "complete"}))
                return 0

            stdout = StringIO()
            with (
                patch.object(
                    cli_module,
                    "load_models",
                    return_value=SimpleNamespace(
                        products={product.name: product for product in products}
                    ),
                ),
                patch.object(
                    cli_module,
                    "_build_exact_gfs_short_run_plans",
                    return_value=plans,
                ),
                patch.object(
                    cli_module,
                    "_matching_group_releases",
                    side_effect=[{}, {run_id: {}}, {run_id: {}}],
                ),
                patch.object(
                    cli_module,
                    "_download_openmeteo_group_release",
                    side_effect=fake_download,
                ),
                patch.object(
                    cli_module,
                    "_read_json_if_exists",
                    return_value={"status": "complete"},
                ),
                patch.object(cli_module, "_manifest_matches_plan", return_value=True),
                patch.object(cli_module, "_group_release_matches_plans", return_value=True),
                patch.object(
                    cli_module,
                    "retain_group_release_from_mirror",
                    return_value={"status": "retained"},
                ) as retain,
                patch.object(
                    cli_module,
                    "_clear_group_download_payloads",
                    return_value=["cleared"],
                ),
                patch.object(cli_module, "activate_group_release") as activate,
                redirect_stdout(stdout),
            ):
                result = cli_module._recover_openmeteo_gfs_short_run(
                    args,
                    SimpleNamespace(error=lambda message: self.fail(message)),
                )

            payload = json.loads(stdout.getvalue())
            self.assertEqual(result, 0)
            self.assertEqual(payload["status"], "complete")
            self.assertFalse(payload["activated"])
            self.assertTrue(payload["current_markers_unchanged"])
            self.assertEqual(marker.read_bytes(), before)
            self.assertEqual(len(seen_download_args), 1)
            self.assertEqual(seen_download_args[0].download_openmeteo_group, "gfs")
            self.assertEqual(seen_download_args[0].now, "2026-07-19T18:00:00Z")
            self.assertEqual(
                Path(seen_download_args[0].output),
                output / "recovery" / "gfs" / run_id,
            )
            retain.assert_called_once_with(
                "gfs",
                output / "recovery" / "gfs" / run_id / "published",
                api_root,
            )
            activate.assert_not_called()

    def test_gfs_short_run_recovery_rejects_target_inside_recovery_staging(self):
        products = [
            SimpleNamespace(name=name)
            for name in ("gfs013_surface", "gfs025", "gfs_pressure_profile")
        ]
        run_id = "2026071918"
        plans = {
            product.name: (None, [], SimpleNamespace(latest_complete_run=run_id))
            for product in products
        }
        with tempfile.TemporaryDirectory() as tmp:
            output = Path(tmp) / "downloader"
            args = SimpleNamespace(
                config="models.json",
                recover_openmeteo_gfs_short_run=run_id,
                retain_openmeteo_group_to=str(output / "recovery" / "gfs" / run_id),
                openmeteo_bucket_url="https://example.invalid",
                output=str(output),
            )
            with (
                patch.object(
                    cli_module,
                    "load_models",
                    return_value=SimpleNamespace(
                        products={product.name: product for product in products}
                    ),
                ),
                patch.object(
                    cli_module,
                    "_build_exact_gfs_short_run_plans",
                    return_value=plans,
                ),
                redirect_stderr(StringIO()),
                self.assertRaises(SystemExit),
            ):
                cli_module._recover_openmeteo_gfs_short_run(
                    args,
                    cli_module.argparse.ArgumentParser(),
                )

    def test_cli_recovers_exact_gfs_short_run_without_activating_it(self):
        _OpenMeteoDownloadHandler.complete_run_forecast_hour_end = 5
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            config = root / "models.json"
            output = root / "out"
            api_root = root / "api"
            _write_gfs_group_config(config, forecast_hour_end=5)
            marker_paths = [
                api_root / "groups" / "gfs" / "current" / "latest.json",
                api_root
                / "groups"
                / "gfs"
                / "current"
                / "ready_for_processing.json",
            ]
            for product in ("gfs013_surface", "gfs025", "gfs_pressure_profile"):
                marker_paths.extend(
                    [
                        api_root / product / "current" / "latest.json",
                        api_root / product / "current" / "ready_for_processing.json",
                    ]
                )
            for index, path in enumerate(marker_paths):
                path.parent.mkdir(parents=True, exist_ok=True)
                path.write_text(json.dumps({"frozen_current": index}), encoding="utf-8")
            markers_before = {path: path.read_bytes() for path in marker_paths}

            command = [
                sys.executable,
                "-m",
                "om_downloader.cli",
                "--recover-openmeteo-gfs-short-run",
                "2026070712",
                "--retain-openmeteo-group-to",
                str(api_root),
                "--config",
                str(config),
                "--openmeteo-bucket-url",
                self.bucket_url,
                "--output",
                str(output),
                "--lut-codec",
                "plain",
            ]
            result = subprocess.run(
                command,
                cwd=Path.cwd(),
                text=True,
                capture_output=True,
            )
            self.assertEqual(result.returncode, 0, result.stderr)
            payload = json.loads(result.stdout)
            second = subprocess.run(
                command,
                cwd=Path.cwd(),
                text=True,
                capture_output=True,
            )
            self.assertEqual(second.returncode, 0, second.stderr)
            second_payload = json.loads(second.stdout)
            release_paths = list((api_root / "groups" / "gfs" / "releases").glob("*.json"))
            release = json.loads(release_paths[0].read_text(encoding="utf-8"))
            retained_manifests = {
                product: json.loads(
                    next((api_root / product / "coverages").glob("*/latest.json")).read_text(
                        encoding="utf-8"
                    )
                )
                for product in ("gfs013_surface", "gfs025", "gfs_pressure_profile")
            }

            self.assertEqual(payload["status"], "complete")
            self.assertFalse(payload["activated"])
            self.assertTrue(payload["current_markers_unchanged"])
            self.assertEqual(second_payload["status"], "skipped")
            self.assertEqual(
                second_payload["reason"],
                "exact short-run release already retained",
            )
            self.assertEqual(
                {path: path.read_bytes() for path in marker_paths},
                markers_before,
            )
            self.assertEqual(len(release_paths), 1)
            self.assertEqual(release["latest_complete_run"], "2026070712")
            for manifest in retained_manifests.values():
                self.assertEqual(manifest["latest_complete_run"], "2026070712")
                self.assertEqual(manifest["valid_time_count"], 6)
                self.assertEqual(
                    [entry["forecast_hour"] for entry in manifest["files"][0]["entries"]],
                    list(range(6)),
                )
                self.assertEqual(
                    {entry["source_run"] for entry in manifest["files"][0]["entries"]},
                    {"2026070712"},
                )
            recovery_published = output / "recovery" / "gfs" / "2026070712" / "published"
            for product in retained_manifests:
                self.assertFalse((recovery_published / product / "coverages").exists())

    def test_matching_group_releases_selects_target_shape_for_duplicate_run(self):
        run = "2026071412"
        products = [SimpleNamespace(name="gfs013_surface")]
        plans = {run: {"gfs013_surface": (None, [], SimpleNamespace())}}
        full = {"latest_complete_run": run, "shape": "full"}
        partial = {"latest_complete_run": run, "shape": "partial"}

        with (
            patch.object(
                cli_module,
                "_available_group_release_candidates",
                return_value={run: [full, partial]},
            ),
            patch.object(
                cli_module,
                "_group_release_matches_plans",
                side_effect=lambda manifest, _products, _plans: manifest["shape"] == "partial",
            ),
        ):
            selected = cli_module._matching_group_releases(
                Path("/tmp/unused"),
                "gfs",
                products,
                plans,
            )

        self.assertIs(selected[run], partial)

    def test_gfs_reconcile_downloads_only_missing_target_coverages(self):
        products = [
            SimpleNamespace(name=name)
            for name in ("gfs013_surface", "gfs025", "gfs_pressure_profile")
        ]
        runs = ["2026071500", "2026071418", "2026071412", "2026071406"]
        target_plans = [
            (
                run,
                {
                    product.name: (
                        None,
                        [],
                        SimpleNamespace(latest_complete_run=run),
                    )
                    for product in products
                },
            )
            for run in runs
        ]
        initial = {runs[1]: {"latest_complete_run": runs[1]}, runs[3]: {"latest_complete_run": runs[3]}}
        final = {run: {"latest_complete_run": run} for run in runs}
        downloaded = []

        def fake_download(_args, _parser, *, plan_by_product_override, preserve_published):
            self.assertTrue(preserve_published)
            downloaded.append(next(iter(plan_by_product_override.values()))[2].latest_complete_run)
            print(json.dumps({"status": "complete"}))
            return 0

        args = SimpleNamespace(
            config="models.json",
            output="/tmp/gfs-reconcile-test",
            publish_openmeteo_group_to="/tmp/gfs-reconcile-api-test",
            openmeteo_bucket_url="https://example.invalid",
        )
        with (
            patch.object(
                cli_module,
                "load_models",
                return_value=SimpleNamespace(products={product.name: product for product in products}),
            ),
            patch.object(
                cli_module,
                "_discover_recent_gfs_retention_plans",
                return_value=target_plans,
            ),
            patch.object(
                cli_module,
                "_matching_group_releases",
                side_effect=[initial, final],
            ),
            patch.object(cli_module, "_group_release_matches_plans", return_value=True),
            patch.object(cli_module, "_download_openmeteo_group_release", side_effect=fake_download),
            patch.object(cli_module, "retain_group_release_from_mirror", return_value={"status": "retained"}) as retain,
            patch.object(cli_module, "_clear_group_download_payloads", return_value=[]),
            patch.object(
                cli_module,
                "_read_json_if_exists",
                return_value={"latest_complete_run": runs[1]},
            ),
            patch.object(cli_module, "activate_group_release", return_value={"status": "activated"}),
            patch.object(cli_module, "prune_expired_group_releases", return_value=[]) as prune,
        ):
            result = cli_module._reconcile_gfs_retention_window(args, SimpleNamespace())

        self.assertEqual(result, 0)
        self.assertEqual(downloaded, [runs[2], runs[0]])
        self.assertEqual(retain.call_count, 2)
        self.assertEqual(prune.call_count, 3)
        self.assertTrue(prune.call_args_list[0].kwargs["preserve_current"])
        self.assertTrue(prune.call_args_list[1].kwargs["preserve_current"])

    def test_gfs_reconcile_reports_busy_group_without_false_incomplete_error(self):
        products = [
            SimpleNamespace(name=name)
            for name in ("gfs013_surface", "gfs025", "gfs_pressure_profile")
        ]
        run = "2026071500"
        target_plans = [
            (
                run,
                {
                    product.name: (
                        None,
                        [],
                        SimpleNamespace(latest_complete_run=run),
                    )
                    for product in products
                },
            )
        ]

        def fake_download(*_args, **_kwargs):
            print(
                json.dumps(
                    {
                        "group": "gfs",
                        "status": "skipped",
                        "reason": "group already running",
                    }
                )
            )
            return 0

        args = SimpleNamespace(
            config="models.json",
            output="/tmp/gfs-reconcile-busy-test",
            publish_openmeteo_group_to="/tmp/gfs-reconcile-busy-api-test",
            openmeteo_bucket_url="https://example.invalid",
        )
        stdout = StringIO()
        with (
            patch.object(
                cli_module,
                "load_models",
                return_value=SimpleNamespace(products={product.name: product for product in products}),
            ),
            patch.object(
                cli_module,
                "_discover_recent_gfs_retention_plans",
                return_value=target_plans,
            ),
            patch.object(cli_module, "_matching_group_releases", return_value={}),
            patch.object(cli_module, "_download_openmeteo_group_release", side_effect=fake_download),
            patch.object(cli_module, "retain_group_release_from_mirror") as retain,
            patch.object(cli_module, "prune_expired_group_releases", return_value=[]),
            redirect_stdout(stdout),
        ):
            result = cli_module._reconcile_gfs_retention_window(args, SimpleNamespace())

        self.assertEqual(result, 0)
        self.assertEqual(
            json.loads(stdout.getvalue()),
            {
                "group": "gfs",
                "status": "skipped",
                "reason": "group already running",
            },
        )
        retain.assert_not_called()

    def test_gfs_reconcile_defers_legacy_activation_for_native_publisher(self):
        products = [
            SimpleNamespace(name=name)
            for name in ("gfs013_surface", "gfs025", "gfs_pressure_profile")
        ]
        runs = ["2026072018", "2026072012", "2026072006", "2026072000", "2026071918"]
        target_plans = [
            (
                run,
                {
                    product.name: (
                        None,
                        [],
                        SimpleNamespace(latest_complete_run=run),
                    )
                    for product in products
                },
            )
            for run in runs
        ]
        available = {run: {"latest_complete_run": run} for run in runs}
        args = SimpleNamespace(
            config="models.json",
            output="/tmp/gfs-native-source",
            publish_openmeteo_group_to="/tmp/gfs-native-api",
            openmeteo_bucket_url="https://example.invalid",
            defer_openmeteo_gfs_activation=True,
        )
        stdout = StringIO()
        with (
            patch.object(
                cli_module,
                "load_models",
                return_value=SimpleNamespace(
                    products={product.name: product for product in products}
                ),
            ),
            patch.object(
                cli_module,
                "_discover_recent_gfs_retention_plans",
                return_value=target_plans,
            ),
            patch.object(
                cli_module,
                "_matching_group_releases",
                side_effect=[available, available],
            ),
            patch.object(
                cli_module,
                "_read_json_if_exists",
                return_value={
                    "runtime_format": "openmeteo-native-v1",
                    "latest_complete_run": "2026072012",
                },
            ),
            patch.object(cli_module, "activate_group_release") as activate,
            patch.object(cli_module, "prune_expired_group_releases", return_value=[]),
            redirect_stdout(stdout),
        ):
            result = cli_module._reconcile_gfs_retention_window(
                args,
                SimpleNamespace(error=lambda message: self.fail(message)),
            )

        self.assertEqual(result, 0)
        self.assertTrue(json.loads(stdout.getvalue())["activation_deferred"])
        activate.assert_not_called()

    def test_cams_reconcile_downloads_missing_runs_before_activation_and_prune(self):
        products = [
            SimpleNamespace(name=name)
            for name in ("cams_global", "cams_global_greenhouse_gases")
        ]
        runs = ["2026071500", "2026071412", "2026071400"]
        target_plans = [
            (
                run,
                {
                    product.name: (
                        None,
                        [],
                        SimpleNamespace(latest_complete_run=run),
                    )
                    for product in products
                },
            )
            for run in runs
        ]
        initial = {
            runs[1]: {"latest_complete_run": runs[1]},
            runs[2]: {"latest_complete_run": runs[2]},
        }
        final = {run: {"latest_complete_run": run} for run in runs}
        events = []

        def fake_download(_args, _parser, *, plan_by_product_override):
            run = next(iter(plan_by_product_override.values()))[2].latest_complete_run
            events.append(("download", run))
            print(json.dumps({"status": "complete"}))
            return 0

        args = SimpleNamespace(
            config="models.json",
            publish_openmeteo_group_to="/tmp/cams-reconcile-api-test",
            openmeteo_bucket_url="https://example.invalid",
            retain_complete_releases=cli_module.CAMS_COMPLETE_RUN_RETENTION,
        )
        with (
            patch.object(
                cli_module,
                "load_models",
                return_value=SimpleNamespace(
                    products={product.name: product for product in products}
                ),
            ),
            patch.object(
                cli_module,
                "_discover_recent_complete_cams_ranked_plans",
                return_value=target_plans,
            ),
            patch.object(
                cli_module,
                "_available_group_releases",
                side_effect=[initial, final],
            ),
            patch.object(cli_module, "_group_release_matches_plans", return_value=True),
            patch.object(
                cli_module,
                "_download_openmeteo_group_release",
                side_effect=fake_download,
            ),
            patch.object(
                cli_module,
                "_read_json_if_exists",
                return_value={"latest_complete_run": runs[1]},
            ),
            patch.object(
                cli_module,
                "activate_group_release",
                side_effect=lambda *_args: events.append(("activate", runs[0])) or {"status": "activated"},
            ),
            patch.object(
                cli_module,
                "prune_expired_group_releases",
                side_effect=lambda *_args, **_kwargs: events.append(("prune", None)) or [],
            ) as prune,
        ):
            result = cli_module._reconcile_cams_complete_runs(args, SimpleNamespace())

        self.assertEqual(result, 0)
        self.assertEqual(
            events,
            [("prune", None), ("download", runs[0]), ("activate", runs[0]), ("prune", None)],
        )
        self.assertEqual(prune.call_count, 2)

    def test_cams_reconcile_reports_busy_group_without_false_incomplete_error(self):
        products = [
            SimpleNamespace(name=name)
            for name in ("cams_global", "cams_global_greenhouse_gases")
        ]
        run = "2026071500"
        target_plans = [
            (
                run,
                {
                    product.name: (
                        None,
                        [],
                        SimpleNamespace(latest_complete_run=run),
                    )
                    for product in products
                },
            )
        ]

        def fake_download(*_args, **_kwargs):
            print(
                json.dumps(
                    {
                        "group": "cams",
                        "status": "skipped",
                        "reason": "group already running",
                    }
                )
            )
            return 0

        args = SimpleNamespace(
            config="models.json",
            publish_openmeteo_group_to="/tmp/cams-reconcile-busy-api-test",
            openmeteo_bucket_url="https://example.invalid",
            retain_complete_releases=cli_module.CAMS_COMPLETE_RUN_RETENTION,
        )
        stdout = StringIO()
        with (
            patch.object(
                cli_module,
                "load_models",
                return_value=SimpleNamespace(products={product.name: product for product in products}),
            ),
            patch.object(
                cli_module,
                "_discover_recent_complete_cams_ranked_plans",
                return_value=target_plans,
            ),
            patch.object(cli_module, "_available_group_releases", return_value={}),
            patch.object(cli_module, "_download_openmeteo_group_release", side_effect=fake_download),
            patch.object(cli_module, "prune_expired_group_releases", return_value=[]),
            redirect_stdout(stdout),
        ):
            result = cli_module._reconcile_cams_complete_runs(args, SimpleNamespace())

        self.assertEqual(result, 0)
        self.assertEqual(
            json.loads(stdout.getvalue()),
            {
                "group": "cams",
                "status": "skipped",
                "reason": "group already running",
            },
        )

    def test_cams_group_manifest_accepts_independent_product_runs(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            manifests = {
                "cams_global": {
                    "status": "complete",
                    "latest_complete_run": "2026071500",
                    "files": [],
                },
                "cams_global_greenhouse_gases": {
                    "status": "complete",
                    "latest_complete_run": "2026071400",
                    "files": [],
                },
            }
            group = cli_module._write_group_manifest(root, "cams", manifests)

        self.assertEqual(group["status"], "complete")
        self.assertEqual(group["latest_complete_run"], "2026071500")

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

    def test_cli_downloads_all_missing_gfs_retention_targets(self):
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
        self.assertEqual(group_manifest["bytes"], 840)
        self.assertEqual(
            sorted(group_manifest["products"]),
            ["gfs013_surface", "gfs025", "gfs_pressure_profile"],
        )
        for product_name in group_manifest["products"]:
            self.assertTrue(product_latest_exists[product_name])
            self.assertEqual(group_manifest["product_manifests"][product_name]["files"], 1)
            self.assertEqual(group_manifest["product_manifests"][product_name]["bytes"], 280)
        group_runs = [
            record
            for record in run_summary
            if record["kind"] == "group" and record["group"] == "gfs"
        ]
        self.assertEqual(len(group_runs), 5)
        group_run = group_runs[-1]
        self.assertEqual(group_run["status"], "complete")
        self.assertEqual(group_run["latest_complete_run"], "2026070712")
        self.assertEqual(group_run["files"], 3)
        self.assertEqual(group_run["bytes"], 840)
        self.assertEqual(
            sorted(
                record["product"]
                for record in run_summary
                if record["kind"] == "product"
            ),
            [
                "gfs013_surface",
                "gfs013_surface",
                "gfs013_surface",
                "gfs013_surface",
                "gfs013_surface",
                "gfs025",
                "gfs025",
                "gfs025",
                "gfs025",
                "gfs025",
                "gfs_pressure_profile",
                "gfs_pressure_profile",
                "gfs_pressure_profile",
                "gfs_pressure_profile",
                "gfs_pressure_profile",
            ],
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
            self.assertEqual(second_payload["reason"], "target retention window already complete")
            self.assertFalse(
                any(header == "bytes=2000-2039" for header in _OpenMeteoDownloadHandler.range_headers)
            )

    def test_cli_waits_until_remote_gfs_products_share_a_coherent_run(self):
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

        self.assertEqual(second_payload["status"], "skipped")
        self.assertEqual(second_payload["reason"], "target retention window already complete")
        self.assertEqual(api_ready["latest_complete_run"], "2026070712")
        self.assertEqual(new_product_records, [])
        self.assertEqual(
            {
                product: summary["latest_complete_run"]
                for product, summary in api_ready["product_manifests"].items()
            },
            {
                "gfs013_surface": "2026070712",
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

    def test_cli_prunes_stale_product_coverage_only_after_success(self):
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

            self.assertTrue(stale_group_file.exists())
            self.assertFalse(stale_product_file.exists())
            self.assertTrue(unrelated_file.exists())

    def test_cli_gfs_group_selects_latest_coherent_run_when_latest_products_differ(self):
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
        self.assertEqual(payload["latest_complete_run"], "2026070712")
        self.assertEqual(group_manifest["status"], "complete")
        self.assertEqual(group_manifest["latest_complete_run"], "2026070712")
        self.assertEqual(
            {
                product: summary["latest_complete_run"]
                for product, summary in group_manifest["product_manifests"].items()
            },
            {
                "gfs013_surface": "2026070712",
                "gfs025": "2026070712",
                "gfs_pressure_profile": "2026070712",
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

    def test_cli_group_download_ignores_obsolete_lock_files(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            config = root / "models.json"
            output = root / "out"
            _write_gfs_group_config(config)
            (output / "locks").mkdir(parents=True)
            (output / "locks" / "gfs_reconcile.lock").write_text("pid=1\n", encoding="utf-8")

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

        self.assertEqual(payload["status"], "complete")
        self.assertNotEqual(payload.get("reason"), "GFS reconciliation already running")


if __name__ == "__main__":
    unittest.main()
