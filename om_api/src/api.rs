use crate::ecmwf_route::{model_name, EcmwfRouteSelector};
use crate::grid_export::{
    catalog as grid_catalog_payload, encode_grid_file, GridModel, GridReleaseIdentity,
    RequestedBounds,
};
use crate::official::OfficialDecoder;
use crate::query::{
    ecmwf_ifs9km_public_daily_variables, ecmwf_ifs9km_public_hourly_variables,
    ecmwf_public_hourly_variables, forecast_for_query, route_forecast, validate_cams_query,
    validate_explicit_variables, validate_gfs_query, PointQuery, RouteQuery, WeatherModel,
    ECMWF_PUBLIC_DAILY_VARIABLES,
};
use crate::snapshot::OmDataSnapshot;
use crate::stargazing::{load_details, StargazingDetailsQuery};
use anyhow::{Context, Result};
use axum::extract::{Query, State};
use axum::http::{header, HeaderMap, HeaderName, HeaderValue, StatusCode};
use axum::middleware;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::json;
use sha2::Digest;
use std::collections::BTreeMap;
use std::fs;
use std::net::SocketAddr;
use std::path::Path;
use std::path::PathBuf;
use std::sync::{Arc, RwLock};
use std::time::Duration;
use tokio::sync::Semaphore;
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
    stargazing_root: PathBuf,
    decoder: Option<OfficialDecoder>,
    cache: Arc<RwLock<SnapshotCache>>,
    ecmwf_route: EcmwfRouteSelector,
    internal_grid_token_file: Option<PathBuf>,
    internal_grid_permits: Arc<Semaphore>,
    internal_grid_queue_timeout: Duration,
}

struct SnapshotCache {
    identity: SnapshotIdentity,
    snapshot: Arc<OmDataSnapshot>,
}

fn authorize_internal_grid_token_file(path: Option<&Path>, headers: &HeaderMap) -> Result<()> {
    let path = path.context("internal grid API is disabled")?;
    if !path.is_absolute() {
        anyhow::bail!("internal grid token path must be absolute");
    }
    let expected = fs::read_to_string(path)
        .with_context(|| format!("read internal grid token file {}", path.display()))?;
    let expected = expected.trim();
    if expected.len() < 32 || expected.chars().any(char::is_whitespace) {
        anyhow::bail!("internal grid token file is invalid");
    }
    let supplied = headers
        .get(header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer "))
        .unwrap_or("");
    let expected_digest = sha2::Sha256::digest(expected.as_bytes());
    let supplied_digest = sha2::Sha256::digest(supplied.as_bytes());
    let mismatch = expected_digest
        .iter()
        .zip(supplied_digest.iter())
        .fold(0_u8, |difference, (expected, supplied)| {
            difference | (expected ^ supplied)
        });
    if mismatch != 0 {
        anyhow::bail!("internal grid authorization failed");
    }
    Ok(())
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct SnapshotIdentity {
    gfs_ready: Option<GroupIdentity>,
    cams_ready: Option<GroupIdentity>,
    cams_greenhouse_ready: Option<GroupIdentity>,
    ecmwf_ready: Option<GroupIdentity>,
    ecmwf_ifs9km_ready: Option<GroupIdentity>,
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
    batch_id: String,
    #[serde(default)]
    batch_ready_sha256: String,
    #[serde(default)]
    public_start_utc: String,
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
            ecmwf_ifs9km_ready: marker(data_root, "ecmwf_ifs9km")?,
        })
    }
}

impl AppState {
    pub fn new(data_root: PathBuf, decoder: Option<OfficialDecoder>) -> Result<Self> {
        Self::new_with_ecmwf_route(data_root, decoder, None)
    }

