//! XTEA block cipher, 32 cycles, matching `reference/tfs/src/xtea.cpp`.
//!
//! After the login handshake all game traffic is XTEA-encrypted in 8-byte
//! blocks. Each 32-bit half of a block is read/written little-endian (TFS uses
//! `memcpy` on a little-endian host). The key is expanded once into 64 round
//! keys; `encrypt`/`decrypt` then operate over those.

/// The golden-ratio constant that drives the key schedule.
const DELTA: u32 = 0x9E37_79B9;

/// The 64 precomputed round keys derived from a 128-bit key.
pub type RoundKeys = [u32; 64];

/// Expand a 128-bit key (four `u32`s) into 64 round keys.
pub fn expand_key(key: &[u32; 4]) -> RoundKeys {
    let mut expanded = [0u32; 64];
    let mut sum: u32 = 0;
    let mut next_sum = sum.wrapping_add(DELTA);
    let mut i = 0;
    while i < expanded.len() {
        expanded[i] = sum.wrapping_add(key[(sum & 3) as usize]);
        expanded[i + 1] = next_sum.wrapping_add(key[((next_sum >> 11) & 3) as usize]);
        sum = next_sum;
        next_sum = next_sum.wrapping_add(DELTA);
        i += 2;
    }
    expanded
}

fn read_block(block: &[u8]) -> (u32, u32) {
    (
        u32::from_le_bytes(block[0..4].try_into().unwrap()),
        u32::from_le_bytes(block[4..8].try_into().unwrap()),
    )
}

fn write_block(block: &mut [u8], left: u32, right: u32) {
    block[0..4].copy_from_slice(&left.to_le_bytes());
    block[4..8].copy_from_slice(&right.to_le_bytes());
}

/// Encrypt `data` in place. Only whole 8-byte blocks are processed; any
/// trailing bytes shorter than a block are left untouched.
pub fn encrypt(data: &mut [u8], keys: &RoundKeys) {
    let mut i = 0;
    while i < keys.len() {
        for block in data.chunks_exact_mut(8) {
            let (mut left, mut right) = read_block(block);
            left = left.wrapping_add((((right << 4) ^ (right >> 5)).wrapping_add(right)) ^ keys[i]);
            right =
                right.wrapping_add((((left << 4) ^ (left >> 5)).wrapping_add(left)) ^ keys[i + 1]);
            write_block(block, left, right);
        }
        i += 2;
    }
}

/// Decrypt `data` in place, the inverse of [`encrypt`].
pub fn decrypt(data: &mut [u8], keys: &RoundKeys) {
    for i in (1..keys.len()).rev().step_by(2) {
        for block in data.chunks_exact_mut(8) {
            let (mut left, mut right) = read_block(block);
            right = right.wrapping_sub((((left << 4) ^ (left >> 5)).wrapping_add(left)) ^ keys[i]);
            left = left
                .wrapping_sub((((right << 4) ^ (right >> 5)).wrapping_add(right)) ^ keys[i - 1]);
            write_block(block, left, right);
        }
    }
}

/// Errors from the XTEA message framing layer.
#[derive(Debug, thiserror::Error)]
pub enum MessageError {
    /// The encrypted body was not a whole number of 8-byte blocks.
    #[error("encrypted body length {0} is not a multiple of 8")]
    NotPadded(usize),
    /// The declared inner length did not fit within the decrypted body.
    #[error("declared inner length {inner} does not fit in {body} body bytes")]
    BadInnerLength {
        /// Length read from the inner `u16`.
        inner: usize,
        /// Bytes actually available after the length field.
        body: usize,
    },
}

/// Wrap `payload` as an encrypted message body: prepend a `u16` little-endian
/// inner length, zero-pad the whole thing to a multiple of 8, then XTEA-encrypt.
/// The result is what sits between the frame checksum and the end of the frame.
pub fn encrypt_message(payload: &[u8], keys: &RoundKeys) -> Vec<u8> {
    let mut buf = Vec::with_capacity(2 + payload.len() + 7);
    buf.extend_from_slice(&(payload.len() as u16).to_le_bytes());
    buf.extend_from_slice(payload);
    let padding = (8 - buf.len() % 8) % 8;
    buf.resize(buf.len() + padding, 0);
    encrypt(&mut buf, keys);
    buf
}

