use axum::body::{to_bytes, Body};
use axum::http::{Request, StatusCode};
use om_api::api::{router, AppState};
use om_api::query::weather_code;
use serde_json::Value;
use std::fs;
use std::path::Path;
use tempfile::TempDir;
use tower::ServiceExt;

fn write_product(root: &Path, product: &str, entries: Vec<TestEntry>) -> String {
    let coverage_id = format!("{product}_test_1h");
    write_product_coverage(root, product, &coverage_id, entries, true);
    coverage_id
}

fn write_product_coverage(
    root: &Path,
    product: &str,
    coverage_id: &str,
    entries: Vec<TestEntry>,
    make_current: bool,
) {
    write_product_coverage_timed(
        root,
        product,
        coverage_id,
        entries
            .into_iter()
            .map(|entry| TimedTestEntry {
                variable: entry.variable,
                values: entry.values,
                valid_time_utc: "2026-07-08T00:00:00Z",
            })
            .collect(),
        make_current,
    )
}

fn write_product_coverage_timed(
    root: &Path,
    product: &str,
    coverage_id: &str,
    entries: Vec<TimedTestEntry>,
    make_current: bool,
) {
    let product_root = root.join(product);
    let coverage_root = product_root.join("coverages").join(&coverage_id);
    fs::create_dir_all(&coverage_root).unwrap();
    let bundle_path = coverage_root.join(format!("{product}.omranges"));
    let mut bundle = Vec::new();
    let mut manifest_entries = Vec::new();
    for entry in entries {
        let bundle_offset = bundle.len() as u64;
        let payload = floats_to_bytes(&entry.values);
        bundle.extend_from_slice(&payload);
        manifest_entries.push(serde_json::json!({
            "variable": entry.variable,
            "variable_path": entry.variable,
            "valid_time_utc": entry.valid_time_utc,
            "source_run": "2026070800",
            "forecast_hour": 0,
            "source_url": "fixture",
            "selection_ranges": [[0, 2], [0, 2]],
            "array": {
                "data_type": 20,
                "compression": 4,
                "dimensions": [2, 2],
                "chunks": [2, 2],
                "lut_offset": 0,
                "lut_size": 0,
                "scale_factor": 1.0,
                "add_offset": 0.0
            },
            "lut_byte_ranges": [],
            "data_byte_ranges": [[0, payload.len()]],
            "lut_bytes_read": 0,
            "byte_ranges": [[0, payload.len() - 1]],
            "bundle_offset": bundle_offset,
            "bundle_bytes": payload.len()
        }));
    }
    fs::write(&bundle_path, &bundle).unwrap();
    let manifest = serde_json::json!({
        "model": product,
        "coverage_id": coverage_id,
        "status": "complete",
        "latest_complete_run": "2026070800",
        "files": [{
            "kind": "om_coverage_bundle",
            "path": format!("coverages/{coverage_id}/{product}.omranges"),
            "bytes": bundle.len(),
            "sha256": "not_checked_in_api_tests",
            "entries": manifest_entries
        }]
    });
    fs::write(
        coverage_root.join("latest.json"),
        serde_json::to_vec_pretty(&manifest).unwrap(),
    )
    .unwrap();
    if !make_current {
        return;
    }
    let current = product_root.join("current");
    fs::create_dir_all(&current).unwrap();
    fs::write(
        current.join("latest.json"),
        serde_json::to_vec_pretty(&manifest).unwrap(),
    )
    .unwrap();
}

fn write_group_ready(root: &Path, group: &str, products: &[(&str, &str)]) {
    let product_manifests = products
        .iter()
        .map(|(product, coverage_id)| {
            (
                (*product).to_string(),
                serde_json::json!({
                    "coverage_id": coverage_id,
                    "status": "complete",
                    "latest_complete_run": "2026070800",
                    "path": format!("../{product}/latest.json")
                }),
            )
        })
        .collect::<serde_json::Map<_, _>>();
    let ready = serde_json::json!({
        "group": group,
        "status": "complete",
        "latest_complete_run": "2026070800",
        "product_manifests": product_manifests
    });
    let current = root.join("groups").join(group).join("current");
    fs::create_dir_all(&current).unwrap();
    fs::write(
        current.join("ready_for_processing.json"),
        serde_json::to_vec_pretty(&ready).unwrap(),
    )
    .unwrap();
}

fn write_group_release(root: &Path, group: &str, run: &str, products: &[(&str, &str)]) {
    let product_manifests = products
        .iter()
        .map(|(product, coverage_id)| {
            (
                (*product).to_string(),
                serde_json::json!({
                    "coverage_id": coverage_id,
                    "status": "complete",
                    "latest_complete_run": run,
                }),
            )
        })
        .collect::<serde_json::Map<_, _>>();
    let release = serde_json::json!({
        "group": group,
        "status": "complete",
        "latest_complete_run": run,
        "product_manifests": product_manifests,
    });
    let releases = root.join("groups").join(group).join("releases");
    fs::create_dir_all(&releases).unwrap();
    fs::write(
        releases.join(format!("{group}-{run}.json")),
        serde_json::to_vec_pretty(&release).unwrap(),
    )
    .unwrap();
}

fn set_coverage_public_start(root: &Path, product: &str, coverage_id: &str, value: &str) {
    let path = root
        .join(product)
        .join("coverages")
        .join(coverage_id)
        .join("latest.json");
    let mut manifest: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    manifest["public_start_utc"] = Value::String(value.to_string());
    fs::write(path, serde_json::to_vec_pretty(&manifest).unwrap()).unwrap();
}

fn set_coverage_forecast_hour(root: &Path, product: &str, coverage_id: &str, forecast_hour: i64) {
    let path = root
        .join(product)
        .join("coverages")
        .join(coverage_id)
        .join("latest.json");
    let mut manifest: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    for file in manifest["files"].as_array_mut().unwrap() {
        for entry in file["entries"].as_array_mut().unwrap() {
            entry["forecast_hour"] = Value::from(forecast_hour);
        }
    }
    fs::write(path, serde_json::to_vec_pretty(&manifest).unwrap()).unwrap();
}

struct TestEntry {
    variable: &'static str,
    values: [f32; 4],
}

struct TimedTestEntry {
    variable: &'static str,
    values: [f32; 4],
    valid_time_utc: &'static str,
}

#[tokio::test]
async fn gfs_nan_fallback_uses_only_the_other_retained_full_run() {
    let root = tempfile::tempdir().unwrap();
    let current = "gfs013_surface_2026070800_full";
    let previous = "gfs013_surface_2026070718_full";
    let partial = "gfs013_surface_2026070712_6h";
    for (coverage, value, forecast_hour) in [
        (current, f32::NAN, 6),
        (previous, 18.0, 6),
        (partial, 30.0, 5),
    ] {
        write_product_coverage(
            root.path(),
            "gfs013_surface",
            coverage,
            vec![TestEntry {
                variable: "temperature_2m",
                values: [value, value, value, value],
            }],
            false,
        );
        set_coverage_forecast_hour(root.path(), "gfs013_surface", coverage, forecast_hour);
    }
    write_group_release(
        root.path(),
        "gfs",
        "2026070718",
        &[("gfs013_surface", previous)],
    );
    write_group_release(
        root.path(),
        "gfs",
        "2026070712",
        &[("gfs013_surface", partial)],
    );
    write_group_ready(root.path(), "gfs", &[("gfs013_surface", current)]);

    let state = AppState::new(root.path().to_path_buf(), None).unwrap();
    let app = router(state);
    let (status, body) = request_json(
        app,
        "/v1/forecast?latitude=-90&longitude=-180&hourly=temperature_2m&start_hour=2026-07-08T00:00&end_hour=2026-07-08T00:00",
    )
    .await;

    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["hourly"]["temperature_2m"], serde_json::json!([18.0]));
}

#[tokio::test]
async fn gfs_nan_fallback_does_not_continue_into_partial_runs() {
    let root = tempfile::tempdir().unwrap();
    let current = "gfs013_surface_2026070800_full";
    let previous = "gfs013_surface_2026070718_full";
    let partial = "gfs013_surface_2026070712_6h";
    for (coverage, value, forecast_hour) in [
        (current, f32::NAN, 6),
        (previous, f32::NAN, 6),
        (partial, 30.0, 5),
    ] {
        write_product_coverage(
            root.path(),
            "gfs013_surface",
            coverage,
            vec![TestEntry {
                variable: "temperature_2m",
                values: [value, value, value, value],
            }],
            false,
        );
        set_coverage_forecast_hour(root.path(), "gfs013_surface", coverage, forecast_hour);
    }
    write_group_release(
        root.path(),
        "gfs",
        "2026070718",
        &[("gfs013_surface", previous)],
    );
    write_group_release(
        root.path(),
        "gfs",
        "2026070712",
        &[("gfs013_surface", partial)],
    );
    write_group_ready(root.path(), "gfs", &[("gfs013_surface", current)]);

    let state = AppState::new(root.path().to_path_buf(), None).unwrap();
    let app = router(state);
    let (status, body) = request_json(
        app,
        "/v1/forecast?latitude=-90&longitude=-180&hourly=temperature_2m&start_hour=2026-07-08T00:00&end_hour=2026-07-08T00:00",
    )
    .await;

    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["hourly"]["temperature_2m"], serde_json::json!([null]));
}

#[tokio::test]
async fn gfs_newest_covering_short_run_overrides_previous_complete_run() {
    let root = tempfile::tempdir().unwrap();
    let current = "gfs013_surface_2026070800_full";
    let short = "gfs013_surface_2026070718_6h";
    let previous = "gfs013_surface_2026070700_full";
    for (coverage, value, forecast_hour, valid_time) in [
        (current, 40.0, 6, "2026-07-08T00:00:00Z"),
        (short, 30.0, 5, "2026-07-07T18:00:00Z"),
        (previous, 18.0, 18, "2026-07-07T18:00:00Z"),
    ] {
        write_product_coverage_timed(
            root.path(),
            "gfs013_surface",
            coverage,
            vec![TimedTestEntry {
                variable: "temperature_2m",
                values: [value, value, value, value],
                valid_time_utc: valid_time,
            }],
            false,
        );
        set_coverage_forecast_hour(root.path(), "gfs013_surface", coverage, forecast_hour);
    }
    write_group_release(
        root.path(),
        "gfs",
        "2026070700",
        &[("gfs013_surface", previous)],
    );
    write_group_release(
        root.path(),
        "gfs",
        "2026070718",
        &[("gfs013_surface", short)],
    );
    write_group_ready(root.path(), "gfs", &[("gfs013_surface", current)]);

    let (status, body) = request_json(
        router(AppState::new(root.path().to_path_buf(), None).unwrap()),
        "/v1/forecast?latitude=-90&longitude=-180&hourly=temperature_2m&start_hour=2026-07-07T18:00&end_hour=2026-07-07T18:00",
    )
    .await;

    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["hourly"]["temperature_2m"], serde_json::json!([30.0]));
}

