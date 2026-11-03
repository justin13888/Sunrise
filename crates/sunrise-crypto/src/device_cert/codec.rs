//! The certificate body's CBOR encoding and decoding: the `DeviceCertBody` map
//! of the CDDL in [`super`]'s header, to and from [`DeviceCertInner`].
//!
//! Both directions check the nickname against [`MAX_NICKNAME_BYTES`], so the
//! encoder never mints a body its own decoder refuses. The outer
//! `{1: body_bytes, 2: sig}` map is not here: it is [`super::DeviceCert`]'s,
//! and carries the body as the opaque byte string these functions produce and
//! read.

use super::{DeviceCertError, DeviceCertInner, MAX_NICKNAME_BYTES};
use ciborium::value::{Integer, Value};

pub(super) fn body_to_cbor(body: &DeviceCertInner) -> Result<Vec<u8>, DeviceCertError> {
    if body.nickname.is_empty() || body.nickname.len() > MAX_NICKNAME_BYTES {
        return Err(DeviceCertError::Validation("nickname length"));
    }
    let map = vec![
        (
            Value::Integer(Integer::from(1)),
            Value::Integer(Integer::from(body.v)),
        ),
        (
            Value::Integer(Integer::from(2)),
            Value::Bytes(body.device_id.to_vec()),
        ),
        (
            Value::Integer(Integer::from(3)),
            Value::Bytes(body.d_s_pub.to_vec()),
        ),
        (
            Value::Integer(Integer::from(4)),
            Value::Bytes(body.d_d_pub.to_vec()),
        ),
        (
            Value::Integer(Integer::from(5)),
            Value::Bytes(body.identity_id.to_vec()),
        ),
        (
            Value::Integer(Integer::from(6)),
            Value::Integer(body.created_at_ms.into()),
        ),
        (
            Value::Integer(Integer::from(7)),
            Value::Text(body.nickname.clone()),
        ),
        (
            Value::Integer(Integer::from(8)),
            Value::Text(body.platform.clone()),
        ),
    ];
    let mut out = Vec::with_capacity(256);
    ciborium::ser::into_writer(&Value::Map(map), &mut out)
        .map_err(|e| DeviceCertError::Cbor(e.to_string()))?;
    Ok(out)
}

pub(super) fn body_from_cbor(bytes: &[u8]) -> Result<DeviceCertInner, DeviceCertError> {
    // Same rule the outer map follows, and now it matters one level deeper:
    // `body_bytes` is a `bstr` of caller-chosen length, so a body with junk
    // appended is signed junk rather than ignored junk. Refusing it keeps
    // "these bytes" and "this body" the same thing on both sides of the
    // signature.
    let mut cursor = bytes;
    let value = sunrise_cbor::decode_value(&mut cursor)
        .map_err(|e| DeviceCertError::Cbor(e.to_string()))?;
    if !cursor.is_empty() {
        return Err(DeviceCertError::Cbor(format!(
            "{} trailing byte(s) after the device cert body",
            cursor.len()
        )));
    }
    let map = match value {
        Value::Map(m) => m,
        _ => return Err(DeviceCertError::Cbor("body must be a map".into())),
    };
    let mut inner = DeviceCertInner {
        v: 0,
        device_id: [0u8; 16],
        d_s_pub: [0u8; 32],
        d_d_pub: [0u8; 32],
        identity_id: [0u8; 16],
        created_at_ms: 0,
        nickname: String::new(),
        platform: String::new(),
    };
    let mut got = [false; 8];
    for (k, v) in map {
        let id = match k {
            Value::Integer(i) => i128::from(i),
            _ => return Err(DeviceCertError::Cbor("non-int key".into())),
        };
        match (id, v) {
            (1, Value::Integer(i)) => {
                let raw: i128 = i128::from(i);
                inner.v = u32::try_from(raw).map_err(|_| DeviceCertError::BadField("v"))?;
                got[0] = true;
            }
            (2, Value::Bytes(b)) if b.len() == 16 => {
                inner.device_id.copy_from_slice(&b);
                got[1] = true;
            }
            (3, Value::Bytes(b)) if b.len() == 32 => {
                inner.d_s_pub.copy_from_slice(&b);
                got[2] = true;
            }
            (4, Value::Bytes(b)) if b.len() == 32 => {
                inner.d_d_pub.copy_from_slice(&b);
                got[3] = true;
            }
            (5, Value::Bytes(b)) if b.len() == 16 => {
                inner.identity_id.copy_from_slice(&b);
                got[4] = true;
            }
            (6, Value::Integer(i)) => {
                let raw: i128 = i128::from(i);
                inner.created_at_ms =
                    u64::try_from(raw).map_err(|_| DeviceCertError::BadField("created_at"))?;
                got[5] = true;
            }
            (7, Value::Text(s)) => {
                inner.nickname = s;
                got[6] = true;
            }
            (8, Value::Text(s)) => {
                inner.platform = s;
                got[7] = true;
            }
            (id, _) if (1..=8).contains(&id) => {
                return Err(DeviceCertError::BadField("body field shape"));
            }
            _ => {} // forward-compat
        }
    }
    if !got.iter().all(|x| *x) {
        return Err(DeviceCertError::BadField("missing body field"));
    }
    if inner.nickname.is_empty() || inner.nickname.len() > MAX_NICKNAME_BYTES {
        return Err(DeviceCertError::Validation("nickname length"));
    }
    Ok(inner)
}
