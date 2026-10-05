//! `[observability]`: OpenTelemetry trace export.
//!
//! Its own module, like `limits`, because it is one table with its own value
//! type, defaults, refusals and tests, and none of the rest of the config reads
//! it. Pure, as `model` is: the exporter itself is built by the binary from
//! [`ServerConfig::trace_export`] once a runtime exists to send on.

use serde::{Deserialize, Serialize};

use super::ServerConfig;

/// `[observability]`: where traces go and how many.
///
/// Written at all, the table turns export on; there is no `enabled` key,
/// because a table that names a collector and does not send to it is a
/// configuration nobody reading it would expect. `endpoint` is the one key
/// without a default.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct ObservabilityConfig {
    /// The collector's OTLP/HTTP traces URL, used verbatim, so it normally
    /// ends in `/v1/traces`: `http://127.0.0.1:4318/v1/traces` for a local
    /// collector. `http://` or `https://`.
    pub endpoint: String,
    /// The fraction of traces sampled, from `0.0` to `1.0`. It is also the
    /// cap: a client's `traceparent` can ask for less and never for more.
    #[serde(default = "default_sample_ratio")]
    pub sample_ratio: f64,
    /// `service.name` on every exported span.
    #[serde(default = "default_service_name")]
    pub service_name: String,
    /// `deployment.environment.name` on every exported span, such as
    /// `"production"` or `"staging"`. Unset, the attribute is left off.
    #[serde(default)]
    pub deployment: Option<String>,
}

/// Why an `[observability]` table was refused at startup.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum ObservabilityError {
    /// An `endpoint` the exporter cannot send to.
    #[error(
        "[observability] endpoint {0:?} is not an http:// or https:// URL with a host \
         (e.g. http://127.0.0.1:4318/v1/traces)"
    )]
    BadEndpoint(String),
    /// A `sample_ratio` that is not a fraction.
    #[error("[observability] sample_ratio {0} is not between 0.0 and 1.0")]
    BadSampleRatio(String),
    /// An empty `service_name`.
    #[error("[observability] service_name is empty; leave it unset for \"sunrise-server\"")]
    EmptyServiceName,
}

/// 1%: what `docs/06-server/observability.md` §Tracing names for production.
/// A staging relay that wants every trace sets `1.0`.
const fn default_sample_ratio() -> f64 {
    0.01
}

fn default_service_name() -> String {
    "sunrise-server".into()
}

impl ObservabilityConfig {
    /// Refuse a table the exporter cannot be built from, or that would sample
    /// a fraction that is not one.
    ///
    /// # Errors
    /// The first [`ObservabilityError`] the table earns.
    pub fn validate(&self) -> Result<(), ObservabilityError> {
        let collector = self.endpoint.parse::<kynos::http::Uri>().is_ok_and(|uri| {
            matches!(uri.scheme_str(), Some("http" | "https")) && uri.authority().is_some()
        });
        if !collector {
            return Err(ObservabilityError::BadEndpoint(self.endpoint.clone()));
        }
        // `contains` is false for NaN, so a NaN ratio is refused here too.
        if !(0.0..=1.0).contains(&self.sample_ratio) {
            return Err(ObservabilityError::BadSampleRatio(
                self.sample_ratio.to_string(),
            ));
        }
        if self.service_name.trim().is_empty() {
            return Err(ObservabilityError::EmptyServiceName);
        }
        Ok(())
    }
}

