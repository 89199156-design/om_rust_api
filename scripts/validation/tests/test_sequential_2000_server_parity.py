from pathlib import Path
import json
import sys
import threading
from unittest import mock


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


def test_hourly_and_daily_variables_are_paired_into_the_fewest_requests() -> None:
    assert parity.paired_variable_groups(
        ("h1", "h2", "h3"), ("d1", "d2"), 2
    ) == [
        (("h1", "h2"), ("d1", "d2")),
        (("h3",), ()),
    ]
    for model in ("gfs", "ec", "cams"):
        groups = parity.paired_variable_groups(
            parity.VARIABLES[model]["hourly"],
            parity.VARIABLES[model]["daily"],
            256,
        )
        assert len(groups) == 1
        assert groups[0][0]
        assert groups[0][1]


def test_fetch_uses_a_persistent_production_ssh_client() -> None:
    client = mock.Mock()
    client.ssh_host = "singapore"
    client.request.return_value = (b'{"hourly":{"time":["x"]}}', {}, 0.001)

    response, performance = parity.fetch(
        "http://127.0.0.1:8088",
        "__BASE__/v1/gfs?latitude=31.2",
        10.0,
        0,
        client,
    )

    assert response["hourly"]["time"] == ["x"]
    assert performance["transport"] == "production_ssh:singapore"
    assert performance["response_bytes"] > 0
    assert performance["api_elapsed_seconds"] == 0.001
    assert performance["wall_elapsed_seconds"] >= 0
    assert client.request.call_count == 1
    assert "127.0.0.1:8088" in client.request.call_args.args[0]


def test_same_point_server_requests_start_in_parallel() -> None:
    barrier = threading.Barrier(2)

    def fetch_stub(*_args: object, **_kwargs: object):
        barrier.wait(timeout=1.0)
        return (
            {"hourly": {"time": ["2026-07-30T00:00"], "temperature_2m": [1.0]}},
            {
                "transport": "test",
                "response_bytes": 1,
                "api_elapsed_seconds": 0.001,
                "wall_elapsed_seconds": 0.001,
            },
        )

    with mock.patch.object(parity, "fetch", side_effect=fetch_stub):
        difference, values_compared, performance = parity.compare_request(
            shanghai_url="http://127.0.0.1:8088",
            singapore_url="http://127.0.0.1:8088",
            model="gfs",
            point={"latitude": 30.0, "longitude": 120.0},
            hourly=("temperature_2m",),
            daily=(),
            hourly_time_range=("2026-07-30T00:00", "2026-07-30T00:00"),
            daily_time_range=("2026-07-30", "2026-07-30"),
            timeout=10.0,
            retries=0,
        )

    assert difference is None
    assert values_compared == 1
    assert set(performance) == {"shanghai", "singapore"}


def test_request_performance_is_aggregated_and_summarized() -> None:
    checkpoint: dict[str, object] = {}
    parity.record_request_performance(
        checkpoint,
        {
            "shanghai": {
                "transport": "production_ssh:sh-om-new",
                "response_bytes": 100,
                "api_elapsed_seconds": 0.04,
                "wall_elapsed_seconds": 0.08,
            },
            "singapore": {
                "transport": "production_ssh:singapore",
                "response_bytes": 120,
                "api_elapsed_seconds": 0.01,
                "wall_elapsed_seconds": 0.22,
            },
        },
    )
    parity.record_request_performance(
        checkpoint,
        {
            "shanghai": {
                "transport": "production_ssh:sh-om-new",
                "response_bytes": 110,
                "api_elapsed_seconds": 0.02,
                "wall_elapsed_seconds": 0.06,
            },
            "singapore": {
                "transport": "production_ssh:singapore",
                "response_bytes": 130,
                "api_elapsed_seconds": 0.03,
                "wall_elapsed_seconds": 0.18,
            },
        },
    )

    summary = parity.summarized_performance(checkpoint)

    assert summary["shanghai"]["requests"] == 2
    assert summary["shanghai"]["response_bytes"] == 210
    assert summary["shanghai"]["api_elapsed_seconds_average"] == 0.03
    assert summary["shanghai"]["wall_elapsed_seconds_max"] == 0.08
    assert summary["singapore"]["api_elapsed_seconds_average"] == 0.02
    assert summary["singapore"]["wall_elapsed_seconds_average"] == 0.2


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
