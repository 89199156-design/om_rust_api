use crate::official::OfficialDecoder;
use crate::query::{
    ecmwf_public_hourly_variables, forecast_for_query, route_forecast, validate_cams_query,
    validate_explicit_variables, validate_gfs_query, PointQuery, RouteQuery, WeatherModel,
    ECMWF_PUBLIC_DAILY_VARIABLES,
};
use crate::snapshot::OmDataSnapshot;
use anyhow::{Context, Result};
use axum::extract::{Query, State};
use axum::http::{header, HeaderName, HeaderValue, StatusCode};
use axum::middleware;
use axum::response::Response;
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::json;
use std::collections::BTreeMap;
use std::fs;
use std::net::SocketAddr;
use std::path::Path;
use std::path::PathBuf;
use std::sync::{Arc, RwLock};
use std::time::Duration;
use tower_http::trace::TraceLayer;

const SOURCE_REPOSITORY: &str = "https://github.com/89199156-design/om_rust_api";
const SOURCE_LICENSE: &str = "AGPL-3.0-or-later";
const BUILD_REVISION: &str = match option_env!("OM_BUILD_REVISION") {
    Some(revision) => revision,
    None => "development",
};
const AGPL_LICENSE_URL: &str = "https://www.gnu.org/licenses/agpl-3.0.html";
const ECMWF_LICENSE_URL: &str = "https://creativecommons.org/licenses/by/4.0/";

#[derive(Clone)]
pub struct AppState {
    data_root: PathBuf,
    decoder: Option<OfficialDecoder>,
    cache: Arc<RwLock<SnapshotCache>>,
}

