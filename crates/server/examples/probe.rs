//! Minimal login client: send one login request and decode the response.
//!
//! Acts as a stand-in for OTClient so the login flow can be exercised over real
//! sockets (directly against the server, or through the `sniff` proxy).
//!
//! Usage:
//!   cargo run -p server --example probe -- [addr] [account] [password]
//!   defaults: 127.0.0.1:7171  test  test

use protocol::message::MessageReader;
use protocol::{charlist, frame, login, xtea};
use tokio::net::TcpStream;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut args = std::env::args().skip(1);
    let addr = args.next().unwrap_or_else(|| "127.0.0.1:7171".to_string());
    let account = args.next().unwrap_or_else(|| "test".to_string());
    let password = args.next().unwrap_or_else(|| "test".to_string());

    let key = [0x1111_2222u32, 0x3333_4444, 0x5555_6666, 0x7777_8888];

    let mut stream = TcpStream::connect(&addr).await?;
    let payload = login::build_request(2, 1098, key, account.as_bytes(), password.as_bytes());
    net::frame::write_frame(&mut stream, &frame::checksummed(&payload)).await?;

    let inner = net::frame::read_frame(&mut stream)
        .await?
        .ok_or("server closed without a response")?;
    let body = frame::verify(&inner)?;
    let response = xtea::decrypt_message(body, &xtea::expand_key(&key))?;

    let mut r = MessageReader::new(&response);
    let opcode = r.read_u8()?;
    if opcode == charlist::OPCODE_ERROR || opcode == charlist::OPCODE_ERROR_OLD {
        println!("login error: {}", String::from_utf8_lossy(r.read_string()?));
        return Ok(());
    }

    if opcode == charlist::OPCODE_MOTD {
        println!("MOTD: {}", String::from_utf8_lossy(r.read_string()?));
        let _session_opcode = r.read_u8()?;
    }
    let _session = r.read_string()?; // session key
    let _list_opcode = r.read_u8()?; // 0x64
    let worlds = r.read_u8()?;
    for _ in 0..worlds {
        let _id = r.read_u8()?;
        let name = r.read_string()?.to_vec();
        let host = r.read_string()?.to_vec();
        let port = r.read_u16()?;
        let _preview = r.read_u8()?;
        println!(
            "world: {} @ {}:{}",
            String::from_utf8_lossy(&name),
            String::from_utf8_lossy(&host),
            port
        );
    }
    let count = r.read_u8()?;
    println!("{count} character(s):");
    for _ in 0..count {
        let _status = r.read_u8()?;
        let name = r.read_string()?.to_vec();
        println!("  - {}", String::from_utf8_lossy(&name));
    }

    Ok(())
}
