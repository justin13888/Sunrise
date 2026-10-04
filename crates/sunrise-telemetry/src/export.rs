//! The OTLP exporter: protobuf over HTTP, on the server's own TLS stack.
//!
//! `opentelemetry-otlp`'s bundled HTTP clients are `reqwest` 0.13, a second
//! major line beside the workspace's 0.12, and its gRPC transport is `tonic`,
//! a second HTTP/2 and TLS stack. Neither is needed: the exporter takes any
//! [`HttpClient`], and the server already links `hyper` with `hyper-rustls` for
//! its JWKS fetch and its APNs provider. [`HyperExport`] is that client.
//!
//! The batch span processor exports from a thread of its own, outside any
//! `tokio` runtime, and drives the export future with a plain executor. A
//! `hyper` request needs a runtime's reactor, so the request is spawned onto
//! the server's runtime and its join handle awaited, which works from any
//! executor.

use std::time::Duration;

use bytes::Bytes;
use http_body_util::{BodyExt as _, Full, Limited};
use opentelemetry::KeyValue;
use opentelemetry_http::{HttpClient, HttpError};
use opentelemetry_otlp::{Protocol, WithExportConfig as _, WithHttpConfig as _};
use opentelemetry_sdk::trace::SdkTracerProvider;
use opentelemetry_sdk::Resource;

use crate::{CappedSampler, Telemetry, TelemetryError};

/// How long one export may take, request and response together.
pub const EXPORT_TIMEOUT: Duration = Duration::from_secs(10);

/// The most of a collector's response body that is read. OTLP answers with a
/// small status message; anything larger is not one.
const MAX_RESPONSE_BYTES: usize = 64 * 1024;

/// What the exporter needs to know, already validated by the server's config.
#[derive(Debug, Clone, PartialEq)]
pub struct ExportConfig {
    /// The collector's OTLP/HTTP traces URL, used verbatim — normally ending in
    /// `/v1/traces`.
    pub endpoint: String,
    /// The fraction of traces sampled, in `[0, 1]`. Also the cap on traces a
    /// client asks to have sampled; see [`CappedSampler`].
    pub sample_ratio: f64,
    /// `service.name`.
    pub service_name: String,
    /// `deployment.environment.name`, when the operator names one.
    pub deployment: Option<String>,
    /// `service.version`: the server's own version.
    pub version: String,
    /// `vcs.ref.head.revision`: the commit the server was built from.
    pub commit: String,
}

impl ExportConfig {
    /// The resource every exported span carries: the service, its version and
    /// commit, and the deployment. Nothing the SDK detects from the host or
    /// the environment is added, so a host name or a process id never leaves.
    #[must_use]
    pub fn resource(&self) -> Resource {
        let mut attributes = vec![
            KeyValue::new("service.version", self.version.clone()),
            KeyValue::new("vcs.ref.head.revision", self.commit.clone()),
        ];
        if let Some(deployment) = &self.deployment {
            attributes.push(KeyValue::new(
                "deployment.environment.name",
                deployment.clone(),
            ));
        }
        Resource::builder_empty()
            .with_service_name(self.service_name.clone())
            .with_attributes(attributes)
            .build()
    }
}

/// An OTLP/HTTP exporter over `cfg`, batching on its own thread and sending on
/// `runtime`.
///
/// # Errors
/// [`TelemetryError::Exporter`] when the endpoint does not parse.
pub(crate) fn otlp(
    cfg: &ExportConfig,
    runtime: tokio::runtime::Handle,
) -> Result<Telemetry, TelemetryError> {
    let exporter = opentelemetry_otlp::SpanExporter::builder()
        .with_http()
        .with_http_client(HyperExport::new(runtime, EXPORT_TIMEOUT))
        .with_protocol(Protocol::HttpBinary)
        .with_endpoint(cfg.endpoint.clone())
        .with_timeout(EXPORT_TIMEOUT)
        .build()
        .map_err(|e| TelemetryError::Exporter(e.to_string()))?;
    let provider = SdkTracerProvider::builder()
        .with_sampler(CappedSampler::new(cfg.sample_ratio))
        .with_resource(cfg.resource())
        .with_batch_exporter(exporter)
        .build();
    Ok(Telemetry::from_provider(provider))
}

type Client = hyper_util::client::legacy::Client<
    hyper_rustls::HttpsConnector<hyper_util::client::legacy::connect::HttpConnector>,
    Full<Bytes>,
>;

