//! [`S3Blobs`]: attachment ciphertext on an S3-compatible object store
//! (ADR-0062 §7), chosen by `[blobs] backend = "s3"`.
//!
//! ```text
//! blobs/<account_h>/pending/up_<upload>/<idx>
//! blobs/<account_h>/pending/up_<upload>/opened
//! blobs/<account_h>/committed/<blob>/<idx>
//! blobs/<account_h>/committed/manifests/<blob>
//! probes/<random>
//! ```
//!
//! Every name is lowercase hex of bytes the relay derived, as on the
//! filesystem, so per-account isolation is a property of the key. Each chunk
//! is one object of at most 1 MiB, so no multipart upload is ever started and
//! none can be left half-finished; a `PUT` answers only once the store has
//! confirmed it, so the relay never acknowledges a chunk the store has not.
//! The manifest is written last and removed first, which is the two-phase
//! commit [`super`] states.
//!
//! Requests are path-style (`<endpoint>/<bucket>/<key>`), signed with AWS
//! Signature Version 4 ([`sign`]) over the `hyper` client the relay already
//! carries. The secret is read from `[blobs] s3_credentials_file`, never from
//! the environment ([`S3Credentials::from_file`]).

mod credentials;
mod keys;
mod sign;
#[cfg(test)]
mod tests;

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use bytes::Bytes;
use http_body_util::{BodyExt as _, Full, Limited};
use hyper::{Method, StatusCode};
use sha2::{Digest as _, Sha256};

pub use credentials::{S3Credentials, S3OpenError};
use keys::{
    account_prefix, chunk_prefix, committed_key, manifest_key, manifest_text, opened_key,
    parse_listing, parse_manifest, parse_stale_name, pending_key, stale_name, tag, unescape,
    upload_prefix, Object, Parsed, BLOBS, OPENED,
};

use super::{
    AccountKey, AccountTree, Area, BackupTotals, BlobBackend, BlobError, BlobUsage,
    CommittedManifest, DiskSpace, FsBlobs, Manifest, StaleUpload,
};
use crate::config::{BlobBackendKind, BlobsConfig, BlobsConfigError};

/// How long one request may take, connect to last byte. A chunk is at most
/// 1 MiB; a store slower than this is unavailable as far as a client is
/// concerned, and the relay answers `503` rather than hold the request.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

/// The largest response body read: a chunk is at most 1 MiB, and a listing
/// page of 1000 keys is a few hundred KiB.
const MAX_RESPONSE_BYTES: usize = 4 * 1024 * 1024;

type Connector = hyper_rustls::HttpsConnector<hyper_util::client::legacy::connect::HttpConnector>;
type HttpClient = hyper_util::client::legacy::Client<Connector, Full<Bytes>>;

/// The blob tree in one bucket of an S3-compatible object store.
#[derive(Clone)]
pub struct S3Blobs {
    client: HttpClient,
    /// `scheme://authority`, no trailing slash.
    endpoint: String,
    /// The `Host` header, signed.
    host: String,
    bucket: String,
    region: String,
    credentials: S3Credentials,
    /// Shared by every clone, so the maintenance pass on any handle reuses
    /// what an earlier one read.
    read_manifests: Arc<Mutex<ReadManifests>>,
}

/// The manifests [`BlobBackend::committed_manifests`] has already read, so a
/// pass reads only the ones written or rewritten since the last, rather than
/// one `GET` per committed blob in the bucket. An entry stands while the
/// listing shows its object under the same entity tag, which changes on any
/// rewrite; one the listing no longer shows is dropped, so there is at most one
/// entry per committed blob.
#[derive(Debug, Default)]
struct ReadManifests(BTreeMap<(AccountKey, [u8; 16]), (String, Manifest)>);

/// One listed manifest: its account, its blob, and its entity tag.
type ListedManifest = (AccountKey, [u8; 16], String);

impl ReadManifests {
    /// `listed` split into the manifests already read and the ones still to
    /// read, after dropping every entry `listed` does not hold. An object the
    /// listing gave no entity tag is always read again.
    fn split(
        &mut self,
        listed: Vec<ListedManifest>,
    ) -> (Vec<CommittedManifest>, Vec<ListedManifest>) {
        let present: BTreeSet<_> = listed.iter().map(|(a, b, _)| (*a, *b)).collect();
        self.0.retain(|key, _| present.contains(key));
        let mut known = Vec::new();
        let mut unread = Vec::new();
        for (account, blob, etag) in listed {
            match self.0.get(&(account, blob)) {
                Some((seen, manifest)) if !etag.is_empty() && *seen == etag => {
                    known.push(CommittedManifest {
                        account,
                        blob,
                        manifest: *manifest,
                    });
                }
                _ => unread.push((account, blob, etag)),
            }
        }
        (known, unread)
    }

