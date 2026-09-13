//! `GET /api/v1/health` — liveness probe.

use kynos::extract::body::json::Json;
use serde::{Deserialize, Serialize};

/// Health response.
///
/// `Deserialize` is here for the generated client, not for the server: spargen
/// reads this shape out of the document and the client decodes into it.
#[derive(Debug, Clone, Serialize, Deserialize, kynos::Schema)]
pub struct HealthResponse {
    /// `"ok"` while the server is healthy.
    pub status: String,
}

/// Liveness probe.
///
/// Unauthenticated on purpose: a load balancer has no token, and this answers
/// "is the process up", not "is the database reachable". `docs/06-server/api.md`
/// specifies a `?deep=1` variant that has never existed and is still not here.
#[kynos::get("/api/v1/health", operation_id = "health")]
pub async fn health() -> Json<HealthResponse> {
    Json(HealthResponse {
        status: "ok".to_owned(),
    })
}

#[cfg(test)]
mod tests {
    use crate::api::testing::Client;
    use crate::state::ServerState;
    use crate::{ServerConfig, StaticVerifier, Subject};
    use kynos::http::{Method, StatusCode};
    use std::sync::Arc;

    /// Nothing asserted what this route returns, only that the router had one.
    ///
    /// The body is one member and a load balancer branches on it, so a handler
    /// that started answering `{"status":"degraded"}` — or `{}` — would have
    /// failed no test here.
    #[tokio::test]
    async fn health_answers_ok() {
        let client = Client::new(ServerConfig::default());
        let res = client.send(Method::GET, "/api/v1/health", None).await;
        res.assert_status(StatusCode::OK);
        assert_eq!(res.json()["status"], serde_json::json!("ok"));
    }

    /// Unauthenticated on purpose: a load balancer has no token.
    ///
    /// Against a verifier that actually checks, because the default
    /// `NullVerifier` accepts an absent bearer and so cannot tell an open route
    /// from a closed one.
    #[tokio::test]
    async fn health_needs_no_credential() {
        let state = ServerState::new(ServerConfig::default()).with_verifier(Arc::new(
            StaticVerifier::default().with("test", Subject::new("https://idp.example", "alice")),
        ));
        let client = Client::from_state(state);
        let res = client
            .send_as(Method::GET, "/api/v1/health", None, None)
            .await;
        res.assert_status(StatusCode::OK);
        assert_eq!(res.json()["status"], serde_json::json!("ok"));
    }
}