    pub fn new_with_ecmwf_route(
        data_root: PathBuf,
        decoder: Option<OfficialDecoder>,
        ecmwf_route_state: Option<PathBuf>,
    ) -> Result<Self> {
        let internal_grid_max_concurrent = std::env::var("OM_INTERNAL_GRID_MAX_CONCURRENT")
            .ok()
            .map(|value| value.parse::<usize>())
            .transpose()
            .context("OM_INTERNAL_GRID_MAX_CONCURRENT must be a positive integer")?
            .unwrap_or(1);
        if internal_grid_max_concurrent == 0 {
            anyhow::bail!("OM_INTERNAL_GRID_MAX_CONCURRENT must be a positive integer");
        }
        let internal_grid_queue_timeout_seconds =
            std::env::var("OM_INTERNAL_GRID_QUEUE_TIMEOUT_SECONDS")
                .ok()
                .map(|value| value.parse::<u64>())
                .transpose()
                .context("OM_INTERNAL_GRID_QUEUE_TIMEOUT_SECONDS must be an integer")?
                .unwrap_or(30);
        let internal_grid_token_file =
            std::env::var_os("OM_INTERNAL_GRID_TOKEN_FILE").map(PathBuf::from);
        let identity = SnapshotIdentity::read(&data_root)?;
        let snapshot = Arc::new(OmDataSnapshot::load(&data_root)?);
        Ok(Self {
            data_root,
            stargazing_root: std::env::var_os("OM_STARGAZING_ROOT")
                .map(PathBuf::from)
                .unwrap_or_else(|| PathBuf::from("/opt/1panel/apps/weather_om_webp/data")),
            decoder,
            cache: Arc::new(RwLock::new(SnapshotCache { identity, snapshot })),
            ecmwf_route: EcmwfRouteSelector::new(ecmwf_route_state),
            internal_grid_token_file,
            internal_grid_permits: Arc::new(Semaphore::new(internal_grid_max_concurrent)),
            internal_grid_queue_timeout: Duration::from_secs(internal_grid_queue_timeout_seconds),
        })
    }

