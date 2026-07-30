from pathlib import Path
import json
import sys


VALIDATION_ROOT = Path(__file__).resolve().parents[1]
if str(VALIDATION_ROOT) not in sys.path:
    sys.path.insert(0, str(VALIDATION_ROOT))

import sequential_2000_server_parity as parity


def test_point_contract_has_equal_grid_and_offgrid_coverage() -> None:
    points = parity.sample_points(20260730)

    assert len(points) == 2000
    assert sum(point["kind"] == "exact_common_grid" for point in points) == 1000
    assert sum(point["kind"] == "offgrid_uniform" for point in points) == 1000
    assert all(0.0 <= point["latitude"] <= 58.0 for point in points)
    assert all(70.0 <= point["longitude"] <= 140.0 for point in points)


def test_full_model_ranges_end_at_published_horizons() -> None:
    assert parity.comparison_ranges("gfs", "2026073000")[0] == (
        "2026-07-30T00:00",
        "2026-08-15T00:00",
    )
    assert parity.comparison_ranges("ec", "2026073000")[0] == (
        "2026-07-30T00:00",
        "2026-08-14T00:00",
    )
    assert parity.comparison_ranges("cams", "2026073000")[0] == (
        "2026-07-30T00:00",
        "2026-08-04T00:00",
    )


def test_probability_is_in_hourly_and_daily_acceptance_contracts() -> None:
    assert "precipitation_probability" in parity.VARIABLES["gfs"]["hourly"]
    assert "precipitation_probability_max" in parity.VARIABLES["gfs"]["daily"]
    assert "precipitation_probability" in parity.VARIABLES["ec"]["hourly"]
    assert "precipitation_probability_mean" in parity.VARIABLES["ec"]["daily"]


def test_batch_identity_requires_equal_source_runs_and_horizons(tmp_path: Path) -> None:
    runs = {
        "gfs": "2026073000",
        "ec": "2026073000",
        "cams": "2026073000",
    }
    models = {}
    for model, run in runs.items():
        server = {
            "latest_complete_run": run,
            "source_runs": [run],
            "source_run_max_forecast_hours": {run: parity.HORIZONS[model]},
            "products": {
                model: {
                    "latest_complete_run": run,
                    "max_forecast_hour": parity.HORIZONS[model],
                }
            },
        }
        models[model] = {
            "expected_run": run,
            "shanghai": server,
            "singapore": dict(server),
        }
    path = tmp_path / "identity.json"
    path.write_text(
        json.dumps({"status": "matched", "models": models}),
        encoding="utf-8",
    )

    loaded = parity.validate_identity_report(path, runs)

    assert loaded["status"] == "matched"


def test_batch_identity_rejects_different_source_run(tmp_path: Path) -> None:
    path = tmp_path / "identity.json"
    path.write_text(
        json.dumps(
            {
                "status": "matched",
                "models": {
                    "gfs": {
                        "expected_run": "2026073000",
                        "shanghai": {
                            "latest_complete_run": "2026073000",
                            "source_runs": ["2026073000"],
                            "source_run_max_forecast_hours": {
                                "2026073000": 384
                            },
                            "products": {
                                "gfs": {
                                    "latest_complete_run": "2026073000",
                                    "max_forecast_hour": 384,
                                }
                            },
                        },
                        "singapore": {
                            "latest_complete_run": "2026073000",
                            "source_runs": ["2026072918"],
                            "source_run_max_forecast_hours": {
                                "2026072918": 384
                            },
                            "products": {
                                "gfs": {
                                    "latest_complete_run": "2026072918",
                                    "max_forecast_hour": 384,
                                }
                            },
                        },
                    }
                },
            }
        ),
        encoding="utf-8",
    )

    try:
        parity.validate_identity_report(path, {"gfs": "2026073000"})
    except ValueError as error:
        assert "source runs differ" in str(error)
    else:
        raise AssertionError("different source runs were accepted")
