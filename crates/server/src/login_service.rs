//! The login-server connection handler: read one login frame, authenticate,
//! reply with the character list (or an error), all XTEA-encrypted.
//!
//! This is the composition root for the login flow — it is the only place that
//! ties together `net` framing, `protocol` parsing/crypto, and `persistence`.

use anyhow::Result;
use persistence::Store;
use protocol::charlist::{self, CharacterList, World};
use protocol::rsa::RsaPrivateKey;
use protocol::{frame, login, xtea};
use tokio::io::{AsyncRead, AsyncWrite};
use tracing::{debug, info, warn};

/// Static bits the login response needs that come from server config.
#[derive(Debug, Clone)]
pub struct LoginConfig {
    /// Advertised world name.
    pub world_name: String,
    /// Advertised game-server host/IP.
    pub host: String,
    /// Advertised game-server port.
    pub game_port: u16,
    /// Optional Message Of The Day text.
    pub motd: Option<String>,
    /// MOTD revision number.
    pub motd_num: u32,
}

/// Handle a single login connection on `stream`.
pub async fn handle_login<S>(
    stream: &mut S,
    store: &Store,
    rsa: &RsaPrivateKey,
    cfg: &LoginConfig,
) -> Result<()>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    // First (and for M1, only) frame: the login request. Plaintext + checksum,
    // not yet XTEA-encrypted — the key lives inside its RSA block.
    let Some(inner) = net::frame::read_frame(stream).await? else {
        debug!("peer closed before sending a login request");
        return Ok(());
    };
    let request_payload = frame::verify(&inner)?;
    let request = login::parse(request_payload, rsa)?;

    let account_name = String::from_utf8_lossy(&request.account).into_owned();
    let password = String::from_utf8_lossy(&request.password).into_owned();
    let keys = xtea::expand_key(&request.xtea_key);

    let response_payload = match store.authenticate(&account_name, &password).await? {
        Some(account) => {
            info!(account = %account_name, characters = account.characters.len(), "login ok");
            let names: Vec<String> = account.characters.into_iter().map(|c| c.name).collect();
            let session_key = format!("{account_name}\n{password}\n\n0");
            CharacterList {
                motd: cfg.motd.as_deref().map(|text| (cfg.motd_num, text)),
                session_key: &session_key,
                world: World {
                    name: &cfg.world_name,
                    host: &cfg.host,
                    port: cfg.game_port,
                },
                characters: &names,
                premium_ends_at: 0,
            }
            .encode()
        }
        None => {
            warn!(account = %account_name, "login rejected: bad credentials");
            charlist::build_error("Account name or password is not correct.", request.version)
        }
    };

    // The response is XTEA-encrypted (TFS enables encryption right after reading
    // the key) and then checksummed before the length prefix is added.
    let body = xtea::encrypt_message(&response_payload, &keys);
    let response_inner = frame::checksummed(&body);
    net::frame::write_frame(stream, &response_inner).await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use protocol::message::MessageReader;
    use protocol::{frame, login, xtea};

    fn test_config() -> LoginConfig {
        LoginConfig {
            world_name: "Rusted".to_string(),
            host: "127.0.0.1".to_string(),
            game_port: 7172,
            motd: Some("Hello tester".to_string()),
            motd_num: 7,
        }
    }

    /// Frame a client login request the way the wire expects: checksum + length.
    async fn send_login<W: AsyncWrite + Unpin>(
        client: &mut W,
        key: [u32; 4],
        account: &[u8],
        password: &[u8],
    ) {
        let payload = login::build_request(2, 1098, key, account, password);
        let inner = frame::checksummed(&payload);
        net::frame::write_frame(client, &inner).await.unwrap();
    }

    /// Read one response frame and XTEA-decrypt it back to a payload.
    async fn recv_response<R: AsyncRead + Unpin>(client: &mut R, key: [u32; 4]) -> Vec<u8> {
        let inner = net::frame::read_frame(client).await.unwrap().expect("a response frame");
        let body = frame::verify(&inner).unwrap();
        xtea::decrypt_message(body, &xtea::expand_key(&key)).unwrap()
    }

    #[tokio::test]
    async fn valid_login_yields_motd_and_character_list() {
        let store = Store::open_in_memory().await.unwrap();
        store.seed_test_account().await.unwrap();
        let rsa = RsaPrivateKey::open_tibia();
        let cfg = test_config();
        let key = [0x1111_2222, 0x3333_4444, 0x5555_6666, 0x7777_8888];

        let (mut client, mut server) = tokio::io::duplex(4096);
        send_login(&mut client, key, b"test", b"test").await;
        handle_login(&mut server, &store, &rsa, &cfg).await.unwrap();

        let payload = recv_response(&mut client, key).await;
        let mut r = MessageReader::new(&payload);

        assert_eq!(r.read_u8().unwrap(), protocol::charlist::OPCODE_MOTD);
        assert_eq!(r.read_string().unwrap(), b"7\nHello tester");
        assert_eq!(r.read_u8().unwrap(), protocol::charlist::OPCODE_SESSION_KEY);
        let _session = r.read_string().unwrap();
        assert_eq!(r.read_u8().unwrap(), protocol::charlist::OPCODE_CHARACTER_LIST);
        assert_eq!(r.read_u8().unwrap(), 1); // world count
        assert_eq!(r.read_u8().unwrap(), 0); // world id
        assert_eq!(r.read_string().unwrap(), b"Rusted");
        assert_eq!(r.read_string().unwrap(), b"127.0.0.1");
        assert_eq!(r.read_u16().unwrap(), 7172);
        assert_eq!(r.read_u8().unwrap(), 0); // preview
        assert_eq!(r.read_u8().unwrap(), 2); // character count
        r.read_u8().unwrap();
        assert_eq!(r.read_string().unwrap(), b"Test Knight");
        r.read_u8().unwrap();
        assert_eq!(r.read_string().unwrap(), b"Test Sorcerer");
    }

    #[tokio::test]
    async fn wrong_password_yields_an_error_response() {
        let store = Store::open_in_memory().await.unwrap();
        store.seed_test_account().await.unwrap();
        let rsa = RsaPrivateKey::open_tibia();
        let cfg = test_config();
        let key = [1, 2, 3, 4];

        let (mut client, mut server) = tokio::io::duplex(4096);
        send_login(&mut client, key, b"test", b"wrong").await;
        handle_login(&mut server, &store, &rsa, &cfg).await.unwrap();

        let payload = recv_response(&mut client, key).await;
        assert_eq!(payload[0], protocol::charlist::OPCODE_ERROR);
    }
}