    /// Remember `manifest`, read from the object the listing tagged `etag`.
    fn record(&mut self, account: AccountKey, blob: [u8; 16], etag: String, manifest: Manifest) {
        if !etag.is_empty() {
            self.0.insert((account, blob), (etag, manifest));
        }
    }
}

impl std::fmt::Debug for S3Blobs {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("S3Blobs")
            .field("endpoint", &self.endpoint)
            .field("bucket", &self.bucket)
            .field("region", &self.region)
            .finish_non_exhaustive()
    }
}

impl S3Blobs {
    /// The store `[blobs]` names, its credentials read from
    /// `s3_credentials_file`. Opens no connection: the first request does,
    /// and readiness probes it.
    ///
    /// # Errors
    /// Each [`S3OpenError`].
    pub fn from_config(cfg: &BlobsConfig) -> Result<Self, S3OpenError> {
        cfg.validate()?;
        let missing = BlobsConfigError::Missing;
        let file = cfg
            .s3_credentials_file
            .as_deref()
            .ok_or(missing("s3_credentials_file"))?;
        let credentials = S3Credentials::from_file(file)?;
        Self::new(
            cfg.s3_endpoint.as_deref().ok_or(missing("s3_endpoint"))?,
            cfg.s3_bucket.as_deref().ok_or(missing("s3_bucket"))?,
            cfg.s3_region.as_deref().ok_or(missing("s3_region"))?,
            credentials,
            cfg.s3_ca_file.as_deref(),
        )
    }

    /// The bucket `bucket` at `endpoint`, signed for `region`. Over
    /// `https://` the store's certificate is verified against the PEM
    /// certificates in `ca_file` alone where one is named, else against the
    /// public web roots.
    ///
    /// # Errors
    /// [`S3OpenError::Config`] for an endpoint or bucket
    /// [`BlobsConfig::validate`] refuses, and [`S3OpenError::CaFile`] for a
    /// CA file that cannot be read or holds no certificate.
    pub fn new(
        endpoint: &str,
        bucket: &str,
        region: &str,
        credentials: S3Credentials,
        ca_file: Option<&Path>,
    ) -> Result<Self, S3OpenError> {
        BlobsConfig {
            backend: BlobBackendKind::S3,
            s3_endpoint: Some(endpoint.to_owned()),
            s3_bucket: Some(bucket.to_owned()),
            s3_region: Some(region.to_owned()),
            s3_credentials_file: Some(PathBuf::new()),
            s3_ca_file: ca_file.map(Path::to_owned),
        }
        .validate()?;
        let endpoint = endpoint.trim_end_matches('/').to_owned();
        let host = endpoint
            .split_once("://")
            .map(|(_, authority)| authority.to_owned())
            .unwrap_or_default();
        let roots = hyper_rustls::HttpsConnectorBuilder::new();
        let https = match ca_file {
            Some(ca) => roots.with_tls_config(
                crate::store::postgres::client_config_from_ca_file(ca, "[blobs] s3_ca_file")
                    .map_err(S3OpenError::CaFile)?,
            ),
            None => roots.with_webpki_roots(),
        }
        .https_or_http()
        .enable_http1()
        .build();
        let client =
            hyper_util::client::legacy::Client::builder(hyper_util::rt::TokioExecutor::new())
                .build(https);
        Ok(Self {
            client,
            endpoint,
            host,
            bucket: bucket.to_owned(),
            region: region.trim().to_owned(),
            credentials,
            read_manifests: Arc::default(),
        })
    }