struct SnapshotCache {
    identity: SnapshotIdentity,
    snapshot: Arc<OmDataSnapshot>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct SnapshotIdentity {
    gfs_ready: Option<GroupIdentity>,
    cams_ready: Option<GroupIdentity>,
    cams_greenhouse_ready: Option<GroupIdentity>,
    ecmwf_ready: Option<GroupIdentity>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
struct GroupIdentity {
    status: String,
    #[serde(default)]
    runtime_format: String,
    #[serde(default)]
    latest_complete_run: String,
    #[serde(default)]
    coverage_id: String,
    #[serde(default)]
    products: serde_json::Value,
    #[serde(default)]
    product_manifests: BTreeMap<String, ProductIdentity>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
struct ProductIdentity {
    coverage_id: String,
}

impl SnapshotIdentity {
    fn read(data_root: &Path) -> Result<Self> {
        fn marker(data_root: &Path, group: &str) -> Result<Option<GroupIdentity>> {
            let path = data_root
                .join("groups")
                .join(group)
                .join("current")
                .join("ready_for_processing.json");
            match fs::read(&path) {
                Ok(bytes) => Ok(Some(serde_json::from_slice(&bytes).with_context(|| {
                    format!("parse snapshot marker identity {}", path.display())
                })?)),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
                Err(error) => {
                    Err(error).with_context(|| format!("read snapshot marker {}", path.display()))
                }
            }
        }
        Ok(Self {
            gfs_ready: marker(data_root, "gfs")?,
            cams_ready: marker(data_root, "cams")?,
            cams_greenhouse_ready: marker(data_root, "cams_greenhouse")?,
            ecmwf_ready: marker(data_root, "ecmwf")?,
        })
    }
}

impl AppState {
    pub fn new(data_root: PathBuf, decoder: Option<OfficialDecoder>) -> Result<Self> {
        let identity = SnapshotIdentity::read(&data_root)?;
        let snapshot = Arc::new(OmDataSnapshot::load(&data_root)?);
        Ok(Self {
            data_root,
            decoder,
            cache: Arc::new(RwLock::new(SnapshotCache { identity, snapshot })),
        })
    }

    fn snapshot(&self) -> Result<Arc<OmDataSnapshot>> {
        let guard = self
            .cache
            .read()
            .map_err(|_| anyhow::anyhow!("snapshot cache poisoned"))?;
        Ok(guard.snapshot.clone())
    }

    fn data_identity(&self) -> Result<serde_json::Value> {
        fn group(identity: Option<&GroupIdentity>) -> serde_json::Value {
            match identity {
                Some(identity) => json!({
                    "status": identity.status,
                    "runtime_format": identity.runtime_format,
                    "latest_complete_run": identity.latest_complete_run,
                    "coverage_id": identity.coverage_id,
                }),
                None => serde_json::Value::Null,
            }
        }

        let guard = self
            .cache
            .read()
            .map_err(|_| anyhow::anyhow!("snapshot cache poisoned"))?;
        Ok(json!({
            "gfs": group(guard.identity.gfs_ready.as_ref()),
            "cams": group(guard.identity.cams_ready.as_ref()),
            "cams_greenhouse": group(guard.identity.cams_greenhouse_ready.as_ref()),
            "ecmwf": group(guard.identity.ecmwf_ready.as_ref()),
        }))
    }

    fn weather_snapshot(&self, model: WeatherModel) -> Result<(Arc<OmDataSnapshot>, String)> {
        let guard = self
            .cache
            .read()
            .map_err(|_| anyhow::anyhow!("snapshot cache poisoned"))?;
        let identity = match model {
            WeatherModel::Gfs => guard.identity.gfs_ready.as_ref(),
            WeatherModel::EcmwfIfs025 => guard.identity.ecmwf_ready.as_ref(),
        }
        .context("weather OM group marker is unavailable")?;
        if identity.status != "complete" || identity.latest_complete_run.is_empty() {
            anyhow::bail!("weather OM group marker is not complete");
        }
        Ok((guard.snapshot.clone(), identity.latest_complete_run.clone()))
    }

    fn refresh_if_changed(&self) -> Result<bool> {
        let identity_before = SnapshotIdentity::read(&self.data_root)?;
        {
            let guard = self
                .cache
                .read()
                .map_err(|_| anyhow::anyhow!("snapshot cache poisoned"))?;
            if guard.identity == identity_before {
                return Ok(false);
            }
        }
        let snapshot = Arc::new(OmDataSnapshot::load(&self.data_root)?);
        let identity_after = SnapshotIdentity::read(&self.data_root)?;
        if identity_after != identity_before {
            return Ok(false);
        }
        let mut guard = self
            .cache
            .write()
            .map_err(|_| anyhow::anyhow!("snapshot cache poisoned"))?;
        if guard.identity == identity_after {
            return Ok(false);
        }
        guard.identity = identity_after;
        guard.snapshot = snapshot;
        Ok(true)
    }

    #[cfg(unix)]
    async fn refresh_on_publish_signal(
        self,
        mut published: tokio::signal::unix::Signal,
    ) -> Result<()> {
        while published.recv().await.is_some() {
            let state = self.clone();
            match tokio::task::spawn_blocking(move || state.refresh_if_changed()).await {
                Ok(Ok(true)) => tracing::info!("published new immutable OM API snapshot"),
                Ok(Ok(false)) => {}
                Ok(Err(error)) => tracing::error!(
                    error = %error,
                    "OM snapshot refresh failed; retaining previous snapshot"
                ),
                Err(error) => tracing::error!(
                    error = %error,
                    "OM snapshot refresh worker failed; retaining previous snapshot"
                ),
            }
        }
        Ok(())
    }

    async fn refresh_periodically(self, refresh_interval: Duration) {
        let mut ticker = tokio::time::interval(refresh_interval);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        ticker.tick().await;
        loop {
            ticker.tick().await;
            let state = self.clone();
            match tokio::task::spawn_blocking(move || state.refresh_if_changed()).await {
                Ok(Ok(true)) => tracing::info!("periodically refreshed immutable OM API snapshot"),
                Ok(Ok(false)) => {}
                Ok(Err(error)) => tracing::error!(
                    error = %error,
                    "periodic OM snapshot refresh failed; retaining previous snapshot"
                ),
                Err(error) => tracing::error!(
                    error = %error,
                    "periodic OM snapshot refresh worker failed; retaining previous snapshot"
                ),
            }
        }
    }
}

pub fn router(state: AppState) -> Router {
    Router::new()
        .route("/", get(source_offer))
        .route("/v1/source", get(source_offer))
        .route("/v1/data-identity", get(data_identity))
        .route(
            "/.well-known/weather-attribution.json",
            get(weather_attribution),
        )
        .route("/v1/gfs", get(gfs_forecast))
        .route("/v1/ecmwf", get(ecmwf_forecast).post(ecmwf_forecast_post))
        .route("/v1/ecmwf/catalog", get(ecmwf_catalog))
        .route("/v1/cams", get(cams_forecast))
        .route("/v1/route", post(route))
        .route("/v1/ecmwf/route", post(ecmwf_route))
        .with_state(state)
        .layer(middleware::map_response(source_offer_headers))
        .layer(TraceLayer::new_for_http())
}

async fn source_offer() -> Json<serde_json::Value> {
    Json(json!({
        "schema_version": 1,
        "component": "om_rust_api",
        "build_revision": BUILD_REVISION,
        "license": SOURCE_LICENSE,
        "license_url": AGPL_LICENSE_URL,
        "source_code": SOURCE_REPOSITORY,
        "source_archive_url": format!("/source/om_rust_api-{BUILD_REVISION}.tar.gz"),
        "source_archive_sha256_url": format!("/source/om_rust_api-{BUILD_REVISION}.tar.gz.sha256"),
        "weather_attribution_url": "/.well-known/weather-attribution.json",
        "notice": "Corresponding Source for this network service is available at source_code."
    }))
}

async fn data_identity(State(state): State<AppState>) -> Result<Json<serde_json::Value>, ApiError> {
    Ok(Json(state.data_identity()?))
}

fn weather_attribution_payload() -> serde_json::Value {
    json!({
        "schema_version": 1,
        "generated_by": "om_rust_api",
        "build_revision": BUILD_REVISION,
        "source_software": {
            "name": "Open-Meteo",
            "project_url": "https://github.com/open-meteo/open-meteo",
            "license": SOURCE_LICENSE,
            "license_url": AGPL_LICENSE_URL,
            "modifications": "Separately maintained implementation over transformed local forecast products."
        },
        "data_sources": {
            "ecmwf_ifs025": {
                "provider": "European Centre for Medium-Range Weather Forecasts (ECMWF)",
                "provider_url": "https://www.ecmwf.int/",
                "distributor": "Open-Meteo",
                "distributor_url": "https://open-meteo.com/",
                "dataset": "ECMWF IFS deterministic and ensemble open data",
                "license": "CC-BY-4.0",
                "license_url": ECMWF_LICENSE_URL,
                "terms_url": "https://apps.ecmwf.int/datasets/licences/general/",
                "attribution": "Weather data by Open-Meteo.com. This service is based on data and products of the European Centre for Medium-Range Weather Forecasts (ECMWF). Contains modified ECMWF data.",
                "modified": true,
                "transformations": [
                    "spatial subsetting",
                    "range extraction",
                    "temporal and spatial interpolation where requested",
                    "unit conversion and derived-variable calculation where requested",
                    "lossless WebP encoding for map layers"
                ],
                "disclaimer": "ECMWF has no liability in respect of this service or its transformed outputs."
            },
            "gfs": {
                "provider": "NOAA National Centers for Environmental Prediction (NCEP)",
                "provider_url": "https://www.ncep.noaa.gov/",
                "dataset": "Global Forecast System (GFS) and Global Ensemble Forecast System (GEFS)",
                "distributor": "Open-Meteo",
                "distributor_url": "https://open-meteo.com/",
                "terms_url": "https://www.weather.gov/disclaimer",
                "modified": true
            },
            "cams": {
                "provider": "Copernicus Atmosphere Monitoring Service (CAMS)",
                "provider_url": "https://atmosphere.copernicus.eu/",
                "terms_url": "https://atmosphere.copernicus.eu/data-licence",
                "modified": true
            },
            "dem": {
                "provider": "Copernicus DEM",
                "provider_url": "https://dataspace.copernicus.eu/explore-data/data-collections/copernicus-contributing-missions/collections-description/COP-DEM",
                "modified": true
            }
        },
        "details": "/DATA_SOURCES.md"
    })
}

async fn weather_attribution() -> Json<serde_json::Value> {
    Json(weather_attribution_payload())
}

async fn source_offer_headers(mut response: Response) -> Response {
    let link = format!("<{SOURCE_REPOSITORY}>; rel=\"source\"");
    response.headers_mut().insert(
        header::LINK,
        HeaderValue::from_str(&link).expect("static source repository URL is a valid Link header"),
    );
    response.headers_mut().insert(
        HeaderName::from_static("x-source-code"),
        HeaderValue::from_static(SOURCE_REPOSITORY),
    );
    response
}

pub async fn serve(
    state: AppState,
    bind: SocketAddr,
    snapshot_refresh_interval: Duration,
) -> Result<()> {
    #[cfg(unix)]
    let refresh_task = {
        use tokio::signal::unix::{signal, SignalKind};
        let published = signal(SignalKind::hangup())?;
        tokio::spawn(state.clone().refresh_on_publish_signal(published))
    };
    let periodic_refresh_task = if snapshot_refresh_interval.is_zero() {
        None
    } else {
        Some(tokio::spawn(
            state
                .clone()
                .refresh_periodically(snapshot_refresh_interval),
        ))
    };
    let listener = tokio::net::TcpListener::bind(bind)
        .await
        .with_context(|| format!("failed to bind {}", bind))?;
    let result = axum::serve(listener, router(state)).await;
    if let Some(task) = periodic_refresh_task {
        task.abort();
    }
    #[cfg(unix)]
    refresh_task.abort();
    result?;
    Ok(())
}

async fn gfs_forecast(
    State(state): State<AppState>,
    Query(mut query): Query<PointQuery>,
) -> Result<Json<serde_json::Value>, ApiError> {
    validate_gfs_query(&query)?;
    query.models = Some("gfs".to_string());
    let (snapshot, model_run) = state.weather_snapshot(WeatherModel::Gfs)?;
    let decoder = state.decoder.clone();
    let mut payload = tokio::task::spawn_blocking(move || {
        forecast_for_query(&snapshot, decoder.as_ref(), &query)
    })
    .await
    .context("forecast worker failed")??;
    attach_model_run(&mut payload, &model_run)?;
    Ok(Json(payload))
}

async fn ecmwf_forecast(
    State(state): State<AppState>,
    Query(mut query): Query<PointQuery>,
) -> Result<Json<serde_json::Value>, ApiError> {
    validate_explicit_variables(&query)?;
    query.models = Some("ecmwf_ifs025".to_string());
    let (snapshot, model_run) = state.weather_snapshot(WeatherModel::EcmwfIfs025)?;
    let decoder = state.decoder.clone();
    let mut payload = tokio::task::spawn_blocking(move || {
        forecast_for_query(&snapshot, decoder.as_ref(), &query)
    })
    .await
    .context("ECMWF forecast worker failed")??;
    attach_model_run(&mut payload, &model_run)?;
    Ok(Json(payload))
}

async fn ecmwf_forecast_post(
    State(state): State<AppState>,
    Json(mut query): Json<PointQuery>,
) -> Result<Json<serde_json::Value>, ApiError> {
    validate_explicit_variables(&query)?;
    query.models = Some("ecmwf_ifs025".to_string());
    let (snapshot, model_run) = state.weather_snapshot(WeatherModel::EcmwfIfs025)?;
    let decoder = state.decoder.clone();
    let mut payload = tokio::task::spawn_blocking(move || {
        forecast_for_query(&snapshot, decoder.as_ref(), &query)
    })
    .await
    .context("ECMWF POST forecast worker failed")??;
    attach_model_run(&mut payload, &model_run)?;
    Ok(Json(payload))
}

fn attach_model_run(payload: &mut serde_json::Value, model_run: &str) -> Result<()> {
    fn attach(response: &mut serde_json::Value, model_run: &str) -> Result<()> {
        response
            .as_object_mut()
            .context("forecast response is not an object")?
            .insert(
                "model_run".to_string(),
                serde_json::Value::String(model_run.to_string()),
            );
        Ok(())
    }

    match payload {
        serde_json::Value::Array(responses) => {
            for response in responses {
                attach(response, model_run)?;
            }
            Ok(())
        }
        response => attach(response, model_run),
    }
}

async fn ecmwf_catalog(State(state): State<AppState>) -> Result<Json<serde_json::Value>, ApiError> {
    let snapshot = state.snapshot()?;
    let product = snapshot.require_product("ecmwf_ifs025")?;
    let probability_product = snapshot.require_product("ecmwf_ifs025_ensemble")?;
    let hourly = ecmwf_public_hourly_variables();
    let daily = ECMWF_PUBLIC_DAILY_VARIABLES.to_vec();
    let available_variables = hourly
        .iter()
        .map(String::as_str)
        .chain(daily.iter().copied())
        .collect::<std::collections::BTreeSet<_>>();
    Ok(Json(json!({
        "model": "ecmwf_ifs025",
        "coverage_id": product.manifest.coverage_id,
        "latest_complete_run": product.manifest.latest_complete_run,
        "public_start_utc": product.manifest.public_start_utc,
        "products": {
            "ecmwf_ifs025": {
                "coverage_id": product.manifest.coverage_id,
                "latest_complete_run": product.manifest.latest_complete_run,
                "public_start_utc": product.manifest.public_start_utc,
            },
            "ecmwf_ifs025_ensemble": {
                "coverage_id": probability_product.manifest.coverage_id,
                "latest_complete_run": probability_product.manifest.latest_complete_run,
                "public_start_utc": probability_product.manifest.public_start_utc,
                "variables": ["precipitation_probability"],
            }
        },
        "available_hourly_variables": hourly,
        "available_daily_variables": daily,
        "available_variables": available_variables,
    })))
}

async fn cams_forecast(
    State(state): State<AppState>,
    Query(mut query): Query<PointQuery>,
) -> Result<Json<serde_json::Value>, ApiError> {
    validate_cams_query(&query)?;
    query.models = Some("gfs".to_string());
    let snapshot = state.snapshot()?;
    let decoder = state.decoder.clone();
    let payload = tokio::task::spawn_blocking(move || {
        forecast_for_query(&snapshot, decoder.as_ref(), &query)
    })
    .await
    .context("CAMS forecast worker failed")??;
    Ok(Json(payload))
}

async fn route(
    State(state): State<AppState>,
    Json(query): Json<RouteQuery>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let snapshot = state.snapshot()?;
    let decoder = state.decoder.clone();
    let payload =
        tokio::task::spawn_blocking(move || route_forecast(&snapshot, decoder.as_ref(), &query))
            .await
            .context("route worker failed")??;
    Ok(Json(serde_json::to_value(payload)?))
}

async fn ecmwf_route(
    State(state): State<AppState>,
    Json(mut query): Json<RouteQuery>,
) -> Result<Json<serde_json::Value>, ApiError> {
    query.models = Some("ecmwf_ifs025".to_string());
    let snapshot = state.snapshot()?;
    let decoder = state.decoder.clone();
    let payload =
        tokio::task::spawn_blocking(move || route_forecast(&snapshot, decoder.as_ref(), &query))
            .await
            .context("ECMWF route worker failed")??;
    Ok(Json(serde_json::to_value(payload)?))
}

pub struct ApiError(anyhow::Error);

impl<E> From<E> for ApiError
where
    E: Into<anyhow::Error>,
{
    fn from(error: E) -> Self {
        Self(error.into())
    }
}

impl axum::response::IntoResponse for ApiError {
    fn into_response(self) -> axum::response::Response {
        let status = StatusCode::BAD_REQUEST;
        let body = Json(json!({
            "error": self.0.to_string(),
        }));
        (status, body).into_response()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::Request;
    use tempfile::TempDir;
    use tower::ServiceExt;

    #[tokio::test]
    async fn source_offer_is_present_on_root_and_api_errors() {
        let root = TempDir::new().unwrap();
        let app = router(AppState::new(root.path().to_path_buf(), None).unwrap());
        for uri in ["/", "/v1/gfs"] {
            let response = app
                .clone()
                .oneshot(Request::builder().uri(uri).body(Body::empty()).unwrap())
                .await
                .unwrap();
            assert_eq!(
                response.headers().get(header::LINK).unwrap(),
                &HeaderValue::from_str(&format!("<{SOURCE_REPOSITORY}>; rel=\"source\"")).unwrap()
            );
            assert_eq!(
                response.headers().get("x-source-code").unwrap(),
                SOURCE_REPOSITORY
            );
        }
    }

    #[tokio::test]
    async fn public_model_routes_enforce_explicit_model_specific_variables() {
        let root = TempDir::new().unwrap();
        let app = router(AppState::new(root.path().to_path_buf(), None).unwrap());

        for uri in ["/v1/forecast", "/v1/air-quality", "/v1/ecmwf/forecast"] {
            let response = app
                .clone()
                .oneshot(Request::builder().uri(uri).body(Body::empty()).unwrap())
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::NOT_FOUND, "{uri}");
        }

        for uri in [
            "/v1/gfs?latitude=31.23&longitude=121.47",
            "/v1/ecmwf?latitude=31.23&longitude=121.47",
            "/v1/cams?latitude=31.23&longitude=121.47",
            "/v1/gfs?latitude=31.23&longitude=121.47&hourly=pm2_5",
            "/v1/cams?latitude=31.23&longitude=121.47&hourly=temperature_2m",
        ] {
            let response = app
                .clone()
                .oneshot(Request::builder().uri(uri).body(Body::empty()).unwrap())
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::BAD_REQUEST, "{uri}");
        }
    }

    #[tokio::test]
    async fn data_identity_reports_the_exact_loaded_coverage() {
        let root = TempDir::new().unwrap();
        let marker = root
            .path()
            .join("groups/gfs/current/ready_for_processing.json");
        fs::create_dir_all(marker.parent().unwrap()).unwrap();
        fs::write(
            marker,
            br#"{
                "status":"incomplete",
                "runtime_format":"openmeteo-native-v1",
                "latest_complete_run":"2026073006",
                "coverage_id":"gfs_native_2026073006_probability",
                "products":{}
            }"#,
        )
        .unwrap();
        let app = router(AppState::new(root.path().to_path_buf(), None).unwrap());
        let response = app
            .oneshot(
                Request::builder()
                    .uri("/v1/data-identity")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let payload: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(
            payload["gfs"]["coverage_id"],
            "gfs_native_2026073006_probability"
        );
        assert_eq!(payload["gfs"]["latest_complete_run"], "2026073006");
        assert!(payload["cams"].is_null());
        assert!(payload["cams_greenhouse"].is_null());
    }

    #[tokio::test]
    async fn ecmwf_post_accepts_official_single_location_array_shape_with_full_catalog() {
        let root = TempDir::new().unwrap();
        let app = router(AppState::new(root.path().to_path_buf(), None).unwrap());
        let body = json!({
            "latitude": [31.2304],
            "longitude": [121.4737],
            "hourly": ecmwf_public_hourly_variables(),
            "daily": ECMWF_PUBLIC_DAILY_VARIABLES,
            "models": ["ecmwf_ifs025"],
            "start_hour": ["2026-07-23T00:00"],
            "end_hour": ["2026-08-07T00:00"],
            "start_date": ["2026-07-23"],
            "end_date": ["2026-08-06"],
            "timezone": ["GMT"],
            "temperature_unit": "celsius",
            "wind_speed_unit": "ms",
            "precipitation_unit": "mm",
            "timeformat": "iso8601",
            "cell_selection": "land"
        });
        let response = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/v1/ecmwf")
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(serde_json::to_vec(&body).unwrap()))
                    .unwrap(),
            )
            .await
            .unwrap();

        // The empty fixture has neither an ECMWF product nor DEM selection
        // data. Reaching either domain error proves the complete 197+65
        // official JSON shape was accepted and dispatched as one location.
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let error: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        let error_text = error["error"].as_str().unwrap();
        assert!(
            error_text.contains("product is not available")
                || error_text.contains("requires DEM/static grid selection data")
                || error_text.contains("weather OM group marker is unavailable"),
            "unexpected domain error after valid POST decode: {error_text}"
        );
    }

