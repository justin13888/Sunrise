//! `sunrise-server healthcheck`: probe this server's own listener.
//!
//! The runtime image carries no `curl` or `wget`, and adding one for a
//! `HEALTHCHECK` would put an HTTP client into every deployment to serve one
//! line of a Dockerfile. The binary already knows where it listens, so it
//! probes that itself: one HTTP/1.1 `GET` over a plain socket, read to the
//! status line.
//!
//! It probes the address the config binds. An unspecified address (`0.0.0.0`,
//! `[::]`) is not something to connect to, so it becomes the loopback of the
//! same family, which a wildcard listener also answers on.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::time::Duration;
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

/// How long the whole probe may take: resolving, connecting, and reading the
/// status line. Under Docker's default 30 s `--timeout` with room to spare, and
/// longer than the deep check's own 5 s blob deadline.
pub const DEADLINE: Duration = Duration::from_secs(10);

/// The most of the response the probe reads looking for the status line.
const HEAD_LIMIT: usize = 1024;

/// Why a probe did not find a healthy server.
#[derive(Debug, thiserror::Error)]
pub enum ProbeError {
    /// The bind address did not resolve to anything.
    #[error("cannot resolve {bind}: {cause}")]
    Resolve {
        /// The configured bind address.
        bind: String,
        /// What resolution said.
        cause: String,
    },
    /// Nothing answered, or the exchange failed part way.
    #[error("cannot reach {addr}: {cause}")]
    Unreachable {
        /// The address probed.
        addr: SocketAddr,
        /// The I/O failure.
        cause: std::io::Error,
    },
    /// The probe ran out of time.
    #[error("no answer from {addr} within {:?}", DEADLINE)]
    TimedOut {
        /// The address probed.
        addr: SocketAddr,
    },
    /// The server answered, with something other than `200`.
    #[error("{addr} answered {status:?}")]
    Unhealthy {
        /// The address probed.
        addr: SocketAddr,
        /// The status line it sent, or what came back instead of one.
        status: String,
    },
}

/// Probe `bind`'s health route; `deep` asks for readiness rather than
/// liveness.
///
/// # Errors
/// [`ProbeError`] naming why the server is not healthy.
pub async fn probe(bind: &str, deep: bool) -> Result<(), ProbeError> {
    let addr = target(bind).await?;
    match tokio::time::timeout(DEADLINE, exchange(addr, deep)).await {
        Ok(result) => result,
        Err(_) => Err(ProbeError::TimedOut { addr }),
    }
}

/// The address to connect to for a listener bound at `bind`.
async fn target(bind: &str) -> Result<SocketAddr, ProbeError> {
    let resolve = |cause: String| ProbeError::Resolve {
        bind: bind.to_owned(),
        cause,
    };
    let addr = tokio::net::lookup_host(bind)
        .await
        .map_err(|e| resolve(e.to_string()))?
        .next()
        .ok_or_else(|| resolve("no addresses".to_owned()))?;
    Ok(connectable(addr))
}

/// `addr` with an unspecified IP replaced by the loopback of its family.
fn connectable(mut addr: SocketAddr) -> SocketAddr {
    if addr.ip().is_unspecified() {
        addr.set_ip(match addr.ip() {
            IpAddr::V4(_) => IpAddr::V4(Ipv4Addr::LOCALHOST),
            IpAddr::V6(_) => IpAddr::V6(Ipv6Addr::LOCALHOST),
        });
    }
    addr
}

async fn exchange(addr: SocketAddr, deep: bool) -> Result<(), ProbeError> {
    let unreachable = |cause| ProbeError::Unreachable { addr, cause };
    let mut socket = tokio::net::TcpStream::connect(addr)
        .await
        .map_err(unreachable)?;
    let path = if deep {
        "/api/v1/health?deep=1"
    } else {
        "/api/v1/health"
    };
    let request = format!("GET {path} HTTP/1.1\r\nHost: {addr}\r\nConnection: close\r\n\r\n");
    socket
        .write_all(request.as_bytes())
        .await
        .map_err(unreachable)?;

    let mut head = Vec::with_capacity(HEAD_LIMIT);
    let mut chunk = [0_u8; 256];
    while !head.windows(2).any(|w| w == b"\r\n") && head.len() < HEAD_LIMIT {
        let n = socket.read(&mut chunk).await.map_err(unreachable)?;
        if n == 0 {
            break;
        }
        head.extend_from_slice(&chunk[..n]);
    }
    let text = String::from_utf8_lossy(&head);
    let status = text.lines().next().unwrap_or_default();
    if is_ok_status_line(status) {
        Ok(())
    } else {
        Err(ProbeError::Unhealthy {
            addr,
            status: status.to_owned(),
        })
    }
}

/// Whether `line` is an HTTP/1.x status line with status `200`.
fn is_ok_status_line(line: &str) -> bool {
    let mut parts = line.split(' ');
    matches!(
        (parts.next(), parts.next()),
        (Some("HTTP/1.1" | "HTTP/1.0"), Some("200"))
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{ServerConfig, ServerState};

    #[test]
    fn a_wildcard_bind_is_probed_on_loopback() {
        let v4: SocketAddr = "0.0.0.0:8443".parse().unwrap();
        assert_eq!(connectable(v4), "127.0.0.1:8443".parse().unwrap());
        let v6: SocketAddr = "[::]:8443".parse().unwrap();
        assert_eq!(connectable(v6), "[::1]:8443".parse().unwrap());
        let exact: SocketAddr = "10.0.0.2:8443".parse().unwrap();
        assert_eq!(connectable(exact), exact);
    }

    #[test]
    fn only_a_200_status_line_is_healthy() {
        assert!(is_ok_status_line("HTTP/1.1 200 OK"));
        assert!(is_ok_status_line("HTTP/1.0 200"));
        assert!(!is_ok_status_line("HTTP/1.1 503 Service Unavailable"));
        assert!(!is_ok_status_line("HTTP/1.1 2000 OK"));
        assert!(!is_ok_status_line(""));
        assert!(!is_ok_status_line("SSH-2.0-OpenSSH"));
    }

    /// Against a real listener: liveness passes, readiness passes until the
    /// drain begins, and a port nothing listens on fails.
    #[tokio::test]
    async fn probes_a_running_server_and_sees_the_drain() {
        let dir = tempfile::tempdir().unwrap();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let bind = listener.local_addr().unwrap().to_string();
        let state = ServerState::new(ServerConfig {
            bind: bind.clone(),
            blob_root: Some(dir.path().join("blobs")),
            ..ServerConfig::default()
        });
        let drain = state.drain.clone();
        let server = tokio::spawn(crate::serve(state, listener));

        probe(&bind, false).await.expect("liveness");
        probe(&bind, true).await.expect("readiness");

        drain.begin();
        let err = probe(&bind, true).await.expect_err("draining is not ready");
        assert!(
            matches!(&err, ProbeError::Unhealthy { status, .. } if status.contains("503")),
            "{err}"
        );
        probe(&bind, false)
            .await
            .expect("a draining server is alive");
        server.abort();

        let closed = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let gone = closed.local_addr().unwrap().to_string();
        drop(closed);
        let err = probe(&gone, false)
            .await
            .expect_err("nothing listens there");
        assert!(matches!(err, ProbeError::Unreachable { .. }), "{err}");
    }
}