    /// The manifests already read. Never held across a request; a panic
    /// while it was held leaves entries that are each still whole.
    fn read_manifests(&self) -> std::sync::MutexGuard<'_, ReadManifests> {
        self.read_manifests
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// One signed request. `key` is `None` for the bucket itself.
    async fn send(
        &self,
        method: Method,
        key: Option<&str>,
        query: &[(&str, &str)],
        body: Bytes,
    ) -> Result<(StatusCode, Bytes), BlobError> {
        let mut path = format!("/{}", sign::uri_encode(&self.bucket, false));
        if let Some(key) = key {
            path.push('/');
            path.push_str(&sign::uri_encode(key, true));
        }
        let query = sign::canonical_query(query);
        let payload_hash = hex::encode(Sha256::digest(&body));
        let amz_date = jiff::Timestamp::now()
            .strftime("%Y%m%dT%H%M%SZ")
            .to_string();
        let headers = [
            ("host", self.host.as_str()),
            ("x-amz-content-sha256", payload_hash.as_str()),
            ("x-amz-date", amz_date.as_str()),
        ];
        let authorization = sign::authorization(
            &self.credentials,
            &self.region,
            &sign::Signed {
                method: method.as_str(),
                path: &path,
                query: &query,
                headers: &headers,
                payload_hash: &payload_hash,
                amz_date: &amz_date,
            },
        );
        let uri = if query.is_empty() {
            format!("{}{path}", self.endpoint)
        } else {
            format!("{}{path}?{query}", self.endpoint)
        };
        let mut builder = hyper::Request::builder().method(method.clone()).uri(&uri);
        for (name, value) in headers {
            builder = builder.header(name, value);
        }
        let request = builder
            .header(hyper::header::AUTHORIZATION, authorization)
            .body(Full::new(body))
            .map_err(|e| unavailable(&method, key, e))?;
        let exchange = async {
            let response = self
                .client
                .request(request)
                .await
                .map_err(|e| e.to_string())?;
            let status = response.status();
            let body = Limited::new(response.into_body(), MAX_RESPONSE_BYTES)
                .collect()
                .await
                .map_err(|e| e.to_string())?
                .to_bytes();
            Ok::<_, String>((status, body))
        };
        match tokio::time::timeout(REQUEST_TIMEOUT, exchange).await {
            Ok(Ok(answer)) => Ok(answer),
            Ok(Err(e)) => Err(unavailable(&method, key, e)),
            Err(_) => Err(unavailable(&method, key, "timed out")),
        }
    }

    /// Store `bytes` under `key`, replacing any earlier object whole.
    async fn put(&self, key: &str, bytes: &[u8]) -> Result<(), BlobError> {
        let (status, body) = self
            .send(Method::PUT, Some(key), &[], Bytes::copy_from_slice(bytes))
            .await?;
        if status.is_success() {
            Ok(())
        } else {
            Err(refused(&Method::PUT, key, status, &body))
        }
    }

    /// The object under `key`, or `None` if there is none.
    async fn get(&self, key: &str) -> Result<Option<Bytes>, BlobError> {
        let (status, body) = self.send(Method::GET, Some(key), &[], Bytes::new()).await?;
        match status {
            s if s.is_success() => Ok(Some(body)),
            StatusCode::NOT_FOUND => Ok(None),
            s => Err(refused(&Method::GET, key, s, &body)),
        }
    }

    /// Whether an object exists under `key`.
    async fn exists(&self, key: &str) -> Result<bool, BlobError> {
        let (status, body) = self
            .send(Method::HEAD, Some(key), &[], Bytes::new())
            .await?;
        match status {
            s if s.is_success() => Ok(true),
            StatusCode::NOT_FOUND => Ok(false),
            s => Err(refused(&Method::HEAD, key, s, &body)),
        }
    }

    /// Remove the object under `key`. Absent is not an error.
    async fn delete(&self, key: &str) -> Result<(), BlobError> {
        let (status, body) = self
            .send(Method::DELETE, Some(key), &[], Bytes::new())
            .await?;
        if status.is_success() || status == StatusCode::NOT_FOUND {
            Ok(())
        } else {
            Err(refused(&Method::DELETE, key, status, &body))
        }
    }

    /// Every object under `prefix`, every page of `ListObjectsV2`.
    async fn list(&self, prefix: &str) -> Result<Vec<Object>, BlobError> {
        let mut out = Vec::new();
        let mut token: Option<String> = None;
        loop {
            let mut query = vec![("list-type", "2"), ("prefix", prefix)];
            if let Some(t) = token.as_deref() {
                query.push(("continuation-token", t));
            }
            let (status, body) = self.send(Method::GET, None, &query, Bytes::new()).await?;
            if !status.is_success() {
                return Err(refused(&Method::GET, prefix, status, &body));
            }
            let page = std::str::from_utf8(&body)
                .map_err(|_| BlobError::Unavailable("S3 listing is not UTF-8".to_owned()))?;
            let (objects, next) = parse_listing(page)?;
            out.extend(objects);
            match next {
                Some(next) => token = Some(next),
                None => return Ok(out),
            }
        }
    }

