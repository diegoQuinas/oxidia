//! Raw RSA decryption for the login handshake.
//!
//! The client encrypts a 128-byte block with the server's public key; the
//! server recovers it as `m = c^d mod n` and reads the inner payload (XTEA key,
//! account, password). This is **raw** RSA — no PKCS padding — exactly as
//! `reference/tfs/src/rsa.cpp` (`RSA::decrypt`) does with a CryptoPP `Integer`.
//!
//! The bundled key is the well-known OpenTibia 1024-bit key (see
//! `reference/tfs/key.pem`).

use num_bigint_dig::BigUint;

/// Size in bytes of an RSA block for the 1024-bit key.
pub const RSA_BLOCK_SIZE: usize = 128;

/// Canonical OpenTibia public exponent.
pub const OPEN_TIBIA_E: u32 = 65537;

/// Modulus `n` of the OpenTibia key, decimal.
const OPEN_TIBIA_N: &str = "109120132967399429278860960508995541528237502902798129123468757937266291492576446330739696001110603907230888610072655818825358503429057592827629436413108566029093628212635953836686562675849720620786279431090218017681061521755056710823876476444260558147179707119674283982419152118103759076030616683978566631413";

/// Private exponent `d` of the OpenTibia key, decimal.
const OPEN_TIBIA_D: &str = "46730330223584118622160180015036832148732986808519344675210555262940258739805766860224610646919605860206328024326703361630109888417839241959507572247284807035235569619173792292786907845791904955103601652822519121908367187885509270025388641700821735345222087940578381210879116823013776808975766851829020659073";

/// Errors raised by the RSA layer.
#[derive(Debug, thiserror::Error)]
pub enum RsaError {
    /// The block handed to [`RsaPrivateKey::decrypt`] was not 128 bytes.
    #[error("rsa block must be {RSA_BLOCK_SIZE} bytes, got {0}")]
    BadBlockSize(usize),
}

/// Encrypt a 128-byte block in place with the bundled OpenTibia **public** key
/// (`m^e mod n`), the operation a client performs. Useful for the replay /
/// sniff tooling and tests that simulate a client. The result is the
/// big-endian, zero-padded 128-byte encoding.
pub fn encrypt_open_tibia_public(block: &mut [u8]) -> Result<(), RsaError> {
    if block.len() != RSA_BLOCK_SIZE {
        return Err(RsaError::BadBlockSize(block.len()));
    }
    let n: BigUint = OPEN_TIBIA_N.parse().expect("valid decimal modulus");
    let e = BigUint::from(OPEN_TIBIA_E);
    let m = BigUint::from_bytes_be(block);
    let c = m.modpow(&e, &n);
    let bytes = c.to_bytes_be();
    block.fill(0);
    block[RSA_BLOCK_SIZE - bytes.len()..].copy_from_slice(&bytes);
    Ok(())
}

/// An RSA private key able to decrypt a single raw 128-byte block.
#[derive(Debug, Clone)]
pub struct RsaPrivateKey {
    n: BigUint,
    d: BigUint,
}

impl RsaPrivateKey {
    /// The bundled OpenTibia private key.
    pub fn open_tibia() -> Self {
        Self {
            n: OPEN_TIBIA_N.parse().expect("valid decimal modulus"),
            d: OPEN_TIBIA_D.parse().expect("valid decimal exponent"),
        }
    }

    /// Decrypt a 128-byte big-endian block in place. The result is the
    /// big-endian, zero-padded 128-byte encoding of `c^d mod n`.
    pub fn decrypt(&self, block: &mut [u8]) -> Result<(), RsaError> {
        if block.len() != RSA_BLOCK_SIZE {
            return Err(RsaError::BadBlockSize(block.len()));
        }
        let c = BigUint::from_bytes_be(block);
        let m = c.modpow(&self.d, &self.n);
        let bytes = m.to_bytes_be();
        // Left-pad to exactly 128 bytes (modpow drops leading zeros).
        block.fill(0);
        block[RSA_BLOCK_SIZE - bytes.len()..].copy_from_slice(&bytes);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn public_encrypt_then_private_decrypt_round_trips() {
        let key = RsaPrivateKey::open_tibia();
        let mut original = [0u8; RSA_BLOCK_SIZE];
        original[0] = 0x00;
        for (i, b) in original.iter_mut().enumerate().skip(1) {
            *b = (i * 13 % 251) as u8;
        }

        let mut block = original;
        encrypt_open_tibia_public(&mut block).unwrap();
        assert_ne!(block, original, "ciphertext must differ from plaintext");
        key.decrypt(&mut block).unwrap();

        assert_eq!(block, original);
    }

    #[test]
    fn decrypt_output_is_exactly_128_bytes() {
        let key = RsaPrivateKey::open_tibia();
        let mut block = [0u8; RSA_BLOCK_SIZE];
        block[RSA_BLOCK_SIZE - 1] = 2; // ciphertext = 2, tiny m = 2^d mod n
        key.decrypt(&mut block).unwrap();
        assert_eq!(block.len(), RSA_BLOCK_SIZE);
    }

    #[test]
    fn wrong_block_size_is_rejected() {
        let key = RsaPrivateKey::open_tibia();
        let mut short = [0u8; 64];
        match key.decrypt(&mut short) {
            Err(RsaError::BadBlockSize(64)) => {}
            other => panic!("expected BadBlockSize(64), got {other:?}"),
        }
    }
}
