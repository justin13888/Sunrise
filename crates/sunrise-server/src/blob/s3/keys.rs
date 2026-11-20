//! The object keys ADR-0062 §7 lays blobs out under, the parse that reads a
//! listing's keys back, and the two bodies the relay writes and reads as text:
//! a manifest, and a `ListObjectsV2` page.

use super::super::{AccountKey, Area, BlobError, Manifest};

/// Every key the relay writes for blobs is under this prefix.
pub(super) const BLOBS: &str = "blobs/";

/// The marker holding when `init` opened an upload, as on the filesystem.
pub(super) const OPENED: &str = "opened";

pub(super) fn account_prefix(area: Area, account: AccountKey) -> String {
    let area = match area {
        Area::Pending => "pending",
        Area::Committed => "committed",
    };
    format!("{BLOBS}{}/{area}/", hex::encode(account))
}

pub(super) fn upload_prefix(account: AccountKey, upload: [u8; 16]) -> String {
    format!(
        "{}up_{}/",
        account_prefix(Area::Pending, account),
        hex::encode(upload)
    )
}

pub(super) fn pending_key(account: AccountKey, upload: [u8; 16], idx: u32) -> String {
    format!("{}{idx}", upload_prefix(account, upload))
}

pub(super) fn opened_key(account: AccountKey, upload: [u8; 16]) -> String {
    format!("{}{OPENED}", upload_prefix(account, upload))
}

pub(super) fn chunk_prefix(account: AccountKey, blob: [u8; 16]) -> String {
    format!(
        "{}{}/",
        account_prefix(Area::Committed, account),
        hex::encode(blob)
    )
}

pub(super) fn committed_key(account: AccountKey, blob: [u8; 16], idx: u32) -> String {
    format!("{}{idx}", chunk_prefix(account, blob))
}

pub(super) fn manifest_key(account: AccountKey, blob: [u8; 16]) -> String {
    format!(
        "{}manifests/{}",
        account_prefix(Area::Committed, account),
        hex::encode(blob)
    )
}

/// 32 lowercase hex characters as 16 bytes: only a name this backend wrote.
fn hex16(s: &str) -> Option<[u8; 16]> {
    if s.len() != 32 || !s.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f')) {
        return None;
    }
    let mut out = [0u8; 16];
    hex::decode_to_slice(s, &mut out).ok()?;
    Some(out)
}

/// What a key under `blobs/` names, for the listings the maintenance pass and
/// `admin` read. Anything this backend could not have written is `None`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Parsed {
    /// A chunk or the marker of one pending upload.
    Pending {
        account: AccountKey,
        upload: [u8; 16],
    },
    /// One committed chunk.
    Chunk {
        account: AccountKey,
        blob: [u8; 16],
        idx: u32,
    },
    /// One manifest.
    Manifest { account: AccountKey, blob: [u8; 16] },
}

impl Parsed {
    pub(super) fn of(key: &str) -> Option<Self> {
        let rest = key.strip_prefix(BLOBS)?;
        let (account, rest) = rest.split_once('/')?;
        let account = hex16(account)?;
        let (area, rest) = rest.split_once('/')?;
        match area {
            "pending" => {
                let (upload, leaf) = rest.strip_prefix("up_")?.split_once('/')?;
                let upload = hex16(upload)?;
                (leaf == OPENED || leaf.parse::<u32>().is_ok())
                    .then_some(Self::Pending { account, upload })
            }
            "committed" => {
                let (dir, leaf) = rest.split_once('/')?;
                if dir == "manifests" {
                    return Some(Self::Manifest {
                        account,
                        blob: hex16(leaf)?,
                    });
                }
                Some(Self::Chunk {
                    account,
                    blob: hex16(dir)?,
                    idx: leaf.parse().ok()?,
                })
            }
            _ => None,
        }
    }

    pub(super) const fn account(self) -> AccountKey {
        match self {
            Self::Pending { account, .. }
            | Self::Chunk { account, .. }
            | Self::Manifest { account, .. } => account,
        }
    }

