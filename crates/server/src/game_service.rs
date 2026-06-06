//! Game-port connection handler: challenge -> parse game-login -> validate ->
//! enableXTEA -> enter-world burst. Mirrors `login_service` but for ProtocolGame.

use anyhow::Result;
use protocol::map_description::{self, Center};
use protocol::rsa::RsaPrivateKey;
use protocol::{enter_world, frame, game_login, xtea};
use tokio::io::{AsyncRead, AsyncWrite};
use world::game::WorldHandle;

pub const CLIENT_VERSION_MIN: u16 = 1097;
pub const CLIENT_VERSION_MAX: u16 = 1098;
const OS_OTCLIENT_LINUX: u16 = 10;

/// Build the full enter-world burst payload, in order, for a fresh login.
pub fn build_enter_world_burst(
    snapshot_id: u32,
    center: Center,
    map: &impl map_description::GroundSource,
) -> Vec<u8> {
    let stats = enter_world::Stats {
        health: 150,
        max_health: 150,
        free_capacity: 40_000,
        total_capacity: 40_000,
        experience: 0,
        level: 1,
        level_percent: 0,
        mana: 0,
        max_mana: 0,
        magic_level: 0,
        soul: 100,
        stamina_minutes: 2520,
        base_speed: 220,
    };

    let mut burst = Vec::new();
    burst.extend(enter_world::self_info(snapshot_id));
    burst.extend(enter_world::pending_state());
    burst.extend(enter_world::enter_world());
    burst.extend(map_description::encode(center, map));
    burst.extend(enter_world::magic_effect(
        center.x,
        center.y,
        center.z,
        enter_world::EFFECT_TELEPORT,
    ));
    burst.extend(enter_world::empty_inventory());
    burst.extend(enter_world::stats(&stats));
    burst.extend(enter_world::skills());
    burst.extend(enter_world::world_light(215, 215));
    burst.extend(enter_world::creature_light(snapshot_id, 0, 0));
    burst.extend(enter_world::basic_data());
    burst.extend(enter_world::icons());
    burst
}

/// Handle one game connection.
pub async fn handle_game<S>(
    stream: &mut S,
    rsa: &RsaPrivateKey,
    world: &WorldHandle,
    challenge_timestamp: u32,
    challenge_random: u8,
) -> Result<()>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    // 1. Send the challenge (checksummed, NOT XTEA). Every TFS message carries an
    // inner `[u16 payload length]` (TFS `onConnect`, protocolgame.cpp:429 writes
    // 0x0006). For XTEA packets `xtea::encrypt_message` adds it; the plaintext
    // challenge must prepend it by hand, or OTClient's first-packet size check
    // fails and it never sends the game-login. The checksum covers length+payload.
    let challenge = protocol::challenge::encode(challenge_timestamp, challenge_random);
    let mut message = Vec::with_capacity(2 + challenge.len());
    message.extend_from_slice(&(challenge.len() as u16).to_le_bytes());
    message.extend_from_slice(&challenge);
    let inner = frame::checksummed(&message);
    net::frame::write_frame(stream, &inner).await?;

    // 2. Read the client's first packet (the game-login).
    let Some(raw) = net::frame::read_frame(stream).await? else {
        return Ok(()); // client closed before sending the login
    };
    let payload = frame::verify(&raw)?;
    let req = game_login::parse(payload, rsa)?;

    // 3. Validate challenge echo + version (silent disconnect on echo mismatch).
    if req.challenge_timestamp != challenge_timestamp || req.challenge_random != challenge_random {
        return Ok(());
    }
    let keys = xtea::expand_key(&req.xtea_key);
    if !(CLIENT_VERSION_MIN..=CLIENT_VERSION_MAX).contains(&req.version) {
        return send_disconnect(stream, &keys, "Only protocol 10.98 is supported.").await;
    }

    // 4. OTClient extended-opcode init.
    if req.os >= OS_OTCLIENT_LINUX {
        send_encrypted(stream, &keys, &enter_world::extended_opcode_init()).await?;
    }

    // 5. "Player load": register in the world.
    let name = String::from_utf8_lossy(&req.character_name).to_string();
    let Some(snapshot) = world.login(name.clone()).await else {
        return send_disconnect(stream, &keys, "Your character could not be loaded.").await;
    };

    // 6. Build + send the enter-world burst as one encrypted frame.
    let center =
        Center { x: snapshot.position.x, y: snapshot.position.y, z: snapshot.position.z };
    let burst = build_enter_world_burst(snapshot.id, center, world.map.as_ref());
    send_encrypted(stream, &keys, &burst).await?;
    tracing::info!(character = %name, id = snapshot.id, ?center, "player entered game");

    // 7. Hold the session open. The client times out if it receives nothing for a
    // while, so keep the link warm: drain the client's packets, and whenever it
    // goes quiet for PING_INTERVAL send a ping (`0x1D`). M4 will handle these
    // packets for real (walk, ping/pong, logout); M3 just keeps the world visible.
    run_session(stream, &keys).await
}

/// Game opcode `0x1D` — ping. TFS sends it to keep the connection alive.
const PING_OPCODE: u8 = 0x1D;
const PING_INTERVAL: std::time::Duration = std::time::Duration::from_secs(10);