/// Inverse of [`encrypt_message`]: XTEA-decrypt `body`, read the inner length,
/// and return that many payload bytes (dropping the length field and padding).
pub fn decrypt_message(body: &[u8], keys: &RoundKeys) -> Result<Vec<u8>, MessageError> {
    if body.is_empty() || body.len() % 8 != 0 {
        return Err(MessageError::NotPadded(body.len()));
    }
    let mut buf = body.to_vec();
    decrypt(&mut buf, keys);
    let inner = u16::from_le_bytes([buf[0], buf[1]]) as usize;
    if 2 + inner > buf.len() {
        return Err(MessageError::BadInnerLength {
            inner,
            body: buf.len() - 2,
        });
    }
    Ok(buf[2..2 + inner].to_vec())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Independent textbook XTEA (Needham–Wheeler), 32 cycles, over a u32 pair.
    /// This is a *different* code path from the precomputed-round-key port —
    /// agreement between the two is the correctness oracle.
    fn textbook_encipher(v: &mut [u32; 2], key: &[u32; 4]) {
        let (mut v0, mut v1) = (v[0], v[1]);
        let mut sum: u32 = 0;
        for _ in 0..32 {
            v0 = v0.wrapping_add(
                (((v1 << 4) ^ (v1 >> 5)).wrapping_add(v1))
                    ^ sum.wrapping_add(key[(sum & 3) as usize]),
            );
            sum = sum.wrapping_add(DELTA);
            v1 = v1.wrapping_add(
                (((v0 << 4) ^ (v0 >> 5)).wrapping_add(v0))
                    ^ sum.wrapping_add(key[((sum >> 11) & 3) as usize]),
            );
        }
        v[0] = v0;
        v[1] = v1;
    }

    #[test]
    fn matches_independent_textbook_xtea_on_one_block() {
        let key = [0x1122_3344u32, 0x5566_7788, 0x99AA_BBCC, 0xDDEE_FF00];

        // Block as two LE u32 words.
        let v0 = 0x0403_0201u32;
        let v1 = 0x0807_0605u32;
        let mut block = [0u8; 8];
        block[0..4].copy_from_slice(&v0.to_le_bytes());
        block[4..8].copy_from_slice(&v1.to_le_bytes());

        let mut expected = [v0, v1];
        textbook_encipher(&mut expected, &key);

        encrypt(&mut block, &expand_key(&key));

        assert_eq!(
            u32::from_le_bytes(block[0..4].try_into().unwrap()),
            expected[0]
        );
        assert_eq!(
            u32::from_le_bytes(block[4..8].try_into().unwrap()),
            expected[1]
        );
    }

    #[test]
    fn decrypt_is_the_inverse_of_encrypt() {
        let key = [0xDEAD_BEEFu32, 0x0BAD_F00D, 0xFEED_FACE, 0xCAFE_BABE];
        let keys = expand_key(&key);
        let original: Vec<u8> = (0..32u8).collect(); // four 8-byte blocks
        let mut data = original.clone();

        encrypt(&mut data, &keys);
        assert_ne!(data, original, "ciphertext must differ from plaintext");
        decrypt(&mut data, &keys);

        assert_eq!(data, original);
    }

    #[test]
    fn trailing_partial_block_is_untouched() {
        let keys = expand_key(&[0, 0, 0, 0]);
        let mut data = vec![0xABu8; 8 + 3]; // one full block + 3 leftover bytes
        encrypt(&mut data, &keys);
        assert_eq!(&data[8..], &[0xAB, 0xAB, 0xAB]);
    }

    #[test]
    fn message_round_trips_for_various_lengths() {
        let keys = expand_key(&[0x11, 0x22, 0x33, 0x44]);
        for len in [0usize, 1, 5, 6, 7, 8, 9, 100] {
            let payload: Vec<u8> = (0..len).map(|i| (i * 3) as u8).collect();
            let body = encrypt_message(&payload, &keys);
            assert_eq!(body.len() % 8, 0, "encrypted body must be block-aligned");
            let got = decrypt_message(&body, &keys).unwrap();
            assert_eq!(got, payload, "len {len}");
        }
    }

    #[test]
    fn encrypted_body_hides_the_inner_length() {
        let keys = expand_key(&[1, 2, 3, 4]);
        let payload = b"character-list";
        let body = encrypt_message(payload, &keys);
        // The first two bytes are encrypted, not the plaintext length.
        assert_ne!(&body[0..2], &(payload.len() as u16).to_le_bytes());
    }

    #[test]
    fn unaligned_body_is_rejected() {
        let keys = expand_key(&[0, 0, 0, 0]);
        match decrypt_message(&[0u8; 5], &keys) {
            Err(MessageError::NotPadded(5)) => {}
            other => panic!("expected NotPadded(5), got {other:?}"),
        }
    }
}
