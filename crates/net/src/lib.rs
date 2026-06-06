#![forbid(unsafe_code)]

//! TCP listener and connection lifecycle for the two Tibia protocols.
//!
//! M0 only accepts connections and logs their lifecycle. M1 adds framing
//! (2-byte LE length prefix + Adler-32) and hands payloads to `protocol`.

pub mod frame;

use std::io;
use std::net::SocketAddr;

use thiserror::Error;
use tokio::io::AsyncReadExt;
use tokio::net::{TcpListener, TcpStream};
use tracing::{debug, info, warn};

/// Which protocol a listener speaks. Used for log context for now.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Protocol {
    /// Login protocol, conventionally port 7171.
    Login,
    /// Game protocol, conventionally port 7172.
    Game,
}

impl Protocol {
    /// Lower-case label used in tracing spans.
    pub const fn label(self) -> &'static str {
        match self {
            Protocol::Login => "login",
            Protocol::Game => "game",
        }
    }
}

/// Errors raised while binding or running a listener.
#[derive(Debug, Error)]
pub enum NetError {
    #[error("failed to bind {proto} listener on {addr}: {source}")]
    Bind {
        proto: &'static str,
        addr: SocketAddr,
        #[source]
        source: io::Error,
    },
}

/// Bind a listener for `proto` on `addr` and serve connections until the task
/// is cancelled. Each accepted connection is handled on its own task.
///
/// In M0 a connection handler simply logs connect/disconnect and drains bytes.
pub async fn serve(proto: Protocol, addr: SocketAddr) -> Result<(), NetError> {
    let listener = TcpListener::bind(addr).await.map_err(|source| NetError::Bind {
        proto: proto.label(),
        addr,
        source,
    })?;

    info!(protocol = proto.label(), %addr, "listening");

    loop {
        match listener.accept().await {
            Ok((stream, peer)) => {
                info!(protocol = proto.label(), %peer, "connection accepted");
                tokio::spawn(async move {
                    handle_connection(proto, stream, peer).await;
                });
            }
            Err(source) => {
                // Transient accept errors should not kill the listener.
                warn!(protocol = proto.label(), %addr, error = %source, "accept failed");
            }
        }
    }
}

/// Bind a listener for `proto` on `addr` and serve each connection by handing
/// the accepted stream to `handler`. The handler owns the whole per-connection
/// lifecycle (framing, protocol, response); this function only does accept and
/// spawn. Keeping the handler a caller-supplied closure lets `net` stay free of
/// any dependency on `protocol` or `persistence`.
pub async fn serve_with<H, Fut>(
    proto: Protocol,
    addr: SocketAddr,
    handler: H,
) -> Result<(), NetError>
where
    H: Fn(TcpStream, SocketAddr) -> Fut + Clone + Send + 'static,
    Fut: std::future::Future<Output = ()> + Send + 'static,
{
    let listener = TcpListener::bind(addr).await.map_err(|source| NetError::Bind {
        proto: proto.label(),
        addr,
        source,
    })?;

    info!(protocol = proto.label(), %addr, "listening");

    loop {
        match listener.accept().await {
            Ok((stream, peer)) => {
                info!(protocol = proto.label(), %peer, "connection accepted");
                let handler = handler.clone();
                tokio::spawn(async move {
                    handler(stream, peer).await;
                    info!(protocol = proto.label(), %peer, "connection closed");
                });
            }
            Err(source) => {
                warn!(protocol = proto.label(), %addr, error = %source, "accept failed");
            }
        }
    }
}

/// M0 connection handler: drain the stream and log when the peer goes away.
async fn handle_connection(proto: Protocol, mut stream: TcpStream, peer: SocketAddr) {
    let mut buf = [0u8; 4096];
    loop {
        match stream.read(&mut buf).await {
            Ok(0) => {
                info!(protocol = proto.label(), %peer, "connection closed");
                return;
            }
            Ok(n) => {
                debug!(protocol = proto.label(), %peer, bytes = n, "received raw bytes");
            }
            Err(source) => {
                warn!(protocol = proto.label(), %peer, error = %source, "read error");
                return;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn protocol_labels_are_stable() {
        assert_eq!(Protocol::Login.label(), "login");
        assert_eq!(Protocol::Game.label(), "game");
    }

    #[tokio::test]
    async fn serve_accepts_and_drains_a_connection() {
        use tokio::io::AsyncWriteExt;
        use tokio::net::TcpStream as ClientStream;

        // Bind on an ephemeral port so the test is hermetic.
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        // Re-implement the accept loop body inline against the bound listener
        // so we exercise handle_connection without racing on a fixed port.
        let server = tokio::spawn(async move {
            let (stream, peer) = listener.accept().await.unwrap();
            handle_connection(Protocol::Login, stream, peer).await;
        });

        let mut client = ClientStream::connect(addr).await.unwrap();
        client.write_all(b"hello").await.unwrap();
        client.shutdown().await.unwrap();
        drop(client);

        // The handler must observe EOF and return cleanly.
        server.await.unwrap();
    }
}
