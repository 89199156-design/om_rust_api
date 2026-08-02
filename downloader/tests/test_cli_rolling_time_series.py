import io
import json
import unittest
from datetime import datetime, timedelta, timezone
from types import SimpleNamespace
from unittest.mock import patch

from om_downloader import cli
from om_downloader.model_config import Bounds, ProductConfig


UTC = timezone.utc


def _product() -> ProductConfig:
    return ProductConfig(
        name="ncep_gefs05",
        download_product="om_ncep_gefs05",
        openmeteo_model="ncep_gefs05",
        forecast_hour_start=3,
        forecast_hour_end=384,
        run_cadence_hours=6,
        timezone_anchors=(8, 6),
        requested_bounds=Bounds(70.0, 0.0, 140.0, 58.0),
        bounds_padding_degrees=2.0,
        required_variables=("precipitation_probability",),
        optional_variables=(),
        requested_pressure_levels_hpa=(),
        source_mode="rolling_time_series",
    )


def _plan(run: datetime, forecast_hours: tuple[int, ...]):
    return SimpleNamespace(
        latest_complete_run=run.strftime("%Y%m%d%H"),
        slots=[
            SimpleNamespace(valid_time_utc=run + timedelta(hours=hour))
            for hour in forecast_hours
        ],
    )


def _response(payload):
    return io.BytesIO(json.dumps(payload).encode("utf-8"))


class RollingTimeSeriesPlanTests(unittest.TestCase):
    def test_plans_regular_axis_and_marks_only_public_slots_as_public(self):
        run = datetime(2026, 7, 29, tzinfo=UTC)
        meta = {
            "chunk_time_length": 313,
            "temporal_resolution_seconds": 3 * 3600,
            "last_run_initialisation_time": int(run.timestamp()),
            "data_end_time": int((run + timedelta(hours=18)).timestamp()),
        }

        with patch.object(cli, "urlopen", return_value=_response(meta)):
            records = cli._rolling_time_series_object_records(
                _product(),
                _plan(run, (3, 6, 12)),
                bucket_url="https://example.invalid",
            )

        aliases = sorted(
            (
                alias
                for record in records
                for alias in record["rolling_aliases"]
            ),
            key=lambda item: item["valid_time_utc"],
        )
        self.assertEqual(
            [item["forecast_hour"] for item in aliases],
            [0, 3, 6, 9, 12, 15],
        )
        self.assertEqual(
            [item["interpolation_support"] for item in aliases],
            [True, False, False, True, False, True],
        )
        self.assertTrue(
            all(item["source_run"] == "2026072900" for item in aliases)
        )
        self.assertTrue(
            all(
                record["url"].startswith(
                    "https://example.invalid/data/ncep_gefs05/"
                    "precipitation_probability/chunk_"
                )
                for record in records
            )
        )

    def test_newer_rolling_database_defers_to_immutable_spatial_run(self):
        selected = datetime(2026, 7, 29, tzinfo=UTC)
        actual = selected + timedelta(hours=6)
        meta = {
            "chunk_time_length": 313,
            "temporal_resolution_seconds": 3 * 3600,
            "last_run_initialisation_time": int(actual.timestamp()),
            "data_end_time": int((actual + timedelta(days=20)).timestamp()),
        }

        with patch.object(cli, "urlopen", return_value=_response(meta)):
            records = cli._rolling_time_series_object_records(
                _product(),
                _plan(selected, (3, 6)),
                bucket_url="https://example.invalid",
            )

        self.assertIsNone(records)

    def test_refuses_rolling_database_older_than_selected_run(self):
        selected = datetime(2026, 7, 29, 6, tzinfo=UTC)
        actual = selected - timedelta(hours=6)
        meta = {
            "chunk_time_length": 313,
            "temporal_resolution_seconds": 3 * 3600,
            "last_run_initialisation_time": int(actual.timestamp()),
            "data_end_time": int((selected + timedelta(days=20)).timestamp()),
        }

        with patch.object(cli, "urlopen", return_value=_response(meta)):
            with self.assertRaisesRegex(ValueError, "older than selected"):
                cli._rolling_time_series_object_records(
                    _product(),
                    _plan(selected, (3, 6)),
                    bucket_url="https://example.invalid",
                )

    def test_historical_rolling_product_reads_its_spatial_objects(self):
        product = _product()
        plan = _plan(datetime(2026, 7, 29, tzinfo=UTC), (3, 6))
        runs = [object()]
        spatial = [{"url": "https://example.invalid/data_spatial/run.om"}]
        supported = spatial + [{"interpolation_support": True}]

        with (
            patch.object(cli, "_rolling_time_series_object_records", return_value=None),
            patch.object(cli, "coverage_object_records", return_value=spatial) as coverage,
            patch.object(
                cli,
                "_with_interpolation_support_records",
                return_value=supported,
            ) as support,
        ):
            records = cli._product_coverage_object_records(
                product,
                plan,
                runs,
                bucket_url="https://example.invalid",
            )

        self.assertEqual(records, supported)
        coverage.assert_called_once_with(
            plan,
            runs,
            bucket_url="https://example.invalid",
            openmeteo_model="ncep_gefs05",
        )
        support.assert_called_once_with(
            product,
            plan,
            runs,
            spatial,
            bucket_url="https://example.invalid",
        )

    def test_refuses_missing_tail_lookahead(self):
        run = datetime(2026, 7, 29, tzinfo=UTC)
        meta = {
            "chunk_time_length": 313,
            "temporal_resolution_seconds": 3 * 3600,
            "last_run_initialisation_time": int(run.timestamp()),
            "data_end_time": int((run + timedelta(hours=12)).timestamp()),
        }

        with patch.object(cli, "urlopen", return_value=_response(meta)):
            with self.assertRaisesRegex(ValueError, "lookahead"):
                cli._rolling_time_series_object_records(
                    _product(),
                    _plan(run, (3, 6, 12)),
                    bucket_url="https://example.invalid",
                )


if __name__ == "__main__":
    unittest.main()
