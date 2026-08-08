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


def test_openresty_mobile_client_entrypoint_is_http_only_and_uses_shared_surface():
    server = (ROOT / "nginx" / "openresty_om_client_http.conf").read_text(
        encoding="utf-8"
    )
    compose_override = (
        ROOT / "nginx" / "openresty_compose.override.yml"
    ).read_text(encoding="utf-8")

    assert "listen 80 default_server;" in server
    assert "server_name 124.222.212.233 _;" in server
    assert "om_client_api.inc" in server
    assert "listen 443" not in server
    assert "weather_om_webp" in compose_override
    assert "weather_forecast_server" in compose_override
    assert ":ro" in compose_override