impl ServerConfig {
    /// What the trace exporter is built from, or `None` without
    /// `[observability]`. `commit` is the build's, which the config cannot
    /// know.
    #[must_use]
    pub fn trace_export(&self, commit: &str) -> Option<sunrise_telemetry::ExportConfig> {
        self.observability
            .as_ref()
            .map(|obs| sunrise_telemetry::ExportConfig {
                endpoint: obs.endpoint.clone(),
                sample_ratio: obs.sample_ratio,
                service_name: obs.service_name.clone(),
                deployment: obs.deployment.clone(),
                version: self.server_app_v.clone(),
                commit: commit.to_owned(),
            })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{ConfigError, FileConfig};

    fn observability(endpoint: &str) -> ObservabilityConfig {
        ObservabilityConfig {
            endpoint: endpoint.into(),
            sample_ratio: default_sample_ratio(),
            service_name: default_service_name(),
            deployment: None,
        }
    }

    fn refusal(obs: ObservabilityConfig) -> Result<(), ConfigError> {
        ServerConfig {
            observability: Some(obs),
            ..ServerConfig::default()
        }
        .validate(true)
    }

    #[test]
    fn tracing_is_off_unless_the_table_is_written() {
        let c = ServerConfig::default();
        assert!(c.observability.is_none());
        assert!(c.trace_export("abc").is_none());
    }

    #[test]
    fn a_collector_endpoint_must_be_an_http_url_with_a_host() {
        for good in [
            "http://127.0.0.1:4318/v1/traces",
            "https://otel.example/v1/traces",
        ] {
            assert_eq!(refusal(observability(good)), Ok(()), "{good}");
        }
        for bad in ["127.0.0.1:4318", "grpc://collector:4317", "/v1/traces", ""] {
            assert_eq!(
                refusal(observability(bad)),
                Err(ConfigError::Observability(ObservabilityError::BadEndpoint(
                    bad.into()
                ))),
                "{bad}"
            );
        }
    }

    #[test]
    fn a_sample_ratio_outside_the_unit_interval_is_refused() {
        let at = |sample_ratio| ObservabilityConfig {
            sample_ratio,
            ..observability("http://127.0.0.1:4318/v1/traces")
        };
        for ratio in [0.0, 0.01, 1.0] {
            assert_eq!(refusal(at(ratio)), Ok(()), "{ratio}");
        }
        for ratio in [-0.1, 1.5, f64::NAN, f64::INFINITY] {
            assert!(
                matches!(
                    refusal(at(ratio)),
                    Err(ConfigError::Observability(
                        ObservabilityError::BadSampleRatio(_)
                    ))
                ),
                "{ratio}"
            );
        }
    }

    #[test]
    fn an_empty_service_name_is_refused() {
        let obs = ObservabilityConfig {
            service_name: "  ".into(),
            ..observability("http://127.0.0.1:4318/v1/traces")
        };
        assert_eq!(
            refusal(obs),
            Err(ConfigError::Observability(
                ObservabilityError::EmptyServiceName
            ))
        );
    }

    /// The exporter's resource takes the server's own version and the build's
    /// commit, and nothing else from the config but what the table names.
    #[test]
    fn the_trace_export_carries_the_tables_values_and_the_build_identity() {
        let c = ServerConfig {
            observability: Some(ObservabilityConfig {
                deployment: Some("staging".into()),
                sample_ratio: 1.0,
                ..observability("http://127.0.0.1:4318/v1/traces")
            }),
            ..ServerConfig::default()
        };
        let export = c.trace_export("abc1234").expect("configured");
        assert_eq!(export.endpoint, "http://127.0.0.1:4318/v1/traces");
        assert!((export.sample_ratio - 1.0).abs() < f64::EPSILON);
        assert_eq!(export.service_name, "sunrise-server");
        assert_eq!(export.deployment.as_deref(), Some("staging"));
        assert_eq!(export.version, env!("CARGO_PKG_VERSION"));
        assert_eq!(export.commit, "abc1234");
    }

    /// `[observability]` was one of the refused tables until tracing was
    /// built. Written, it needs only `endpoint`; a misspelled key is refused
    /// like every other table's.
    #[test]
    fn the_table_is_read_from_a_file_with_its_defaults() {
        let cfg = FileConfig::parse(
            "[observability]\nendpoint = \"http://127.0.0.1:4318/v1/traces\"",
            "t.toml",
        )
        .unwrap()
        .apply(ServerConfig::default());
        let obs = cfg.observability.as_ref().expect("the table is applied");
        assert_eq!(obs, &observability("http://127.0.0.1:4318/v1/traces"));
        assert!(cfg.validate(true).is_ok());

        let full = FileConfig::parse(
            "[observability]\nendpoint = \"https://otel.example/v1/traces\"\n\
             sample_ratio = 1.0\nservice_name = \"relay\"\ndeployment = \"staging\"",
            "t.toml",
        )
        .unwrap()
        .apply(ServerConfig::default());
        let obs = full.observability.as_ref().expect("applied");
        assert!((obs.sample_ratio - 1.0).abs() < f64::EPSILON);
        assert_eq!(obs.service_name, "relay");
        assert_eq!(obs.deployment.as_deref(), Some("staging"));

        let missing =
            FileConfig::parse("[observability]\nsample_ratio = 0.5", "t.toml").unwrap_err();
        assert!(missing.to_string().contains("endpoint"), "{missing}");
        let unknown = FileConfig::parse(
            "[observability]\nendpoint = \"http://c/v1/traces\"\nsampling = 0.5",
            "t.toml",
        )
        .unwrap_err();
        assert!(unknown.to_string().contains("sampling"), "{unknown}");
        assert!(
            FileConfig::parse("", "t.toml")
                .unwrap()
                .apply(ServerConfig::default())
                .observability
                .is_none(),
            "absent, there is no tracing"
        );
    }
}
