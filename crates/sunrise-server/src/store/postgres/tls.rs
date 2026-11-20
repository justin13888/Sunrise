//! TLS for Postgres connections, on the `rustls` the relay already links.
//!
//! `tokio-postgres` takes its TLS as a [`MakeTlsConnect`], and the published
//! adapters either link OpenSSL or add an X.509 parser this relay has no other
//! use for. What it needs is small: a connector built from one
//! `rustls::ClientConfig`, and a stream that is the `tokio-rustls` one. Channel
//! binding is not offered, so SCRAM authenticates without it, as `libpq` does
//! when the server's certificate is not channel-bindable.
//!
//! The trust roots are the operator's CA file and nothing else: a Postgres
//! server certificate is as often from a private CA as a public one, and
//! trusting the web's roots by default would accept a certificate for any
//! name a public CA vouches for.

use std::future::Future;
use std::io;
use std::path::Path;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};

use rustls::pki_types::pem::PemObject as _;
use rustls::pki_types::{CertificateDer, ServerName};
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio_postgres::tls::{ChannelBinding, MakeTlsConnect, TlsConnect, TlsStream};

/// Makes a verified TLS connection to each Postgres host it is asked for.
#[derive(Clone)]
pub(crate) struct MakeRustls(Arc<rustls::ClientConfig>);

impl std::fmt::Debug for MakeRustls {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MakeRustls").finish_non_exhaustive()
    }
}

impl MakeRustls {
    /// A connector trusting the PEM certificates in `ca_file` alone.
    ///
    /// # Errors
    /// The file cannot be read, holds no certificate, or holds one rustls
    /// will not take as a root.
    pub(crate) fn from_ca_file(ca_file: &Path) -> Result<Self, String> {
        client_config_from_ca_file(ca_file, "[storage] postgres_ca_file")
            .map(|config| Self(Arc::new(config)))
    }
}

/// A TLS client configuration trusting the PEM certificates in `ca_file`
/// alone; `key` is the config key that named it, for the error. Also the
/// object store's, for `[blobs] s3_ca_file`.
///
/// # Errors
/// The file cannot be read, holds no certificate, or holds one rustls will not
/// take as a root.
pub(crate) fn client_config_from_ca_file(
    ca_file: &Path,
    key: &str,
) -> Result<rustls::ClientConfig, String> {
    let shown = ca_file.display();
    let mut roots = rustls::RootCertStore::empty();
    let certs =
        CertificateDer::pem_file_iter(ca_file).map_err(|e| format!("{key} {shown}: {e}"))?;
    for cert in certs {
        let cert = cert.map_err(|e| format!("{key} {shown}: {e}"))?;
        roots.add(cert).map_err(|e| format!("{key} {shown}: {e}"))?;
    }
    if roots.is_empty() {
        return Err(format!("{key} {shown} holds no PEM certificate"));
    }
    Ok(rustls::ClientConfig::builder_with_provider(Arc::new(
        rustls::crypto::ring::default_provider(),
    ))
    .with_safe_default_protocol_versions()
    .map_err(|e| e.to_string())?
    .with_root_certificates(roots)
    .with_no_client_auth())
}

impl<S> MakeTlsConnect<S> for MakeRustls
where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    type Stream = RustlsStream<S>;
    type TlsConnect = RustlsConnect;
    type Error = io::Error;

    fn make_tls_connect(&mut self, domain: &str) -> io::Result<RustlsConnect> {
        let name = ServerName::try_from(domain.to_owned()).map_err(io::Error::other)?;
        Ok(RustlsConnect {
            config: Arc::clone(&self.0),
            name,
        })
    }
}

/// One TLS handshake, to one host.
pub(crate) struct RustlsConnect {
    config: Arc<rustls::ClientConfig>,
    name: ServerName<'static>,
}

impl<S> TlsConnect<S> for RustlsConnect
where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    type Stream = RustlsStream<S>;
    type Error = io::Error;
    type Future = Pin<Box<dyn Future<Output = io::Result<RustlsStream<S>>> + Send>>;

    fn connect(self, stream: S) -> Self::Future {
        Box::pin(async move {
            let tls = tokio_rustls::TlsConnector::from(self.config)
                .connect(self.name, stream)
                .await?;
            Ok(RustlsStream(tls))
        })
    }
}

/// The TLS stream a Postgres connection runs over.
pub(crate) struct RustlsStream<S>(tokio_rustls::client::TlsStream<S>);

impl<S: AsyncRead + AsyncWrite + Unpin> AsyncRead for RustlsStream<S> {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        Pin::new(&mut self.0).poll_read(cx, buf)
    }
}

impl<S: AsyncRead + AsyncWrite + Unpin> AsyncWrite for RustlsStream<S> {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        Pin::new(&mut self.0).poll_write(cx, buf)
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.0).poll_flush(cx)
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.0).poll_shutdown(cx)
    }
}

impl<S: AsyncRead + AsyncWrite + Unpin> TlsStream for RustlsStream<S> {
    fn channel_binding(&self) -> ChannelBinding {
        ChannelBinding::none()
    }
}

#[cfg(test)]
mod tests {
    use super::MakeRustls;

    /// A CA file that is missing, or holds no certificate, is refused by
    /// name rather than becoming a connector that trusts nothing.
    #[test]
    fn a_ca_file_without_a_certificate_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("absent.pem");
        assert!(MakeRustls::from_ca_file(&missing)
            .unwrap_err()
            .contains("postgres_ca_file"));
        let empty = dir.path().join("empty.pem");
        std::fs::write(&empty, "not a certificate\n").unwrap();
        assert!(MakeRustls::from_ca_file(&empty)
            .unwrap_err()
            .contains("holds no PEM certificate"));
    }
}
