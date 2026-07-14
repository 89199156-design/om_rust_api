use crate::official::OfficialDecoder;
use crate::query::{forecast_for_query, route_forecast, PointQuery, RouteQuery};
use crate::snapshot::OmDataSnapshot;
use anyhow::{Context, Result};
use axum::extract::{Query, State};
use axum::http::StatusCode;
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
use tower_http::trace::TraceLayer;

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
    products: BTreeMap<String, ProductIdentity>,
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

        // Build without holding either cache lock. Client requests continue to
        // clone and use the previous immutable snapshot throughout this work.
        let snapshot = Arc::new(OmDataSnapshot::load(&self.data_root)?);
        let identity_after = SnapshotIdentity::read(&self.data_root)?;
        if identity_after != identity_before {
            // A producer published again while this snapshot was built. Keep
            // the old snapshot and retry the newest marker on the next tick.
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
                Ok(Err(error)) => {
                    tracing::error!(error = %error, "OM snapshot refresh failed; retaining previous snapshot")
                }
                Err(error) => {
                    tracing::error!(error = %error, "OM snapshot refresh worker failed; retaining previous snapshot")
                }
            }
        }
        Ok(())
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
    #[cfg(unix)]
    let refresh_task = {
        use tokio::signal::unix::{signal, SignalKind};
        let published = signal(SignalKind::hangup())?;
        tokio::spawn(state.clone().refresh_on_publish_signal(published))
    };
    let listener = tokio::net::TcpListener::bind(bind)
        .await
        .with_context(|| format!("failed to bind {}", bind))?;
    let result = axum::serve(listener, router(state)).await;
    #[cfg(unix)]
    refresh_task.abort();
    result?;
    Ok(())
}

async fn forecast(
    State(state): State<AppState>,
    Query(query): Query<PointQuery>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let snapshot = state.snapshot()?;
    let payload = forecast_for_query(&snapshot, state.decoder.as_ref(), &query, true)?;
    Ok(Json(payload))
}

async fn air_quality(
    State(state): State<AppState>,
    Query(query): Query<PointQuery>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let snapshot = state.snapshot()?;
    let payload = forecast_for_query(&snapshot, state.decoder.as_ref(), &query, false)?;
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

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::fs;
    use tempfile::TempDir;

    fn state(root: &Path) -> AppState {
        AppState::new(root.to_path_buf(), None).unwrap()
    }

    #[test]
    fn requests_always_clone_current_snapshot_without_periodic_reload() {
        let temp = TempDir::new().unwrap();
        let state = state(temp.path());
        let first = state.snapshot().unwrap();
        let second = state.snapshot().unwrap();

        assert!(Arc::ptr_eq(&first, &second));
    }

    #[test]
    fn failed_background_refresh_retains_previous_snapshot() {
        let temp = TempDir::new().unwrap();
        let state = state(temp.path());
        let old = state.snapshot().unwrap();
        let marker = temp
            .path()
            .join("groups/gfs/current/ready_for_processing.json");
        fs::create_dir_all(marker.parent().unwrap()).unwrap();
        fs::write(&marker, b"{\"runtime_format\":\"openmeteo-native-v1\"}").unwrap();

        assert!(state.refresh_if_changed().is_err());
        assert!(Arc::ptr_eq(&old, &state.snapshot().unwrap()));
    }

    #[test]
    fn unchanged_small_ready_markers_do_not_rebuild_snapshot() {
        let temp = TempDir::new().unwrap();
        let marker = temp
            .path()
            .join("groups/gfs/current/ready_for_processing.json");
        fs::create_dir_all(marker.parent().unwrap()).unwrap();
        let mut marker_payload = json!({
            "status": "building",
            "runtime_format": "openmeteo-native-v1",
            "group": "gfs",
            "coverage_id": "pending",
            "latest_complete_run": "2026071300",
            "source_runs": [],
            "public_start_utc": "2026-07-13T00:00:00Z",
            "coverage_path": "coverages/gfs/pending",
            "products": {},
            "generated_at": "2026-07-13T00:01:00Z"
        });
        fs::write(&marker, serde_json::to_vec(&marker_payload).unwrap()).unwrap();
        let state = state(temp.path());
        let old = state.snapshot().unwrap();

        marker_payload["generated_at"] = json!("2026-07-13T00:02:00Z");
        fs::write(&marker, serde_json::to_vec(&marker_payload).unwrap()).unwrap();
        assert!(!state.refresh_if_changed().unwrap());
        assert!(Arc::ptr_eq(&old, &state.snapshot().unwrap()));
    }
}