    fn selected_ecmwf_model(&self) -> Result<WeatherModel> {
        let (model, selected_run) = self.ecmwf_route.selection();
        let Some(selected_run) = selected_run else {
            return Ok(model);
        };
        let loaded_run = {
            let guard = self
                .cache
                .read()
                .map_err(|_| anyhow::anyhow!("snapshot cache poisoned"))?;
            match model {
                WeatherModel::EcmwfIfs025 => guard.identity.ecmwf_ready.as_ref(),
                WeatherModel::EcmwfIfs9km => guard.identity.ecmwf_ifs9km_ready.as_ref(),
                WeatherModel::Gfs => unreachable!("ECMWF route selector cannot select GFS"),
            }
            .map(|identity| identity.latest_complete_run.as_str())
            .unwrap_or("")
            .to_string()
        };
        if loaded_run != selected_run {
            self.refresh_if_changed()?;
        }
        let refreshed_run = {
            let guard = self
                .cache
                .read()
                .map_err(|_| anyhow::anyhow!("snapshot cache poisoned"))?;
            match model {
                WeatherModel::EcmwfIfs025 => guard.identity.ecmwf_ready.as_ref(),
                WeatherModel::EcmwfIfs9km => guard.identity.ecmwf_ifs9km_ready.as_ref(),
                WeatherModel::Gfs => unreachable!("ECMWF route selector cannot select GFS"),
            }
            .map(|identity| identity.latest_complete_run.as_str())
            .unwrap_or("")
            .to_string()
        };
        if refreshed_run != selected_run {
            anyhow::bail!(
                "selected ECMWF route run is not loaded: selected={} loaded={}",
                selected_run,
                refreshed_run
            );
        }
        Ok(model)
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
                    "batch_id": identity.batch_id,
                    "batch_ready_sha256": identity.batch_ready_sha256,
                    "public_start_utc": identity.public_start_utc,
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
            "ecmwf_ifs9km": group(guard.identity.ecmwf_ifs9km_ready.as_ref()),
            "ecmwf_route": self.ecmwf_route.identity(),
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
            WeatherModel::EcmwfIfs9km => guard.identity.ecmwf_ifs9km_ready.as_ref(),
        }
        .context("weather OM group marker is unavailable")?;
        if identity.status != "complete" || identity.latest_complete_run.is_empty() {
            anyhow::bail!("weather OM group marker is not complete");
        }
        Ok((guard.snapshot.clone(), identity.latest_complete_run.clone()))
    }

    fn grid_snapshot(
        &self,
        model: GridModel,
    ) -> Result<(Arc<OmDataSnapshot>, GridReleaseIdentity)> {
        let guard = self
            .cache
            .read()
            .map_err(|_| anyhow::anyhow!("snapshot cache poisoned"))?;
        let identity = match model {
            GridModel::Gfs => guard.identity.gfs_ready.as_ref(),
            GridModel::EcmwfIfs025 => guard.identity.ecmwf_ready.as_ref(),
            GridModel::EcmwfIfs9km => guard.identity.ecmwf_ifs9km_ready.as_ref(),
            GridModel::Cams => guard.identity.cams_ready.as_ref(),
        }
        .context("requested grid group marker is unavailable")?;
        if identity.status != "complete"
            || identity.runtime_format != "openmeteo-native-v1"
            || identity.latest_complete_run.is_empty()
            || identity.coverage_id.is_empty()
        {
            anyhow::bail!("requested grid group is not a complete native release");
        }
        Ok((
            guard.snapshot.clone(),
            GridReleaseIdentity {
                model_run: identity.latest_complete_run.clone(),
                coverage_id: identity.coverage_id.clone(),
            },
        ))
    }

    fn authorize_internal_grid(&self, headers: &HeaderMap) -> Result<()> {
        authorize_internal_grid_token_file(self.internal_grid_token_file.as_deref(), headers)
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
        .route("/v1/ecmwf/route-state", get(ecmwf_route_state))
        .route(
            "/v1/ecmwf-ifs9km",
            get(ecmwf_ifs9km_forecast).post(ecmwf_ifs9km_forecast_post),
        )
        .route("/v1/ecmwf-ifs9km/catalog", get(ecmwf_ifs9km_catalog))
        .route("/v1/cams", get(cams_forecast))
        .route("/v1/stargazing/details", get(stargazing_details))
        .route("/v1/internal/grid", get(internal_grid))
        .route("/v1/internal/grid/catalog", get(internal_grid_catalog))
        .route("/v1/route", post(route))
        .route("/v1/ecmwf/route", post(ecmwf_route))
        .route("/v1/ecmwf-ifs9km/route", post(ecmwf_ifs9km_route))
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

async fn stargazing_details(
    State(state): State<AppState>,
    Query(query): Query<StargazingDetailsQuery>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let root = state.stargazing_root.clone();
    let details = tokio::task::spawn_blocking(move || load_details(&root, &query))
        .await
        .context("stargazing details worker failed")??;
    Ok(Json(serde_json::to_value(details)?))
}

async fn ecmwf_route_state(State(state): State<AppState>) -> Json<serde_json::Value> {
    Json(state.ecmwf_route.identity())
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
            "ecmwf_ifs9km": {
                "provider": "European Centre for Medium-Range Weather Forecasts (ECMWF)",
                "provider_url": "https://www.ecmwf.int/",
                "dataset": "ECMWF IFS 9 km deterministic forecast open data",
                "license": "CC-BY-4.0",
                "license_url": ECMWF_LICENSE_URL,
                "terms_url": "https://apps.ecmwf.int/datasets/licences/general/",
                "attribution": "This service is based on data and products of the European Centre for Medium-Range Weather Forecasts (ECMWF). Contains modified ECMWF data.",
                "modified": true,
                "transformations": [
                    "HTTP range extraction and spatial subsetting",
                    "materialization into immutable Open-Meteo OM arrays",
                    "native O1280 reduced-Gaussian cell preservation and nearest-cell sampling",
                    "temporal interpolation where requested",
                    "unit conversion and derived-variable calculation where requested",
                    "regular-grid resampling and lossless WebP encoding for map layers"
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
    let model = state.selected_ecmwf_model()?;
    query.models = Some(model_name(model).to_string());
    let (snapshot, model_run) = state.weather_snapshot(model)?;
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
    let model = state.selected_ecmwf_model()?;
    query.models = Some(model_name(model).to_string());
    let (snapshot, model_run) = state.weather_snapshot(model)?;
    let decoder = state.decoder.clone();
    let mut payload = tokio::task::spawn_blocking(move || {
        forecast_for_query(&snapshot, decoder.as_ref(), &query)
    })
    .await
    .context("ECMWF POST forecast worker failed")??;
    attach_model_run(&mut payload, &model_run)?;
    Ok(Json(payload))
}

async fn ecmwf_ifs9km_forecast(
    State(state): State<AppState>,
    Query(mut query): Query<PointQuery>,
) -> Result<Json<serde_json::Value>, ApiError> {
    validate_explicit_variables(&query)?;
    query.models = Some("ecmwf_ifs9km".to_string());
    let (snapshot, model_run) = state.weather_snapshot(WeatherModel::EcmwfIfs9km)?;
    let decoder = state.decoder.clone();
    let mut payload = tokio::task::spawn_blocking(move || {
        forecast_for_query(&snapshot, decoder.as_ref(), &query)
    })
    .await
    .context("ECMWF IFS 9 km forecast worker failed")??;
    attach_model_run(&mut payload, &model_run)?;
    Ok(Json(payload))
}

async fn ecmwf_ifs9km_forecast_post(
    State(state): State<AppState>,
    Json(mut query): Json<PointQuery>,
) -> Result<Json<serde_json::Value>, ApiError> {
    validate_explicit_variables(&query)?;
    query.models = Some("ecmwf_ifs9km".to_string());
    let (snapshot, model_run) = state.weather_snapshot(WeatherModel::EcmwfIfs9km)?;
    let decoder = state.decoder.clone();
    let mut payload = tokio::task::spawn_blocking(move || {
        forecast_for_query(&snapshot, decoder.as_ref(), &query)
    })
    .await
    .context("ECMWF IFS 9 km POST forecast worker failed")??;
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
    match state.selected_ecmwf_model()? {
        WeatherModel::EcmwfIfs9km => ecmwf_ifs9km_catalog(State(state)).await,
        WeatherModel::EcmwfIfs025 => ecmwf_ifs025_catalog(&state),
        WeatherModel::Gfs => unreachable!("ECMWF route selector cannot select GFS"),
    }
}

fn ecmwf_ifs025_catalog(state: &AppState) -> Result<Json<serde_json::Value>, ApiError> {
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

async fn ecmwf_ifs9km_catalog(
    State(state): State<AppState>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let snapshot = state.snapshot()?;
    let hourly = ecmwf_ifs9km_public_hourly_variables();
    let daily = ecmwf_ifs9km_public_daily_variables();
    let available_variables = hourly
        .iter()
        .map(String::as_str)
        .chain(daily.iter().map(String::as_str))
        .collect::<std::collections::BTreeSet<_>>();
    if let Some(product) = snapshot.product("ecmwf_ifs9km") {
        let time_start = product
            .entries
            .keys()
            .map(|key| key.valid_time_utc)
            .min()
            .context("EC9 native product has no first valid time")?;
        let time_end = product
            .entries
            .keys()
            .map(|key| key.valid_time_utc)
            .max()
            .context("EC9 native product has no final valid time")?;
        let available_source_variables = product
            .entries
            .keys()
            .map(|key| key.variable.as_str())
            .collect::<std::collections::BTreeSet<_>>();
        let grid = product
            .entries
            .values()
            .find_map(|entry| entry.native_grid.as_ref())
            .context("EC9 native product has no grid metadata")?;
        let grid_payload = if grid.grid_type.as_deref() == Some("o1280_reduced_gaussian_crop") {
            json!({
                "name": "O1280 reduced Gaussian regional compact grid",
                "nominal_resolution_km": 9,
                "bounds": {
                    "west": 70.0,
                    "east": 140.0,
                    "south": 0.0,
                    "north": 58.0,
                },
                "storage_locations": grid.nx * grid.ny,
                "storage_row_count": grid.o1280_column_counts.as_ref().map(Vec::len),
            })
        } else {
            json!({
                "name": "ECMWF IFS 9 km regular materialized grid (legacy)",
                "nominal_resolution_km": 9,
                "bounds": {
                    "west": grid.lon_min,
                    "east": grid.lon_min + (grid.nx - 1) as f64 * grid.dx,
                    "south": grid.lat_min,
                    "north": grid.lat_min + (grid.ny - 1) as f64 * grid.dy,
                },
                "nx": grid.nx,
                "ny": grid.ny,
                "dx": grid.dx,
                "dy": grid.dy,
            })
        };
        return Ok(Json(json!({
            "model": "ecmwf_ifs9km",
            "runtime_format": "openmeteo-native-v1",
            "coverage_id": product.manifest.coverage_id,
            "latest_complete_run": product.manifest.latest_complete_run,
            "public_start_utc": product.manifest.public_start_utc,
            "time_start_utc": time_start,
            "time_end_utc": time_end,
            "grid": grid_payload,
            "available_source_variables": available_source_variables,
            "available_hourly_variables": hourly,
            "available_daily_variables": daily,
            "available_variables": available_variables,
            "attribution": weather_attribution_payload()["data_sources"]["ecmwf_ifs9km"].clone(),
        })));
    }
    let product = snapshot
        .ecmwf_ifs9km()
        .context("ECMWF IFS 9 km snapshot is unavailable")?;
    let (time_start, time_end) = product.time_bounds()?;
    let bounds = product.bounds();
    Ok(Json(json!({
        "model": "ecmwf_ifs9km",
        "runtime_format": "weather-region-pack-v1",
        "batch_id": product.batch_id(),
        "latest_complete_run": product.latest_complete_run(),
        "public_start_utc": product.public_start_utc(),
        "time_start_utc": time_start,
        "time_end_utc": time_end,
        "grid": {
            "name": "O1280 reduced Gaussian",
            "nominal_resolution_km": 9,
            "bounds": {
                "west": bounds.west,
                "east": bounds.east,
                "south": bounds.south,
                "north": bounds.north,
            }
        },
        "available_source_variables": product.available_variables(),
        "available_hourly_variables": hourly,
        "available_daily_variables": daily,
        "available_variables": available_variables,
        "attribution": weather_attribution_payload()["data_sources"]["ecmwf_ifs9km"].clone(),
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

#[derive(Debug, Deserialize)]
struct InternalGridCatalogQuery {
    model: String,
}

#[derive(Debug, Deserialize)]
struct InternalGridQuery {
    model: String,
    variable: String,
    valid_time: String,
    #[serde(default)]
    expected_run: Option<String>,
    #[serde(default)]
    west: Option<f64>,
    #[serde(default)]
    east: Option<f64>,
    #[serde(default)]
    south: Option<f64>,
    #[serde(default)]
    north: Option<f64>,
}

impl InternalGridQuery {
    fn requested_bounds(&self) -> Result<Option<RequestedBounds>> {
        match (self.west, self.east, self.south, self.north) {
            (None, None, None, None) => Ok(None),
            (Some(west), Some(east), Some(south), Some(north)) => Ok(Some(
                RequestedBounds {
                    west,
                    east,
                    south,
                    north,
                }
                .validate()?,
            )),
            _ => anyhow::bail!("west, east, south and north must be supplied together"),
        }
    }

    fn valid_time(&self) -> Result<chrono::DateTime<chrono::Utc>> {
        chrono::DateTime::parse_from_rfc3339(&self.valid_time)
            .map(|value| value.with_timezone(&chrono::Utc))
            .with_context(|| "valid_time must be RFC3339, for example 2026-09-10T03:00:00Z")
    }
}

fn internal_grid_error(status: StatusCode, message: impl Into<String>) -> Response {
    (
        status,
        Json(json!({
            "error": message.into(),
        })),
    )
        .into_response()
}

async fn internal_grid_catalog(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<InternalGridCatalogQuery>,
) -> Response {
    if let Err(error) = state.authorize_internal_grid(&headers) {
        let status = if error.to_string() == "internal grid authorization failed" {
            StatusCode::UNAUTHORIZED
        } else {
            StatusCode::SERVICE_UNAVAILABLE
        };
        return internal_grid_error(status, error.to_string());
    }
    let model = match GridModel::parse(&query.model) {
        Ok(model) => model,
        Err(error) => return internal_grid_error(StatusCode::BAD_REQUEST, error.to_string()),
    };
    let (snapshot, release) = match state.grid_snapshot(model) {
        Ok(context) => context,
        Err(error) => {
            return internal_grid_error(StatusCode::SERVICE_UNAVAILABLE, error.to_string())
        }
    };
    match grid_catalog_payload(&snapshot, model, &release) {
        Ok(payload) => Json(payload).into_response(),
        Err(error) => internal_grid_error(StatusCode::UNPROCESSABLE_ENTITY, error.to_string()),
    }
}

async fn internal_grid(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<InternalGridQuery>,
) -> Response {
    if let Err(error) = state.authorize_internal_grid(&headers) {
        let status = if error.to_string() == "internal grid authorization failed" {
            StatusCode::UNAUTHORIZED
        } else {
            StatusCode::SERVICE_UNAVAILABLE
        };
        return internal_grid_error(status, error.to_string());
    }
    let model = match GridModel::parse(&query.model) {
        Ok(model) => model,
        Err(error) => return internal_grid_error(StatusCode::BAD_REQUEST, error.to_string()),
    };
    let valid_time = match query.valid_time() {
        Ok(value) => value,
        Err(error) => return internal_grid_error(StatusCode::BAD_REQUEST, error.to_string()),
    };
    let requested_bounds = match query.requested_bounds() {
        Ok(bounds) => bounds,
        Err(error) => return internal_grid_error(StatusCode::BAD_REQUEST, error.to_string()),
    };
    let (snapshot, release) = match state.grid_snapshot(model) {
        Ok(context) => context,
        Err(error) => {
            return internal_grid_error(StatusCode::SERVICE_UNAVAILABLE, error.to_string())
        }
    };
    if query
        .expected_run
        .as_deref()
        .is_some_and(|expected| expected != release.model_run)
    {
        return internal_grid_error(
            StatusCode::CONFLICT,
            format!(
                "model run changed: expected {}, current {}",
                query.expected_run.as_deref().unwrap_or_default(),
                release.model_run
            ),
        );
    }
    let Some(decoder) = state.decoder.clone() else {
        return internal_grid_error(
            StatusCode::SERVICE_UNAVAILABLE,
            "official OM decoder is unavailable",
        );
    };
    let permit = match tokio::time::timeout(
        state.internal_grid_queue_timeout,
        state.internal_grid_permits.clone().acquire_owned(),
    )
    .await
    {
        Ok(Ok(permit)) => permit,
        Ok(Err(_)) => {
            return internal_grid_error(
                StatusCode::SERVICE_UNAVAILABLE,
                "internal grid worker is shutting down",
            )
        }
        Err(_) => {
            return internal_grid_error(
                StatusCode::TOO_MANY_REQUESTS,
                "internal grid worker is busy; retry later",
            )
        }
    };
    let variable = query.variable.clone();
    let release_for_worker = release.clone();
    let generated = tokio::task::spawn_blocking(move || {
        let _permit = permit;
        encode_grid_file(
            &snapshot,
            &decoder,
            model,
            release_for_worker,
            &variable,
            valid_time,
            requested_bounds,
        )
    })
    .await;
    let encoded = match generated {
        Ok(Ok(encoded)) => encoded,
        Ok(Err(error)) => {
            return internal_grid_error(StatusCode::UNPROCESSABLE_ENTITY, error.to_string())
        }
        Err(error) => {
            return internal_grid_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("internal grid worker failed: {error}"),
            )
        }
    };
    let filename = format!(
        "{}_{}_{}_{}.wgrid",
        model.name(),
        query.variable,
        valid_time.format("%Y%m%d%H"),
        release.model_run
    );
    let mut response = Response::new(encoded.bytes.into());
    *response.status_mut() = StatusCode::OK;
    let response_headers = response.headers_mut();
    response_headers.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/vnd.weather-grid-v1"),
    );
    response_headers.insert(
        header::CONTENT_DISPOSITION,
        HeaderValue::from_str(&format!("attachment; filename=\"{filename}\""))
            .expect("validated model and variable produce a valid filename"),
    );
    response_headers.insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static("private, no-store"),
    );
    response_headers.insert(
        HeaderName::from_static("x-weather-model-run"),
        HeaderValue::from_str(&release.model_run).expect("validated run is a header value"),
    );
    response_headers.insert(
        HeaderName::from_static("x-weather-coverage-id"),
        HeaderValue::from_str(&release.coverage_id).expect("validated coverage is a header value"),
    );
    response_headers.insert(
        HeaderName::from_static("x-weather-grid-payload-sha256"),
        HeaderValue::from_str(&encoded.payload_sha256).expect("SHA-256 is a header value"),
    );
    response_headers.insert(
        HeaderName::from_static("x-weather-grid-points"),
        HeaderValue::from_str(&encoded.point_count.to_string())
            .expect("point count is a header value"),
    );
    response
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
    let model = state.selected_ecmwf_model()?;
    query.models = Some(model_name(model).to_string());
    let snapshot = state.snapshot()?;
    let decoder = state.decoder.clone();
    let payload =
        tokio::task::spawn_blocking(move || route_forecast(&snapshot, decoder.as_ref(), &query))
            .await
            .context("ECMWF route worker failed")??;
    Ok(Json(serde_json::to_value(payload)?))
}

async fn ecmwf_ifs9km_route(
    State(state): State<AppState>,
    Json(mut query): Json<RouteQuery>,
) -> Result<Json<serde_json::Value>, ApiError> {
    query.models = Some("ecmwf_ifs9km".to_string());
    let snapshot = state.snapshot()?;
    let decoder = state.decoder.clone();
    let payload =
        tokio::task::spawn_blocking(move || route_forecast(&snapshot, decoder.as_ref(), &query))
            .await
            .context("ECMWF IFS 9 km route worker failed")??;
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

    #[test]
    fn internal_grid_token_is_required_and_can_rotate_without_restart() {
        let root = TempDir::new().unwrap();
        let token_path = root.path().join("internal-grid-token");
        let first = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
        let second = "abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789";
        fs::write(&token_path, format!("{first}\n")).unwrap();

        let mut headers = HeaderMap::new();
        assert!(authorize_internal_grid_token_file(Some(&token_path), &headers).is_err());
        headers.insert(
            header::AUTHORIZATION,
            HeaderValue::from_str(&format!("Bearer {first}")).unwrap(),
        );
        authorize_internal_grid_token_file(Some(&token_path), &headers).unwrap();

        fs::write(&token_path, format!("{second}\n")).unwrap();
        assert!(authorize_internal_grid_token_file(Some(&token_path), &headers).is_err());
        headers.insert(
            header::AUTHORIZATION,
            HeaderValue::from_str(&format!("Bearer {second}")).unwrap(),
        );
        authorize_internal_grid_token_file(Some(&token_path), &headers).unwrap();
    }

    #[test]
    fn internal_grid_query_requires_a_complete_exact_window() {
        let query = InternalGridQuery {
            model: "ec9".to_string(),
            variable: "temperature_2m".to_string(),
            valid_time: "2026-09-10T03:00:00Z".to_string(),
            expected_run: None,
            west: Some(100.0),
            east: None,
            south: None,
            north: None,
        };
        assert!(query.requested_bounds().is_err());
        assert_eq!(query.valid_time().unwrap().timestamp(), 1_789_009_200);
    }

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