#[tokio::test]
async fn gfs_null_short_run_falls_back_only_to_previous_complete_run() {
    let root = tempfile::tempdir().unwrap();
    let current = "gfs013_surface_2026070800_full";
    let short = "gfs013_surface_2026070718_6h";
    let previous = "gfs013_surface_2026070700_full";
    for (coverage, value, forecast_hour, valid_time) in [
        (current, 40.0, 6, "2026-07-08T00:00:00Z"),
        (short, f32::NAN, 5, "2026-07-07T18:00:00Z"),
        (previous, 18.0, 18, "2026-07-07T18:00:00Z"),
    ] {
        write_product_coverage_timed(
            root.path(),
            "gfs013_surface",
            coverage,
            vec![TimedTestEntry {
                variable: "temperature_2m",
                values: [value, value, value, value],
                valid_time_utc: valid_time,
            }],
            false,
        );
        set_coverage_forecast_hour(root.path(), "gfs013_surface", coverage, forecast_hour);
    }
    write_group_release(
        root.path(),
        "gfs",
        "2026070700",
        &[("gfs013_surface", previous)],
    );
    write_group_release(
        root.path(),
        "gfs",
        "2026070718",
        &[("gfs013_surface", short)],
    );
    write_group_ready(root.path(), "gfs", &[("gfs013_surface", current)]);

    let (status, body) = request_json(
        router(AppState::new(root.path().to_path_buf(), None).unwrap()),
        "/v1/forecast?latitude=-90&longitude=-180&hourly=temperature_2m&start_hour=2026-07-07T18:00&end_hour=2026-07-07T18:00",
    )
    .await;

    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["hourly"]["temperature_2m"], serde_json::json!([18.0]));
}

#[tokio::test]
async fn rain_remains_null_when_any_direct_component_is_null() {
    let root = tempfile::tempdir().unwrap();
    let coverage = "gfs013_surface_2026070800_full";
    write_product_coverage(
        root.path(),
        "gfs013_surface",
        coverage,
        vec![
            TestEntry {
                variable: "precipitation",
                values: [f32::NAN; 4],
            },
            TestEntry {
                variable: "showers",
                values: [f32::NAN; 4],
            },
            TestEntry {
                variable: "snowfall_water_equivalent",
                values: [f32::NAN; 4],
            },
        ],
        false,
    );
    set_coverage_forecast_hour(root.path(), "gfs013_surface", coverage, 6);
    write_group_ready(root.path(), "gfs", &[("gfs013_surface", coverage)]);

    let (status, body) = request_json(
        router(AppState::new(root.path().to_path_buf(), None).unwrap()),
        "/v1/forecast?latitude=-90&longitude=-180&hourly=rain&start_hour=2026-07-08T00:00&end_hour=2026-07-08T00:00",
    )
    .await;

    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["hourly"]["rain"], serde_json::json!([null]));
}

#[tokio::test]
async fn cams_nan_fallback_uses_previous_retained_run() {
    let root = tempfile::tempdir().unwrap();
    let current = "cams_global_2026070800_120h";
    let previous = "cams_global_2026070712_120h";
    for (coverage, value) in [(current, f32::NAN), (previous, 2808.9)] {
        write_product_coverage(
            root.path(),
            "cams_global",
            coverage,
            vec![TestEntry {
                variable: "pm10",
                values: [value, value, value, value],
            }],
            false,
        );
    }
    write_group_release(
        root.path(),
        "cams",
        "2026070712",
        &[("cams_global", previous)],
    );
    write_group_ready(root.path(), "cams", &[("cams_global", current)]);

    let state = AppState::new(root.path().to_path_buf(), None).unwrap();
    let app = router(state);
    let (status, body) = request_json(
        app,
        "/v1/air-quality?latitude=-90&longitude=-180&hourly=pm10&start_hour=2026-07-08T00:00&end_hour=2026-07-08T00:00",
    )
    .await;

    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["hourly"]["pm10"], serde_json::json!([2808.9]));
}

#[tokio::test]
async fn cams_nan_fallback_stops_after_previous_retained_run() {
    let root = tempfile::tempdir().unwrap();
    let current = "cams_global_2026070800_120h";
    let previous = "cams_global_2026070712_120h";
    let third = "cams_global_2026070700_120h";
    for (coverage, value) in [(current, f32::NAN), (previous, f32::NAN), (third, 30.0)] {
        write_product_coverage(
            root.path(),
            "cams_global",
            coverage,
            vec![TestEntry {
                variable: "pm10",
                values: [value, value, value, value],
            }],
            false,
        );
    }
    write_group_release(root.path(), "cams", "2026070700", &[("cams_global", third)]);
    write_group_release(
        root.path(),
        "cams",
        "2026070712",
        &[("cams_global", previous)],
    );
    write_group_ready(root.path(), "cams", &[("cams_global", current)]);

    let (status, body) = request_json(
        router(AppState::new(root.path().to_path_buf(), None).unwrap()),
        "/v1/air-quality?latitude=-90&longitude=-180&hourly=pm10&start_hour=2026-07-08T00:00&end_hour=2026-07-08T00:00",
    )
    .await;

    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["hourly"]["pm10"], serde_json::json!([null]));
}

#[tokio::test]
async fn cams_latest_tail_does_not_fall_back_to_third_retained_run() {
    let root = tempfile::tempdir().unwrap();
    let current = "cams_global_2026070800_120h";
    let previous = "cams_global_2026070712_120h";
    let third = "cams_global_2026070700_120h";
    for (coverage, value, valid_time) in [
        (current, f32::NAN, "2026-07-08T12:00:00Z"),
        (previous, 20.0, "2026-07-08T00:00:00Z"),
        (third, 30.0, "2026-07-08T12:00:00Z"),
    ] {
        write_product_coverage_timed(
            root.path(),
            "cams_global",
            coverage,
            vec![TimedTestEntry {
                variable: "pm10",
                values: [value, value, value, value],
                valid_time_utc: valid_time,
            }],
            false,
        );
    }
    write_group_release(root.path(), "cams", "2026070700", &[("cams_global", third)]);
    write_group_release(
        root.path(),
        "cams",
        "2026070712",
        &[("cams_global", previous)],
    );
    write_group_ready(root.path(), "cams", &[("cams_global", current)]);

    let (status, body) = request_json(
        router(AppState::new(root.path().to_path_buf(), None).unwrap()),
        "/v1/air-quality?latitude=-90&longitude=-180&hourly=pm10&start_hour=2026-07-08T12:00&end_hour=2026-07-08T12:00",
    )
    .await;

    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["hourly"]["pm10"], serde_json::json!([null]));
}

fn floats_to_bytes(values: &[f32]) -> Vec<u8> {
    let mut out = Vec::with_capacity(values.len() * 4);
    for value in values {
        out.extend_from_slice(&value.to_le_bytes());
    }
    out
}

#[tokio::test]
async fn forecast_endpoint_hides_interpolation_history_before_public_start() {
    let root = tempfile::tempdir().unwrap();
    let coverage_id = "gfs013_surface_public_start";
    write_product_coverage_timed(
        root.path(),
        "gfs013_surface",
        coverage_id,
        vec![
            TimedTestEntry {
                variable: "temperature_2m",
                values: [10.0, 0.0, 0.0, 0.0],
                valid_time_utc: "2026-07-08T00:00:00Z",
            },
            TimedTestEntry {
                variable: "temperature_2m",
                values: [11.0, 0.0, 0.0, 0.0],
                valid_time_utc: "2026-07-08T01:00:00Z",
            },
            TimedTestEntry {
                variable: "temperature_2m",
                values: [12.0, 0.0, 0.0, 0.0],
                valid_time_utc: "2026-07-08T02:00:00Z",
            },
            TimedTestEntry {
                variable: "temperature_2m",
                values: [13.0, 0.0, 0.0, 0.0],
                valid_time_utc: "2026-07-08T03:00:00Z",
            },
        ],
        false,
    );
    set_coverage_public_start(
        root.path(),
        "gfs013_surface",
        coverage_id,
        "2026-07-08T02:00:00Z",
    );
    write_group_ready(root.path(), "gfs", &[("gfs013_surface", coverage_id)]);

    let state = AppState::new(root.path().to_path_buf(), None).unwrap();
    let app = router(state);
    let (status, body) = request_json(
        app.clone(),
        "/v1/forecast?latitude=-90&longitude=-180&hourly=temperature_2m&start_hour=2026-07-08T00:00&end_hour=2026-07-08T03:00",
    )
    .await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        body["hourly"]["time"],
        serde_json::json!(["2026-07-08T02:00", "2026-07-08T03:00"])
    );
    assert_eq!(
        body["hourly"]["temperature_2m"],
        serde_json::json!([12.0, 13.0])
    );
}

#[tokio::test]
async fn cams_hermite_uses_b_when_second_lookahead_is_missing() {
    let root = tempfile::tempdir().unwrap();
    let coverage_id = "cams_global_greenhouse_gases_hermite_edge";
    write_product_coverage_timed(
        root.path(),
        "cams_global_greenhouse_gases",
        coverage_id,
        vec![
            TimedTestEntry {
                variable: "carbon_monoxide",
                values: [148.0, 0.0, 0.0, 0.0],
                valid_time_utc: "2026-07-08T00:00:00Z",
            },
            TimedTestEntry {
                variable: "carbon_monoxide",
                values: [144.0, 0.0, 0.0, 0.0],
                valid_time_utc: "2026-07-08T03:00:00Z",
            },
            TimedTestEntry {
                variable: "carbon_monoxide",
                values: [133.0, 0.0, 0.0, 0.0],
                valid_time_utc: "2026-07-08T06:00:00Z",
            },
        ],
        false,
    );
    write_group_ready(
        root.path(),
        "cams",
        &[("cams_global_greenhouse_gases", coverage_id)],
    );

    let state = AppState::new(root.path().to_path_buf(), None).unwrap();
    let app = router(state);
    let (status, body) = request_json(
        app,
        "/v1/air-quality?latitude=-90&longitude=-180&hourly=carbon_monoxide&start_hour=2026-07-08T05:00&end_hour=2026-07-08T05:00",
    )
    .await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        body["hourly"]["carbon_monoxide"],
        serde_json::json!([135.0])
    );
}

