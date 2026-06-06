# M3 — Enter Game Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** A real OTClient Redemption (1098) connects to the game port, completes the handshake, and is rendered standing on real `forgotten.otbm` ground tiles with stats/skills/light/empty inventory.

**Architecture:** `protocol` gets pure encoders/decoders (challenge, game-login parse, enter-world burst, map description with skip-encoding). The world map is immutable in M3 → held in an `Arc<StaticMap>` implementing a `GroundSource` trait; a minimal `GameWorld` actor (tokio mpsc + oneshot) owns the mutable player registry. `server::game_service` mirrors `login_service`: challenge → parse → validate → enableXTEA → burst → encrypt+checksum.

**Tech Stack:** Rust edition 2024 (rustc ≥ 1.85), tokio, sqlx/sqlite, `#![forbid(unsafe_code)]` in every crate. Reference oracle: TFS 1.4.2 at `reference/tfs/`.

**Commits:** The repo's standing rule is *commit only when the user asks*. Each "Commit" step below is a CHECKPOINT — run the build/tests and pause; only `git commit` if the user has given the go-ahead. Use conventional-commit messages, never add Co-Authored-By.

**All commands run from `/home/tito/tibia/server`.**

---

## File structure

| File | Responsibility | New? |
|---|---|---|
| `crates/protocol/src/challenge.rs` | encode the `0x1F` challenge | create |
| `crates/protocol/src/game_login.rs` | parse the first game packet (outer + RSA block) | create |
| `crates/protocol/src/map_description.rs` | `GroundSource` trait + `0x64` viewport encoder + `add_item` | create |
| `crates/protocol/src/enter_world.rs` | the login burst encoders (`0x17/0x0A/0x0F/0xA0/0xA1/0x82/0x8D/0x78-79/0x9F/0xA2/0x83/0x32`) | create |
| `crates/protocol/src/lib.rs` | register + re-export the new modules | modify |
| `crates/world/Cargo.toml` | add `protocol`, `formats`, `tokio` deps | modify |
| `crates/world/src/lib.rs` | re-export `Position` + new modules | modify |
| `crates/world/src/map.rs` | `StaticMap` (ground lookup + spawn) impl `GroundSource` | create |
| `crates/world/src/game.rs` | `GameWorld` actor + `WorldHandle` + `PlayerSnapshot` | create |
| `crates/server/src/game_service.rs` | game handshake + burst assembly | create |
| `crates/server/src/main.rs` | load otb+otbm, spawn world, serve game port | modify |
| `crates/server/Cargo.toml` | (already has world/formats/protocol) | — |
| `PROGRESS.md` | mark M3 done + record gotchas | modify |

---

## Task 1: `protocol::challenge` — encode the `0x1F` challenge

**Files:**
- Create: `crates/protocol/src/challenge.rs`
- Modify: `crates/protocol/src/lib.rs`

- [ ] **Step 1: Write the failing test**

In `crates/protocol/src/challenge.rs`:

```rust
//! The game server's first packet: the `0x1F` login challenge.
//! Checksummed (Adler-32) but NOT XTEA-encrypted — XTEA is enabled only after
//! the client's first packet is parsed. See `reference/tfs/src/protocolgame.cpp`
//! (`ProtocolGame::onConnect`).
#![allow(clippy::module_name_repetitions)]

use crate::message::MessageWriter;

pub const OPCODE_CHALLENGE: u8 = 0x1F;

/// Encode the challenge payload: `[0x1F][u32 LE timestamp][u8 random]`.
pub fn encode(timestamp: u32, random: u8) -> Vec<u8> {
    let mut w = MessageWriter::new();
    w.write_u8(OPCODE_CHALLENGE);
    w.write_u32(timestamp);
    w.write_u8(random);
    w.into_bytes()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encodes_opcode_timestamp_and_random() {
        let bytes = encode(0x1122_3344, 0xAB);
        assert_eq!(bytes, [0x1F, 0x44, 0x33, 0x22, 0x11, 0xAB]);
    }
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p protocol challenge`
Expected: FAIL to COMPILE — `challenge` module not declared in `lib.rs`.

- [ ] **Step 3: Register the module**

In `crates/protocol/src/lib.rs`, add alongside the other `pub mod` lines (keep alphabetical with the existing `adler, charlist, frame, login, message, rsa, xtea`):

```rust
pub mod challenge;
```

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test -p protocol challenge`
Expected: PASS (1 test).

- [ ] **Step 5: Commit (checkpoint)**

```bash
git add crates/protocol/src/challenge.rs crates/protocol/src/lib.rs
git commit -m "feat(protocol): encode 0x1F game challenge packet"
```

---

## Task 2: `protocol::game_login` — parse the first game packet

The game packet differs from the login packet: outer `[u8 0x0A protocol id][u16 os][u16 version][skip 7]`, then a 128-byte RSA block decrypting to `[u8 0][u32x4 xtea][u8 gamemaster][string sessionKey][string charName][u32 challengeTs][u8 challengeRandom]`. `sessionKey` is `"account\npassword\ntoken\ntokenTime"`.

**Files:**
- Create: `crates/protocol/src/game_login.rs`
- Modify: `crates/protocol/src/lib.rs`

- [ ] **Step 1: Write the failing test (round-trip via a build helper)**

In `crates/protocol/src/game_login.rs`:

```rust
//! Parser for the first client->game packet (protocol 10.98).
//! Mirrors `reference/tfs/src/protocolgame.cpp` (`onRecvFirstMessage`).
//! Layout: `[u8 0x0A][u16 os][u16 version][7 skipped bytes][128-byte RSA block]`.
//! RSA block: `[u8 0][u32x4 xtea][u8 gamemaster][string sessionKey][string name][u32 ts][u8 rnd]`.

use crate::message::{MessageReader, MessageWriter};
use crate::rsa::{self, RsaError, RsaPrivateKey, RSA_BLOCK_SIZE};
use crate::ProtocolError;

/// ProtocolGame identifier byte (TFS `ProtocolGame::protocolIdentifier` = 0x0A).
pub const GAME_PROTOCOL_ID: u8 = 0x0A;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GameLoginRequest {
    pub os: u16,
    pub version: u16,
    pub xtea_key: [u32; 4],
    pub gamemaster: bool,
    pub account: Vec<u8>,
    pub password: Vec<u8>,
    pub character_name: Vec<u8>,
    pub challenge_timestamp: u32,
    pub challenge_random: u8,
}

