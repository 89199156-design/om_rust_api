#!/usr/bin/env python3
from __future__ import annotations

import json
import subprocess
from pathlib import Path

APP = Path("/opt/1panel/apps/weather_om_webp")
RAW = Path("/data/om_raw")
WEBP = Path("/data/om_webp")
PUBLIC = Path("/opt/1panel/apps/weather/data")
EXPECTED = {
    "gfs": {
        "ready_group": "gfs",
        "product": "gfs013_surface",
        "manifest": "gfs013_surface_data.json",
        "layers": {
            "cloud_total_1", "cloud_high_1", "cloud_mid_1", "cloud_low_1",
            "t2m", "d2m", "r2", "wind", "tp", "snod", "gust", "vis",
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
            "t2m", "d2m", "r2", "wind", "tp", "snod", "gust", "precip_phase",
            "thunderstorm_code", "cape", "prmsl", "sp",
        },
    },
}


def load(path: Path) -> dict:
    return json.loads(path.read_text(encoding="utf-8"))


def main() -> None:
    report: dict[str, object] = {"status": "success", "groups": {}}
    catalog = load(PUBLIC / "weather_layer_catalog.json")
    for group, expected in EXPECTED.items():
        ready = load(
            RAW
            / "groups"
            / expected["ready_group"]
            / "current"
            / "ready_for_processing.json"
        )
        marker = load(WEBP / "current" / f"{group}.json")
        assert ready["status"] == "complete"
        assert marker["status"] == "complete"
        assert marker["release_id"] == ready["release_id"]
        assert marker["run"] == ready["latest_complete_run"]
        product_link = PUBLIC / expected["product"]
        assert product_link.is_symlink()
        product_root = product_link.resolve(strict=True)
        assert product_root.parent == Path(marker["path"]).resolve(strict=True)
        manifest = load(product_root / expected["manifest"])
        assert manifest["source"] == group
        assert manifest["source_release_id"] == ready["release_id"]
        assert manifest["source_run"] == ready["latest_complete_run"]
        assert manifest["frame_count"] == 121
        assert manifest["grid"]["width"] == 597
        assert manifest["grid"]["height"] == 495
        assert set(manifest["layers"]) == expected["layers"]
        assert catalog["products"][group]["source"] == group
        assert catalog["products"][group]["manifest"] == expected["manifest"]
        assert set(catalog["products"][group]["layers"]) == expected["layers"]
        if group == "ecmwf_ifs025":
            assert set(catalog["products"][group]["unavailable_layers"]) == {
                "vis", "uv_index", "showers"
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
                [str(APP / "bin" / "om-webp-inspect"), str(sample), "--x", "0", "--y", "0"],
                text=True,
            )
        )
        assert (inspected["width"], inspected["height"]) == (597, 495)
        report["groups"][group] = {
            "release_id": ready["release_id"],
            "run": ready["latest_complete_run"],
            "layers": len(expected["layers"]),
            "frames_per_layer": min(layer_counts.values()),
            "webp_files": sum(layer_counts.values()),
            "public_target": str(product_root),
        }
    staging = WEBP / "staging"
    report["staging_entries"] = len(list(staging.iterdir())) if staging.exists() else 0
    assert report["staging_entries"] == 0
    print(json.dumps(report, ensure_ascii=False, indent=2))


if __name__ == "__main__":
    main()