    /// Remove every object under `prefix`, the ones `first` picks before the
    /// rest: manifests before chunks, a marker before what it guards.
    async fn delete_prefix(
        &self,
        prefix: &str,
        first: impl Fn(&str) -> bool,
    ) -> Result<(), BlobError> {
        let (head, tail): (Vec<Object>, Vec<Object>) = self
            .list(prefix)
            .await?
            .into_iter()
            .partition(|o| first(&o.key));
        for object in head.iter().chain(&tail) {
            self.delete(&object.key).await?;
        }
        Ok(())
    }

    /// Create the bucket: the test suites' fresh store.
    #[cfg(test)]
    async fn create_bucket(&self) -> Result<(), BlobError> {
        let (status, body) = self.send(Method::PUT, None, &[], Bytes::new()).await?;
        if status.is_success() {
            Ok(())
        } else {
            Err(refused(&Method::PUT, "", status, &body))
        }
    }
}

fn unavailable(method: &Method, key: Option<&str>, cause: impl std::fmt::Display) -> BlobError {
    BlobError::Unavailable(format!(
        "S3 {method} {}: {cause}",
        key.unwrap_or("(bucket)")
    ))
}

/// A status the store answered that the call cannot use, with the store's
/// own error code where its body names one.
fn refused(method: &Method, key: &str, status: StatusCode, body: &[u8]) -> BlobError {
    let code = std::str::from_utf8(body)
        .ok()
        .and_then(|xml| tag(xml, "Code"))
        .map(unescape)
        .unwrap_or_default();
    BlobError::Unavailable(
        format!("S3 {method} {key}: {status} {code}")
            .trim_end()
            .to_owned(),
    )
}

#[async_trait::async_trait]
impl BlobBackend for S3Blobs {
    async fn open_upload(
        &self,
        account: AccountKey,
        upload: [u8; 16],
        opened_at_ms: u64,
    ) -> Result<(), BlobError> {
        self.put(
            &opened_key(account, upload),
            opened_at_ms.to_string().as_bytes(),
        )
        .await
    }

    /// A marker that is absent or does not parse is no reading rather than an
    /// error, as on the filesystem.
    async fn upload_opened_at(
        &self,
        account: AccountKey,
        upload: [u8; 16],
    ) -> Result<Option<u64>, BlobError> {
        Ok(self
            .get(&opened_key(account, upload))
            .await?
            .and_then(|raw| std::str::from_utf8(&raw).ok()?.trim().parse().ok()))
    }

    /// The marker is checked before the write: `open_upload` is the only
    /// thing that makes an upload. A discard racing the check can leave a
    /// chunk behind with no marker, which is swept like any abandoned upload.
    async fn put_pending(
        &self,
        account: AccountKey,
        upload: [u8; 16],
        idx: u32,
        bytes: &[u8],
    ) -> Result<(), BlobError> {
        if !self.exists(&opened_key(account, upload)).await? {
            return Err(BlobError::UploadNotOpen);
        }
        self.put(&pending_key(account, upload, idx), bytes).await
    }

    async fn get_pending(
        &self,
        account: AccountKey,
        upload: [u8; 16],
        idx: u32,
    ) -> Result<Option<Vec<u8>>, BlobError> {
        Ok(self
            .get(&pending_key(account, upload, idx))
            .await?
            .map(Vec::from))
    }

    /// The marker first, so a chunk arriving during the discard is refused
    /// rather than left behind.
    async fn discard_upload(&self, account: AccountKey, upload: [u8; 16]) -> Result<(), BlobError> {
        self.delete_prefix(&upload_prefix(account, upload), |key| key.ends_with(OPENED))
            .await
    }

