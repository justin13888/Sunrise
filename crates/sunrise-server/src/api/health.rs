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
#[kynos::get("/api/v1/health")]
pub async fn health() -> Json<HealthResponse> {
    Json(HealthResponse {
        status: "ok".to_owned(),
    })
}
