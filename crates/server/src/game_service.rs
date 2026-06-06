//! Game-port connection handler: challenge -> parse game-login -> validate ->
//! enableXTEA -> enter-world burst. Mirrors `login_service` but for ProtocolGame.

use anyhow::Result;
use protocol::map_description::{self, Center, PlacedCreature};
use protocol::rsa::RsaPrivateKey;
use protocol::{creature, enter_world, frame, game_login, walk, xtea};
use tokio::io::{AsyncRead, AsyncWrite};
use world::game::{MoveOutcome, WorldHandle};
use world::{Direction, Position};

pub const CLIENT_VERSION_MIN: u16 = 1097;
pub const CLIENT_VERSION_MAX: u16 = 1098;
const OS_OTCLIENT_LINUX: u16 = 10;

/// Fixed outfit for the M4 Test Knight (a visible male citizen look).
fn knight_outfit() -> creature::Outfit {
    creature::Outfit { look_type: 128, head: 78, body: 69, legs: 58, feet: 76, addons: 0, mount: 0 }
}

/// Serialize the player as an (unknown) creature thing for the initial 0x64.
fn player_creature_bytes(id: u32, name: &[u8], direction: u8) -> Vec<u8> {
    let view = creature::CreatureView {
        id,
        name,
        health_percent: 100,
        direction,
        outfit: knight_outfit(),
        light_level: 0,
        light_color: 0,
        speed: 220,
    };
    creature::add_creature(&view, false, 0)
}

/// Build the full enter-world burst payload, in order, for a fresh login.
pub fn build_enter_world_burst(
    snapshot_id: u32,
    center: Center,
    direction: u8,
    name: &[u8],
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

    let placed = [PlacedCreature {
        x: center.x,
        y: center.y,
        z: center.z,
        bytes: player_creature_bytes(snapshot_id, name, direction),
    }];

    let mut burst = Vec::new();
    burst.extend(enter_world::self_info(snapshot_id));
    burst.extend(enter_world::pending_state());
    burst.extend(enter_world::enter_world());
    burst.extend(map_description::encode(center, map, &placed));
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
    let burst = build_enter_world_burst(
        snapshot.id,
        center,
        snapshot.direction.to_byte(),
        name.as_bytes(),
        world.map.as_ref(),
    );
    send_encrypted(stream, &keys, &burst).await?;
    tracing::info!(character = %name, id = snapshot.id, ?center, "player entered game");

    let session = Session { id: snapshot.id, pos: snapshot.position, facing: snapshot.direction };
    run_session(stream, &keys, world, session).await
}

/// Game opcode `0x1D` — ping. TFS sends it to keep the connection alive.
const PING_OPCODE: u8 = 0x1D;
const PING_INTERVAL: std::time::Duration = std::time::Duration::from_secs(10);

/// Per-connection mutable player state the dispatcher walks/turns.
struct Session {
    id: u32,
    pos: Position,
    facing: Direction,
}

/// Keep a connected session alive, dispatching client walk/turn packets.
async fn run_session<S>(
    stream: &mut S,
    keys: &xtea::RoundKeys,
    world: &WorldHandle,
    mut session: Session,
) -> Result<()>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    loop {
        match tokio::time::timeout(PING_INTERVAL, net::frame::read_frame(stream)).await {
            Ok(frame) => {
                let Some(raw) = frame? else {
                    break; // client disconnected
                };
                let body = frame::verify(&raw)?;
                let payload = match xtea::decrypt_message(body, keys) {
                    Ok(p) => p,
                    Err(e) => {
                        tracing::debug!(?e, "dropping undecryptable frame");
                        continue;
                    }
                };
                if let Some(&opcode) = payload.first() {
                    handle_client_packet(stream, keys, world, &mut session, opcode).await?;
                }
            }
            Err(_elapsed) => {
                send_encrypted(stream, keys, &[PING_OPCODE]).await?;
            }
        }
    }
    Ok(())
}

/// Map an incoming opcode to a (direction, is_turn) action, or `None` to drain.
fn opcode_action(opcode: u8) -> Option<(Direction, bool)> {
    match opcode {
        0x65 => Some((Direction::North, false)),
        0x66 => Some((Direction::East, false)),
        0x67 => Some((Direction::South, false)),
        0x68 => Some((Direction::West, false)),
        0x6A => Some((Direction::NorthEast, false)),
        0x6B => Some((Direction::SouthEast, false)),
        0x6C => Some((Direction::SouthWest, false)),
        0x6D => Some((Direction::NorthWest, false)),
        0x6F => Some((Direction::North, true)),
        0x70 => Some((Direction::East, true)),
        0x71 => Some((Direction::South, true)),
        0x72 => Some((Direction::West, true)),
        _ => None, // pong (0x1E), unknown -> drain; logout handled by EOF
    }
}

async fn handle_client_packet<S>(
    stream: &mut S,
    keys: &xtea::RoundKeys,
    world: &WorldHandle,
    session: &mut Session,
    opcode: u8,
) -> Result<()>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let Some((direction, is_turn)) = opcode_action(opcode) else {
        return Ok(());
    };

    if is_turn {
        if let Some(facing) = world.turn_player(session.id, direction).await {
            session.facing = facing;
            let pos = (session.pos.x, session.pos.y, session.pos.z);
            let pkt = walk::creature_turn(pos, 1, session.id, facing.to_byte());
            send_encrypted(stream, keys, &pkt).await?;
        }
        return Ok(());
    }

    if let Some(res) = world.move_player(session.id, direction).await {
        session.facing = res.facing;
        match res.outcome {
            MoveOutcome::Moved { from, to } => {
                session.pos = to;
                let pkt = walk::walk_update(
                    (from.x, from.y, from.z),
                    (to.x, to.y, to.z),
                    world.map.as_ref(),
                    &[],
                );
                send_encrypted(stream, keys, &pkt).await?;
            }
            MoveOutcome::Blocked => {
                let pkt = walk::cancel_walk(res.facing.to_byte());
                send_encrypted(stream, keys, &pkt).await?;
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
            tiles: vec![
                MapTile {
                    x: 95,
                    y: 117,
                    z: 7,
                    flags: 0,
                    house_id: None,
                    items: vec![MapItem { id: 100, contents: vec![] }],
                },
                MapTile {
                    x: 96,
                    y: 117,
                    z: 7,
                    flags: 0,
                    house_id: None,
                    items: vec![MapItem { id: 100, contents: vec![] }],
                },
            ],
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

        // Send a walk-east (0x66) and expect a 0x6D creature move back.
        let mut walk_pkt = protocol::message::MessageWriter::new();
        walk_pkt.write_u8(0x66);
        let body = xtea::encrypt_message(&walk_pkt.into_bytes(), &keys);
        let inner = frame::checksummed(&body);
        net::frame::write_frame(&mut client, &inner).await.unwrap();

        let move_raw = net::frame::read_frame(&mut client).await.unwrap().unwrap();
        let move_pkt = xtea::decrypt_message(frame::verify(&move_raw).unwrap(), &keys).unwrap();
        assert_eq!(move_pkt[0], walk::OP_CREATURE_MOVE);

        // Closing the client makes `run_session` see EOF and return; without this
        // the handler would hold the session open (pinging) until timeout.
        drop(client);
        server_task.await.unwrap();
    }
}
