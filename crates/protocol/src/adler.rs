//! Adler-32 checksum (RFC 1950), used as the outer frame checksum.
//!
//! The 4-byte little-endian checksum follows the 2-byte length prefix and
//! covers the rest of the frame. See `reference/tfs/src/protocol.cpp`
//! (`adlerChecksum`).

/// Largest prime below 2^16, the Adler-32 modulus.
const MOD_ADLER: u32 = 65521;

/// Compute the Adler-32 checksum of `data`.
pub fn adler32(data: &[u8]) -> u32 {
    let mut a: u32 = 1;
    let mut b: u32 = 0;
    for &byte in data {
        a = (a + byte as u32) % MOD_ADLER;
        b = (b + a) % MOD_ADLER;
    }
    (b << 16) | a
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_input_is_one() {
        assert_eq!(adler32(b""), 1);
    }

    #[test]
    fn single_byte_vector() {
        // a=0x61: a_sum = 1+0x61 = 0x62, b_sum = 0x62 -> 0x0062_0062
        assert_eq!(adler32(b"a"), 0x0062_0062);
    }

    #[test]
    fn wikipedia_reference_vector() {
        assert_eq!(adler32(b"Wikipedia"), 0x11E6_0398);
    }
}
