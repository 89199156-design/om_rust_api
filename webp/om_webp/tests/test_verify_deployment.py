from __future__ import annotations

import importlib.util
from pathlib import Path

import pytest


SCRIPT = (
    Path(__file__).resolve().parents[1]
    / "scripts"
    / "verify_deployment.py"
)
SPEC = importlib.util.spec_from_file_location("verify_deployment", SCRIPT)
assert SPEC and SPEC.loader
verify_deployment = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(verify_deployment)


def test_verifier_defaults_to_all_scopes_and_legacy_roots(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    for name in (
        "OM_WEBP_APP_ROOT",
        "OM_DATA_ROOT",
        "OM_WEBP_DATA_ROOT",
        "OM_WEBP_PUBLIC_ROOT",
    ):
        monkeypatch.delenv(name, raising=False)

    args = verify_deployment.parse_args([])

    assert args.app_root == Path("/opt/1panel/apps/weather_om_webp")
    assert args.raw_root == Path("/data/om_raw")
    assert args.webp_root == Path("/data/om_webp")
    assert args.public_root == Path("/opt/1panel/apps/weather/data")
    assert args.scopes == ("gfs", "cams", "ecmwf_ifs025")


def test_verifier_accepts_singapore_roots_and_deduplicates_scopes() -> None:
    args = verify_deployment.parse_args(
        [
            "--app-root",
            "/opt/webp",
            "--raw-root",
            "/opt/weather/data/om_producer",
            "--webp-root",
            "/srv/weather-data/weather_om_webp/data",
            "--public-root",
            "/opt/weather/static",
            "--scope",
            "gfs",
            "--scope",
            "gfs",
            "--scope",
            "cams",
        ]
    )

    assert args.app_root == Path("/opt/webp")
    assert args.raw_root == Path("/opt/weather/data/om_producer")
    assert args.webp_root == Path("/srv/weather-data/weather_om_webp/data")
    assert args.public_root == Path("/opt/weather/static")
    assert args.scopes == ("gfs", "cams")


def test_verifier_rejects_relative_roots() -> None:
    with pytest.raises(SystemExit):
        verify_deployment.parse_args(["--raw-root", "relative/path"])