    /// A plain `PUT` of the bytes finalize re-read and re-hashed, rather than
    /// a server-side copy of the pending key: a copy would commit whatever
    /// that key holds when the copy runs, which a re-sent chunk can have
    /// replaced after the hash was checked.
    async fn put_committed(
        &self,
        account: AccountKey,
        blob: [u8; 16],
        idx: u32,
        bytes: &[u8],
    ) -> Result<(), BlobError> {
        self.put(&committed_key(account, blob, idx), bytes).await
    }

    async fn write_manifest(
        &self,
        account: AccountKey,
        blob: [u8; 16],
        manifest: Manifest,
    ) -> Result<(), BlobError> {
        self.put(
            &manifest_key(account, blob),
            manifest_text(manifest).as_bytes(),
        )
        .await
    }

    async fn manifest(
        &self,
        account: AccountKey,
        blob: [u8; 16],
    ) -> Result<Option<Manifest>, BlobError> {
        self.get(&manifest_key(account, blob))
            .await?
            .map(|raw| parse_manifest(&raw))
            .transpose()
    }

    async fn has_all(
        &self,
        account: AccountKey,
        blob: [u8; 16],
        chunk_count: u32,
    ) -> Result<bool, BlobError> {
        let prefix = chunk_prefix(account, blob);
        let present: BTreeSet<u32> = self
            .list(&prefix)
            .await?
            .iter()
            .filter_map(|o| o.key.strip_prefix(&prefix)?.parse().ok())
            .collect();
        Ok((0..chunk_count).all(|idx| present.contains(&idx)))
    }

    async fn get_committed(
        &self,
        account: AccountKey,
        blob: [u8; 16],
        idx: u32,
    ) -> Result<Option<Vec<u8>>, BlobError> {
        Ok(self
            .get(&committed_key(account, blob, idx))
            .await?
            .map(Vec::from))
    }

    async fn delete_committed(&self, account: AccountKey, blob: [u8; 16]) -> Result<(), BlobError> {
        self.delete(&manifest_key(account, blob)).await?;
        self.delete_prefix(&chunk_prefix(account, blob), |_| false)
            .await
    }

    async fn erase_account(&self, account: AccountKey) -> Result<(), BlobError> {
        for area in [Area::Pending, Area::Committed] {
            self.remove_account_tree(AccountTree { area, account })
                .await?;
        }
        Ok(())
    }

    /// One listing of `blobs/`, each upload's age its newest object's.
    async fn stale_uploads(&self, now_ms: u64, ttl_ms: u64) -> Result<Vec<StaleUpload>, BlobError> {
        let mut newest: BTreeMap<(AccountKey, [u8; 16]), u64> = BTreeMap::new();
        for object in self.list(BLOBS).await? {
            if let Some(Parsed::Pending { account, upload }) = Parsed::of(&object.key) {
                let at = newest.entry((account, upload)).or_default();
                *at = (*at).max(object.last_modified_ms);
            }
        }
        Ok(newest
            .into_iter()
            .filter(|(_, at)| now_ms.saturating_sub(*at) > ttl_ms)
            .map(|((account, upload), _)| StaleUpload(stale_name(account, upload)))
            .collect())
    }

    /// Only ever a name `stale_uploads` produced; anything else is refused,
    /// so a forged one cannot reach another prefix.
    async fn remove_upload(&self, upload: &StaleUpload) -> Result<(), BlobError> {
        let (account, id) = parse_stale_name(&upload.0).ok_or_else(|| {
            BlobError::Unavailable(format!("{} is not a pending upload", upload.0))
        })?;
        self.discard_upload(account, id).await
    }

    async fn stale_account_trees(
        &self,
        now_ms: u64,
        ttl_ms: u64,
    ) -> Result<Vec<AccountTree>, BlobError> {
        let mut newest: BTreeMap<(bool, AccountKey), u64> = BTreeMap::new();
        for object in self.list(BLOBS).await? {
            if let Some(parsed) = Parsed::of(&object.key) {
                let committed = parsed.area() == Area::Committed;
                let at = newest.entry((committed, parsed.account())).or_default();
                *at = (*at).max(object.last_modified_ms);
            }
        }
        Ok(newest
            .into_iter()
            .filter(|(_, at)| now_ms.saturating_sub(*at) > ttl_ms)
            .map(|((committed, account), _)| AccountTree {
                area: if committed {
                    Area::Committed
                } else {
                    Area::Pending
                },
                account,
            })
            .collect())
    }