    pub(super) const fn area(self) -> Area {
        match self {
            Self::Pending { .. } => Area::Pending,
            Self::Chunk { .. } | Self::Manifest { .. } => Area::Committed,
        }
    }
}

/// The name `stale_uploads` hands out for one upload, and the only shape
/// `remove_upload` accepts back.
pub(super) fn stale_name(account: AccountKey, upload: [u8; 16]) -> String {
    upload_prefix(account, upload)
        .trim_end_matches('/')
        .to_owned()
}

pub(super) fn parse_stale_name(name: &str) -> Option<(AccountKey, [u8; 16])> {
    let rest = name.strip_prefix(BLOBS)?;
    let (account, rest) = rest.split_once('/')?;
    let upload = rest.strip_prefix("pending/up_")?;
    Some((hex16(account)?, hex16(upload)?))
}

/// `"<chunk_count> <size_bytes>"`, as on the filesystem, so a backup of
/// either reads the same.
pub(super) fn manifest_text(manifest: Manifest) -> String {
    format!("{} {}", manifest.chunk_count, manifest.size_bytes)
}

pub(super) fn parse_manifest(raw: &[u8]) -> Result<Manifest, BlobError> {
    let text = std::str::from_utf8(raw)
        .map_err(|_| BlobError::Corrupt("manifest is not text".to_owned()))?;
    let mut parts = text.split_whitespace();
    let mut field = |name: &str| {
        parts
            .next()
            .ok_or_else(|| BlobError::Corrupt(format!("no {name}")))
    };
    let chunk_count = field("chunk count")?
        .parse()
        .map_err(|_| BlobError::Corrupt("chunk count is not a number".to_owned()))?;
    let size_bytes = field("size")?
        .parse()
        .map_err(|_| BlobError::Corrupt("size is not a number".to_owned()))?;
    Ok(Manifest {
        chunk_count,
        size_bytes,
    })
}

/// One object a listing returned.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Object {
    pub(super) key: String,
    pub(super) last_modified_ms: u64,
    pub(super) size: u64,
}

/// One `ListObjectsV2` page: its objects, and the token for the next page
/// when it is truncated.
pub(super) fn parse_listing(xml: &str) -> Result<(Vec<Object>, Option<String>), BlobError> {
    let bad = |what: &str| BlobError::Unavailable(format!("S3 listing: {what}"));
    let mut objects = Vec::new();
    let mut rest = xml;
    while let Some(start) = rest.find("<Contents>") {
        let after = &rest[start + "<Contents>".len()..];
        let end = after
            .find("</Contents>")
            .ok_or_else(|| bad("unterminated <Contents>"))?;
        let entry = &after[..end];
        let key = tag(entry, "Key").ok_or_else(|| bad("an object with no <Key>"))?;
        let last_modified = tag(entry, "LastModified")
            .and_then(|t| t.parse::<jiff::Timestamp>().ok())
            .ok_or_else(|| bad("an object with no readable <LastModified>"))?;
        objects.push(Object {
            key: unescape(key),
            last_modified_ms: u64::try_from(last_modified.as_millisecond()).unwrap_or(0),
            size: tag(entry, "Size").and_then(|s| s.parse().ok()).unwrap_or(0),
        });
        rest = &after[end + "</Contents>".len()..];
    }
    let truncated = tag(xml, "IsTruncated") == Some("true");
    let next = if truncated {
        Some(
            tag(xml, "NextContinuationToken")
                .map(unescape)
                .ok_or_else(|| bad("truncated with no <NextContinuationToken>"))?,
        )
    } else {
        None
    };
    Ok((objects, next))
}

/// The text of the first `<name>…</name>` in `xml`.
pub(super) fn tag<'a>(xml: &'a str, name: &str) -> Option<&'a str> {
    let open = format!("<{name}>");
    let close = format!("</{name}>");
    let start = xml.find(&open)? + open.len();
    let len = xml[start..].find(&close)?;
    Some(&xml[start..start + len])
}

