use anyhow::{bail, Context, Result};
use serde_json::{json, Value};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};

use crate::query::WeatherModel;

const ROUTE_STATE_SCHEMA_VERSION: u64 = 1;

#[derive(Clone)]
pub(crate) struct EcmwfRouteSelector {
    state_path: Option<PathBuf>,
    last_good: Arc<RwLock<WeatherModel>>,
}

impl EcmwfRouteSelector {
    pub(crate) fn new(state_path: Option<PathBuf>) -> Self {
        let initial = state_path
            .as_deref()
            .and_then(|path| read_route_state(path).ok().map(|(model, _)| model))
            .unwrap_or(WeatherModel::EcmwfIfs025);
        Self {
            state_path,
            last_good: Arc::new(RwLock::new(initial)),
        }
    }

    pub(crate) fn selection(&self) -> (WeatherModel, Option<String>) {
        let Some(path) = self.state_path.as_deref() else {
            return (WeatherModel::EcmwfIfs025, None);
        };
        match read_route_state(path) {
            Ok((model, value)) => {
                if let Ok(mut last_good) = self.last_good.write() {
                    *last_good = model;
                }
                let selected_run = value
                    .get("selected_run")
                    .and_then(Value::as_str)
                    .map(str::to_string);
                (model, selected_run)
            }
            Err(error) => {
                let fallback = self
                    .last_good
                    .read()
                    .map(|model| *model)
                    .unwrap_or(WeatherModel::EcmwfIfs025);
                tracing::warn!(
                    route_state = %path.display(),
                    error = %error,
                    fallback = model_name(fallback),
                    "ECMWF route state is unavailable; retaining last known selection"
                );
                (fallback, None)
            }
        }
    }

    pub(crate) fn identity(&self) -> Value {
        let Some(path) = self.state_path.as_deref() else {
            return json!({
                "configured": false,
                "effective_selected_model": "ecmwf_ifs025",
            });
        };
        match read_route_state(path) {
            Ok((model, mut value)) => {
                if let Ok(mut last_good) = self.last_good.write() {
                    *last_good = model;
                }
                if let Some(object) = value.as_object_mut() {
                    object.insert("configured".to_string(), Value::Bool(true));
                    object.insert(
                        "effective_selected_model".to_string(),
                        Value::String(model_name(model).to_string()),
                    );
                }
                value
            }
            Err(error) => {
                let fallback = self
                    .last_good
                    .read()
                    .map(|model| *model)
                    .unwrap_or(WeatherModel::EcmwfIfs025);
                json!({
                    "configured": true,
                    "effective_selected_model": model_name(fallback),
                    "state_error": error.to_string(),
                })
            }
        }
    }
}

pub(crate) fn model_name(model: WeatherModel) -> &'static str {
    match model {
        WeatherModel::Gfs => "gfs",
        WeatherModel::EcmwfIfs025 => "ecmwf_ifs025",
        WeatherModel::EcmwfIfs9km => "ecmwf_ifs9km",
    }
}

fn read_route_state(path: &Path) -> Result<(WeatherModel, Value)> {
    let bytes =
        fs::read(path).with_context(|| format!("read ECMWF route state {}", path.display()))?;
    let value: Value = serde_json::from_slice(&bytes)
        .with_context(|| format!("parse ECMWF route state {}", path.display()))?;
    let schema_version = value
        .get("schema_version")
        .and_then(Value::as_u64)
        .context("ECMWF route state has no schema_version")?;
    if schema_version != ROUTE_STATE_SCHEMA_VERSION {
        bail!("unsupported ECMWF route state schema version: {schema_version}");
    }
    let model = match value
        .get("selected_model")
        .and_then(Value::as_str)
        .context("ECMWF route state has no selected_model")?
    {
        "ecmwf_ifs025" => WeatherModel::EcmwfIfs025,
        "ecmwf_ifs9km" => WeatherModel::EcmwfIfs9km,
        other => bail!("unsupported ECMWF route selection: {other}"),
    };
    Ok((model, value))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_state(path: &Path, model: &str) {
        fs::write(
            path,
            serde_json::to_vec(&json!({
                "schema_version": 1,
                "selected_model": model,
            }))
            .unwrap(),
        )
        .unwrap();
    }

    #[test]
    fn no_runtime_state_preserves_the_legacy_ec25_default() {
        let selector = EcmwfRouteSelector::new(None);
        assert_eq!(selector.selection().0, WeatherModel::EcmwfIfs025);
    }

    #[test]
    fn runtime_state_changes_are_visible_without_rebuilding_app_state() {
        let root = tempfile::tempdir().unwrap();
        let state = root.path().join("route.json");
        write_state(&state, "ecmwf_ifs9km");
        let selector = EcmwfRouteSelector::new(Some(state.clone()));
        assert_eq!(selector.selection().0, WeatherModel::EcmwfIfs9km);

        let replacement = root.path().join("replacement.json");
        write_state(&replacement, "ecmwf_ifs025");
        fs::rename(replacement, state).unwrap();
        assert_eq!(selector.selection().0, WeatherModel::EcmwfIfs025);
    }

    #[test]
    fn invalid_update_retains_the_last_known_good_selection() {
        let root = tempfile::tempdir().unwrap();
        let state = root.path().join("route.json");
        write_state(&state, "ecmwf_ifs9km");
        let selector = EcmwfRouteSelector::new(Some(state.clone()));
        assert_eq!(selector.selection().0, WeatherModel::EcmwfIfs9km);

        fs::write(&state, b"not-json").unwrap();
        assert_eq!(selector.selection().0, WeatherModel::EcmwfIfs9km);
        assert_eq!(
            selector.identity()["effective_selected_model"],
            "ecmwf_ifs9km"
        );
    }
}