#[tokio::test]
async fn cams_co_history_rounds_each_source_before_mixing() {
    let root = tempfile::tempdir().unwrap();
    let global = "cams_global_2026072012_120h";
    let current_ghg = "cams_global_greenhouse_gases_2026072100";
    let previous_ghg = "cams_global_greenhouse_gases_2026072000";

    write_product_coverage_timed(
        root.path(),
        "cams_global",
        global,
        vec![
            TimedTestEntry {
                variable: "carbon_monoxide",
                values: [100.0; 4],
                valid_time_utc: "2026-07-20T09:00:00Z",
            },
            TimedTestEntry {
                variable: "carbon_monoxide",
                values: [100.0; 4],
                valid_time_utc: "2026-07-20T12:00:00Z",
            },
            TimedTestEntry {
                variable: "carbon_monoxide",
                values: [100.0; 4],
                valid_time_utc: "2026-07-20T15:00:00Z",
            },
            TimedTestEntry {
                variable: "carbon_monoxide",
                values: [100.0; 4],
                valid_time_utc: "2026-07-20T18:00:00Z",
            },
        ],
        false,
    );
    write_product_coverage_timed(
        root.path(),
        "cams_global_greenhouse_gases",
        previous_ghg,
        vec![
            TimedTestEntry {
                variable: "carbon_monoxide",
                values: [94.0; 4],
                valid_time_utc: "2026-07-20T09:00:00Z",
            },
            TimedTestEntry {
                variable: "carbon_monoxide",
                values: [91.0; 4],
                valid_time_utc: "2026-07-20T12:00:00Z",
            },
            TimedTestEntry {
                variable: "carbon_monoxide",
                values: [91.0; 4],
                valid_time_utc: "2026-07-20T15:00:00Z",
            },
            TimedTestEntry {
                variable: "carbon_monoxide",
                values: [92.0; 4],
                valid_time_utc: "2026-07-20T18:00:00Z",
            },
        ],
        false,
    );
    write_product_coverage_timed(
        root.path(),
        "cams_global_greenhouse_gases",
        current_ghg,
        vec![
            TimedTestEntry {
                variable: "carbon_monoxide",
                values: [91.0; 4],
                valid_time_utc: "2026-07-20T15:00:00Z",
            },
            TimedTestEntry {
                variable: "carbon_monoxide",
                values: [92.0; 4],
                valid_time_utc: "2026-07-20T18:00:00Z",
            },
            TimedTestEntry {
                variable: "carbon_monoxide",
                values: [93.0; 4],
                valid_time_utc: "2026-07-20T21:00:00Z",
            },
        ],
        false,
    );
    write_group_release(
        root.path(),
        "cams",
        "2026072000",
        &[("cams_global_greenhouse_gases", previous_ghg)],
    );
    write_group_ready(
        root.path(),
        "cams",
        &[
            ("cams_global", global),
            ("cams_global_greenhouse_gases", current_ghg),
        ],
    );

    let (status, body) = request_json(
        router(AppState::new(root.path().to_path_buf(), None).unwrap()),
        "/v1/air-quality?latitude=-90&longitude=-180&hourly=carbon_monoxide&start_hour=2026-07-20T13:00&end_hour=2026-07-20T13:00",
    )
    .await;

    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["hourly"]["carbon_monoxide"], serde_json::json!([91.0]));
}

#[tokio::test]
async fn cams_global_hermite_uses_c_when_second_lookahead_is_missing() {
    let root = tempfile::tempdir().unwrap();
    let coverage_id = "cams_global_hermite_edge";
    write_product_coverage_timed(
        root.path(),
        "cams_global",
        coverage_id,
        vec![
            TimedTestEntry {
                variable: "ozone",
                values: [131.0, 0.0, 0.0, 0.0],
                valid_time_utc: "2026-07-08T00:00:00Z",
            },
            TimedTestEntry {
                variable: "ozone",
                values: [127.0, 0.0, 0.0, 0.0],
                valid_time_utc: "2026-07-08T03:00:00Z",
            },
            TimedTestEntry {
                variable: "ozone",
                values: [107.0, 0.0, 0.0, 0.0],
                valid_time_utc: "2026-07-08T06:00:00Z",
            },
        ],
        false,
    );
    write_group_ready(root.path(), "cams", &[("cams_global", coverage_id)]);

    let state = AppState::new(root.path().to_path_buf(), None).unwrap();
    let app = router(state);
    let (status, body) = request_json(
        app,
        "/v1/air-quality?latitude=-90&longitude=-180&hourly=ozone&start_hour=2026-07-08T05:00&end_hour=2026-07-08T05:00",
    )
    .await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["hourly"]["ozone"], serde_json::json!([113.0]));
}

#[tokio::test]
async fn cams_carbon_monoxide_smooths_three_hours_before_greenhouse_gap() {
    let root = tempfile::tempdir().unwrap();
    let global_coverage = "cams_global_co_mixer";
    let greenhouse_coverage = "cams_global_greenhouse_gases_co_mixer";
    let global_entries = (0..4)
        .map(|hour| TimedTestEntry {
            variable: "carbon_monoxide",
            values: [100.0, 100.0, 100.0, 100.0],
            valid_time_utc: Box::leak(format!("2026-07-08T{hour:02}:00:00Z").into_boxed_str()),
        })
        .collect();
    let greenhouse_values = [200.0, 200.0, f32::NAN, 100.0];
    let greenhouse_entries = greenhouse_values
        .iter()
        .enumerate()
        .map(|(hour, value)| TimedTestEntry {
            variable: "carbon_monoxide",
            values: [*value, *value, *value, *value],
            valid_time_utc: Box::leak(format!("2026-07-08T{hour:02}:00:00Z").into_boxed_str()),
        })
        .collect();
    write_product_coverage_timed(
        root.path(),
        "cams_global",
        global_coverage,
        global_entries,
        false,
    );
    write_product_coverage_timed(
        root.path(),
        "cams_global_greenhouse_gases",
        greenhouse_coverage,
        greenhouse_entries,
        false,
    );
    write_group_ready(
        root.path(),
        "cams",
        &[
            ("cams_global", global_coverage),
            ("cams_global_greenhouse_gases", greenhouse_coverage),
        ],
    );

    let state = AppState::new(root.path().to_path_buf(), None).unwrap();
    let app = router(state);
    let (status, body) = request_json(
        app,
        "/v1/air-quality?latitude=-90&longitude=-180&hourly=carbon_monoxide&start_hour=2026-07-08T00:00&end_hour=2026-07-08T00:00",
    )
    .await;

    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(
        body["hourly"]["carbon_monoxide"],
        serde_json::json!([150.0])
    );
}

#[tokio::test]
async fn cams_carbon_monoxide_hermite_extrapolates_greenhouse_tail_before_mixing() {
    let root = tempfile::tempdir().unwrap();
    let global_coverage = "cams_global_co_hermite_tail_mixer";
    let greenhouse_coverage = "cams_global_greenhouse_gases_co_hermite_tail_mixer";
    write_product_coverage_timed(
        root.path(),
        "cams_global",
        global_coverage,
        vec![
            TimedTestEntry {
                variable: "carbon_monoxide",
                values: [333.0; 4],
                valid_time_utc: "2026-07-24T21:00:00Z",
            },
            TimedTestEntry {
                variable: "carbon_monoxide",
                values: [340.0; 4],
                valid_time_utc: "2026-07-25T00:00:00Z",
            },
            TimedTestEntry {
                variable: "carbon_monoxide",
                values: [202.0; 4],
                valid_time_utc: "2026-07-25T03:00:00Z",
            },
            TimedTestEntry {
                variable: "carbon_monoxide",
                values: [168.0; 4],
                valid_time_utc: "2026-07-25T06:00:00Z",
            },
            TimedTestEntry {
                variable: "ozone",
                values: [333.0; 4],
                valid_time_utc: "2026-07-24T21:00:00Z",
            },
            TimedTestEntry {
                variable: "ozone",
                values: [340.0; 4],
                valid_time_utc: "2026-07-25T00:00:00Z",
            },
            TimedTestEntry {
                variable: "ozone",
                values: [202.0; 4],
                valid_time_utc: "2026-07-25T03:00:00Z",
            },
            TimedTestEntry {
                variable: "ozone",
                values: [168.0; 4],
                valid_time_utc: "2026-07-25T06:00:00Z",
            },
        ],
        false,
    );
    write_product_coverage_timed(
        root.path(),
        "cams_global_greenhouse_gases",
        greenhouse_coverage,
        vec![
            TimedTestEntry {
                variable: "carbon_monoxide",
                values: [145.0; 4],
                valid_time_utc: "2026-07-24T21:00:00Z",
            },
            TimedTestEntry {
                variable: "carbon_monoxide",
                values: [136.0; 4],
                valid_time_utc: "2026-07-25T00:00:00Z",
            },
        ],
        false,
    );
    write_group_ready(
        root.path(),
        "cams",
        &[
            ("cams_global", global_coverage),
            ("cams_global_greenhouse_gases", greenhouse_coverage),
        ],
    );

    let state = AppState::new(root.path().to_path_buf(), None).unwrap();
    for (hour, expected) in [(0, 187.0), (1, 218.0), (2, 218.5), (3, 202.0)] {
        let url = format!(
            "/v1/air-quality?latitude=-90&longitude=-180&hourly=ozone,carbon_monoxide&start_hour=2026-07-25T{hour:02}:00&end_hour=2026-07-25T{hour:02}:00"
        );
        let (status, body) = request_json(router(state.clone()), &url).await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(
            body["hourly"]["carbon_monoxide"],
            serde_json::json!([expected]),
            "hour {hour:02}"
        );
    }
}

#[tokio::test]
async fn chinese_daily_aqi_uses_hj663_08_to_24_o3_windows() {
    let root = tempfile::tempdir().unwrap();
    let historical_global = "cams_global_2026070700_120h";
    let current_global = "cams_global_2026070800_120h";
    let current_ghg = "cams_global_greenhouse_gases_2026070800_37h";
    let cells = [100.0, 100.0, 100.0, 100.0];
    let historical_ozone = (9..16)
        .map(|hour| TimedTestEntry {
            variable: "ozone",
            values: cells,
            valid_time_utc: Box::leak(format!("2026-07-07T{hour:02}:00:00Z").into_boxed_str()),
        })
        .collect();
    write_product_coverage_timed(
        root.path(),
        "cams_global",
        historical_global,
        historical_ozone,
        false,
    );

    let mut current_entries = Vec::new();
    for hour in 0..=24 {
        let timestamp = format!(
            "2026-07-{:02}T{:02}:00:00Z",
            if hour < 8 { 7 } else { 8 },
            (hour + 16) % 24
        );
        for (variable, value) in [
            ("pm2_5", 30.0),
            ("pm10", 50.0),
            ("nitrogen_dioxide", 100.0),
            (
                "ozone",
                if hour == 0 {
                    800.0
                } else if hour == 24 {
                    300.0
                } else {
                    100.0
                },
            ),
            ("sulphur_dioxide", 50.0),
            ("carbon_monoxide", 2000.0),
        ] {
            current_entries.push(TimedTestEntry {
                variable,
                values: [value, value, value, value],
                valid_time_utc: Box::leak(timestamp.clone().into_boxed_str()),
            });
        }
    }
    write_product_coverage_timed(
        root.path(),
        "cams_global",
        current_global,
        current_entries,
        false,
    );
    let current_co = (0..=24)
        .map(|hour| TimedTestEntry {
            variable: "carbon_monoxide",
            values: [2000.0, 2000.0, 2000.0, 2000.0],
            valid_time_utc: Box::leak(
                format!(
                    "2026-07-{:02}T{:02}:00:00Z",
                    if hour < 8 { 7 } else { 8 },
                    (hour + 16) % 24
                )
                .into_boxed_str(),
            ),
        })
        .collect();
    write_product_coverage_timed(
        root.path(),
        "cams_global_greenhouse_gases",
        current_ghg,
        current_co,
        false,
    );
    write_group_release(
        root.path(),
        "cams",
        "2026070700",
        &[("cams_global", historical_global)],
    );
    write_group_ready(
        root.path(),
        "cams",
        &[
            ("cams_global", current_global),
            ("cams_global_greenhouse_gases", current_ghg),
        ],
    );

    let state = AppState::new(root.path().to_path_buf(), None).unwrap();
    let app = router(state);
    let (status, body) = request_json(
        app.clone(),
        "/v1/air-quality?latitude=-90&longitude=-180&hourly=chinese_aqi&start_hour=2026-07-08T15:00&end_hour=2026-07-08T15:00&daily=chinese_aqi,chinese_aqi_o3,chinese_aqi_pm2_5,pm2_5_mean,pm10_mean,nitrogen_dioxide_mean,ozone_maximum_8h_mean,sulphur_dioxide_mean,carbon_monoxide_mean&start_date=2026-07-08&end_date=2026-07-08",
    )
    .await;

    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["hourly"]["chinese_aqi"], serde_json::json!([50]));
    assert_eq!(body["daily"]["time"], serde_json::json!(["2026-07-08"]));
    assert_eq!(body["daily"]["chinese_aqi"], serde_json::json!([110]));
    assert_eq!(body["daily"]["chinese_aqi_o3"], serde_json::json!([71]));
    assert_eq!(body["daily"]["chinese_aqi_pm2_5"], serde_json::json!([50]));
    assert_eq!(body["daily"]["pm2_5_mean"], serde_json::json!([30]));
    assert_eq!(body["daily"]["pm10_mean"], serde_json::json!([50]));
    assert_eq!(
        body["daily"]["nitrogen_dioxide_mean"],
        serde_json::json!([100])
    );
    assert_eq!(
        body["daily"]["ozone_maximum_8h_mean"],
        serde_json::json!([125])
    );
    assert_eq!(
        body["daily"]["sulphur_dioxide_mean"],
        serde_json::json!([50])
    );
    assert_eq!(
        body["daily"]["carbon_monoxide_mean"],
        serde_json::json!([2.0])
    );
    assert_eq!(body["daily_units"]["pm2_5_mean"], "μg/m³");
    assert_eq!(body["daily_units"]["carbon_monoxide_mean"], "mg/m³");

    let (daily_only_status, daily_only_body) = request_json(
        app,
        "/v1/air-quality?latitude=-90&longitude=-180&daily=chinese_aqi&start_date=2026-07-08&end_date=2026-07-08",
    )
    .await;
    assert_eq!(daily_only_status, StatusCode::OK, "{daily_only_body}");
    assert!(daily_only_body.get("hourly").is_none());
    assert_eq!(
        daily_only_body["daily"]["chinese_aqi"],
        serde_json::json!([110])
    );
}