/// XML's five predefined entities. A key the relay wrote is hex and slashes;
/// a continuation token is the store's own, and may hold any of them.
pub(super) fn unescape(s: &str) -> String {
    s.replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&apos;", "'")
        .replace("&amp;", "&")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The layout ADR-0062 §7 fixes, and the parse that reads it back.
    #[test]
    fn keys_follow_the_adr_layout_and_parse_back() {
        let (a, u, b) = ([0xa1; 16], [0x01; 16], [0xbb; 16]);
        let ah = hex::encode(a);
        assert_eq!(
            pending_key(a, u, 3),
            format!("blobs/{ah}/pending/up_{}/3", hex::encode(u))
        );
        assert_eq!(
            committed_key(a, b, 0),
            format!("blobs/{ah}/committed/{}/0", hex::encode(b))
        );
        assert_eq!(
            manifest_key(a, b),
            format!("blobs/{ah}/committed/manifests/{}", hex::encode(b))
        );
        let pending = Some(Parsed::Pending {
            account: a,
            upload: u,
        });
        assert_eq!(Parsed::of(&pending_key(a, u, 3)), pending);
        assert_eq!(Parsed::of(&opened_key(a, u)), pending);
        assert_eq!(
            Parsed::of(&committed_key(a, b, 7)),
            Some(Parsed::Chunk {
                account: a,
                blob: b,
                idx: 7
            })
        );
        assert_eq!(
            Parsed::of(&manifest_key(a, b)),
            Some(Parsed::Manifest {
                account: a,
                blob: b
            })
        );
        for foreign in [
            "probes/ready-00",
            "blobs/AB/pending/up_00/0",
            &format!("blobs/{ah}/pending/up_{}/x", hex::encode(u)),
            &format!("blobs/{ah}/other/{}/0", hex::encode(b)),
        ] {
            assert_eq!(Parsed::of(foreign), None, "{foreign}");
        }
        assert_eq!(parse_stale_name(&stale_name(a, u)), Some((a, u)));
    }

    #[test]
    fn a_listing_page_reads_its_objects_and_its_continuation() {
        let xml = "<?xml version=\"1.0\"?><ListBucketResult><Name>b</Name>\
            <Contents><Key>blobs/a&amp;b</Key><LastModified>2026-01-02T03:04:05.678Z</LastModified>\
            <ETag>&quot;x&quot;</ETag><Size>12</Size></Contents>\
            <Contents><Key>probes/p</Key><LastModified>1970-01-01T00:00:01Z</LastModified>\
            <Size>2</Size></Contents>\
            <IsTruncated>true</IsTruncated><NextContinuationToken>t&lt;1</NextContinuationToken>\
            </ListBucketResult>";
        let (objects, next) = parse_listing(xml).unwrap();
        assert_eq!(
            objects,
            vec![
                Object {
                    key: "blobs/a&b".to_owned(),
                    last_modified_ms: 1_767_323_045_678,
                    size: 12
                },
                Object {
                    key: "probes/p".to_owned(),
                    last_modified_ms: 1000,
                    size: 2
                },
            ]
        );
        assert_eq!(next.as_deref(), Some("t<1"));
        let (none, last) =
            parse_listing("<ListBucketResult><IsTruncated>false</IsTruncated></ListBucketResult>")
                .unwrap();
        assert!(none.is_empty() && last.is_none());
        assert!(parse_listing("<IsTruncated>true</IsTruncated>").is_err());
    }

    #[test]
    fn a_manifest_that_does_not_parse_is_corrupt() {
        assert_eq!(
            parse_manifest(b"2 8").unwrap(),
            Manifest {
                chunk_count: 2,
                size_bytes: 8
            }
        );
        assert!(matches!(parse_manifest(b"one"), Err(BlobError::Corrupt(_))));
        assert!(matches!(parse_manifest(b"1"), Err(BlobError::Corrupt(_))));
    }
}