    #[test]
    fn snapshot_identity_accepts_product_name_list() {
        let identity: GroupIdentity = serde_json::from_value(json!({
            "status": "complete",
            "latest_complete_run": "2026071506",
            "products": ["gfs013_surface", "gfs025", "gfs_pressure_profile"],
            "product_manifests": {
                "gfs013_surface": {"coverage_id": "gfs013_surface_2026071506_209h"}
            }
        }))
        .unwrap();

        assert_eq!(identity.products.as_array().unwrap().len(), 3);
        assert_eq!(identity.product_manifests.len(), 1);
    }

    #[test]
    fn snapshot_reads_do_not_refresh_without_a_refresh_trigger() {
        let root = TempDir::new().unwrap();
        let state = AppState::new(root.path().to_path_buf(), None).unwrap();
        assert!(state.cache.read().unwrap().identity.gfs_ready.is_none());

        let marker = root
            .path()
            .join("groups/gfs/current/ready_for_processing.json");
        fs::create_dir_all(marker.parent().unwrap()).unwrap();
        fs::write(
            marker,
            br#"{
                "status":"incomplete",
                "runtime_format":"legacy",
                "latest_complete_run":"2026071300",
                "coverage_id":"",
                "product_manifests":{}
            }"#,
        )
        .unwrap();