#[tokio::test]
async fn chinese_daily_air_quality_keeps_date_when_one_pollutant_is_missing() {
    let root = tempfile::tempdir().unwrap();
    let coverage = "cams_global_chinese_daily_missing_pollutant";
    let mut entries = Vec::new();
    for hour in 0..24 {
        let timestamp = format!(
            "2026-07-{:02}T{:02}:00:00Z",
            if hour < 8 { 7 } else { 8 },
            (hour + 16) % 24
        );
        for (variable, value) in [
            ("pm2_5", 30.0),
            ("pm10", if hour == 13 { f32::NAN } else { 50.0 }),
            ("nitrogen_dioxide", 40.0),
            ("ozone", 100.0),
            ("sulphur_dioxide", 50.0),
            ("carbon_monoxide", 2000.0),
        ] {
            entries.push(TimedTestEntry {
                variable,
                values: [value, value, value, value],
                valid_time_utc: Box::leak(timestamp.clone().into_boxed_str()),
            });
        }
    }
    write_product_coverage_timed(root.path(), "cams_global", coverage, entries, false);
    write_group_ready(root.path(), "cams", &[("cams_global", coverage)]);

    let state = AppState::new(root.path().to_path_buf(), None).unwrap();
    let app = router(state);
    let (status, body) = request_json(
        app,
        "/v1/air-quality?latitude=-90&longitude=-180&daily=pm2_5_mean,pm10_mean,chinese_aqi&start_date=2026-07-08&end_date=2026-07-08",
    )
    .await;

    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["daily"]["time"], serde_json::json!(["2026-07-08"]));
    assert_eq!(body["daily"]["pm2_5_mean"], serde_json::json!([30]));
    assert_eq!(body["daily"]["pm10_mean"], serde_json::json!([null]));
    assert_eq!(body["daily"]["chinese_aqi"], serde_json::json!([null]));
}

#[tokio::test]
async fn missing_historical_release_coverage_does_not_block_current_cams_api() {
    let root = tempfile::tempdir().unwrap();
    let current = write_product(
        root.path(),
        "cams_global",
        vec![TestEntry {
            variable: "pm2_5",
            values: [42.0, 42.0, 42.0, 42.0],
        }],
    );
    write_group_ready(root.path(), "cams", &[("cams_global", &current)]);
    write_group_release(
        root.path(),
        "cams",
        "2026070700",
        &[("cams_global", "cams_global_missing_120h")],
    );

    let state = AppState::new(root.path().to_path_buf(), None).unwrap();
    let app = router(state);
    let (status, body) = request_json(
        app,
        "/v1/air-quality?latitude=-90&longitude=-180&hourly=pm2_5&forecast_hours=1",
    )
    .await;

    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["hourly"]["pm2_5"], serde_json::json!([42.0]));
}

#[tokio::test]
async fn chinese_hourly_pm2_5_uses_hj633_2026_breakpoints() {
    let root = tempfile::tempdir().unwrap();
    let current = "cams_global_chinese_pm25_24h";
    let entries = (0..24)
        .map(|hour| TimedTestEntry {
            variable: "pm2_5",
            values: [60.0, 60.0, 60.0, 60.0],
            valid_time_utc: Box::leak(format!("2026-07-08T{hour:02}:00:00Z").into_boxed_str()),
        })
        .collect();
    write_product_coverage_timed(root.path(), "cams_global", current, entries, false);
    write_group_ready(root.path(), "cams", &[("cams_global", current)]);

    let state = AppState::new(root.path().to_path_buf(), None).unwrap();
    let app = router(state);
    let (status, body) = request_json(
        app,
        "/v1/air-quality?latitude=-90&longitude=-180&hourly=chinese_aqi_pm2_5&start_hour=2026-07-08T23:00&end_hour=2026-07-08T23:00",
    )
    .await;

    assert_eq!(status, StatusCode::OK, "{body}");
    // HJ 633-2026 uses the PM2.5 daily-average breakpoint column for 1h reporting.
    assert_eq!(
        body["hourly"]["chinese_aqi_pm2_5"],
        serde_json::json!([100])
    );
}

#[tokio::test]
async fn chinese_hourly_aqi_uses_current_one_hour_concentrations() {
    let root = tempfile::tempdir().unwrap();
    let current = "cams_global_chinese_sliding_means";
    let mut entries = Vec::new();
    for hour in 0..24 {
        let pm2_5 = if hour == 23 { 60.0 } else { 30.0 };
        let ozone = if hour >= 16 && hour < 23 {
            300.0
        } else {
            100.0
        };
        for (variable, value) in [("pm2_5", pm2_5), ("ozone", ozone)] {
            entries.push(TimedTestEntry {
                variable,
                values: [value, value, value, value],
                valid_time_utc: Box::leak(format!("2026-07-08T{hour:02}:00:00Z").into_boxed_str()),
            });
        }
    }
    write_product_coverage_timed(root.path(), "cams_global", current, entries, false);
    write_group_ready(root.path(), "cams", &[("cams_global", current)]);

    let state = AppState::new(root.path().to_path_buf(), None).unwrap();
    let app = router(state);
    let (status, body) = request_json(
        app,
        "/v1/air-quality?latitude=-90&longitude=-180&hourly=chinese_aqi_pm2_5,chinese_aqi_o3&start_hour=2026-07-08T23:00&end_hour=2026-07-08T23:00",
    )
    .await;

    assert_eq!(status, StatusCode::OK, "{body}");
    // Real-time reporting uses the current preceding 1-hour concentration.
    assert_eq!(
        body["hourly"]["chinese_aqi_pm2_5"],
        serde_json::json!([100])
    );
    // O3 also uses the current 1-hour concentration, not an 8-hour rolling mean.
    assert_eq!(body["hourly"]["chinese_aqi_o3"], serde_json::json!([32]));
}

fn fixture_root() -> TempDir {
    let tmp = tempfile::tempdir().unwrap();
    let gfs013 = write_product(
        tmp.path(),
        "gfs013_surface",
        vec![
            TestEntry {
                variable: "temperature_2m",
                values: [12.5, 13.0, 14.0, 15.0],
            },
            TestEntry {
                variable: "relative_humidity_2m",
                values: [80.0, 70.0, 60.0, 50.0],
            },
            TestEntry {
                variable: "cloud_cover",
                values: [10.0, 90.0, 90.0, 90.0],
            },
            TestEntry {
                variable: "precipitation",
                values: [0.0, 0.0, 0.0, 0.0],
            },
            TestEntry {
                variable: "snowfall_water_equivalent",
                values: [0.0, 0.0, 0.0, 0.0],
            },
            TestEntry {
                variable: "showers",
                values: [0.0, 0.0, 0.0, 0.0],
            },
            TestEntry {
                variable: "wind_u_component_10m",
                values: [3.0, 0.0, 0.0, 0.0],
            },
            TestEntry {
                variable: "wind_v_component_10m",
                values: [4.0, 0.0, 0.0, 0.0],
            },
        ],
    );
    let gfs025 = write_product(
        tmp.path(),
        "gfs025",
        vec![
            TestEntry {
                variable: "wind_gusts_10m",
                values: [1.0, 2.0, 3.0, 4.0],
            },
            TestEntry {
                variable: "pressure_msl",
                values: [1012.24, 1012.25, 1012.26, 1012.27],
            },
        ],
    );
    let cams = write_product(
        tmp.path(),
        "cams_global",
        vec![
            TestEntry {
                variable: "pm2_5",
                values: [6.0, 7.0, 8.0, 9.0],
            },
            TestEntry {
                variable: "pm10",
                values: [20.0, 21.0, 22.0, 23.0],
            },
            TestEntry {
                variable: "aerosol_optical_depth",
                values: [0.1, 0.2, 0.3, 0.4],
            },
            TestEntry {
                variable: "dust",
                values: [11.0, 12.0, 13.0, 14.0],
            },
            TestEntry {
                variable: "carbon_monoxide",
                values: [500.0, 510.0, 520.0, 530.0],
            },
            TestEntry {
                variable: "nitrogen_dioxide",
                values: [94.0, 95.0, 96.0, 97.0],
            },
            TestEntry {
                variable: "ozone",
                values: [98.0, 99.0, 100.0, 101.0],
            },
            TestEntry {
                variable: "sulphur_dioxide",
                values: [50.0, 51.0, 52.0, 53.0],
            },
        ],
    );
    let pressure = write_product(
        tmp.path(),
        "gfs_pressure_profile",
        vec![
            TestEntry {
                variable: "temperature_1000hPa",
                values: [11.0, 12.0, 13.0, 14.0],
            },
            TestEntry {
                variable: "relative_humidity_1000hPa",
                values: [70.0, 71.0, 72.0, 73.0],
            },
            TestEntry {
                variable: "geopotential_height_1000hPa",
                values: [30.0, 31.0, 32.0, 33.0],
            },
            TestEntry {
                variable: "geopotential_height_300hPa",
                values: [9706.45, 9706.45, 9706.45, 9706.45],
            },
            TestEntry {
                variable: "vertical_velocity_1000hPa",
                values: [0.1, 0.2, 0.3, 0.4],
            },
        ],
    );
    write_group_ready(
        tmp.path(),
        "gfs",
        &[
            ("gfs013_surface", &gfs013),
            ("gfs025", &gfs025),
            ("gfs_pressure_profile", &pressure),
        ],
    );
    write_group_ready(tmp.path(), "cams", &[("cams_global", &cams)]);
    tmp
}

