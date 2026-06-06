//! A little-endian cursor over a node's property bytes.
//!
//! Mirrors TFS `PropStream`: fixed-width integers are little-endian and strings
//! are a `u16` length followed by that many bytes.

use crate::FormatError;

/// Reads little-endian values out of a slice of (already un-escaped) prop bytes.
pub struct PropReader<'a> {
    data: &'a [u8],
    pos: usize,
}

impl<'a> PropReader<'a> {
    /// Wrap a property byte slice.
    pub fn new(data: &'a [u8]) -> Self {
        Self { data, pos: 0 }
    }

    /// Bytes not yet consumed.
    pub fn remaining(&self) -> usize {
        self.data.len() - self.pos
    }

    /// Read a `u8`.
    pub fn read_u8(&mut self) -> Result<u8, FormatError> {
        let b = *self.data.get(self.pos).ok_or(FormatError::UnexpectedEof { what: "u8" })?;
        self.pos += 1;
        Ok(b)
    }

    /// Read a little-endian `u16`.
    pub fn read_u16(&mut self) -> Result<u16, FormatError> {
        let bytes = self.take(2, "u16")?;
        Ok(u16::from_le_bytes(bytes.try_into().unwrap()))
    }

    /// Read a little-endian `u32`.
    pub fn read_u32(&mut self) -> Result<u32, FormatError> {
        let bytes = self.take(4, "u32")?;
        Ok(u32::from_le_bytes(bytes.try_into().unwrap()))
    }

    /// Read a `u16` length-prefixed string.
    pub fn read_string(&mut self) -> Result<String, FormatError> {
        let len = self.read_u16()? as usize;
        let bytes = self.take(len, "string")?;
        Ok(String::from_utf8_lossy(bytes).into_owned())
    }

    /// Skip `n` bytes.
    pub fn skip(&mut self, n: usize) -> Result<(), FormatError> {
        self.take(n, "skip")?;
        Ok(())
    }

    fn take(&mut self, n: usize, what: &'static str) -> Result<&'a [u8], FormatError> {
        let end = self.pos.checked_add(n).filter(|&e| e <= self.data.len());
        let end = end.ok_or(FormatError::UnexpectedEof { what })?;
        let slice = &self.data[self.pos..end];
        self.pos = end;
        Ok(slice)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_little_endian_integers_in_sequence() {
        let mut r = PropReader::new(&[0x01, 0x34, 0x12, 0x78, 0x56, 0x34, 0x12]);
        assert_eq!(r.read_u8().unwrap(), 0x01);
        assert_eq!(r.read_u16().unwrap(), 0x1234);
        assert_eq!(r.read_u32().unwrap(), 0x1234_5678);
        assert_eq!(r.remaining(), 0);
    }

    #[test]
    fn reads_a_length_prefixed_string() {
        let mut r = PropReader::new(b"\x03\x00abc\xff");
        assert_eq!(r.read_string().unwrap(), "abc");
        assert_eq!(r.read_u8().unwrap(), 0xff);
    }

    #[test]
    fn reading_past_the_end_errors() {
        let mut r = PropReader::new(&[0x01, 0x02]);
        match r.read_u32() {
            Err(FormatError::UnexpectedEof { .. }) => {}
            other => panic!("expected UnexpectedEof, got {other:?}"),
        }
    }
}