/// Keep a connected session alive until the client disconnects.
async fn run_session<S>(stream: &mut S, keys: &xtea::RoundKeys) -> Result<()>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    loop {
        // Wait for a client packet, but no longer than PING_INTERVAL — when the
        // client is idle the timeout fires and we ping it. The read future is only
        // cancelled while idle (awaiting the first byte), so no partial frame is lost.
        match tokio::time::timeout(PING_INTERVAL, net::frame::read_frame(stream)).await {
            Ok(frame) => {
                if frame?.is_none() {
                    break; // client disconnected
                }
                // M3 ignores client packets (walk/ping/etc.) — drained, not parsed.
            }
            Err(_elapsed) => {
                send_encrypted(stream, keys, &[PING_OPCODE]).await?;
            }
        }
    }
    Ok(())
}

async fn send_encrypted<S>(stream: &mut S, keys: &xtea::RoundKeys, payload: &[u8]) -> Result<()>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let body = xtea::encrypt_message(payload, keys);
    let inner = frame::checksummed(&body);
    net::frame::write_frame(stream, &inner).await?;
    Ok(())
}

async fn send_disconnect<S>(
    stream: &mut S,
    keys: &xtea::RoundKeys,
    message: &str,
) -> Result<()>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let mut w = protocol::message::MessageWriter::new();
    w.write_u8(0x14);
    w.write_string(message.as_bytes());
    send_encrypted(stream, keys, &w.into_bytes()).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use formats::otb::{ItemType, ItemsOtb};
    use formats::otbm::{MapItem, MapTile, OtbmMap, Town};
    use std::sync::Arc;
    use world::map::StaticMap;

    fn test_world() -> WorldHandle {
        let items = ItemsOtb {
            major_version: 3,
            minor_version: 57,
            build_number: 0,
            items: vec![ItemType { group: 0, flags: 0, server_id: 100, client_id: 4526 }],
        };
        let map = OtbmMap {
            width: 200,
            height: 200,
            major_items: 3,
            minor_items: 57,
            description: String::new(),
            spawn_file: None,
            house_file: None,
            tiles: vec![MapTile {
                x: 95,
                y: 117,
                z: 7,
                flags: 0,
                house_id: None,
                items: vec![MapItem { id: 100, contents: vec![] }],
            }],
            towns: vec![Town { id: 1, name: "Thais".into(), x: 95, y: 117, z: 7 }],
            waypoints: vec![],
        };
        world::game::spawn(Arc::new(StaticMap::from_formats(&map, &items)))
    }

    #[tokio::test]
    async fn handshake_emits_burst_in_order() {
        let world = test_world();
        let ts = 0x1234_5678;
        let rnd = 0x42;

        let (mut client, mut server) = tokio::io::duplex(64 * 1024);

        let server_task = {
            let world = world.clone();
            tokio::spawn(async move {
                let rsa = RsaPrivateKey::open_tibia();
                handle_game(&mut server, &rsa, &world, ts, rnd).await.unwrap();
            })
        };

        // 1. Read the challenge frame. After the checksum, the message carries an
        // inner `[u16 length]` (= 6) before the 0x1F opcode, matching TFS onConnect.
        let chal_raw = net::frame::read_frame(&mut client).await.unwrap().unwrap();
        let chal = frame::verify(&chal_raw).unwrap();
        assert_eq!(u16::from_le_bytes([chal[0], chal[1]]), 6); // inner payload length
        assert_eq!(chal[2], protocol::challenge::OPCODE_CHALLENGE);
        let echoed_ts = u32::from_le_bytes([chal[3], chal[4], chal[5], chal[6]]);
        let echoed_rnd = chal[7];

        // 2. Send a game-login packet echoing the challenge.
        let key = [1u32, 2, 3, 4];
        let pkt = game_login::build_request(
            10,
            1098,
            key,
            b"test",
            b"test",
            b"Test Knight",
            echoed_ts,
            echoed_rnd,
        )
        .unwrap();
        let inner = frame::checksummed(&pkt);
        net::frame::write_frame(&mut client, &inner).await.unwrap();

        // 3. Read + decrypt the burst (extended opcode comes first for OS=10).
        let keys = xtea::expand_key(&key);
        let ext_raw = net::frame::read_frame(&mut client).await.unwrap().unwrap();
        let ext = xtea::decrypt_message(frame::verify(&ext_raw).unwrap(), &keys).unwrap();
        assert_eq!(ext[0], enter_world::OP_EXTENDED);

        let burst_raw = net::frame::read_frame(&mut client).await.unwrap().unwrap();
        let burst = xtea::decrypt_message(frame::verify(&burst_raw).unwrap(), &keys).unwrap();
        // self_info is 29 bytes for protocol 1098.
        assert_eq!(burst[0], enter_world::OP_SELF_INFO);
        let self_len = 29;
        assert_eq!(burst[self_len], enter_world::OP_PENDING_STATE);
        assert_eq!(burst[self_len + 1], enter_world::OP_ENTER_WORLD);
        assert_eq!(burst[self_len + 2], map_description::OPCODE_MAP_DESCRIPTION);

        // Closing the client makes `run_session` see EOF and return; without this
        // the handler would hold the session open (pinging) until timeout.
        drop(client);
        server_task.await.unwrap();
    }
}