async fn request_json(app: axum::Router, uri: &str) -> (StatusCode, Value) {
    let response = app
        .oneshot(Request::builder().uri(uri).body(Body::empty()).unwrap())
        .await
        .unwrap();
    let status = response.status();
    let bytes = to_bytes(response.into_body(), usize::MAX).await.unwrap();
    (status, serde_json::from_slice(&bytes).unwrap())
}

#[tokio::test]
async fn forecast_endpoint_returns_point_data_without_client_manifest() {
    let root = fixture_root();
    let state = AppState::new(root.path().to_path_buf(), None).unwrap();
    let app = router(state);

    let (status, body) = request_json(
        app,
        "/v1/forecast?latitude=-90&longitude=-180&hourly=temperature_2m,weather_code,wind_speed_10m&forecast_hours=1",
    )
    .await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["hourly"]["time"].as_array().unwrap().len(), 1);
    assert_eq!(body["hourly"]["temperature_2m"][0], 12.5);
    assert_eq!(body["hourly"]["weather_code"][0], 0);
    assert_eq!(body["hourly"]["wind_speed_10m"][0], 5.0);
}

#[tokio::test]
async fn forecast_endpoint_uses_official_json_precision_time_and_units() {
    let root = fixture_root();
    let state = AppState::new(root.path().to_path_buf(), None).unwrap();
    let app = router(state);

    let (status, body) = request_json(
        app,
        "/v1/forecast?latitude=-90&longitude=-180&hourly=temperature_2m,relative_humidity_2m,dew_point_2m,weather_code,wind_speed_10m,wind_direction_10m,pressure_msl&forecast_hours=1",
    )
    .await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["hourly"]["time"][0], "2026-07-08T00:00");
    assert_eq!(body["hourly"]["temperature_2m"][0], 12.5);
    assert_eq!(body["hourly"]["relative_humidity_2m"][0], 80);
    assert_eq!(body["hourly"]["dew_point_2m"][0], 9.1);
    assert_eq!(body["hourly"]["weather_code"][0], 0);
    assert_eq!(body["hourly"]["wind_speed_10m"][0], 5.0);
    assert_eq!(body["hourly"]["wind_direction_10m"][0], 217);
    assert_eq!(body["hourly"]["pressure_msl"][0], 1012.2);
    assert_eq!(body["hourly_units"]["temperature_2m"], "°C");
    assert_eq!(body["hourly_units"]["dew_point_2m"], "°C");
    assert_eq!(body["hourly_units"]["wind_direction_10m"], "°");
    assert_eq!(body["hourly_units"]["pressure_msl"], "hPa");
}

#[tokio::test]
async fn forecast_endpoint_matches_official_float_rounding_at_decimal_half() {
    let root = tempfile::tempdir().unwrap();
    let gfs013 = write_product(
        root.path(),
        "gfs013_surface",
        vec![TestEntry {
            variable: "temperature_2m",
            values: [28.049999, 0.0, 0.0, 0.0],
        }],
    );
    write_group_ready(root.path(), "gfs", &[("gfs013_surface", &gfs013)]);
    let state = AppState::new(root.path().to_path_buf(), None).unwrap();
    let app = router(state);

    let (status, body) = request_json(
        app,
        "/v1/forecast?latitude=-90&longitude=-180&hourly=temperature_2m&forecast_hours=1",
    )
    .await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["hourly"]["temperature_2m"][0], 28.1);
}

#[tokio::test]
async fn forecast_endpoint_exposes_all_soil_temperature_and_moisture_layers() {
    let root = tempfile::tempdir().unwrap();
    let gfs013 = write_product(
        root.path(),
        "gfs013_surface",
        vec![
            TestEntry {
                variable: "soil_temperature_0_to_10cm",
                values: [20.1; 4],
            },
            TestEntry {
                variable: "soil_temperature_10_to_40cm",
                values: [19.2; 4],
            },
            TestEntry {
                variable: "soil_temperature_40_to_100cm",
                values: [18.3; 4],
            },
            TestEntry {
                variable: "soil_temperature_100_to_200cm",
                values: [17.4; 4],
            },
            TestEntry {
                variable: "soil_moisture_0_to_10cm",
                values: [0.111; 4],
            },
            TestEntry {
                variable: "soil_moisture_10_to_40cm",
                values: [0.222; 4],
            },
            TestEntry {
                variable: "soil_moisture_40_to_100cm",
                values: [0.333; 4],
            },
            TestEntry {
                variable: "soil_moisture_100_to_200cm",
                values: [0.444; 4],
            },
        ],
    );
    write_group_ready(root.path(), "gfs", &[("gfs013_surface", &gfs013)]);
    let state = AppState::new(root.path().to_path_buf(), None).unwrap();
    let app = router(state);

    let (status, body) = request_json(
        app,
        "/v1/forecast?latitude=-90&longitude=-180&hourly=soil_temperature_0_to_10cm,soil_temperature_10_to_40cm,soil_temperature_40_to_100cm,soil_temperature_100_to_200cm,soil_moisture_0_to_10cm,soil_moisture_10_to_40cm,soil_moisture_40_to_100cm,soil_moisture_100_to_200cm&forecast_hours=1",
    )
    .await;

    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(
        body["hourly"]["soil_temperature_0_to_10cm"],
        serde_json::json!([20.1])
    );
    assert_eq!(
        body["hourly"]["soil_temperature_100_to_200cm"],
        serde_json::json!([17.4])
    );
    assert_eq!(
        body["hourly"]["soil_moisture_0_to_10cm"],
        serde_json::json!([0.111])
    );
    assert_eq!(
        body["hourly"]["soil_moisture_100_to_200cm"],
        serde_json::json!([0.444])
    );
    assert_eq!(body["hourly_units"]["soil_temperature_0_to_10cm"], "°C");
    assert_eq!(body["hourly_units"]["soil_moisture_0_to_10cm"], "m³/m³");
}

#[tokio::test]
async fn forecast_endpoint_hides_radiation_and_internal_wind_components() {
    let root = fixture_root();
    let state = AppState::new(root.path().to_path_buf(), None).unwrap();
    let app = router(state);
    let (status, body) = request_json(
        app,
        "/v1/forecast?latitude=-90&longitude=-180&hourly=shortwave_radiation,wind_u_component_10m&forecast_hours=1",
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(body["error"]
        .as_str()
        .unwrap()
        .contains("unsupported public hourly variable: shortwave_radiation"));
}

#[tokio::test]
async fn forecast_endpoint_rounds_weather_code_inputs_before_deriving() {
    let root = tempfile::tempdir().unwrap();
    let gfs013 = write_product(
        root.path(),
        "gfs013_surface",
        vec![
            TestEntry {
                variable: "cloud_cover",
                values: [100.0, 0.0, 0.0, 0.0],
            },
            TestEntry {
                variable: "precipitation",
                values: [1.5, 0.0, 0.0, 0.0],
            },
            TestEntry {
                variable: "showers",
                values: [1.266_666_7, 0.0, 0.0, 0.0],
            },
            TestEntry {
                variable: "snowfall_water_equivalent",
                values: [0.0, 0.0, 0.0, 0.0],
            },
            TestEntry {
                variable: "boundary_layer_height",
                values: [300.0, 0.0, 0.0, 0.0],
            },
        ],
    );
    let gfs025 = write_product(
        root.path(),
        "gfs025",
        vec![
            TestEntry {
                variable: "wind_gusts_10m",
                values: [5.04, 0.0, 0.0, 0.0],
            },
            TestEntry {
                variable: "cape",
                values: [1746.0, 0.0, 0.0, 0.0],
            },
            TestEntry {
                variable: "lifted_index",
                values: [0.04, 0.0, 0.0, 0.0],
            },
            TestEntry {
                variable: "convective_inhibition",
                values: [49.2, 0.0, 0.0, 0.0],
            },
            TestEntry {
                variable: "visibility",
                values: [9066.67, 0.0, 0.0, 0.0],
            },
            TestEntry {
                variable: "categorical_freezing_rain",
                values: [0.0, 0.0, 0.0, 0.0],
            },
        ],
    );
    write_group_ready(
        root.path(),
        "gfs",
        &[("gfs013_surface", &gfs013), ("gfs025", &gfs025)],
    );
    let state = AppState::new(root.path().to_path_buf(), None).unwrap();
    let app = router(state);

    let (status, body) = request_json(
        app,
        "/v1/forecast?latitude=-90&longitude=-180&hourly=precipitation,showers,weather_code&forecast_hours=1",
    )
    .await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["hourly"]["precipitation"][0], 1.5);
    assert_eq!(body["hourly"]["showers"][0], 1.3);
    assert_eq!(body["hourly"]["weather_code"][0], 95);
}

#[tokio::test]
async fn forecast_endpoint_derives_weather_code_from_rounded_cloud_cover() {
    let root = tempfile::tempdir().unwrap();
    let gfs013 = write_product(
        root.path(),
        "gfs013_surface",
        vec![
            TestEntry {
                variable: "cloud_cover",
                values: [19.6, 0.0, 0.0, 0.0],
            },
            TestEntry {
                variable: "precipitation",
                values: [0.0, 0.0, 0.0, 0.0],
            },
            TestEntry {
                variable: "showers",
                values: [0.0, 0.0, 0.0, 0.0],
            },
            TestEntry {
                variable: "snowfall_water_equivalent",
                values: [0.0, 0.0, 0.0, 0.0],
            },
        ],
    );
    write_group_ready(root.path(), "gfs", &[("gfs013_surface", &gfs013)]);
    let state = AppState::new(root.path().to_path_buf(), None).unwrap();
    let app = router(state);

    let (status, body) = request_json(
        app,
        "/v1/forecast?latitude=-90&longitude=-180&hourly=cloud_cover,weather_code&forecast_hours=1",
    )
    .await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["hourly"]["cloud_cover"][0], 20);
    assert_eq!(body["hourly"]["weather_code"][0], 1);
}

