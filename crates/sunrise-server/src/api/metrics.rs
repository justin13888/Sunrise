//! The operator's `/metrics` exposition, in the Prometheus text format.
//!
//! # Why it is mounted conditionally
//!
//! `docs/06-server/overview.md` says the operator surfaces are loopback only.
//! This route once sat at the router root with no auth layer and no bind check,
//! so on any non-loopback deployment it served the relay's counters — session
//! counts, per-account activity shape — to anyone who asked.
//!
//! Mounting it only when the listener is loopback is the narrowest reading of
//! the documented contract that is also enforceable without connect-info
//! plumbing. An operator who wants it remotely puts a proxy in front, which is
//! what the doc already tells them to do.
//!
//! Conditional mounting rather than a runtime check is deliberate: an operation
//! that is not mounted is absent from the description as well as from the
//! router, so the document does not advertise a surface this deployment
//! refuses to serve.

use crate::api::error::ApiError;
use crate::state::ServerState;
use kynos::di::inject::Inject;
use kynos::extract::body::text::Text;

/// The counter registry, in the Prometheus text exposition format.
///
/// Unauthenticated, and mounted only on a loopback listener — see the module
/// docs for why those two facts belong together.
#[kynos::get("/metrics", operation_id = "metrics")]
pub async fn metrics(Inject(state): Inject<ServerState>) -> Result<Text, ApiError> {
    Ok(Text(state.metrics.render()))
}

#[cfg(test)]
mod tests {
    use crate::api::testing::Client;
    use crate::ServerConfig;
    use kynos::http::{Method, StatusCode};

    /// Loopback serves it, unauthenticated, as the operator expects.
    #[tokio::test]
    async fn a_loopback_listener_serves_the_counters() {
        let client = Client::new(ServerConfig {
            bind: "127.0.0.1:8443".to_owned(),
            ..ServerConfig::default()
        });
        // Move a counter so the body is not trivially empty.
        client
            .send(Method::GET, "/api/v1/devices", None)
            .await
            .assert_status(StatusCode::OK);

        let res = client.send_as(Method::GET, "/metrics", None, None).await;
        res.assert_status(StatusCode::OK);
        assert!(
            String::from_utf8_lossy(&res.bytes).contains("sunrise_devices_list_total"),
            "the exposition must carry the counters"
        );
    }

    /// A non-loopback listener does not serve it at all.
    ///
    /// The counters describe session counts and per-account activity shape, and
    /// this route once sat at the router root with no auth and no bind check.
    #[tokio::test]
    async fn a_public_listener_does_not_mount_it() {
        let client = Client::new(ServerConfig {
            bind: "0.0.0.0:8443".to_owned(),
            // A public bind with the single-tenant verifier is refused by
            // `validate`, which this test does not call: the question here is
            // only which routes the router carries.
            ..ServerConfig::default()
        });

        client
            .send_as(Method::GET, "/metrics", None, None)
            .await
            .assert_status(StatusCode::NOT_FOUND);
    }
}
