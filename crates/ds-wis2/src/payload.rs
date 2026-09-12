//! Inline content decoding and integrity verification (pure, no I/O).

use bytes::Bytes;
use sha2::Digest;
use std::io::Read;

use crate::notification::{decode_base64, Content, Integrity};
use crate::Wis2Error;

/// Spec cap on the *encoded* inline value.
pub const MAX_INLINE_ENCODED_BYTES: u64 = 4096;

/// Where a payload came from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PayloadSource {
    /// Decoded from `properties.content`.
    Inline,
    /// Downloaded from this URL (`rel=canonical` or `rel=update`).
    Downloaded(String),
}

/// A data object ready for an engine's decoder.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Payload {
    pub bytes: Bytes,
    pub source: PayloadSource,
    /// Media type from the link (`application/bufr`, `application/xml`, …);
    /// `None` for inline content (the notification does not say).
    pub media_type: Option<String>,
    /// `Some(true)` verified, `Some(false)` would be an error instead;
    /// `None` = no integrity block, or an unsupported method (counted by the
    /// caller as unverified).
    pub verified: Option<bool>,
}

/// Outcome of [`verify_integrity`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verification {
    Verified,
    /// Method not implemented (sha3-*): accepted, but not checked.
    Unsupported,
}

/// Decode `properties.content` into raw bytes. `max_bytes` bounds the decoded
/// size (a 4 KiB gzip can expand a lot).
pub fn decode_inline(content: &Content, max_bytes: u64) -> Result<Bytes, Wis2Error> {
    if content.value.len() as u64 > MAX_INLINE_ENCODED_BYTES {
        return Err(Wis2Error::TooLarge(format!(
            "inline content is {} encoded bytes (spec max {MAX_INLINE_ENCODED_BYTES})",
            content.value.len()
        )));
    }
    let bytes = match content.encoding.as_str() {
        "utf-8" | "utf8" => content.value.as_bytes().to_vec(),
        "base64" => decode_base64(&content.value)
            .ok_or_else(|| Wis2Error::Decode("inline content is not valid base64".into()))?,
        "gzip" => {
            let compressed = decode_base64(&content.value).ok_or_else(|| {
                Wis2Error::Decode("inline gzip content is not valid base64".into())
            })?;
            let mut out = Vec::new();
            let mut dec = flate2::read::GzDecoder::new(compressed.as_slice()).take(max_bytes + 1);
            dec.read_to_end(&mut out)
                .map_err(|e| Wis2Error::Decode(format!("inline gzip: {e}")))?;
            out
        }
        other => {
            return Err(Wis2Error::Decode(format!(
                "unsupported inline content encoding '{other}'"
            )))
        }
    };
    if bytes.len() as u64 > max_bytes {
        return Err(Wis2Error::TooLarge(format!(
            "inline content decodes to more than {max_bytes} bytes"
        )));
    }
    Ok(Bytes::from(bytes))
}

/// Check `bytes` against the notification's `integrity` block.
pub fn verify_integrity(integrity: &Integrity, bytes: &[u8]) -> Result<Verification, Wis2Error> {
    let actual: Vec<u8> = match integrity.method.as_str() {
        "sha256" => sha2::Sha256::digest(bytes).to_vec(),
        "sha384" => sha2::Sha384::digest(bytes).to_vec(),
        "sha512" => sha2::Sha512::digest(bytes).to_vec(),
        "sha3-256" | "sha3-384" | "sha3-512" => return Ok(Verification::Unsupported),
        other => {
            return Err(Wis2Error::Integrity(format!(
                "unknown integrity method '{other}'"
            )))
        }
    };
    if actual == integrity.digest {
        Ok(Verification::Verified)
    } else {
        Err(Wis2Error::Integrity(format!(
            "{} digest mismatch ({} bytes)",
            integrity.method,
            bytes.len()
        )))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use base64::Engine as _;
    use std::io::Write;

    fn content(encoding: &str, value: &str) -> Content {
        Content {
            encoding: encoding.into(),
            size: value.len() as u64,
            value: value.into(),
        }
    }

    #[test]
    fn utf8_base64_and_gzip_inline() {
        let raw = b"<alert>hello</alert>";
        assert_eq!(
            decode_inline(&content("utf-8", "<alert>hello</alert>"), 1024).unwrap(),
            Bytes::from_static(raw)
        );
        let b64 = base64::engine::general_purpose::STANDARD.encode(raw);
        assert_eq!(
            decode_inline(&content("base64", &b64), 1024).unwrap(),
            Bytes::from_static(raw)
        );
        let mut gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        gz.write_all(raw).unwrap();
        let gz_b64 = base64::engine::general_purpose::STANDARD.encode(gz.finish().unwrap());
        assert_eq!(
            decode_inline(&content("gzip", &gz_b64), 1024).unwrap(),
            Bytes::from_static(raw)
        );
    }

    #[test]
    fn inline_size_guards_and_bad_encodings() {
        let big = "A".repeat(MAX_INLINE_ENCODED_BYTES as usize + 1);
        assert!(matches!(
            decode_inline(&content("utf-8", &big), 1 << 20),
            Err(Wis2Error::TooLarge(_))
        ));
        assert!(matches!(
            decode_inline(&content("utf-8", "0123456789"), 4),
            Err(Wis2Error::TooLarge(_))
        ));
        assert!(matches!(
            decode_inline(&content("base64", "!!notbase64!!"), 1024),
            Err(Wis2Error::Decode(_))
        ));
        assert!(matches!(
            decode_inline(&content("rot13", "x"), 1024),
            Err(Wis2Error::Decode(_))
        ));
        // A gzip bomb is cut at max_bytes + 1 and rejected.
        let zeros = vec![0u8; 200_000];
        let mut gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::best());
        gz.write_all(&zeros).unwrap();
        let gz_b64 = base64::engine::general_purpose::STANDARD.encode(gz.finish().unwrap());
        assert!(gz_b64.len() < 4096);
        assert!(matches!(
            decode_inline(&content("gzip", &gz_b64), 10_000),
            Err(Wis2Error::TooLarge(_))
        ));
    }

    #[test]
    fn integrity_methods() {
        let data = b"BUFR....";
        let ok = Integrity {
            method: "sha256".into(),
            digest: sha2::Sha256::digest(data).to_vec(),
        };
        assert_eq!(verify_integrity(&ok, data).unwrap(), Verification::Verified);
        let bad = Integrity {
            method: "sha512".into(),
            digest: vec![0; 64],
        };
        assert!(matches!(
            verify_integrity(&bad, data),
            Err(Wis2Error::Integrity(_))
        ));
        let sha3 = Integrity {
            method: "sha3-256".into(),
            digest: vec![0; 32],
        };
        assert_eq!(
            verify_integrity(&sha3, data).unwrap(),
            Verification::Unsupported
        );
        let unknown = Integrity {
            method: "crc32".into(),
            digest: vec![],
        };
        assert!(verify_integrity(&unknown, data).is_err());
    }
}