#[tokio::test]
async fn forecast_endpoint_expands_sparse_backwards_sum_to_hourly_values() {
    let root = tempfile::tempdir().unwrap();
    write_product_coverage_timed(
        root.path(),
        "gfs013_surface",
        "gfs013_surface_sparse_precip",
        vec![
            TimedTestEntry {
                variable: "precipitation",
                values: [0.0, 0.0, 0.0, 0.0],
                valid_time_utc: "2026-07-08T00:00:00Z",
            },
            TimedTestEntry {
                variable: "precipitation",
                values: [0.3, 0.0, 0.0, 0.0],
                valid_time_utc: "2026-07-08T03:00:00Z",
            },
        ],
        true,
    );
    write_group_ready(
        root.path(),
        "gfs",
        &[("gfs013_surface", "gfs013_surface_sparse_precip")],
    );
    let state = AppState::new(root.path().to_path_buf(), None).unwrap();
    let app = router(state);

    let (status, body) = request_json(
        app,
        "/v1/forecast?latitude=-90&longitude=-180&hourly=precipitation&start_hour=2026-07-08T00:00&end_hour=2026-07-08T03:00",
    )
    .await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        body["hourly"]["time"],
        serde_json::json!([
            "2026-07-08T00:00",
            "2026-07-08T01:00",
            "2026-07-08T02:00",
            "2026-07-08T03:00"
        ])
    );
    assert_eq!(
        body["hourly"]["precipitation"],
        serde_json::json!([0.0, 0.1, 0.1, 0.1])
    );
}

#[tokio::test]
async fn forecast_endpoint_interpolates_sparse_temperature_with_hermite() {
    let root = tempfile::tempdir().unwrap();
    write_product_coverage_timed(
        root.path(),
        "gfs013_surface",
        "gfs013_surface_sparse_temperature",
        vec![
            TimedTestEntry {
                variable: "temperature_2m",
                values: [0.0, 0.0, 0.0, 0.0],
                valid_time_utc: "2026-07-08T00:00:00Z",
            },
            TimedTestEntry {
                variable: "temperature_2m",
                values: [9.0, 0.0, 0.0, 0.0],
                valid_time_utc: "2026-07-08T03:00:00Z",
            },
            TimedTestEntry {
                variable: "temperature_2m",
                values: [0.0, 0.0, 0.0, 0.0],
                valid_time_utc: "2026-07-08T06:00:00Z",
            },
            TimedTestEntry {
                variable: "temperature_2m",
                values: [0.0, 0.0, 0.0, 0.0],
                valid_time_utc: "2026-07-08T09:00:00Z",
            },
        ],
        true,
    );
    write_group_ready(
        root.path(),
        "gfs",
        &[("gfs013_surface", "gfs013_surface_sparse_temperature")],
    );
    let state = AppState::new(root.path().to_path_buf(), None).unwrap();
    let app = router(state);

    let (status, body) = request_json(
        app,
        "/v1/forecast?latitude=-90&longitude=-180&hourly=temperature_2m&start_hour=2026-07-08T00:00&end_hour=2026-07-08T06:00",
    )
    .await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        body["hourly"]["temperature_2m"],
        serde_json::json!([0.0, 3.0, 7.0, 9.0, 7.0, 3.0, 0.0])
    );
}

#[tokio::test]
async fn forecast_endpoint_interpolates_sparse_uv_with_official_solar_method() {
    let root = tempfile::tempdir().unwrap();
    write_product_coverage_timed(
        root.path(),
        "gfs013_surface",
        "gfs013_surface_sparse_uv",
        vec![
            TimedTestEntry {
                variable: "uv_index_clear_sky",
                values: [5.0, 5.0, 5.0, 5.0],
                valid_time_utc: "2026-07-08T12:00:00Z",
            },
            TimedTestEntry {
                variable: "uv_index_clear_sky",
                values: [6.0, 6.0, 6.0, 6.0],
                valid_time_utc: "2026-07-08T15:00:00Z",
            },
            TimedTestEntry {
                variable: "uv_index_clear_sky",
                values: [4.0, 4.0, 4.0, 4.0],
                valid_time_utc: "2026-07-08T18:00:00Z",
            },
        ],
        true,
    );
    write_group_ready(
        root.path(),
        "gfs",
        &[("gfs013_surface", "gfs013_surface_sparse_uv")],
    );
    let state = AppState::new(root.path().to_path_buf(), None).unwrap();
    let app = router(state);

    let (status, body) = request_json(
        app,
        "/v1/forecast?latitude=90&longitude=-180&hourly=uv_index_clear_sky&start_hour=2026-07-08T12:00&end_hour=2026-07-08T18:00",
    )
    .await;

    assert_eq!(status, StatusCode::OK);
    let values = body["hourly"]["uv_index_clear_sky"]
        .as_array()
        .expect("hourly UV series");
    assert_eq!(values.len(), 7);
    assert!(values.iter().all(Value::is_number));
}

#[tokio::test]
async fn forecast_endpoint_uses_model_stride_for_hermite_padding() {
    let root = tempfile::tempdir().unwrap();
    write_product_coverage_timed(
        root.path(),
        "gfs013_surface",
        "gfs013_surface_mixed_stride_cloud_cover",
        vec![
            TimedTestEntry {
                variable: "cloud_cover",
                values: [100.0, 0.0, 0.0, 0.0],
                valid_time_utc: "2026-07-08T15:00:00Z",
            },
            TimedTestEntry {
                variable: "cloud_cover",
                values: [100.0, 0.0, 0.0, 0.0],
                valid_time_utc: "2026-07-08T16:00:00Z",
            },
            TimedTestEntry {
                variable: "cloud_cover",
                values: [24.0, 0.0, 0.0, 0.0],
                valid_time_utc: "2026-07-08T17:00:00Z",
            },
            TimedTestEntry {
                variable: "cloud_cover",
                values: [73.0, 0.0, 0.0, 0.0],
                valid_time_utc: "2026-07-08T18:00:00Z",
            },
            TimedTestEntry {
                variable: "cloud_cover",
                values: [42.0, 0.0, 0.0, 0.0],
                valid_time_utc: "2026-07-08T21:00:00Z",
            },
            TimedTestEntry {
                variable: "cloud_cover",
                values: [9.0, 0.0, 0.0, 0.0],
                valid_time_utc: "2026-07-09T00:00:00Z",
            },
        ],
        true,
    );
    write_group_ready(
        root.path(),
        "gfs",
        &[("gfs013_surface", "gfs013_surface_mixed_stride_cloud_cover")],
    );
    let state = AppState::new(root.path().to_path_buf(), None).unwrap();
    let app = router(state);

    let (status, body) = request_json(
        app,
        "/v1/forecast?latitude=-90&longitude=-180&hourly=cloud_cover&start_hour=2026-07-08T18:00&end_hour=2026-07-08T21:00",
    )
    .await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        body["hourly"]["cloud_cover"],
        serde_json::json!([73, 63, 53, 42])
    );
}

#[tokio::test]
async fn forecast_endpoint_preserves_official_wind_direction_360_boundary() {
    let root = tempfile::tempdir().unwrap();
    let gfs013 = write_product(
        root.path(),
        "gfs013_surface",
        vec![
            TestEntry {
                variable: "wind_u_component_10m",
                values: [-0.0, 0.0, 0.0, 0.0],
            },
            TestEntry {
                variable: "wind_v_component_10m",
                values: [-1.0, 0.0, 0.0, 0.0],
            },
        ],
    );
    write_group_ready(root.path(), "gfs", &[("gfs013_surface", &gfs013)]);
    let state = AppState::new(root.path().to_path_buf(), None).unwrap();
    let app = router(state);

    let (status, body) = request_json(
        app,
        "/v1/forecast?latitude=-90&longitude=-180&hourly=wind_direction_10m&forecast_hours=1",
    )
    .await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["hourly"]["wind_direction_10m"][0], 360);
}

#[tokio::test]
async fn forecast_endpoint_derives_dew_point_from_temperature_and_relative_humidity() {
    let root = fixture_root();
    let state = AppState::new(root.path().to_path_buf(), None).unwrap();
    let app = router(state);

    let (status, body) = request_json(
        app,
        "/v1/forecast?latitude=-90&longitude=-180&hourly=dew_point_2m&forecast_hours=1",
    )
    .await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["hourly"]["dew_point_2m"][0], 9.1);
    assert_eq!(body["hourly_units"]["dew_point_2m"], "°C");
}

#[tokio::test]
async fn forecast_uses_group_ready_coverage_instead_of_product_current() {
    let root = tempfile::tempdir().unwrap();
    write_product_coverage(
        root.path(),
        "gfs013_surface",
        "gfs013_surface_old_1h",
        vec![TestEntry {
            variable: "temperature_2m",
            values: [12.5, 13.0, 14.0, 15.0],
        }],
        false,
    );
    write_product_coverage(
        root.path(),
        "gfs013_surface",
        "gfs013_surface_new_1h",
        vec![TestEntry {
            variable: "temperature_2m",
            values: [99.0, 99.0, 99.0, 99.0],
        }],
        true,
    );
    write_product_coverage(
        root.path(),
        "gfs025",
        "gfs025_old_1h",
        vec![TestEntry {
            variable: "wind_gusts_10m",
            values: [1.0, 2.0, 3.0, 4.0],
        }],
        true,
    );
    write_product_coverage(
        root.path(),
        "gfs_pressure_profile",
        "gfs_pressure_profile_old_1h",
        vec![TestEntry {
            variable: "temperature_1000hPa",
            values: [11.0, 12.0, 13.0, 14.0],
        }],
        true,
    );
    write_group_ready(
        root.path(),
        "gfs",
        &[
            ("gfs013_surface", "gfs013_surface_old_1h"),
            ("gfs025", "gfs025_old_1h"),
            ("gfs_pressure_profile", "gfs_pressure_profile_old_1h"),
        ],
    );
    let state = AppState::new(root.path().to_path_buf(), None).unwrap();
    let app = router(state);

    let (status, body) = request_json(
        app,
        "/v1/forecast?latitude=-90&longitude=-180&hourly=temperature_2m&forecast_hours=1",
    )
    .await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["hourly"]["temperature_2m"][0], 12.5);
}

#[tokio::test]
async fn air_quality_endpoint_reads_cams_product_directly() {
    let root = fixture_root();
    let state = AppState::new(root.path().to_path_buf(), None).unwrap();
    let app = router(state);

    let (status, body) = request_json(
        app,
        "/v1/air-quality?latitude=-90&longitude=-180&hourly=pm2_5&forecast_hours=1",
    )
    .await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["hourly"]["pm2_5"][0], 6.0);
}

#[tokio::test]
async fn air_quality_endpoint_prefers_greenhouse_gas_carbon_monoxide() {
    let root = tempfile::tempdir().unwrap();
    write_product_coverage(
        root.path(),
        "cams_global",
        "cams_global_test",
        vec![TestEntry {
            variable: "carbon_monoxide",
            values: [500.0, 500.0, 500.0, 500.0],
        }],
        true,
    );
    write_product_coverage(
        root.path(),
        "cams_global_greenhouse_gases",
        "cams_global_greenhouse_gases_test",
        vec![TestEntry {
            variable: "carbon_monoxide",
            values: [74.0, 74.0, 74.0, 74.0],
        }],
        true,
    );
    write_group_ready(
        root.path(),
        "cams",
        &[
            ("cams_global", "cams_global_test"),
            (
                "cams_global_greenhouse_gases",
                "cams_global_greenhouse_gases_test",
            ),
        ],
    );
    let state = AppState::new(root.path().to_path_buf(), None).unwrap();
    let app = router(state);

    let (status, body) = request_json(
        app,
        "/v1/air-quality?latitude=-90&longitude=-180&hourly=carbon_monoxide&forecast_hours=1",
    )
    .await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["hourly"]["carbon_monoxide"][0], 74.0);
}

