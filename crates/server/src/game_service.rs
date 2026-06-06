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
    // 1. Send the challenge (checksummed, NOT XTEA).
    let challenge = protocol::challenge::encode(challenge_timestamp, challenge_random);
    let inner = frame::checksummed(&challenge);
    net::frame::write_frame(stream, &inner).await?;

    // 2. Read the client's first packet.
    let Some(raw) = net::frame::read_frame(stream).await? else {
        return Ok(());
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
        let ext = enter_world::extended_opcode_init();
        send_encrypted(stream, &keys, &ext).await?;
    }

    // 5. "Player load": register in the world.
    let name = String::from_utf8_lossy(&req.character_name).to_string();
    let Some(snapshot) = world.login(name).await else {
        return send_disconnect(stream, &keys, "Your character could not be loaded.").await;
    };

    // 6. Build + send the burst as one encrypted frame.
    let center =
        Center { x: snapshot.position.x, y: snapshot.position.y, z: snapshot.position.z };
    let burst = build_enter_world_burst(snapshot.id, center, world.map.as_ref());
    send_encrypted(stream, &keys, &burst).await?;
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

        // 1. Read the challenge frame.
        let chal_raw = net::frame::read_frame(&mut client).await.unwrap().unwrap();
        let chal = frame::verify(&chal_raw).unwrap();
        assert_eq!(chal[0], protocol::challenge::OPCODE_CHALLENGE);
        let echoed_ts = u32::from_le_bytes([chal[1], chal[2], chal[3], chal[4]]);
        let echoed_rnd = chal[5];

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

        server_task.await.unwrap();
    }
}
