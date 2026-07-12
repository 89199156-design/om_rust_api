use crate::official::OfficialDecoder;
use crate::query::{forecast_for_query, route_forecast, PointQuery, RouteQuery};
use crate::snapshot::OmDataSnapshot;
use anyhow::{Context, Result};
use axum::extract::{Query, State};
use axum::http::StatusCode;
use axum::routing::{get, post};
use axum::{Json, Router};
use serde_json::json;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};
use tower_http::trace::TraceLayer;

#[derive(Clone)]
pub struct AppState {
    data_root: PathBuf,
    decoder: Option<OfficialDecoder>,
    refresh_interval: Duration,
    cache: Arc<RwLock<SnapshotCache>>,
}

struct SnapshotCache {
    loaded_at: Instant,
    snapshot: Arc<OmDataSnapshot>,
}

impl AppState {
    pub fn new(
        data_root: PathBuf,
        decoder: Option<OfficialDecoder>,
        refresh_interval: Duration,
    ) -> Result<Self> {
        let snapshot = Arc::new(OmDataSnapshot::load(&data_root)?);
        Ok(Self {
            data_root,
            decoder,
            refresh_interval,
            cache: Arc::new(RwLock::new(SnapshotCache {
                loaded_at: Instant::now(),
                snapshot,
            })),
        })
    }

    fn snapshot(&self) -> Result<Arc<OmDataSnapshot>> {
        {
            let guard = self
                .cache
                .read()
                .map_err(|_| anyhow::anyhow!("snapshot cache poisoned"))?;
            if guard.loaded_at.elapsed() < self.refresh_interval {
                return Ok(guard.snapshot.clone());
            }
        }
        let snapshot = Arc::new(OmDataSnapshot::load(&self.data_root)?);
        let mut guard = self
            .cache
            .write()
            .map_err(|_| anyhow::anyhow!("snapshot cache poisoned"))?;
        guard.loaded_at = Instant::now();
        guard.snapshot = snapshot.clone();
        Ok(snapshot)
    }
}

pub fn router(state: AppState) -> Router {
    Router::new()
        .route("/v1/forecast", get(forecast))
        .route("/v1/air-quality", get(air_quality))
        .route("/v1/route", post(route))
        .with_state(state)
        .layer(TraceLayer::new_for_http())
}

pub async fn serve(state: AppState, bind: SocketAddr) -> Result<()> {
    let listener = tokio::net::TcpListener::bind(bind)
        .await
        .with_context(|| format!("failed to bind {}", bind))?;
    axum::serve(listener, router(state)).await?;
    Ok(())
}

async fn forecast(
    State(state): State<AppState>,
    Query(query): Query<PointQuery>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let snapshot = state.snapshot()?;
    let payload = forecast_for_query(&snapshot, state.decoder.as_ref(), &query)?;
    Ok(Json(payload))
}

async fn air_quality(
    State(state): State<AppState>,
    Query(query): Query<PointQuery>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let snapshot = state.snapshot()?;
    let payload = forecast_for_query(&snapshot, state.decoder.as_ref(), &query)?;
    Ok(Json(payload))
}

async fn route(
    State(state): State<AppState>,
    Json(query): Json<RouteQuery>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let snapshot = state.snapshot()?;
    let payload = route_forecast(&snapshot, state.decoder.as_ref(), &query)?;
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
