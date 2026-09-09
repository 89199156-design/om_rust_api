from pathlib import Path


ROOT = Path(__file__).resolve().parents[1]


def test_shared_runtime_repository_contains_only_api_webp_and_validation_logic():
    assert not (ROOT / "downloader").exists()
    assert not (ROOT / "om_downloader").exists()
    assert not (ROOT / "docs" / "native_turbopfor.md").exists()
    assert not list(ROOT.rglob("libom_turbopfor.so"))

    cargo = (ROOT / "Cargo.toml").read_text(encoding="utf-8")
    assert 'members = ["om_api", "webp/om_webp"]' in cargo

    api = (ROOT / "om_api" / "src" / "api.rs").read_text(encoding="utf-8")
    assert "89199156-design/om_rust_api" in api
    assert "om_weather_server" not in api

    webp_runner = (
        ROOT / "webp" / "om_webp" / "scripts" / "run_scope.sh"
    ).read_text(encoding="utf-8")
    assert "$app_dir/scripts/task_progress_reporter.py" in webp_runner
    assert "weather_om_downloader" not in webp_runner


def test_public_nginx_surface_does_not_expose_raw_om_bundles():
    nginx = (ROOT / "nginx" / "om_client_api.conf").read_text(encoding="utf-8")
    assert "/v1/" in nginx
    assert "/data/webp/" in nginx
    assert "/data/om/" not in nginx
    assert "om_rust_api-" in nginx


def test_internal_grid_1panel_proxy_bypasses_only_outer_auth_and_does_not_buffer():
    nginx = (ROOT / "nginx" / "om_internal_grid_1panel.conf").read_text(
        encoding="utf-8"
    )
    assert "location = /v1/internal/grid {" in nginx
    assert "location = /v1/internal/grid/catalog {" in nginx
    assert nginx.count("auth_request off;") == 2
    assert nginx.count("proxy_set_header Authorization $http_authorization;") == 2
    assert nginx.count("proxy_buffering off;") == 2
    assert "location /" not in nginx
