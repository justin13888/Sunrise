//! AWS Signature Version 4, as S3 takes it: the canonical request, the
//! string to sign, the derived key and the `Authorization` header.

use sha2::{Digest as _, Sha256};

use super::S3Credentials;

/// SigV4's URI encoding: every byte but `A-Z a-z 0-9 - . _ ~` as `%XX`, and
/// `/` kept where it separates a key's segments.
pub(super) fn uri_encode(s: &str, keep_slash: bool) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        if b.is_ascii_alphanumeric()
            || matches!(b, b'-' | b'.' | b'_' | b'~')
            || (keep_slash && b == b'/')
        {
            out.push(char::from(b));
        } else {
            out.push('%');
            out.push_str(&hex::encode_upper([b]));
        }
    }
    out
}

/// The query string, encoded and sorted the way SigV4 signs it, which is
/// also the way it is sent.
pub(super) fn canonical_query(pairs: &[(&str, &str)]) -> String {
    let mut encoded: Vec<(String, String)> = pairs
        .iter()
        .map(|(k, v)| (uri_encode(k, false), uri_encode(v, false)))
        .collect();
    encoded.sort();
    encoded
        .iter()
        .map(|(k, v)| format!("{k}={v}"))
        .collect::<Vec<_>>()
        .join("&")
}

/// What a request signs.
pub(super) struct Signed<'a> {
    pub(super) method: &'a str,
    /// Already URI-encoded.
    pub(super) path: &'a str,
    /// Already canonical.
    pub(super) query: &'a str,
    /// Lowercase names, sorted by name.
    pub(super) headers: &'a [(&'a str, &'a str)],
    pub(super) payload_hash: &'a str,
    /// `YYYYMMDDTHHMMSSZ`.
    pub(super) amz_date: &'a str,
}

fn hmac_sha256(key: &[u8], data: &[u8]) -> [u8; 32] {
    use hmac::Mac as _;
    // HMAC takes a key of any length, so this cannot fail.
    let mut mac = <hmac::Hmac<Sha256> as hmac::Mac>::new_from_slice(key)
        .unwrap_or_else(|_| unreachable!("HMAC accepts any key length"));
    mac.update(data);
    mac.finalize().into_bytes().into()
}

/// The `Authorization` header AWS Signature Version 4 gives `request`.
pub(super) fn authorization(
    credentials: &S3Credentials,
    region: &str,
    request: &Signed<'_>,
) -> String {
    let mut canonical_headers = String::new();
    for (name, value) in request.headers {
        canonical_headers.push_str(name);
        canonical_headers.push(':');
        canonical_headers.push_str(value.trim());
        canonical_headers.push('\n');
    }
    let signed_headers = request
        .headers
        .iter()
        .map(|(name, _)| *name)
        .collect::<Vec<_>>()
        .join(";");
    let canonical_request = format!(
        "{}\n{}\n{}\n{canonical_headers}\n{signed_headers}\n{}",
        request.method, request.path, request.query, request.payload_hash
    );
    let date = request.amz_date.get(..8).unwrap_or_default();
    let scope = format!("{date}/{region}/s3/aws4_request");
    let string_to_sign = format!(
        "AWS4-HMAC-SHA256\n{}\n{scope}\n{}",
        request.amz_date,
        hex::encode(Sha256::digest(canonical_request.as_bytes()))
    );
    let secret = zeroize::Zeroizing::new(format!("AWS4{}", &*credentials.secret_access_key));
    let k_date = hmac_sha256(secret.as_bytes(), date.as_bytes());
    let k_region = hmac_sha256(&k_date, region.as_bytes());
    let k_service = hmac_sha256(&k_region, b"s3");
    let k_signing = hmac_sha256(&k_service, b"aws4_request");
    let signature = hex::encode(hmac_sha256(&k_signing, string_to_sign.as_bytes()));
    format!(
        "AWS4-HMAC-SHA256 Credential={}/{scope}, SignedHeaders={signed_headers}, \
         Signature={signature}",
        credentials.access_key_id
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The AWS example key pair the SigV4 documentation signs with.
    fn example_credentials() -> S3Credentials {
        S3Credentials::new(
            "AKIAIOSFODNN7EXAMPLE",
            "wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY",
        )
    }

    const EMPTY_SHA256: &str = "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";

    /// The signature AWS's "GET Object" example documents for this request.
    #[test]
    fn signing_matches_the_aws_get_object_example() {
        let headers = [
            ("host", "examplebucket.s3.amazonaws.com"),
            ("range", "bytes=0-9"),
            ("x-amz-content-sha256", EMPTY_SHA256),
            ("x-amz-date", "20130524T000000Z"),
        ];
        let auth = authorization(
            &example_credentials(),
            "us-east-1",
            &Signed {
                method: "GET",
                path: "/test.txt",
                query: "",
                headers: &headers,
                payload_hash: EMPTY_SHA256,
                amz_date: "20130524T000000Z",
            },
        );
        assert_eq!(
            auth,
            "AWS4-HMAC-SHA256 Credential=AKIAIOSFODNN7EXAMPLE/20130524/us-east-1/s3/aws4_request, \
             SignedHeaders=host;range;x-amz-content-sha256;x-amz-date, \
             Signature=f0e8bdb87c964420e857bd35b5d6ed310bd44f0170aba48dd91039c6036bdb41"
        );
    }

    /// The signature AWS's "GET Bucket (List Objects)" example documents,
    /// which is the query string's encoding and order as well.
    #[test]
    fn signing_matches_the_aws_list_objects_example() {
        let headers = [
            ("host", "examplebucket.s3.amazonaws.com"),
            ("x-amz-content-sha256", EMPTY_SHA256),
            ("x-amz-date", "20130524T000000Z"),
        ];
        let query = canonical_query(&[("prefix", "J"), ("max-keys", "2")]);
        assert_eq!(query, "max-keys=2&prefix=J");
        let auth = authorization(
            &example_credentials(),
            "us-east-1",
            &Signed {
                method: "GET",
                path: "/",
                query: &query,
                headers: &headers,
                payload_hash: EMPTY_SHA256,
                amz_date: "20130524T000000Z",
            },
        );
        assert!(
            auth.ends_with(
                "Signature=34b48302e7b5fa45bde8084f4b7868a86f0a534bc59db6670ed5711ef69dc6f7"
            ),
            "{auth}"
        );
    }

    #[test]
    fn keys_and_queries_are_encoded_as_sigv4_signs_them() {
        assert_eq!(uri_encode("blobs/ab/up_x/0", true), "blobs/ab/up_x/0");
        assert_eq!(uri_encode("a b/c~", false), "a%20b%2Fc~");
        assert_eq!(
            canonical_query(&[("prefix", "blobs/"), ("list-type", "2")]),
            "list-type=2&prefix=blobs%2F"
        );
    }
}
