#!/usr/bin/env python3
from __future__ import annotations

import argparse
import json
import os
import subprocess
from pathlib import Path

EXPECTED = {
    "gfs": {
        "ready_group": "gfs",
        "product": "gfs013_surface",
        "manifest": "gfs013_surface_data.json",
        "layers": {
            "cloud_total_1", "cloud_high_1", "cloud_mid_1", "cloud_low_1",
            "t2m", "surface_temperature", "t80m", "t100m", "t120m",
            "d2m", "r2", "wind", "wind_80m", "wind_100m", "wind_120m",
            "freezing_level_height", "tp", "snod", "gust", "vis",
            "precip_phase", "thunderstorm_code", "cape", "prmsl", "sp", "uv_index",
        },
    },
    "cams": {
        "ready_group": "cams",
        "product": "cams_global",
        "manifest": "cams_global_data.json",
        "layers": {"pm2_5", "pm10", "aerosol_optical_depth", "dust"},
    },
    "ecmwf_ifs025": {
        "ready_group": "ecmwf",
        "product": "ecmwf_ifs025",
        "manifest": "ecmwf_ifs025_data.json",
        "layers": {
            "cloud_total_1", "cloud_high_1", "cloud_mid_1", "cloud_low_1",
            "t2m", "surface_temperature", "d2m", "r2", "wind", "wind_100m",
            "tp", "snod", "gust", "precip_phase",
            "thunderstorm_code", "cape", "prmsl", "sp",
        },
    },
}


def absolute_path(value: str) -> Path:
    path = Path(value)
    # The deployment target is Linux, while this repository is also tested
    # from Windows workstations where pathlib does not classify "/opt/..." as
    # absolute. Keep the CLI contract explicitly POSIX.
    if not value.startswith("/"):
        raise argparse.ArgumentTypeError(f"path must be absolute: {value}")
    return path


def parse_args(argv: list[str] | None = None) -> argparse.Namespace:
    parser = argparse.ArgumentParser(
        description="Verify immutable OM-to-WebP production deployments."
    )
    parser.add_argument(
        "--app-root",
        type=absolute_path,
        default=absolute_path(
            os.environ.get("OM_WEBP_APP_ROOT", "/opt/1panel/apps/weather_om_webp")
        ),
    )
    parser.add_argument(
        "--raw-root",
        type=absolute_path,
        default=absolute_path(os.environ.get("OM_DATA_ROOT", "/data/om_raw")),
    )
    parser.add_argument(
        "--webp-root",
        type=absolute_path,
        default=absolute_path(
            os.environ.get("OM_WEBP_DATA_ROOT", "/data/om_webp")
        ),
    )
    parser.add_argument(
        "--public-root",
        type=absolute_path,
        default=absolute_path(
            os.environ.get(
                "OM_WEBP_PUBLIC_ROOT",
                "/opt/1panel/apps/weather/data",
            )
        ),
    )
    parser.add_argument(
        "--scope",
        action="append",
        choices=tuple(EXPECTED),
        dest="scopes",
        help="verify only this scope; repeat for multiple scopes (default: all)",
    )
    args = parser.parse_args(argv)
    args.scopes = tuple(dict.fromkeys(args.scopes or EXPECTED))
    return args


def load(path: Path) -> dict:
    return json.loads(path.read_text(encoding="utf-8"))


def verify(args: argparse.Namespace) -> dict[str, object]:
    report: dict[str, object] = {"status": "success", "groups": {}}
    renderer_revision = (
        args.app_root / "source-revision"
    ).read_text(encoding="utf-8").strip()
    assert len(renderer_revision) == 40
    assert all(character in "0123456789abcdef" for character in renderer_revision)
    catalog = load(args.public_root / "weather_layer_catalog.json")
    for group in args.scopes:
        expected = EXPECTED[group]
        ready = load(
            args.raw_root
            / "groups"
            / expected["ready_group"]
            / "current"
            / "ready_for_processing.json"
        )
        marker = load(args.webp_root / "current" / f"{group}.json")
        assert ready["status"] == "complete"
        assert marker["status"] == "complete"
        assert marker["release_id"] == ready["release_id"]
        assert marker["run"] == ready["latest_complete_run"]
        assert marker["renderer_revision"] == renderer_revision
        product_link = args.public_root / expected["product"]
        assert product_link.is_symlink()
        product_root = product_link.resolve(strict=True)
        assert product_root.parent == Path(marker["path"]).resolve(strict=True)
        manifest = load(product_root / expected["manifest"])
        assert manifest["source"] == group
        assert manifest["source_release_id"] == ready["release_id"]
        assert manifest["source_run"] == ready["latest_complete_run"]
        assert manifest["renderer_revision"] == renderer_revision
        assert manifest["frame_count"] == 121
        assert manifest["grid"]["width"] == 597
        assert manifest["grid"]["height"] == 495
        assert set(manifest["layers"]) == expected["layers"]
        assert catalog["products"][group]["source"] == group
        assert catalog["products"][group]["manifest"] == expected["manifest"]
        assert set(catalog["products"][group]["layers"]) == expected["layers"]
        if group == "ecmwf_ifs025":
            assert set(catalog["products"][group]["unavailable_layers"]) == {
                "vis", "uv_index", "showers", "t80m", "t100m", "t120m",
                "wind_80m", "wind_120m", "freezing_level_height"
            }
        batch = manifest["batch"]
        expected_files = {f"{timestamp}_{batch}.webp" for timestamp in manifest["files"]}
        layer_counts: dict[str, int] = {}
        for layer in expected["layers"]:
            files = {path.name for path in (product_root / layer).glob("*.webp")}
            assert files == expected_files, (group, layer, len(files), len(expected_files))
            layer_counts[layer] = len(files)
        sample = next((product_root / sorted(expected["layers"])[0]).glob("*.webp"))
        inspected = json.loads(
            subprocess.check_output(
                [
                    str(args.app_root / "bin" / "om-webp-inspect"),
                    str(sample),
                    "--x",
                    "0",
                    "--y",
                    "0",
                ],
                text=True,
            )
        )
        assert (inspected["width"], inspected["height"]) == (597, 495)
        report["groups"][group] = {
            "release_id": ready["release_id"],
            "run": ready["latest_complete_run"],
            "renderer_revision": renderer_revision,
            "layers": len(expected["layers"]),
            "frames_per_layer": min(layer_counts.values()),
            "webp_files": sum(layer_counts.values()),
            "public_target": str(product_root),
        }
    staging = args.webp_root / "staging"
    report["staging_entries"] = len(list(staging.iterdir())) if staging.exists() else 0
    assert report["staging_entries"] == 0
    return report


def main(argv: list[str] | None = None) -> None:
    report = verify(parse_args(argv))
    print(json.dumps(report, ensure_ascii=False, indent=2))


if __name__ == "__main__":
    main()
