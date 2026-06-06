//! Frame checksum layer.
//!
//! On the wire a Tibia frame is `[u16 LE length][length bytes]`. The length
//! field itself is handled by the `net` crate (it drives the socket read). The
//! `length` bytes — the *inner frame* — begin with a 4-byte little-endian
//! Adler-32 checksum (protocol 10.98 always uses it) covering everything after
//! it. This module adds and verifies that checksum.

use crate::adler32;

/// Length of the Adler-32 checksum prefix on an inner frame.
pub const CHECKSUM_LEN: usize = 4;

/// Errors raised while verifying an inner frame.
#[derive(Debug, thiserror::Error)]
pub enum FrameError {
    /// The inner frame was too short to even hold a checksum.
    #[error("frame too short for checksum: had {0} bytes, need at least 4")]
    TooShort(usize),
    /// The header checksum disagreed with the computed one.
    #[error("checksum mismatch: header {expected:#010x}, computed {actual:#010x}")]
    ChecksumMismatch {
        /// Checksum read from the frame header.
        expected: u32,
        /// Checksum computed over the payload.
        actual: u32,
    },
}

/// Prepend a 4-byte little-endian Adler-32 checksum to `payload`, producing an
/// inner frame ready to be length-prefixed and written.
pub fn checksummed(payload: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(CHECKSUM_LEN + payload.len());
    out.extend_from_slice(&adler32(payload).to_le_bytes());
    out.extend_from_slice(payload);
    out
}

/// Verify the leading checksum of an inner frame and return the payload slice.
pub fn verify(inner: &[u8]) -> Result<&[u8], FrameError> {
    if inner.len() < CHECKSUM_LEN {
        return Err(FrameError::TooShort(inner.len()));
    }
    let (head, payload) = inner.split_at(CHECKSUM_LEN);
    let expected = u32::from_le_bytes(head.try_into().unwrap());
    let actual = adler32(payload);
    if expected != actual {
        return Err(FrameError::ChecksumMismatch { expected, actual });
    }
    Ok(payload)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn verify_accepts_what_checksummed_produced() {
        let payload = b"\x01\x02\x03login-block";
        let inner = checksummed(payload);
        assert_eq!(inner.len(), CHECKSUM_LEN + payload.len());
        assert_eq!(verify(&inner).unwrap(), payload);
    }

    #[test]
    fn a_corrupted_payload_fails_verification() {
        let mut inner = checksummed(b"hello world");
        *inner.last_mut().unwrap() ^= 0xFF; // flip a payload bit
        match verify(&inner) {
            Err(FrameError::ChecksumMismatch { .. }) => {}
            other => panic!("expected checksum mismatch, got {other:?}"),
        }
    }

    #[test]
    fn a_frame_shorter_than_the_checksum_is_rejected() {
        match verify(&[0x00, 0x01]) {
            Err(FrameError::TooShort(2)) => {}
            other => panic!("expected TooShort(2), got {other:?}"),
        }
    }
}