#[tokio::test]
async fn air_quality_endpoint_returns_chinese_aqi_derivative_aliases() {
    let root = fixture_root();
    let state = AppState::new(root.path().to_path_buf(), None).unwrap();
    let app = router(state);

    let (status, body) = request_json(
        app,
        "/v1/air-quality?latitude=-90&longitude=-180&hourly=chinese_aqi_no2,chinese_aqi_nitrogen_dioxide&forecast_hours=1",
    )
    .await;

    assert_eq!(status, StatusCode::OK);
    assert_approx(body["hourly"]["chinese_aqi_no2"][0].as_f64().unwrap(), 47.0);
    assert_approx(
        body["hourly"]["chinese_aqi_nitrogen_dioxide"][0]
            .as_f64()
            .unwrap(),
        47.0,
    );
    assert_eq!(body["hourly_units"]["chinese_aqi_no2"], "Chinese AQI");
}

#[tokio::test]
async fn air_quality_sparse_variable_hour_returns_null_without_failing_group() {
    let root = tempfile::tempdir().unwrap();
    write_product_coverage_timed(
        root.path(),
        "cams_global",
        "cams_global_sparse",
        vec![
            TimedTestEntry {
                variable: "pm2_5",
                values: [6.0, 7.0, 8.0, 9.0],
                valid_time_utc: "2026-07-08T00:00:00Z",
            },
            TimedTestEntry {
                variable: "dust",
                values: [11.0, 12.0, 13.0, 14.0],
                valid_time_utc: "2026-07-08T01:00:00Z",
            },
        ],
        true,
    );
    write_group_ready(
        root.path(),
        "cams",
        &[("cams_global", "cams_global_sparse")],
    );
    let state = AppState::new(root.path().to_path_buf(), None).unwrap();
    let app = router(state);

    let (status, body) = request_json(
        app,
        "/v1/air-quality?latitude=-90&longitude=-180&hourly=pm2_5,dust&forecast_hours=1",
    )
    .await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["hourly"]["pm2_5"][0], 6.0);
    assert!(body["hourly"]["dust"][0].is_null());
}

#[tokio::test]
async fn air_quality_endpoint_interpolates_sparse_cams_variables_with_hermite() {
    let root = tempfile::tempdir().unwrap();
    write_product_coverage_timed(
        root.path(),
        "cams_global",
        "cams_global_sparse_dust",
        vec![
            TimedTestEntry {
                variable: "dust",
                values: [1.0, 0.0, 0.0, 0.0],
                valid_time_utc: "2026-07-08T00:00:00Z",
            },
            TimedTestEntry {
                variable: "dust",
                values: [2.0, 0.0, 0.0, 0.0],
                valid_time_utc: "2026-07-08T03:00:00Z",
            },
            TimedTestEntry {
                variable: "dust",
                values: [2.0, 0.0, 0.0, 0.0],
                valid_time_utc: "2026-07-08T06:00:00Z",
            },
            TimedTestEntry {
                variable: "dust",
                values: [2.0, 0.0, 0.0, 0.0],
                valid_time_utc: "2026-07-08T09:00:00Z",
            },
        ],
        true,
    );
    write_group_ready(
        root.path(),
        "cams",
        &[("cams_global", "cams_global_sparse_dust")],
    );
    let state = AppState::new(root.path().to_path_buf(), None).unwrap();
    let app = router(state);

    let (status, body) = request_json(
        app,
        "/v1/air-quality?latitude=-90&longitude=-180&hourly=dust&start_hour=2026-07-08T00:00&end_hour=2026-07-08T03:00",
    )
    .await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        body["hourly"]["dust"],
        serde_json::json!([1.0, 1.0, 2.0, 2.0])
    );
}

#[tokio::test]
async fn pressure_profile_endpoint_uses_official_units() {
    let root = fixture_root();
    let state = AppState::new(root.path().to_path_buf(), None).unwrap();
    let app = router(state);

    let (status, body) = request_json(
        app,
        "/v1/forecast?latitude=-90&longitude=-180&hourly=temperature_1000hPa,relative_humidity_1000hPa,geopotential_height_1000hPa,geopotential_height_300hPa,vertical_velocity_1000hPa&forecast_hours=1",
    )
    .await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["hourly"]["temperature_1000hPa"][0], 11.0);
    assert_eq!(body["hourly"]["geopotential_height_300hPa"][0], 9706.45);
    assert_eq!(body["hourly_units"]["temperature_1000hPa"], "°C");
    assert_eq!(body["hourly_units"]["relative_humidity_1000hPa"], "%");
    assert_eq!(body["hourly_units"]["geopotential_height_1000hPa"], "m");
    assert_eq!(body["hourly_units"]["vertical_velocity_1000hPa"], "m/s");
}

fn assert_approx(actual: f64, expected: f64) {
    assert!(
        (actual - expected).abs() < 0.001,
        "expected {expected}, got {actual}"
    );
}

#[test]
fn weather_code_reference_returns_light_drizzle_when_cape_is_high() {
    assert_eq!(
        weather_code(
            31.0,
            0.2,
            Some(0.2),
            0.0,
            Some(11.4),
            Some(2500.0),
            Some(-6.0),
            Some(0.0),
            Some(830.0),
            Some(6120.0),
            Some(0.0),
            3600,
            11.656364,
        ),
        Some(51.0)
    );
}

#[test]
fn weather_code_matches_official_thunderstorm_threshold() {
    assert_eq!(
        weather_code(
            100.0,
            0.5,
            Some(0.5),
            0.0,
            Some(11.6),
            Some(2120.0),
            Some(-6.2),
            Some(0.0),
            Some(685.0),
            Some(24140.0),
            Some(0.0),
            3600,
            11.656364,
        ),
        Some(95.0)
    );
}

#[test]
fn weather_code_reference_returns_shower_for_rounded_hk_inputs() {
    assert_eq!(
        weather_code(
            100.0,
            1.8,
            Some(0.4),
            0.0,
            Some(23.0),
            Some(860.0),
            Some(-3.9),
            Some(0.0),
            Some(1440.0),
            Some(10920.0),
            None,
            3600,
            22.75,
        ),
        Some(80.0)
    );
}

#[test]
fn weather_code_matches_official_moderate_drizzle_when_thunderstorm_probability_is_low() {
    assert_eq!(
        weather_code(
            100.0,
            0.6,
            Some(0.6),
            0.0,
            Some(3.3),
            Some(1400.0),
            Some(-4.4),
            Some(0.0),
            Some(365.0),
            Some(24140.0),
            Some(0.0),
            3600,
            0.5,
        ),
        Some(53.0)
    );
}

#[test]
fn weather_code_reference_returns_moderate_drizzle_for_live_gfs_sample() {
    assert_eq!(
        weather_code(
            100.0,
            0.8,
            Some(0.8),
            0.0,
            Some(12.8),
            Some(1760.0),
            Some(-6.1),
            Some(22.0),
            Some(380.0),
            Some(17600.0),
            None,
            3600,
            13.5,
        ),
        Some(53.0)
    );
}

#[test]
fn weather_code_matches_official_thunderstorm_with_low_pbl_height() {
    assert_eq!(
        weather_code(
            100.0,
            0.2,
            Some(0.2),
            0.0,
            Some(11.4),
            Some(2270.0),
            Some(-6.3),
            Some(2.0),
            Some(685.0),
            Some(22240.0),
            Some(0.0),
            3600,
            11.656364,
        ),
        Some(95.0)
    );
}

#[test]
fn weather_code_matches_official_thunderstorm_with_moderate_pbl_height() {
    assert_eq!(
        weather_code(
            100.0,
            0.3,
            Some(0.3),
            0.0,
            Some(11.8),
            Some(2190.0),
            Some(-5.7),
            Some(1.0),
            Some(830.0),
            Some(23180.0),
            Some(0.0),
            3600,
            11.656364,
        ),
        Some(95.0)
    );
}

#[test]
fn weather_code_reference_returns_light_drizzle_with_low_cin() {
    assert_eq!(
        weather_code(
            100.0,
            0.2,
            Some(0.2),
            0.0,
            Some(11.4),
            Some(2160.0),
            Some(-6.0),
            Some(4.0),
            Some(630.0),
            Some(23260.0),
            Some(0.0),
            3600,
            11.656364,
        ),
        Some(51.0)
    );
}

#[test]
fn weather_code_matches_official_thunderstorm_with_high_instability() {
    assert_eq!(
        weather_code(
            100.0,
            0.4,
            Some(0.4),
            0.0,
            Some(12.1),
            Some(2560.0),
            Some(-6.0),
            Some(0.0),
            Some(890.0),
            Some(23200.0),
            Some(0.0),
            3600,
            11.656364,
        ),
        Some(95.0)
    );
}

#[test]
fn weather_code_matches_official_light_thunderstorm_with_high_instability() {
    assert_eq!(
        weather_code(
            100.0,
            0.2,
            Some(0.2),
            0.0,
            Some(11.9),
            Some(2770.0),
            Some(-5.3),
            Some(0.0),
            Some(910.0),
            Some(24140.0),
            Some(0.0),
            3600,
            11.656364,
        ),
        Some(95.0)
    );
}

#[test]
fn weather_code_matches_official_moderate_light_thunderstorm_with_high_instability() {
    assert_eq!(
        weather_code(
            100.0,
            0.3,
            Some(0.3),
            0.0,
            Some(11.4),
            Some(2560.0),
            Some(-5.1),
            Some(0.0),
            Some(855.0),
            Some(24140.0),
            Some(0.0),
            3600,
            11.656364,
        ),
        Some(95.0)
    );
}

#[test]
fn weather_code_matches_current_rule_for_weak_showers_with_low_gusts() {
    assert_eq!(
        weather_code(
            100.0,
            0.4,
            Some(0.4),
            0.0,
            Some(11.1),
            Some(2670.0),
            Some(-6.3),
            Some(0.0),
            Some(805.0),
            Some(24140.0),
            Some(0.0),
            3600,
            11.656364,
        ),
        Some(95.0)
    );
}

#[test]
fn weather_code_matches_official_low_pbl_thunderstorm_without_high_gusts() {
    assert_eq!(
        weather_code(
            100.0,
            0.2,
            Some(0.2),
            0.0,
            Some(11.0),
            Some(2550.0),
            Some(-6.8),
            Some(0.0),
            Some(635.0),
            Some(24140.0),
            Some(0.0),
            3600,
            11.656364,
        ),
        Some(95.0)
    );
}

#[test]
fn weather_code_matches_official_low_pbl_thunderstorm_with_high_gusts_and_cin() {
    assert_eq!(
        weather_code(
            100.0,
            0.2,
            Some(0.2),
            0.0,
            Some(12.4),
            Some(2520.0),
            Some(-6.4),
            Some(8.0),
            Some(670.0),
            Some(24140.0),
            Some(0.0),
            3600,
            11.656364,
        ),
        Some(95.0)
    );
}

