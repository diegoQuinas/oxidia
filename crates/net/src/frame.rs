//! Length-prefixed frame I/O over an async byte stream.
//!
//! The wire format is `[u16 LE length][length bytes]`. This module reads and
//! writes that envelope; the *contents* (checksum, XTEA, payload) are the
//! `protocol` crate's concern. Keeping the socket plumbing here lets the codec
//! stay pure and synchronous.

use std::io;

use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

/// Largest inner frame we accept, matching TFS `NETWORKMESSAGE_MAXSIZE`.
/// Anything larger is treated as a protocol error rather than allocated.
pub const MAX_FRAME: usize = 24590;

/// Read one length-prefixed frame and return its inner bytes (checksum +
/// payload, still encrypted if the connection is past the handshake).
///
/// Returns `Ok(None)` on a clean EOF *before* any frame bytes arrive — the
/// peer closed the connection between messages.
pub async fn read_frame<R>(reader: &mut R) -> io::Result<Option<Vec<u8>>>
where
    R: AsyncRead + Unpin,
{
    let mut len_buf = [0u8; 2];
    match reader.read_exact(&mut len_buf).await {
        Ok(_) => {}
        Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(e) => return Err(e),
    }

    let len = u16::from_le_bytes(len_buf) as usize;
    if len > MAX_FRAME {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("frame length {len} exceeds maximum {MAX_FRAME}"),
        ));
    }

    let mut inner = vec![0u8; len];
    reader.read_exact(&mut inner).await?;
    Ok(Some(inner))
}

/// Write `inner` as a length-prefixed frame and flush it.
pub async fn write_frame<W>(writer: &mut W, inner: &[u8]) -> io::Result<()>
where
    W: AsyncWrite + Unpin,
{
    if inner.len() > MAX_FRAME {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("frame length {} exceeds maximum {MAX_FRAME}", inner.len()),
        ));
    }
    let len = inner.len() as u16;
    writer.write_all(&len.to_le_bytes()).await?;
    writer.write_all(inner).await?;
    writer.flush().await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn write_then_read_round_trips_a_frame() {
        let (mut client, mut server) = tokio::io::duplex(1024);

        let payload = b"\x06\x00ABCDEF".to_vec();
        write_frame(&mut client, &payload).await.unwrap();

        let got = read_frame(&mut server).await.unwrap();
        assert_eq!(got, Some(payload));
    }

    #[tokio::test]
    async fn reads_two_frames_in_sequence() {
        let (mut client, mut server) = tokio::io::duplex(1024);

        write_frame(&mut client, b"first").await.unwrap();
        write_frame(&mut client, b"second").await.unwrap();

        assert_eq!(read_frame(&mut server).await.unwrap().as_deref(), Some(&b"first"[..]));
        assert_eq!(read_frame(&mut server).await.unwrap().as_deref(), Some(&b"second"[..]));
    }

    #[tokio::test]
    async fn clean_eof_before_a_frame_returns_none() {
        let (client, mut server) = tokio::io::duplex(1024);
        drop(client); // peer hangs up with nothing in flight

        assert_eq!(read_frame(&mut server).await.unwrap(), None);
    }

    #[tokio::test]
    async fn an_oversized_length_prefix_is_an_error() {
        let (mut client, mut server) = tokio::io::duplex(8);

        // Announce a frame far larger than MAX_FRAME, then hang up.
        let bogus = (MAX_FRAME as u16).wrapping_add(1).to_le_bytes();
        client.write_all(&bogus).await.unwrap();
        drop(client);

        let err = read_frame(&mut server).await.unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
    }
}
