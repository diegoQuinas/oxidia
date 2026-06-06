//! `NetworkMessage`: little-endian reader/writer over a byte buffer.
//!
//! The Tibia wire format is little-endian. Strings are length-prefixed with a
//! `u16`. The reader borrows a slice and never copies; the writer owns a `Vec`.

use crate::ProtocolError;

/// Cursor over a borrowed buffer that reads little-endian primitives.
#[derive(Debug, Clone)]
pub struct MessageReader<'a> {
    buf: &'a [u8],
    pos: usize,
}

impl<'a> MessageReader<'a> {
    /// Wrap a buffer for reading from the start.
    pub fn new(buf: &'a [u8]) -> Self {
        Self { buf, pos: 0 }
    }

    /// Number of bytes not yet consumed.
    pub fn remaining(&self) -> usize {
        self.buf.len() - self.pos
    }

    fn take(&mut self, n: usize) -> Result<&'a [u8], ProtocolError> {
        if self.remaining() < n {
            return Err(ProtocolError::UnexpectedEof {
                needed: n,
                had: self.remaining(),
            });
        }
        let slice = &self.buf[self.pos..self.pos + n];
        self.pos += n;
        Ok(slice)
    }

    /// Read one byte.
    pub fn read_u8(&mut self) -> Result<u8, ProtocolError> {
        Ok(self.take(1)?[0])
    }

    /// Read a little-endian `u16`.
    pub fn read_u16(&mut self) -> Result<u16, ProtocolError> {
        let b = self.take(2)?;
        Ok(u16::from_le_bytes([b[0], b[1]]))
    }

    /// Read a little-endian `u32`.
    pub fn read_u32(&mut self) -> Result<u32, ProtocolError> {
        let b = self.take(4)?;
        Ok(u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
    }

    /// Read a `u16`-length-prefixed string as raw bytes (Latin-1 on the wire).
    pub fn read_string(&mut self) -> Result<&'a [u8], ProtocolError> {
        let len = self.read_u16()? as usize;
        self.take(len)
    }

    /// Read exactly `n` raw bytes.
    pub fn read_bytes(&mut self, n: usize) -> Result<&'a [u8], ProtocolError> {
        self.take(n)
    }
}

/// Growable little-endian message builder.
#[derive(Debug, Default, Clone)]
pub struct MessageWriter {
    buf: Vec<u8>,
}

impl MessageWriter {
    /// A new empty writer.
    pub fn new() -> Self {
        Self::default()
    }

    /// Append one byte.
    pub fn write_u8(&mut self, v: u8) {
        self.buf.push(v);
    }

    /// Append a little-endian `u16`.
    pub fn write_u16(&mut self, v: u16) {
        self.buf.extend_from_slice(&v.to_le_bytes());
    }

    /// Append a little-endian `u32`.
    pub fn write_u32(&mut self, v: u32) {
        self.buf.extend_from_slice(&v.to_le_bytes());
    }

    /// Append a `u16`-length-prefixed string.
    pub fn write_string(&mut self, s: &[u8]) {
        self.write_u16(s.len() as u16);
        self.buf.extend_from_slice(s);
    }

    /// Append raw bytes verbatim.
    pub fn write_bytes(&mut self, b: &[u8]) {
        self.buf.extend_from_slice(b);
    }

    /// Borrow the accumulated bytes.
    pub fn as_bytes(&self) -> &[u8] {
        &self.buf
    }

    /// Consume the writer, yielding the buffer.
    pub fn into_bytes(self) -> Vec<u8> {
        self.buf
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_little_endian_primitives_in_order() {
        // u8=0x01, u16=0x0302, u32=0x07060504
        let bytes = [0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07];
        let mut r = MessageReader::new(&bytes);
        assert_eq!(r.read_u8().unwrap(), 0x01);
        assert_eq!(r.read_u16().unwrap(), 0x0302);
        assert_eq!(r.read_u32().unwrap(), 0x0706_0504);
        assert_eq!(r.remaining(), 0);
    }

    #[test]
    fn reading_past_the_end_reports_eof_with_counts() {
        let bytes = [0x01];
        let mut r = MessageReader::new(&bytes);
        let _ = r.read_u8().unwrap();
        match r.read_u16() {
            Err(ProtocolError::UnexpectedEof { needed, had }) => {
                assert_eq!(needed, 2);
                assert_eq!(had, 0);
            }
            other => panic!("expected EOF, got {other:?}"),
        }
    }

    #[test]
    fn reads_a_u16_length_prefixed_string() {
        let mut bytes = vec![0x05, 0x00];
        bytes.extend_from_slice(b"hello");
        let mut r = MessageReader::new(&bytes);
        assert_eq!(r.read_string().unwrap(), b"hello");
    }

    #[test]
    fn writer_round_trips_through_reader() {
        let mut w = MessageWriter::new();
        w.write_u8(0x0a);
        w.write_u16(0x1234);
        w.write_u32(0xdead_beef);
        w.write_string(b"account");

        let bytes = w.into_bytes();
        let mut r = MessageReader::new(&bytes);
        assert_eq!(r.read_u8().unwrap(), 0x0a);
        assert_eq!(r.read_u16().unwrap(), 0x1234);
        assert_eq!(r.read_u32().unwrap(), 0xdead_beef);
        assert_eq!(r.read_string().unwrap(), b"account");
        assert_eq!(r.remaining(), 0);
    }
}