        // A client snapshot read performs no filesystem refresh.
        let _ = state.snapshot().unwrap();
        assert!(state.cache.read().unwrap().identity.gfs_ready.is_none());

        // A refresh trigger installs the changed identity.
        assert!(state.refresh_if_changed().unwrap());
        assert!(state.cache.read().unwrap().identity.gfs_ready.is_some());
        assert!(!state.refresh_if_changed().unwrap());
    }

    #[tokio::test]
    async fn periodic_refresh_installs_a_changed_snapshot() {
        let root = TempDir::new().unwrap();
        let state = AppState::new(root.path().to_path_buf(), None).unwrap();
        let marker = root
            .path()
            .join("groups/gfs/current/ready_for_processing.json");
        fs::create_dir_all(marker.parent().unwrap()).unwrap();
        fs::write(
            marker,
            br#"{
                "status":"incomplete",
                "runtime_format":"legacy",
                "latest_complete_run":"2026071800",
                "coverage_id":"",
                "product_manifests":{}
            }"#,
        )
        .unwrap();

        let refresh_task = tokio::spawn(
            state
                .clone()
                .refresh_periodically(Duration::from_millis(10)),
        );
        for _ in 0..50 {
            if state.cache.read().unwrap().identity.gfs_ready.is_some() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        refresh_task.abort();

        assert!(state.cache.read().unwrap().identity.gfs_ready.is_some());
    }
}