#[derive(Debug, thiserror::Error)]
pub enum GameLoginError {
    #[error("unexpected protocol id byte {0:#04x}")]
    UnexpectedProtocolId(u8),
    #[error("rsa padding byte was {0}, expected 0")]
    RsaPadding(u8),
    #[error(transparent)]
    Truncated(#[from] ProtocolError),
    #[error(transparent)]
    Rsa(#[from] RsaError),
}

/// Parse a checksum-stripped game-login payload.
pub fn parse(payload: &[u8], rsa: &RsaPrivateKey) -> Result<GameLoginRequest, GameLoginError> {
    let mut r = MessageReader::new(payload);

    let id = r.read_u8()?;
    if id != GAME_PROTOCOL_ID {
        return Err(GameLoginError::UnexpectedProtocolId(id));
    }
    let os = r.read_u16()?;
    let version = r.read_u16()?;
    let _ = r.read_bytes(7)?; // u32 clientVersion + u8 clientType + u16 datRevision

    let mut block = [0u8; RSA_BLOCK_SIZE];
    block.copy_from_slice(r.read_bytes(RSA_BLOCK_SIZE)?);
    rsa.decrypt(&mut block)?;

    let mut inner = MessageReader::new(&block);
    let pad = inner.read_u8()?;
    if pad != 0 {
        return Err(GameLoginError::RsaPadding(pad));
    }
    let xtea_key = [
        inner.read_u32()?,
        inner.read_u32()?,
        inner.read_u32()?,
        inner.read_u32()?,
    ];
    let gamemaster = inner.read_u8()? != 0;
    let session_key = inner.read_string()?.to_vec();
    let character_name = inner.read_string()?.to_vec();
    let challenge_timestamp = inner.read_u32()?;
    let challenge_random = inner.read_u8()?;

    let (account, password) = split_session_key(&session_key);

    Ok(GameLoginRequest {
        os,
        version,
        xtea_key,
        gamemaster,
        account,
        password,
        character_name,
        challenge_timestamp,
        challenge_random,
    })
}

/// `sessionKey` is `account\npassword\ntoken\ntokenTime`; take the first two parts.
fn split_session_key(session_key: &[u8]) -> (Vec<u8>, Vec<u8>) {
    let mut parts = session_key.split(|&b| b == b'\n');
    let account = parts.next().unwrap_or(&[]).to_vec();
    let password = parts.next().unwrap_or(&[]).to_vec();
    (account, password)
}

/// Build a client-side game-login payload (RSA-public-encrypted) for tests/tooling.
#[allow(clippy::too_many_arguments)]
pub fn build_request(
    os: u16,
    version: u16,
    xtea_key: [u32; 4],
    account: &[u8],
    password: &[u8],
    character_name: &[u8],
    challenge_timestamp: u32,
    challenge_random: u8,
) -> Result<Vec<u8>, RsaError> {
    let mut w = MessageWriter::new();
    w.write_u8(GAME_PROTOCOL_ID);
    w.write_u16(os);
    w.write_u16(version);
    w.write_bytes(&[0u8; 7]); // clientVersion + clientType + datRevision

    let mut block = vec![0u8; RSA_BLOCK_SIZE];
    {
        let mut inner = MessageWriter::new();
        inner.write_u8(0); // padding sentinel
        for k in xtea_key {
            inner.write_u32(k);
        }
        inner.write_u8(0); // gamemaster
        let mut session = Vec::new();
        session.extend_from_slice(account);
        session.push(b'\n');
        session.extend_from_slice(password);
        session.extend_from_slice(b"\n\n0"); // empty token + tokenTime
        inner.write_string(&session);
        inner.write_string(character_name);
        inner.write_u32(challenge_timestamp);
        inner.write_u8(challenge_random);
        let bytes = inner.into_bytes();
        assert!(bytes.len() <= RSA_BLOCK_SIZE, "rsa inner block overflow");
        block[..bytes.len()].copy_from_slice(&bytes);
    }
    rsa::encrypt_open_tibia_public(&mut block)?;
    w.write_bytes(&block);
    Ok(w.into_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_a_built_request() {
        let key = [0x1111_1111, 0x2222_2222, 0x3333_3333, 0x4444_4444];
        let payload = build_request(
            10, 1098, key, b"test", b"test", b"Test Knight", 0xDEAD_BEEF, 0x7C,
        )
        .unwrap();

        let rsa = RsaPrivateKey::open_tibia();
        let req = parse(&payload, &rsa).unwrap();

        assert_eq!(req.os, 10);
        assert_eq!(req.version, 1098);
        assert_eq!(req.xtea_key, key);
        assert!(!req.gamemaster);
        assert_eq!(req.account, b"test");
        assert_eq!(req.password, b"test");
        assert_eq!(req.character_name, b"Test Knight");
        assert_eq!(req.challenge_timestamp, 0xDEAD_BEEF);
        assert_eq!(req.challenge_random, 0x7C);
    }

    #[test]
    fn rejects_wrong_protocol_id() {
        let rsa = RsaPrivateKey::open_tibia();
        let err = parse(&[0x01, 0, 0], &rsa).unwrap_err();
        assert!(matches!(err, GameLoginError::UnexpectedProtocolId(0x01)));
    }
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p protocol game_login`
Expected: FAIL to COMPILE — `game_login` not declared.

- [ ] **Step 3: Register the module**

In `crates/protocol/src/lib.rs` add:

```rust
pub mod game_login;
```

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test -p protocol game_login`
Expected: PASS (2 tests).

- [ ] **Step 5: Commit (checkpoint)**

```bash
git add crates/protocol/src/game_login.rs crates/protocol/src/lib.rs
git commit -m "feat(protocol): parse the game-login packet (RSA block + session key)"
```

---

## Task 3: `protocol::map_description` — `0x64` viewport + skip encoding

**Files:**
- Create: `crates/protocol/src/map_description.rs`
- Modify: `crates/protocol/src/lib.rs`

- [ ] **Step 1: Write the failing test (encode → re-parse round-trip)**

In `crates/protocol/src/map_description.rs`:

```rust
//! `0x64` map description for protocol 10.98.
//! Mirrors `reference/tfs/src/protocolgame.cpp` (`GetMapDescription`/`GetFloorDescription`).
//! Viewport is 18 wide x 14 tall; overground walks floors 7->0. Empty tiles are
//! run-length "skip"-encoded: `[u8 skip][u8 0xFF]` flushes a run; `[0xFF][0xFF]`
//! flushes a full run of 255.

use crate::message::MessageWriter;

pub const OPCODE_MAP_DESCRIPTION: u8 = 0x64;
pub const MARK_UNMARKED: u8 = 0xFF;

pub const VIEWPORT_WIDTH: i32 = 18;
pub const VIEWPORT_HEIGHT: i32 = 14;
const ANCHOR_DX: i32 = 8; // (VIEWPORT_WIDTH / 2) - 1
const ANCHOR_DY: i32 = 6; // (VIEWPORT_HEIGHT / 2) - 1

/// Provides the ground item's client id at a world coordinate, or `None` if the
/// tile has no ground (empty / out of bounds).
pub trait GroundSource {
    fn ground(&self, x: i32, y: i32, z: i32) -> Option<u16>;
}

/// A position the encoder centers the viewport on.
#[derive(Debug, Clone, Copy)]
pub struct Center {
    pub x: u16,
    pub y: u16,
    pub z: u8,
}

/// Encode a full `0x64` map description centered on `center`.
/// M3 supports overground centers (z <= 7) only.
pub fn encode<S: GroundSource>(center: Center, src: &S) -> Vec<u8> {
    let mut w = MessageWriter::new();
    w.write_u8(OPCODE_MAP_DESCRIPTION);
    w.write_u16(center.x);
    w.write_u16(center.y);
    w.write_u8(center.z);
    write_tiles(&mut w, center, src);
    w.into_bytes()
}

fn write_tiles<S: GroundSource>(w: &mut MessageWriter, center: Center, src: &S) {
    let anchor_x = center.x as i32 - ANCHOR_DX;
    let anchor_y = center.y as i32 - ANCHOR_DY;

    // Overground: floors 7 down to 0.
    let mut skip: i32 = -1;
    for nz in (0..=7i32).rev() {
        let offset = center.z as i32 - nz;
        for nx in 0..VIEWPORT_WIDTH {
            for ny in 0..VIEWPORT_HEIGHT {
                let wx = anchor_x + nx + offset;
                let wy = anchor_y + ny + offset;
                match src.ground(wx, wy, nz) {
                    Some(client_id) => {
                        if skip >= 0 {
                            w.write_u8(skip as u8);
                            w.write_u8(0xFF);
                        }
                        skip = 0;
                        w.write_u16(0x0000); // environmental effects placeholder
                        add_item(w, client_id);
                    }
                    None => {
                        skip += 1;
                        if skip == 0xFE {
                            w.write_u8(0xFF);
                            w.write_u8(0xFF);
                            skip = -1;
                        }
                    }
                }
            }
        }
    }
    if skip >= 0 {
        w.write_u8(skip as u8);
        w.write_u8(0xFF);
    }
}

/// Minimal item serialization for a ground tile: `[u16 clientId][u8 0xFF]`.
/// (Stackable count / animation phase are not needed for M3 ground.)
fn add_item(w: &mut MessageWriter, client_id: u16) {
    w.write_u16(client_id);
    w.write_u8(MARK_UNMARKED);
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    struct MapStub(HashMap<(i32, i32, i32), u16>);
    impl GroundSource for MapStub {
        fn ground(&self, x: i32, y: i32, z: i32) -> Option<u16> {
            self.0.get(&(x, y, z)).copied()
        }
    }

    /// Decode the tile stream back into a {(nx,ny,nz)->client_id} map so we can
    /// assert correctness without hand-computing 1900+ skip bytes.
    fn decode_stream(bytes: &[u8], center: Center) -> HashMap<(i32, i32, i32), u16> {
        assert_eq!(bytes[0], OPCODE_MAP_DESCRIPTION);
        // skip [u8 x][u8 y][u8 z] header (5 bytes after opcode = u16+u16+u8)
        let mut p = 6usize;
        let anchor_x = center.x as i32 - ANCHOR_DX;
        let anchor_y = center.y as i32 - ANCHOR_DY;
        let mut found = HashMap::new();
        for nz in (0..=7i32).rev() {
            let offset = center.z as i32 - nz;
            let mut idx = 0i32; // position within this floor, nx*HEIGHT + ny
            while idx < VIEWPORT_WIDTH * VIEWPORT_HEIGHT {
                // peek: is this a tile (env u16 then item) or a skip run?
                // A skip run is [count][0xFF]; tiles start with env effects 0x0000.
                // We disambiguate structurally: the encoder always emits a skip
                // pair before a tile and a trailing pair, so read greedily.
                let b0 = bytes[p];
                let b1 = bytes[p + 1];
                if b1 == 0xFF {
                    // skip run of `b0` empties (b0 may be 0xFF meaning 255)
                    let run = if b0 == 0xFF { 255 } else { b0 as i32 };
                    idx += run;
                    p += 2;
                    if idx >= VIEWPORT_WIDTH * VIEWPORT_HEIGHT {
                        break;
                    }
                    // a real tile follows the run unless we've consumed the floor
                    let env = u16::from_le_bytes([bytes[p], bytes[p + 1]]);
                    assert_eq!(env, 0x0000);
                    let client_id = u16::from_le_bytes([bytes[p + 2], bytes[p + 3]]);
                    assert_eq!(bytes[p + 4], MARK_UNMARKED);
                    let nx = idx / VIEWPORT_HEIGHT;
                    let ny = idx % VIEWPORT_HEIGHT;
                    found.insert((anchor_x + nx + offset, anchor_y + ny + offset, nz), client_id);
                    p += 5;
                    idx += 1;
                } else {
                    panic!("unexpected stream byte at {p}: {b0:#04x} {b1:#04x}");
                }
            }
        }
        found
    }

    #[test]
    fn header_carries_center_position() {
        let stub = MapStub(HashMap::new());
        let bytes = encode(Center { x: 1000, y: 1000, z: 7 }, &stub);
        assert_eq!(bytes[0], OPCODE_MAP_DESCRIPTION);
        assert_eq!(u16::from_le_bytes([bytes[1], bytes[2]]), 1000);
        assert_eq!(u16::from_le_bytes([bytes[3], bytes[4]]), 1000);
        assert_eq!(bytes[5], 7);
    }

    #[test]
    fn empty_map_is_only_skip_flushes() {
        let stub = MapStub(HashMap::new());
        let bytes = encode(Center { x: 1000, y: 1000, z: 7 }, &stub);
        // Header (6) then nothing but skip pairs; the decoder must find no tiles.
        let found = decode_stream(&bytes, Center { x: 1000, y: 1000, z: 7 });
        assert!(found.is_empty());
    }

    #[test]
    fn single_ground_tile_at_center_round_trips() {
        let center = Center { x: 1000, y: 1000, z: 7 };
        let mut m = HashMap::new();
        // player's own floor (offset 0) → tile at (center.x, center.y, 7)
        m.insert((1000, 1000, 7), 4526u16);
        let stub = MapStub(m);
        let bytes = encode(center, &stub);
        let found = decode_stream(&bytes, center);
        assert_eq!(found.get(&(1000, 1000, 7)), Some(&4526));
        assert_eq!(found.len(), 1);
    }
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p protocol map_description`
Expected: FAIL to COMPILE — module not declared.

- [ ] **Step 3: Register the module**

In `crates/protocol/src/lib.rs` add:

```rust
pub mod map_description;
```

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test -p protocol map_description`
Expected: PASS (3 tests).

- [ ] **Step 5: Commit (checkpoint)**

```bash
git add crates/protocol/src/map_description.rs crates/protocol/src/lib.rs
git commit -m "feat(protocol): encode 0x64 map description with skip encoding"
```

---

## Task 4: `protocol::enter_world` — the login burst encoders

All encoders return the **payload** (opcode + fields); the caller concatenates them into one buffer before XTEA. Byte layouts come from the TFS 1.4.2 spec (protocolgame.cpp).

**Files:**
- Create: `crates/protocol/src/enter_world.rs`
- Modify: `crates/protocol/src/lib.rs`

- [ ] **Step 1: Write the failing tests**

In `crates/protocol/src/enter_world.rs`:

```rust
//! Encoders for the enter-world login burst (protocol 10.98).
//! Mirrors `reference/tfs/src/protocolgame.cpp` (`sendAddCreature` self path and
//! the AddPlayerStats/AddPlayerSkills/light helpers). Each function returns one
//! packet's payload (opcode + fields); the caller concatenates them.

use crate::message::MessageWriter;

pub const OP_SELF_INFO: u8 = 0x17;
pub const OP_PENDING_STATE: u8 = 0x0A;
pub const OP_ENTER_WORLD: u8 = 0x0F;
pub const OP_STATS: u8 = 0xA0;
pub const OP_SKILLS: u8 = 0xA1;
pub const OP_WORLD_LIGHT: u8 = 0x82;
pub const OP_CREATURE_LIGHT: u8 = 0x8D;
pub const OP_INVENTORY_SET: u8 = 0x78;
pub const OP_INVENTORY_EMPTY: u8 = 0x79;
pub const OP_BASIC_DATA: u8 = 0x9F;
pub const OP_ICONS: u8 = 0xA2;
pub const OP_MAGIC_EFFECT: u8 = 0x83;
pub const OP_EXTENDED: u8 = 0x32;

pub const INVENTORY_SLOTS: u8 = 11;
pub const EFFECT_TELEPORT: u8 = 10; // CONST_ME_TELEPORT

/// Variable stats the client renders; the rest are M3 constants baked in.
#[derive(Debug, Clone, Copy)]
pub struct Stats {
    pub health: u16,
    pub max_health: u16,
    pub free_capacity: u32, // oz * 100
    pub total_capacity: u32,
    pub experience: u32,
    pub level: u16,
    pub level_percent: u8,
    pub mana: u16,
    pub max_mana: u16,
    pub magic_level: u8,
    pub soul: u8,
    pub stamina_minutes: u16,
    pub base_speed: u16,
}

/// TFS `addDouble(value, precision)`: `[u8 precision][u32 (value*10^precision)+i32::MAX]`.
fn add_double(w: &mut MessageWriter, value: f64, precision: u8) {
    w.write_u8(precision);
    let encoded = (value * 10f64.powi(precision as i32)) + i32::MAX as f64;
    w.write_u32(encoded as u32);
}

/// `0x17` self-info: player id, beat duration, speed formula, store config.
pub fn self_info(player_id: u32) -> Vec<u8> {
    let mut w = MessageWriter::new();
    w.write_u8(OP_SELF_INFO);
    w.write_u32(player_id);
    w.write_u16(0x0032); // beat duration = 50ms
    add_double(&mut w, 857.36, 3); // speed A
    add_double(&mut w, 261.29, 3); // speed B
    add_double(&mut w, -4795.01, 3); // speed C
    w.write_u8(0); // can report bugs
    w.write_u8(0); // can change pvp framing
    w.write_u8(0); // expert mode
    w.write_u16(0); // store images url (empty string length)
    w.write_u16(25); // premium coin package size
    w.into_bytes()
}

/// `0x0A` pending-state-entered (opcode only).
pub fn pending_state() -> Vec<u8> {
    vec![OP_PENDING_STATE]
}

/// `0x0F` enter-world (opcode only).
pub fn enter_world() -> Vec<u8> {
    vec![OP_ENTER_WORLD]
}

/// `0xA0` player stats. Constant fields use sane M3 defaults.
pub fn stats(s: &Stats) -> Vec<u8> {
    let mut w = MessageWriter::new();
    w.write_u8(OP_STATS);
    w.write_u16(s.health);
    w.write_u16(s.max_health);
    w.write_u32(s.free_capacity);
    w.write_u32(s.total_capacity);
    write_u64(&mut w, s.experience as u64);
    w.write_u16(s.level);
    w.write_u8(s.level_percent);
    w.write_u16(100); // base xp gain rate
    w.write_u16(0); // xp voucher
    w.write_u16(0); // low level bonus
    w.write_u16(0); // xp boost
    w.write_u16(100); // stamina multiplier
    w.write_u16(s.mana);
    w.write_u16(s.max_mana);
    w.write_u8(s.magic_level);
    w.write_u8(s.magic_level); // base magic level
    w.write_u8(0); // magic level percent
    w.write_u8(s.soul);
    w.write_u16(s.stamina_minutes);
    w.write_u16(s.base_speed / 2);
    w.write_u16(0); // regeneration ticks
    w.write_u16(0); // offline training time
    w.write_u16(0); // xp boost time
    w.write_u8(0); // xp boost buyable
    w.into_bytes()
}

/// `0xA1` skills. M3 placeholder: all skills level 10, specials zero.
pub fn skills() -> Vec<u8> {
    let mut w = MessageWriter::new();
    w.write_u8(OP_SKILLS);
    for _ in 0..7 {
        // FIST..FISHING
        w.write_u16(10); // level
        w.write_u16(10); // base level
        w.write_u8(0); // percent
    }
    for _ in 0..6 {
        // critical/leech specials
        w.write_u16(0); // value
        w.write_u16(0); // base value
    }
    w.into_bytes()
}

/// `0x82` world light.
pub fn world_light(level: u8, color: u8) -> Vec<u8> {
    vec![OP_WORLD_LIGHT, level, color]
}

/// `0x8D` creature light.
pub fn creature_light(creature_id: u32, level: u8, color: u8) -> Vec<u8> {
    let mut w = MessageWriter::new();
    w.write_u8(OP_CREATURE_LIGHT);
    w.write_u32(creature_id);
    w.write_u8(level);
    w.write_u8(color);
    w.into_bytes()
}

/// `0x79` for every inventory slot 1..=11 (M3 sends all slots empty).
pub fn empty_inventory() -> Vec<u8> {
    let mut w = MessageWriter::new();
    for slot in 1..=INVENTORY_SLOTS {
        w.write_u8(OP_INVENTORY_EMPTY);
        w.write_u8(slot);
    }
    w.into_bytes()
}

/// `0x9F` basic data: not premium, knight-ish vocation, 255 placeholder spell ids.
pub fn basic_data() -> Vec<u8> {
    let mut w = MessageWriter::new();
    w.write_u8(OP_BASIC_DATA);
    w.write_u8(0); // is premium
    w.write_u32(0); // premium ends at
    w.write_u8(1); // vocation client id
    w.write_u16(0x00FF); // known spell count = 255
    for id in 0u8..=0xFE {
        w.write_u8(id);
    }
    w.into_bytes()
}

/// `0xA2` status icons bitmask (none active).
pub fn icons() -> Vec<u8> {
    let mut w = MessageWriter::new();
    w.write_u8(OP_ICONS);
    w.write_u16(0);
    w.into_bytes()
}

/// `0x83` magic effect at a position (login teleport poof).
pub fn magic_effect(x: u16, y: u16, z: u8, effect: u8) -> Vec<u8> {
    let mut w = MessageWriter::new();
    w.write_u8(OP_MAGIC_EFFECT);
    w.write_u16(x);
    w.write_u16(y);
    w.write_u8(z);
    w.write_u8(effect);
    w.into_bytes()
}

/// `0x32` OTClient extended-opcode init (sent only for OTClient OSes).
pub fn extended_opcode_init() -> Vec<u8> {
    vec![OP_EXTENDED, 0x00, 0x00, 0x00]
}

/// MessageWriter has no write_u64; emit 8 LE bytes manually.
fn write_u64(w: &mut MessageWriter, v: u64) {
    w.write_bytes(&v.to_le_bytes());
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn self_info_layout() {
        let p = self_info(0x0000_2A01);
        assert_eq!(p[0], OP_SELF_INFO);
        assert_eq!(u32::from_le_bytes([p[1], p[2], p[3], p[4]]), 0x0000_2A01);
        assert_eq!(u16::from_le_bytes([p[5], p[6]]), 50);
        // precision byte for first speed double
        assert_eq!(p[7], 3);
        // total length: 1+4+2 + 3*(1+4) + 1+1+1 + 2 + 2 = 31
        assert_eq!(p.len(), 31);
    }

    #[test]
    fn opcode_only_packets() {
        assert_eq!(pending_state(), [OP_PENDING_STATE]);
        assert_eq!(enter_world(), [OP_ENTER_WORLD]);
    }

    #[test]
    fn stats_layout_length_and_fields() {
        let s = Stats {
            health: 150, max_health: 150, free_capacity: 40000, total_capacity: 40000,
            experience: 0, level: 1, level_percent: 0, mana: 0, max_mana: 0,
            magic_level: 0, soul: 100, stamina_minutes: 2520, base_speed: 220,
        };
        let p = stats(&s);
        assert_eq!(p[0], OP_STATS);
        assert_eq!(u16::from_le_bytes([p[1], p[2]]), 150); // health
        assert_eq!(u16::from_le_bytes([p[3], p[4]]), 150); // max health
        // 1 + 2+2+4+4+8+2+1 + 2+2+2+2+2 + 2+2+1+1+1+1 + 2+2+2+2+2+1 = 60
        assert_eq!(p.len(), 60);
    }

    #[test]
    fn skills_layout_length() {
        let p = skills();
        assert_eq!(p[0], OP_SKILLS);
        // 1 + 7*(2+2+1) + 6*(2+2) = 1 + 35 + 24 = 60
        assert_eq!(p.len(), 60);
    }

    #[test]
    fn lights_and_inventory_and_basic() {
        assert_eq!(world_light(0xFF, 215), [OP_WORLD_LIGHT, 0xFF, 215]);
        assert_eq!(creature_light(7, 0, 0).len(), 1 + 4 + 1 + 1);
        let inv = empty_inventory();
        assert_eq!(inv.len(), 11 * 2);
        assert_eq!(inv[0], OP_INVENTORY_EMPTY);
        assert_eq!(inv[1], 1); // first slot id
        let basic = basic_data();
        assert_eq!(basic[0], OP_BASIC_DATA);
        // 1 + 1 + 4 + 1 + 2 + 255 = 264
        assert_eq!(basic.len(), 264);
        assert_eq!(icons(), [OP_ICONS, 0, 0]);
        assert_eq!(extended_opcode_init(), [OP_EXTENDED, 0, 0, 0]);
        assert_eq!(magic_effect(1000, 1000, 7, EFFECT_TELEPORT).len(), 1 + 2 + 2 + 1 + 1);
    }
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p protocol enter_world`
Expected: FAIL to COMPILE — module not declared.

- [ ] **Step 3: Register the module**

In `crates/protocol/src/lib.rs` add:

```rust
pub mod enter_world;
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test -p protocol enter_world`
Expected: PASS (5 tests).

- [ ] **Step 5: Commit (checkpoint)**

```bash
git add crates/protocol/src/enter_world.rs crates/protocol/src/lib.rs
git commit -m "feat(protocol): encode the enter-world login burst packets"
```

---

## Task 5: `world` — `StaticMap` + `GameWorld` actor

**Files:**
- Modify: `crates/world/Cargo.toml`
- Modify: `crates/world/src/lib.rs`
- Create: `crates/world/src/map.rs`
- Create: `crates/world/src/game.rs`

- [ ] **Step 1: Add deps**

In `crates/world/Cargo.toml`, add a `[dependencies]` section (the crate currently has none):

```toml
[dependencies]
protocol.workspace = true
formats.workspace = true
tokio = { workspace = true }
```

- [ ] **Step 2: Write the failing test for `StaticMap`**

Create `crates/world/src/map.rs`:

```rust
//! Immutable world map for M3: a ground-tile lookup + a spawn point.
//! Ground client ids are resolved once from items.otb (server_id -> client_id).

use std::collections::HashMap;

use formats::otb::ItemsOtb;
use formats::otbm::OtbmMap;
use protocol::map_description::GroundSource;

use crate::Position;

/// Default spawn if the map has no towns (mid-map, ground level).
const FALLBACK_SPAWN: Position = Position::new(1000, 1000, 7);

pub struct StaticMap {
    ground: HashMap<(u16, u16, u8), u16>,
    spawn: Position,
}

impl StaticMap {
    /// Build from a parsed map + item dictionary. The ground client id of a tile
    /// is its first item's id mapped through items.otb (server_id -> client_id).
    pub fn from_formats(map: &OtbmMap, items: &ItemsOtb) -> Self {
        let server_to_client: HashMap<u16, u16> =
            items.items.iter().map(|it| (it.server_id, it.client_id)).collect();

        let mut ground = HashMap::new();
        for tile in &map.tiles {
            let Some(first) = tile.items.first() else { continue };
            let Some(&client_id) = server_to_client.get(&first.id) else { continue };
            ground.insert((tile.x, tile.y, tile.z), client_id);
        }

        let spawn = map
            .towns
            .first()
            .map(|t| Position::new(t.x, t.y, t.z))
            .unwrap_or(FALLBACK_SPAWN);

        Self { ground, spawn }
    }

    pub fn spawn(&self) -> Position {
        self.spawn
    }
}

impl GroundSource for StaticMap {
    fn ground(&self, x: i32, y: i32, z: i32) -> Option<u16> {
        if !(0..=u16::MAX as i32).contains(&x)
            || !(0..=u16::MAX as i32).contains(&y)
            || !(0..=u8::MAX as i32).contains(&z)
        {
            return None;
        }
        self.ground.get(&(x as u16, y as u16, z as u8)).copied()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use formats::otb::ItemType;
    use formats::otbm::{MapItem, MapTile, Town};

    fn tiny_map() -> (OtbmMap, ItemsOtb) {
        let items = ItemsOtb {
            major_version: 3,
            minor_version: 57,
            build_number: 0,
            items: vec![ItemType { group: 0, flags: 0, server_id: 100, client_id: 4526 }],
        };
        let map = OtbmMap {
            width: 100,
            height: 100,
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
        (map, items)
    }

    #[test]
    fn resolves_ground_client_id_and_spawn() {
        let (map, items) = tiny_map();
        let sm = StaticMap::from_formats(&map, &items);
        assert_eq!(sm.spawn(), Position::new(95, 117, 7));
        assert_eq!(sm.ground(95, 117, 7), Some(4526));
        assert_eq!(sm.ground(0, 0, 7), None);
        assert_eq!(sm.ground(-1, 0, 7), None); // out of bounds is safe
    }
}
```

- [ ] **Step 3: Wire the module + run the test (expect fail then pass)**

In `crates/world/src/lib.rs`, keep the existing `Position` and add:

```rust
pub mod game;
pub mod map;
```

Run: `cargo test -p world map`
Expected: FAIL first if module unwired, then PASS (1 test) once `pub mod map;` is added and deps resolve.

- [ ] **Step 4: Write the failing test for `GameWorld` actor**

Create `crates/world/src/game.rs`:

```rust
//! The authoritative game loop. M3 owns only the player registry (assigning ids
//! and spawn positions); the immutable map is shared as an `Arc<StaticMap>`.
//! M4 will move tile mutations behind this actor too.

use std::collections::HashMap;
use std::sync::Arc;

use tokio::sync::{mpsc, oneshot};

use crate::map::StaticMap;
use crate::Position;

/// What the game service needs to build the enter-world burst for a player.
#[derive(Debug, Clone, Copy)]
pub struct PlayerSnapshot {
    pub id: u32,
    pub position: Position,
}

struct PlayerState {
    #[allow(dead_code)]
    name: String,
    #[allow(dead_code)]
    position: Position,
}

enum Command {
    Login { name: String, reply: oneshot::Sender<PlayerSnapshot> },
}

/// Cloneable handle to the running world.
#[derive(Clone)]
pub struct WorldHandle {
    tx: mpsc::Sender<Command>,
    pub map: Arc<StaticMap>,
}

impl WorldHandle {
    /// Register a player by character name; returns its id + spawn position.
    pub async fn login(&self, name: String) -> Option<PlayerSnapshot> {
        let (reply, rx) = oneshot::channel();
        self.tx.send(Command::Login { name, reply }).await.ok()?;
        rx.await.ok()
    }
}

/// Spawn the world actor task and return a handle.
pub fn spawn(map: Arc<StaticMap>) -> WorldHandle {
    let (tx, mut rx) = mpsc::channel::<Command>(64);
    let handle = WorldHandle { tx, map: Arc::clone(&map) };
    tokio::spawn(async move {
        let mut players: HashMap<u32, PlayerState> = HashMap::new();
        let mut next_id: u32 = 0x1000_0000; // creature id range for players
        while let Some(cmd) = rx.recv().await {
            match cmd {
                Command::Login { name, reply } => {
                    let id = next_id;
                    next_id += 1;
                    let position = map.spawn();
                    players.insert(id, PlayerState { name, position });
                    let _ = reply.send(PlayerSnapshot { id, position });
                }
            }
        }
    });
    handle
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::map::StaticMap;
    use formats::otb::{ItemType, ItemsOtb};
    use formats::otbm::{OtbmMap, Town};

    fn empty_map_with_town() -> Arc<StaticMap> {
        let items = ItemsOtb { major_version: 3, minor_version: 57, build_number: 0, items: vec![ItemType { group: 0, flags: 0, server_id: 100, client_id: 4526 }] };
        let map = OtbmMap {
            width: 100, height: 100, major_items: 3, minor_items: 57,
            description: String::new(), spawn_file: None, house_file: None,
            tiles: vec![], towns: vec![Town { id: 1, name: "Thais".into(), x: 95, y: 117, z: 7 }],
            waypoints: vec![],
        };
        Arc::new(StaticMap::from_formats(&map, &items))
    }

    #[tokio::test]
    async fn login_assigns_id_and_temple_position() {
        let world = spawn(empty_map_with_town());
        let snap = world.login("Test Knight".into()).await.unwrap();
        assert_eq!(snap.position, Position::new(95, 117, 7));
        let snap2 = world.login("Test Sorcerer".into()).await.unwrap();
        assert_ne!(snap.id, snap2.id); // unique ids
    }
}
```

- [ ] **Step 5: Run tests to verify pass**

Run: `cargo test -p world`
Expected: PASS (map + game tests). If the `Position::new` is not `const`-usable in `FALLBACK_SPAWN`, confirm `Position::new` is `const fn` (it is, per `world/src/lib.rs`).

- [ ] **Step 6: Commit (checkpoint)**

```bash
git add crates/world/Cargo.toml crates/world/src/lib.rs crates/world/src/map.rs crates/world/src/game.rs
git commit -m "feat(world): static map ground lookup + minimal GameWorld actor"
```

---

## Task 6: `server::game_service` + `main.rs` wiring

**Files:**
- Create: `crates/server/src/game_service.rs`
- Modify: `crates/server/src/main.rs`

- [ ] **Step 1: Write the failing integration test (handshake replay)**

Create `crates/server/src/game_service.rs`:

```rust
//! Game-port connection handler: challenge -> parse game-login -> validate ->
//! enableXTEA -> enter-world burst. Mirrors `login_service` but for ProtocolGame.

use std::sync::Arc;

use anyhow::Result;
use protocol::map_description::{self, Center};
use protocol::rsa::RsaPrivateKey;
use protocol::{enter_world, frame, game_login, xtea};
use tokio::io::{AsyncRead, AsyncWrite};
use world::game::WorldHandle;

pub const CLIENT_VERSION_MIN: u16 = 1097;
pub const CLIENT_VERSION_MAX: u16 = 1098;
const OS_OTCLIENT_LINUX: u16 = 10;

/// Build the full enter-world burst payload list, in order, for a fresh login.
/// Returned as one concatenated buffer (pre-encryption).
pub fn build_enter_world_burst(
    snapshot_id: u32,
    center: Center,
    map: &impl map_description::GroundSource,
) -> Vec<u8> {
    let stats = enter_world::Stats {
        health: 150, max_health: 150, free_capacity: 40_000, total_capacity: 40_000,
        experience: 0, level: 1, level_percent: 0, mana: 0, max_mana: 0,
        magic_level: 0, soul: 100, stamina_minutes: 2520, base_speed: 220,
    };

    let mut burst = Vec::new();
    burst.extend(enter_world::self_info(snapshot_id));
    burst.extend(enter_world::pending_state());
    burst.extend(enter_world::enter_world());
    burst.extend(map_description::encode(center, map));
    burst.extend(enter_world::magic_effect(
        center.x, center.y, center.z, enter_world::EFFECT_TELEPORT,
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
    let Some(raw) = net::frame::read_frame(stream).await? else { return Ok(()); };
    let payload = frame::verify(&raw)?;
    let req = game_login::parse(payload, rsa)?;

    // 3. Validate challenge echo + version (silent disconnect on echo mismatch).
    if req.challenge_timestamp != challenge_timestamp || req.challenge_random != challenge_random {
        return Ok(()); // silent disconnect, TFS behavior
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
    let center = Center {
        x: snapshot.position.x,
        y: snapshot.position.y,
        z: snapshot.position.z,
    };
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

async fn send_disconnect<S>(stream: &mut S, keys: &xtea::RoundKeys, message: &str) -> Result<()>
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
        let items = ItemsOtb { major_version: 3, minor_version: 57, build_number: 0, items: vec![ItemType { group: 0, flags: 0, server_id: 100, client_id: 4526 }] };
        let map = OtbmMap {
            width: 200, height: 200, major_items: 3, minor_items: 57,
            description: String::new(), spawn_file: None, house_file: None,
            tiles: vec![MapTile { x: 95, y: 117, z: 7, flags: 0, house_id: None, items: vec![MapItem { id: 100, contents: vec![] }] }],
            towns: vec![Town { id: 1, name: "Thais".into(), x: 95, y: 117, z: 7 }],
            waypoints: vec![],
        };
        world::game::spawn(Arc::new(StaticMap::from_formats(&map, &items)))
    }

    #[tokio::test]
    async fn handshake_emits_burst_in_order() {
        let rsa = RsaPrivateKey::open_tibia();
        let world = test_world();
        let ts = 0x1234_5678;
        let rnd = 0x42;

        // Build the client side over an in-memory duplex.
        let (mut client, mut server) = tokio::io::duplex(64 * 1024);

        let server_task = {
            let rsa = RsaPrivateKey::open_tibia();
            let world = world.clone();
            tokio::spawn(async move {
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
        let pkt = game_login::build_request(10, 1098, key, b"test", b"test", b"Test Knight", echoed_ts, echoed_rnd).unwrap();
        let inner = frame::checksummed(&pkt);
        net::frame::write_frame(&mut client, &inner).await.unwrap();

        // 3. Read + decrypt the burst (extended opcode comes first for OS=10).
        let keys = xtea::expand_key(&key);
        let ext_raw = net::frame::read_frame(&mut client).await.unwrap().unwrap();
        let ext = xtea::decrypt_message(frame::verify(&ext_raw).unwrap(), &keys).unwrap();
        assert_eq!(ext[0], enter_world::OP_EXTENDED);

        let burst_raw = net::frame::read_frame(&mut client).await.unwrap().unwrap();
        let burst = xtea::decrypt_message(frame::verify(&burst_raw).unwrap(), &keys).unwrap();
        // First three opcodes in order:
        assert_eq!(burst[0], enter_world::OP_SELF_INFO);
        let self_len = 31;
        assert_eq!(burst[self_len], enter_world::OP_PENDING_STATE);
        assert_eq!(burst[self_len + 1], enter_world::OP_ENTER_WORLD);
        assert_eq!(burst[self_len + 2], map_description::OPCODE_MAP_DESCRIPTION);

        let _ = rsa;
        server_task.await.unwrap();
    }
}
```

- [ ] **Step 2: Run the test (expect compile fail — module unwired)**

Run: `cargo test -p server game_service`
Expected: FAIL to COMPILE — `game_service` not declared in the crate, and `server` deps may need `world`/`formats` (already present per Cargo.toml).

- [ ] **Step 3: Wire the module in `main.rs`**

In `crates/server/src/main.rs`, add near the other `mod` declarations:

```rust
mod game_service;
```

- [ ] **Step 4: Run the test to verify it passes**

Run: `cargo test -p server game_service`
Expected: PASS (1 test). If `world::map`/`world::game` are not `pub`, confirm Task 5 added `pub mod`.

- [ ] **Step 5: Wire startup in `main.rs` (load map, spawn world, serve game port)**

Replace the bare game listener (`net::serve(net::Protocol::Game, game_addr)`) with the real handler. Add this before the listener spawns:

```rust
// Load the world (items.otb + forgotten.otbm) and spawn the authoritative actor.
let items_bytes = std::fs::read("reference/tfs/data/items/items.otb")
    .context("reading items.otb")?;
let map_bytes = std::fs::read("reference/tfs/data/world/forgotten.otbm")
    .context("reading forgotten.otbm")?;
let items = formats::otb::parse(&items_bytes).context("parsing items.otb")?;
let map = formats::otbm::parse(&map_bytes).context("parsing forgotten.otbm")?;
let static_map = std::sync::Arc::new(world::map::StaticMap::from_formats(&map, &items));
let world_handle = world::game::spawn(static_map);
info!(spawn = ?world_handle.map.spawn(), "world loaded");
```

Then build the game handler (mirror the login handler closure) and spawn it with `serve_with`:

```rust
let game_handler = {
    let rsa = Arc::clone(&rsa);
    let world = world_handle.clone();
    move |stream, peer| {
        let rsa = Arc::clone(&rsa);
        let world = world.clone();
        async move {
            let mut stream = stream;
            // Deterministic challenge values are fine for M3; M4 can randomize.
            let ts: u32 = 0x5EED_0000;
            let rnd: u8 = 0x2A;
            if let Err(error) = game_service::handle_game(&mut stream, &rsa, &world, ts, rnd).await {
                warn!(%peer, %error, "game handler failed");
            }
        }
    }
};
let game = tokio::spawn(net::serve_with(net::Protocol::Game, game_addr, game_handler));
```

Ensure `use anyhow::Context as _;` is in scope (add if missing).

- [ ] **Step 6: Build the whole workspace + run all tests**

Run: `cargo build && cargo test`
Expected: clean build; all tests pass.

- [ ] **Step 7: Commit (checkpoint)**

```bash
git add crates/server/src/game_service.rs crates/server/src/main.rs
git commit -m "feat(server): game-port handshake + enter-world burst, wire world startup"
```

---

## Task 7: Clippy + manual OTClient acceptance

**Files:** none (verification) + `PROGRESS.md`

- [ ] **Step 1: Lint clean**

Run: `cargo clippy --all-targets -- -D warnings`
Expected: no warnings. Fix any inline.

- [ ] **Step 2: Run the server**

Run: `RUST_LOG=info cargo run -p server -- config/server.toml`
Expected: logs "world loaded" with the spawn position; listeners on 7171/7172.

- [ ] **Step 3: Manual acceptance with real OTClient Redemption**

- Start the client from `../client/otclient-linux-release`.
- Log in with `test` / `test`, select **Test Knight**.
- Expected: the client enters the game and renders the player standing on ground
  tiles at the Thais temple (or the first town's temple), with stats/skills/light
  shown, no disconnect.
- If it disconnects: per PROGRESS gotchas, suspect (in order) the `0x0A` game
  protocol-id byte, the challenge echo, XTEA padding/inner-length, or the
  map-description skip stream. Capture the frame with the existing `sniff` example
  pointed at 7172.

- [ ] **Step 4: Update `PROGRESS.md`**

Mark M3 done in the milestone table, set Current status to M3 ✅ → next M4, add an
"M3 plan" section summarizing the 6 implemented steps, and append any new protocol
gotchas discovered during acceptance (especially the real game protocol-id byte
and the exact challenge handling if they differed from this plan's assumptions).

- [ ] **Step 5: Commit (checkpoint)**

```bash
git add PROGRESS.md
git commit -m "docs: mark M3 (enter game) complete"
```

---

## Self-review notes (author)

- **Spec coverage:** challenge (T1), game-login parse incl. RSA block + session key (T2),
  map description + skip encoding + add_item (T3), full enter-world burst incl. `0x32`/`0x83`
  and the speed `addDouble` formula (T4), `StaticMap` ground lookup + temple spawn + `GameWorld`
  actor (T5), `game_service` handshake/validation/burst + startup wiring (T6), error paths
  (`0x14`, silent echo disconnect) in T6, integration replay (T6 test), manual acceptance +
  PROGRESS (T7). Persistence task dropped — `player load` is satisfied by `world.login` + the
  account character list (auth happens at the login server in M1; M3 trusts the session).
- **Risks carried from spec:** the game protocol-id byte (assumed `0x0A`) and challenge
  handling are verified only at T7 against the real client; T7 step 3 documents the fallback.
  `forgotten.otbm` temple is assumed overground (z<=7); underground walk is deferred.
- **Type consistency:** `GroundSource`/`Center` defined in `map_description` (T3) and consumed
  in `world::map` (T5) and `game_service` (T6); `enter_world::Stats` defined in T4 and built in
  T6; `PlayerSnapshot{id,position}` defined in T5 and used in T6; `WorldHandle.map` (Arc) used
  as the `GroundSource` in T6.
