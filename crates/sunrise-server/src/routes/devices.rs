//! Device CRUD + push token registration routes.

use crate::push::PushRegistration;
use crate::ServerState;
use axum::{
    extract::State,
    http::StatusCode,
    routing::{get, post},
    Json, Router,
};
use serde::{Deserialize, Serialize};

/// Mount device routes.
#[must_use]
pub fn router() -> Router<ServerState> {
    Router::new()
        .route("/devices", get(list))
        .route("/devices/push-tokens", post(register_push_token))
}

/// Device list row.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DeviceRow {
    /// Hex device id.
    pub device_id_hex: String,
    /// Nickname.
    pub nickname: String,
    /// Platform string.
    pub platform: String,
}

async fn list(State(state): State<ServerState>) -> (StatusCode, Json<Vec<DeviceRow>>) {
    state.metrics.incr("sunrise_devices_list_total");
    (StatusCode::OK, Json(Vec::new()))
}

async fn register_push_token(
    State(state): State<ServerState>,
    Json(reg): Json<PushRegistration>,
) -> Result<(StatusCode, Json<serde_json::Value>), (StatusCode, &'static str)> {
    if reg.device_id_hex.len() != 32 {
        return Err((StatusCode::BAD_REQUEST, "device_id_hex must be 32 chars"));
    }
    if reg.token.is_empty() {
        return Err((StatusCode::BAD_REQUEST, "token required"));
    }
    state.metrics.incr("sunrise_push_register_total");
    Ok((
        StatusCode::OK,
        Json(serde_json::json!({"registered": true})),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{build_router, ServerConfig};
    use axum::http::{Request, StatusCode};
    use tower::ServiceExt;

    #[tokio::test]
    async fn list_returns_empty_array() {
        let app = build_router(ServerState::new(ServerConfig::default()));
        let response = app
            .oneshot(
                Request::builder()
                    .method("GET")
                    .uri("/api/v1/devices")
                    .body(axum::body::Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn register_rejects_bad_device_id() {
        let app = build_router(ServerState::new(ServerConfig::default()));
        let body = serde_json::json!({
            "device_id_hex": "short",
            "platform": "fcm",
            "token": "abc",
        });
        let response = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/v1/devices/push-tokens")
                    .header("content-type", "application/json")
                    .body(axum::body::Body::from(body.to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    }
}