    async fn remove_account_tree(&self, tree: AccountTree) -> Result<(), BlobError> {
        self.delete_prefix(&account_prefix(tree.area, tree.account), |key| {
            matches!(Parsed::of(key), Some(Parsed::Manifest { .. })) || key.ends_with(OPENED)
        })
        .await
    }

    /// A manifest that does not parse is left out, as on the filesystem.
    ///
    /// One listing, then a `GET` only for each manifest [`ReadManifests`]
    /// has not already read under the same entity tag: the first pass after
    /// a start reads every manifest, and each later one only those committed
    /// since.
    async fn committed_manifests(&self) -> Result<Vec<CommittedManifest>, BlobError> {
        let listed = self
            .list(BLOBS)
            .await?
            .into_iter()
            .filter_map(|object| match Parsed::of(&object.key) {
                Some(Parsed::Manifest { account, blob }) => Some((account, blob, object.etag)),
                _ => None,
            })
            .collect();
        let (mut out, unread) = self.read_manifests().split(listed);
        for (account, blob, etag) in unread {
            if let Ok(Some(manifest)) = self.manifest(account, blob).await {
                self.read_manifests().record(account, blob, etag, manifest);
                out.push(CommittedManifest {
                    account,
                    blob,
                    manifest,
                });
            }
        }
        Ok(out)
    }

    async fn usage(&self) -> Result<BlobUsage, BlobError> {
        let mut usage = BlobUsage::default();
        let mut uploads = BTreeSet::new();
        for object in self.list(BLOBS).await? {
            match Parsed::of(&object.key) {
                Some(Parsed::Pending { account, upload }) => {
                    uploads.insert((account, upload));
                }
                Some(Parsed::Chunk { .. } | Parsed::Manifest { .. }) => {
                    usage.blob_files += 1;
                    usage.blob_bytes += object.size;
                }
                None => {}
            }
        }
        usage.pending_uploads = uploads.len() as u64;
        Ok(usage)
    }

    /// A key of its own per call, random, so concurrent probes from any
    /// number of nodes never remove each other's object, and outside
    /// `blobs/`, so no sweep reads it.
    async fn probe(&self) -> Result<(), BlobError> {
        let mut nonce = [0u8; 16];
        getrandom::getrandom(&mut nonce).map_err(|e| BlobError::Unavailable(e.to_string()))?;
        let key = format!("probes/ready-{}", hex::encode(nonce));
        self.put(&key, b"ok").await?;
        let read = self.get(&key).await;
        let removed = self.delete(&key).await;
        match read? {
            Some(body) if &body[..] == b"ok" => removed,
            _ => Err(BlobError::Unavailable(format!(
                "S3 GET {key}: the probe did not read back what was written"
            ))),
        }
    }

    /// An object store has no filesystem for the relay to fill.
    async fn disk_space(&self) -> Result<Option<DiskSpace>, BlobError> {
        Ok(None)
    }

    /// The committed blobs, into the filesystem layout [`FsBlobs`] reads, so
    /// a backup restores onto either backend.
    ///
    /// One listing is taken first; each listed chunk is copied, then each
    /// listed manifest only if it still exists. Every removal takes a
    /// manifest before its chunks, so a blob removed during the copy is left
    /// without one.
    async fn backup_to(&self, dest: &Path) -> Result<BackupTotals, BlobError> {
        let tree = FsBlobs::new(dest);
        let listed: Vec<Parsed> = self
            .list(BLOBS)
            .await?
            .iter()
            .filter_map(|o| Parsed::of(&o.key))
            .collect();
        let mut totals = BackupTotals::default();
        for parsed in &listed {
            if let Parsed::Chunk { account, blob, idx } = *parsed {
                if let Some(bytes) = self.get(&committed_key(account, blob, idx)).await? {
                    tree.put_committed(account, blob, idx, &bytes).await?;
                    totals.files += 1;
                    totals.bytes += bytes.len() as u64;
                }
            }
        }
        for parsed in &listed {
            if let Parsed::Manifest { account, blob } = *parsed {
                if let Some(manifest) = self.manifest(account, blob).await? {
                    tree.write_manifest(account, blob, manifest).await?;
                    totals.files += 1;
                    totals.bytes += manifest_text(manifest).len() as u64;
                }
            }
        }
        Ok(totals)
    }

    fn location(&self) -> String {
        format!("s3://{} at {}", self.bucket, self.endpoint)
    }
}
