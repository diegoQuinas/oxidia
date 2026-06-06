//! Login-handshake sniffing proxy.
//!
//! Sits between an OTClient and the real login server, forwarding bytes while
//! hexdumping every frame — raw, and decrypted. It RSA-decrypts the client's
//! login block (bundled OpenTibia key) to recover the XTEA session key, then
//! XTEA-decrypts the server's character-list response. This is the first tool
//! to reach for when a frame is garbage: it shows exactly where checksum, XTEA
//! padding, or inner-vs-outer length go wrong.
//!
//! Usage:
//!   cargo run -p server --example sniff -- [listen_addr] [upstream_addr]
//!   defaults: 127.0.0.1:7171  ->  127.0.0.1:7271
//!
//! Point the client at `listen_addr` and run the real server on `upstream_addr`
//! (set a different login_port in its config).

use std::fmt::Write as _;

use protocol::rsa::RsaPrivateKey;
use protocol::{frame, login, xtea};
use tokio::net::{TcpListener, TcpStream};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut args = std::env::args().skip(1);
    let listen = args.next().unwrap_or_else(|| "127.0.0.1:7171".to_string());
    let upstream = args.next().unwrap_or_else(|| "127.0.0.1:7271".to_string());

    let listener = TcpListener::bind(&listen).await?;
    eprintln!("sniff: listening on {listen}, forwarding to {upstream}");

    loop {
        let (client, peer) = listener.accept().await?;
        let upstream = upstream.clone();
        tokio::spawn(async move {
            eprintln!("\n=== connection from {peer} ===");
            if let Err(error) = proxy_login(client, &upstream).await {
                eprintln!("sniff: connection error: {error}");
            }
        });
    }
}

/// Proxy and decode a single login handshake (one request, one response).
async fn proxy_login(
    mut client: TcpStream,
    upstream: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    let mut server = TcpStream::connect(upstream).await?;

    // --- client -> server: the login request ---
    let Some(request_inner) = net::frame::read_frame(&mut client).await? else {
        eprintln!("client closed before sending a request");
        return Ok(());
    };
    dump("C->S raw frame", &request_inner);

    match frame::verify(&request_inner) {
        Ok(payload) => match login::parse(payload, &RsaPrivateKey::open_tibia()) {
            Ok(request) => {
                eprintln!(
                    "  decoded login: os={} version={} account={:?} key={:08x?}",
                    request.os,
                    request.version,
                    String::from_utf8_lossy(&request.account),
                    request.xtea_key,
                );
                // Forward verbatim, then decode the response with the sniffed key.
                net::frame::write_frame(&mut server, &request_inner).await?;
                relay_response(&mut server, &mut client, request.xtea_key).await?;
            }
            Err(error) => {
                eprintln!("  login parse failed: {error}");
                net::frame::write_frame(&mut server, &request_inner).await?;
            }
        },
        Err(error) => {
            eprintln!("  checksum verify failed: {error}");
            net::frame::write_frame(&mut server, &request_inner).await?;
        }
    }

    Ok(())
}

/// Read the server's response, decode it with `key`, and forward it to `client`.
async fn relay_response(
    server: &mut TcpStream,
    client: &mut TcpStream,
    key: [u32; 4],
) -> Result<(), Box<dyn std::error::Error>> {
    let Some(response_inner) = net::frame::read_frame(server).await? else {
        eprintln!("server closed without a response");
        return Ok(());
    };
    dump("S->C raw frame", &response_inner);

    match frame::verify(&response_inner) {
        Ok(body) => match xtea::decrypt_message(body, &xtea::expand_key(&key)) {
            Ok(payload) => dump("S->C decrypted payload", &payload),
            Err(error) => eprintln!("  xtea decrypt failed: {error}"),
        },
        Err(error) => eprintln!("  checksum verify failed: {error}"),
    }

    net::frame::write_frame(client, &response_inner).await?;
    Ok(())
}

/// Print a labelled hexdump: offset, 16 hex bytes, ASCII gutter.
fn dump(label: &str, bytes: &[u8]) {
    eprintln!("  {label} ({} bytes):", bytes.len());
    for (offset, chunk) in bytes.chunks(16).enumerate() {
        let mut hex = String::new();
        let mut ascii = String::new();
        for b in chunk {
            let _ = write!(hex, "{b:02x} ");
            ascii.push(if b.is_ascii_graphic() || *b == b' ' {
                *b as char
            } else {
                '.'
            });
        }
        eprintln!("    {:04x}  {:<48} {}", offset * 16, hex, ascii);
    }
}