/// [`HttpClient`] over `hyper` and `rustls`, sending on a `tokio` runtime.
#[derive(Clone)]
pub struct HyperExport {
    client: Client,
    runtime: tokio::runtime::Handle,
    timeout: Duration,
}

impl std::fmt::Debug for HyperExport {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HyperExport")
            .field("timeout", &self.timeout)
            .finish_non_exhaustive()
    }
}

impl HyperExport {
    /// A client over the webpki (Mozilla) roots, speaking `http://` or
    /// `https://`, that gives up on an export after `timeout`.
    ///
    /// Plain HTTP is accepted because a collector is most often a sidecar on
    /// loopback. What crosses that hop is spans this crate has already
    /// redacted, so a plaintext collector link discloses the shape of the
    /// relay's traffic and nothing a request carried.
    #[must_use]
    pub fn new(runtime: tokio::runtime::Handle, timeout: Duration) -> Self {
        let https = hyper_rustls::HttpsConnectorBuilder::new()
            .with_webpki_roots()
            .https_or_http()
            .enable_http1()
            .build();
        let client =
            hyper_util::client::legacy::Client::builder(hyper_util::rt::TokioExecutor::new())
                .build(https);
        Self {
            client,
            runtime,
            timeout,
        }
    }
}

#[derive(Debug, thiserror::Error)]
#[error("the export did not complete within {0:?}")]
struct TimedOut(Duration);

#[async_trait::async_trait]
impl HttpClient for HyperExport {
    async fn send_bytes(
        &self,
        request: http::Request<Bytes>,
    ) -> Result<http::Response<Bytes>, HttpError> {
        let client = self.client.clone();
        let timeout = self.timeout;
        let send = async move {
            let (parts, body) = request.into_parts();
            let response = client
                .request(http::Request::from_parts(parts, Full::new(body)))
                .await?;
            let (parts, body) = response.into_parts();
            let body = Limited::new(body, MAX_RESPONSE_BYTES)
                .collect()
                .await?
                .to_bytes();
            Ok::<_, HttpError>(http::Response::from_parts(parts, body))
        };
        self.runtime
            .spawn(async move {
                tokio::time::timeout(timeout, send)
                    .await
                    .map_err(|_| -> HttpError { Box::new(TimedOut(timeout)) })?
            })
            .await
            .map_err(|e| -> HttpError { Box::new(e) })?
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config() -> ExportConfig {
        ExportConfig {
            endpoint: "http://127.0.0.1:4318/v1/traces".into(),
            sample_ratio: 0.01,
            service_name: "sunrise-relay".into(),
            deployment: Some("staging".into()),
            version: "0.1.0".into(),
            commit: "abc1234".into(),
        }
    }

    /// Exactly the four attributes the issue allows, and nothing the SDK would
    /// detect from the host.
    #[test]
    fn the_resource_carries_only_service_version_commit_and_deployment() {
        let resource = config().resource();
        let mut keys: Vec<String> = resource.iter().map(|(k, _)| k.to_string()).collect();
        keys.sort();
        assert_eq!(
            keys,
            [
                "deployment.environment.name",
                "service.name",
                "service.version",
                "vcs.ref.head.revision"
            ]
        );
    }

    #[test]
    fn an_unnamed_deployment_is_left_out() {
        let resource = ExportConfig {
            deployment: None,
            ..config()
        }
        .resource();
        assert!(resource
            .iter()
            .all(|(k, _)| k.as_str() != "deployment.environment.name"));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn an_exporter_builds_without_contacting_the_collector() {
        let telemetry = otlp(&config(), tokio::runtime::Handle::current()).expect("builds");
        assert!(telemetry.is_enabled());
        // Nothing was recorded, so the flush has nothing to send and returns
        // without a collector listening.
        tokio::task::spawn_blocking(move || telemetry.shutdown())
            .await
            .expect("joins")
            .expect("shuts down");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_collector_that_never_answers_times_out() {
        // Bound but never accepted from: the connection is made and then
        // nothing answers.
        let silent = std::net::TcpListener::bind("127.0.0.1:0").expect("binds");
        let addr = silent.local_addr().expect("has an address");
        let client = HyperExport::new(
            tokio::runtime::Handle::current(),
            Duration::from_millis(200),
        );
        let request = http::Request::post(format!("http://{addr}/v1/traces"))
            .body(Bytes::from_static(b""))
            .expect("a request");
        let err = client.send_bytes(request).await.expect_err("times out");
        assert!(err.to_string().contains("did not complete"), "{err}");
    }
}