#[test]
fn weather_code_reference_returns_light_drizzle_for_low_pbl_weak_showers() {
    assert_eq!(
        weather_code(
            100.0,
            0.2,
            Some(0.2),
            0.0,
            Some(12.1),
            Some(2190.0),
            Some(-6.3),
            Some(0.0),
            Some(685.0),
            Some(24140.0),
            Some(0.0),
            3600,
            11.656364,
        ),
        Some(51.0)
    );
}

#[test]
fn weather_code_matches_current_rule_for_high_pbl_weak_showers() {
    assert_eq!(
        weather_code(
            100.0,
            0.2,
            Some(0.2),
            0.0,
            Some(13.1),
            Some(2200.0),
            Some(-6.9),
            Some(0.0),
            Some(970.0),
            Some(24140.0),
            Some(0.0),
            3600,
            15.3,
        ),
        Some(95.0)
    );
}

#[test]
fn weather_code_reference_returns_thunderstorm_for_high_probability_showers() {
    assert_eq!(
        weather_code(
            100.0,
            1.9,
            Some(1.9),
            0.0,
            Some(11.1),
            Some(2280.0),
            Some(-5.7),
            Some(2.0),
            Some(855.0),
            Some(24140.0),
            Some(0.0),
            3600,
            11.656364,
        ),
        Some(95.0)
    );
}

#[test]
fn weather_code_reference_returns_moderate_drizzle_for_strong_showers() {
    assert_eq!(
        weather_code(
            100.0,
            0.9,
            Some(0.9),
            0.0,
            Some(9.8),
            Some(2020.0),
            Some(-4.7),
            Some(16.0),
            Some(465.0),
            Some(24140.0),
            Some(0.0),
            3600,
            11.656364,
        ),
        Some(53.0)
    );
    assert_eq!(
        weather_code(
            100.0,
            0.7,
            Some(0.7),
            0.0,
            Some(9.3),
            Some(1880.0),
            Some(-4.5),
            Some(10.0),
            Some(395.0),
            Some(23260.0),
            Some(0.0),
            3600,
            11.656364,
        ),
        Some(53.0)
    );
}

#[test]
fn weather_code_reference_returns_drizzle_for_weak_showers() {
    assert_eq!(
        weather_code(
            100.0,
            0.5,
            Some(0.5),
            0.0,
            Some(9.0),
            Some(2000.0),
            Some(-4.9),
            Some(2.0),
            Some(600.0),
            Some(24140.0),
            Some(0.0),
            3600,
            11.656364,
        ),
        Some(53.0)
    );
    assert_eq!(
        weather_code(
            100.0,
            0.2,
            Some(0.2),
            0.0,
            Some(9.0),
            Some(1950.0),
            Some(-4.5),
            Some(1.0),
            Some(640.0),
            Some(24140.0),
            Some(0.0),
            3600,
            11.656364,
        ),
        Some(51.0)
    );
}

#[tokio::test]
async fn health_endpoint_is_not_exposed() {
    let root = fixture_root();
    let state = AppState::new(root.path().to_path_buf(), None).unwrap();
    let app = router(state);

    let response = app
        .oneshot(
            Request::builder()
                .uri("/health")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn latest_json_is_not_a_client_api() {
    let root = fixture_root();
    let state = AppState::new(root.path().to_path_buf(), None).unwrap();
    let app = router(state);

    let response = app
        .oneshot(
            Request::builder()
                .uri("/data/om/gfs013_surface/current/latest.json")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn route_endpoint_returns_each_point_without_client_manifest() {
    let root = fixture_root();
    let state = AppState::new(root.path().to_path_buf(), None).unwrap();
    let app = router(state);

    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/route")
                .header("content-type", "application/json")
                .body(Body::from(
                    serde_json::json!({
                        "hourly": ["temperature_2m"],
                        "points": [
                            {"latitude": -90.0, "longitude": -180.0, "time": "2026-07-08T00:00:00Z"},
                            {"latitude": 90.0, "longitude": 0.0, "time": "2026-07-08T00:00:00Z"}
                        ]
                    })
                    .to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    let bytes = to_bytes(response.into_body(), usize::MAX).await.unwrap();
    let body: Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(body["points"].as_array().unwrap().len(), 2);
    assert_eq!(body["points"][0]["hourly"]["temperature_2m"][0], 12.5);
    assert_eq!(body["points"][1]["hourly"]["temperature_2m"][0], 15.0);
}

#[tokio::test]
async fn land_cell_selection_requires_dem_before_serving() {
    let root = fixture_root();
    let state = AppState::new(root.path().to_path_buf(), None).unwrap();
    let app = router(state);

    let (status, body) = request_json(
        app,
        "/v1/forecast?latitude=-90&longitude=-180&hourly=temperature_2m&forecast_hours=1&cell_selection=land",
    )
    .await;

    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(body["error"]
        .as_str()
        .unwrap()
        .contains("cell_selection=land requires DEM"));
}

fn daily_weather_fixture_root() -> TempDir {
    let root = tempfile::tempdir().unwrap();
    let coverage_id = "gfs013_surface_daily_weather";
    let mut entries = Vec::new();
    for index in 0..24 {
        let (date, hour) = if index < 8 {
            ("2026-07-07", 16 + index)
        } else {
            ("2026-07-08", index - 8)
        };
        let timestamp: &'static str =
            Box::leak(format!("{date}T{hour:02}:00:00Z").into_boxed_str());
        let scalar = |value| [value, value, value, value];
        entries.extend([
            TimedTestEntry {
                variable: "temperature_2m",
                values: scalar(index as f32),
                valid_time_utc: timestamp,
            },
            TimedTestEntry {
                variable: "relative_humidity_2m",
                values: scalar(50.0),
                valid_time_utc: timestamp,
            },
            TimedTestEntry {
                variable: "precipitation",
                values: scalar(if index < 3 { 0.5 } else { 0.0 }),
                valid_time_utc: timestamp,
            },
            TimedTestEntry {
                variable: "shortwave_radiation",
                values: scalar(100.0),
                valid_time_utc: timestamp,
            },
            TimedTestEntry {
                variable: "wind_u_component_10m",
                values: scalar(-3.0),
                valid_time_utc: timestamp,
            },
            TimedTestEntry {
                variable: "wind_v_component_10m",
                values: scalar(0.0),
                valid_time_utc: timestamp,
            },
        ]);
    }
    write_product_coverage_timed(root.path(), "gfs013_surface", coverage_id, entries, false);
    write_group_ready(root.path(), "gfs", &[("gfs013_surface", coverage_id)]);
    root
}

#[tokio::test]
async fn daily_weather_uses_official_aggregation_for_shanghai_local_day() {
    let root = daily_weather_fixture_root();
    let state = AppState::new(root.path().to_path_buf(), None).unwrap();
    let app = router(state);
    let (status, body) = request_json(
        app,
        "/v1/forecast?latitude=-90&longitude=-180&daily=temperature_2m_max,temperature_2m_min,temperature_2m_mean,apparent_temperature_mean,precipitation_sum,precipitation_hours,wind_speed_10m_max,wind_direction_10m_dominant&start_date=2026-07-08&end_date=2026-07-08&timezone=Asia%2FShanghai",
    )
    .await;

    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["utc_offset_seconds"], 28_800);
    assert_eq!(body["timezone"], "Asia/Shanghai");
    assert_eq!(body["daily"]["time"], serde_json::json!(["2026-07-08"]));
    assert_eq!(
        body["daily"]["temperature_2m_max"],
        serde_json::json!([23.0])
    );
    assert_eq!(
        body["daily"]["temperature_2m_min"],
        serde_json::json!([0.0])
    );
    assert_eq!(
        body["daily"]["temperature_2m_mean"],
        serde_json::json!([11.5])
    );
    assert!(body["daily"]["apparent_temperature_mean"][0].is_number());
    assert_eq!(body["daily"]["precipitation_sum"], serde_json::json!([1.5]));
    assert_eq!(
        body["daily"]["precipitation_hours"],
        serde_json::json!([3.0])
    );
    assert_eq!(
        body["daily"]["wind_speed_10m_max"],
        serde_json::json!([3.0])
    );
    assert_eq!(
        body["daily"]["wind_direction_10m_dominant"],
        serde_json::json!([90])
    );
    assert!(body.get("hourly").is_none());
    assert!(body.get("hourly_units").is_none());
}

#[tokio::test]
async fn daily_weather_supports_multiple_coordinates() {
    let root = daily_weather_fixture_root();
    let state = AppState::new(root.path().to_path_buf(), None).unwrap();
    let app = router(state);
    let (status, body) = request_json(
        app,
        "/v1/forecast?latitude=-90,-89&longitude=-180,-179&daily=temperature_2m_max,precipitation_sum&start_date=2026-07-08&end_date=2026-07-08&timezone=Asia%2FShanghai",
    )
    .await;

    assert_eq!(status, StatusCode::OK, "{body}");
    let responses = body.as_array().unwrap();
    assert_eq!(responses.len(), 2);
    assert!(responses[0].get("location_id").is_none());
    assert_eq!(responses[1]["location_id"], serde_json::json!(1));
    for response in responses {
        assert_eq!(
            response["daily"]["temperature_2m_max"],
            serde_json::json!([23.0])
        );
        assert_eq!(
            response["daily"]["precipitation_sum"],
            serde_json::json!([1.5])
        );
    }
}

#[tokio::test]
async fn explicit_timezone_applies_to_hour_selection_and_output() {
    let root = daily_weather_fixture_root();
    let state = AppState::new(root.path().to_path_buf(), None).unwrap();
    let app = router(state);
    let (status, body) = request_json(
        app,
        "/v1/forecast?latitude=-90&longitude=-180&hourly=temperature_2m&start_hour=2026-07-08T00:00&end_hour=2026-07-08T00:00&timezone=Asia%2FShanghai",
    )
    .await;

    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(
        body["hourly"]["time"],
        serde_json::json!(["2026-07-08T00:00"])
    );
    assert_eq!(body["hourly"]["temperature_2m"], serde_json::json!([0.0]));
}

#[tokio::test]
async fn daily_weather_rejects_non_exact_features() {
    let root = daily_weather_fixture_root();
    let state = AppState::new(root.path().to_path_buf(), None).unwrap();
    let app = router(state);

    let (auto_status, auto_body) = request_json(
        app.clone(),
        "/v1/forecast?latitude=-90&longitude=-180&daily=temperature_2m_max&start_date=2026-07-08&end_date=2026-07-08&timezone=auto",
    )
    .await;
    assert_eq!(auto_status, StatusCode::BAD_REQUEST);
    assert!(auto_body["error"]
        .as_str()
        .unwrap()
        .contains("timezone=auto"));

    let (sunrise_status, sunrise_body) = request_json(
        app,
        "/v1/forecast?latitude=-90&longitude=-180&daily=sunrise&start_date=2026-07-08&end_date=2026-07-08",
    )
    .await;
    assert_eq!(sunrise_status, StatusCode::BAD_REQUEST);
    assert!(sunrise_body["error"]
        .as_str()
        .unwrap()
        .contains("unsupported daily weather variable"));
}
