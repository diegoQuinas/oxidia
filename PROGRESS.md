# PROGRESS — Oxidia

Open Tibia server in Rust, protocol **10.98**, client target **OTClient Redemption** (mehah/otclient).
Reference spec: **TFS 1.4.2** at `reference/tfs/` (read-only — never edit, never port line-by-line).

> Read this file first in every session.
> Repo layout: the OTClient test client lives in `../client/`; this Rust workspace **is** the server. Run every command below from inside `server/`. The server binary is `oxidia`.

## Current status

- **Milestone:** M4 ✅ **core walk complete and accepted live** → next is **M5 (multiplayer presence)**. Floor changes / underground (z≥8) and auto-walk are deferred; multiplayer presence is M5.
- **Build:** `cargo build` clean, `cargo test` green (77 tests), `cargo clippy --all-targets -- -D warnings` clean.
- **Toolchain:** Rust 1.96, edition 2024, `#![forbid(unsafe_code)]` in every crate.
- **Accepted (M1):** real **OTClient Redemption** (protocol 1098) connects to `127.0.0.1:7171` with `test`/`test` and shows the MOTD + character list. M1 acceptance criterion fully met.
- **Accepted (M2):** `cargo run -p formats --example mapinfo` parses the real `items.otb` (v3.57, 26 282 items) and `forgotten.otbm` (2048×2048, 340 594 tiles, 429 031 items, 5 towns) — full tree walk, no unknown nodes/attrs.
- **Accepted (M3):** real **OTClient Redemption** enters the game on port 7172 and renders **Test Knight** standing on the real Thais temple ground (`forgotten.otbm`), with stats (HP 150, Soul 100, Cap 400, Level 1) shown. A keep-alive ping holds the session — no more 30 s timeout.

## Milestones

Full roadmap (ROI-ordered, with rationale and ship gates):
`docs/superpowers/specs/2026-06-06-roadmap-to-production.md`.
Architecture is locked: **Rust core + embedded Lua (mlua)** for mutable content
(spells, monster behaviors, NPC dialogue, quests); static data in TOML/RON; everything
performance-critical or stable stays native Rust.

| # | Goal | State |
|---|------|-------|
| M0 | Skeleton: workspace compiles, tests green, server listens on 7171/7172, logs connections | ✅ done |
| M1 | Login server: framing, Adler-32, RSA, XTEA, NetworkMessage, login parse, char list, sniff tool | ✅ done |
| M2 | Formats: `.otb` + `.otbm` parsers, `mapinfo` example | ✅ done |
| M3 | Enter game: game handshake (challenge), player load, initial packet sequence, render map | ✅ done |
| **A** | **Living World → pre-alpha #1** | |
| M4 | Walk (core): visible creature, directional + diagonal walk, map slices, collision, turn (floor changes & auto-walk deferred) | ✅ done |
| M5 | Multiplayer presence: spectator / known-creatures system, broadcast movement | ⬜ |
| M6 | Chat: say / whisper / yell + default channel | ⬜ |
| M7 | Combat core + PvP melee: damage, HP sync, death, respawn, protected zones | ⬜ |
| M8 | Persistence + accounts: per-friend characters, saved position/stats | ⬜ |
| **B** | **Items & Inventory** | |
| M9 | Ground items, stacks, look-at | ⬜ |
| M10 | Inventory & equipment: move, equip, containers, use | ⬜ |
| **C** | **Scripting** | |
| M11 | Lua runtime (mlua): hot-reloadable content hooks (onUse/onStepIn/onSay/…) | ⬜ |
| **D** | **PvE → pre-alpha #2** | |
| M12 | Creatures & monsters: spawns, AI, A* pathfinding | ⬜ |
| M13 | Loot & corpses | ⬜ |
| M14 | Skills, XP, levels, vocations | ⬜ |
| M15 | Spells, runes, conditions | ⬜ |
| **E** | **Social & Economy** | |
| M16 | NPCs: dialogue (Lua) + buy/sell | ⬜ |
| M17 | Depot, bank, money | ⬜ |
| M18 | Parties: shared XP | ⬜ |
| M19 | Guilds + guild channel | ⬜ |
| **F** | **World Systems** | |
| M20 | Houses | ⬜ |
| M21 | Quests | ⬜ |
| M22 | Market | ⬜ |
| M23 | PvP systems: skulls, frags, war/PZ rules | ⬜ |
| **G** | **Production Hardening** 🏁 | |
| M24 | GM/admin tools | ⬜ |
| M25 | Persistence robustness | ⬜ |
| M26 | Account management (in-protocol creation, security) | ⬜ |
| M27 | Ops & stability: metrics, logging, rate-limit, reconnection, load test | ⬜ |
| M28 | Configurability & deploy | ⬜ |

**Ship gates:** pre-alpha #1 after M8 (walk + chat + PvP, persisted) · pre-alpha #2
after M15 (full PvE loop) · production after M28.

## Workspace layout

```
../client/             OTClient Redemption test client (C++) + downloads — NOT the server
server/                this Rust workspace (Oxidia)
crates/
  net/         tokio listener + connection lifecycle      (M0: accept+log; M1: framing)
  protocol/    NetworkMessage, RSA, XTEA, Adler-32, packets (zero game logic)
  formats/     .otb / .otbm parsers (pure, parse from &[u8])
  world/       tile grid, single authoritative game loop over channels
  persistence/ sqlx + sqlite accounts/players (sqlx wired in M1)
  server/      binary: TOML config, tracing, wiring
config/server.toml   ports, world name, db path, log filter
reference/tfs/       TFS 1.4.2 (gitignored, re-clone if missing)
```

Re-clone reference if absent:
`git clone --depth 1 --branch v1.4.2 https://github.com/otland/forgottenserver reference/tfs`

## Run

```bash
cargo build && cargo test
RUST_LOG=info cargo run -p server -- config/server.toml   # binary name: oxidia
```

## M1 plan

1. ✅ `protocol`: NetworkMessage reader/writer (LE) — `message.rs`, round-trip + EOF tests.
2. ✅ `protocol`: Adler-32 (`adler.rs`, canonical vectors), XTEA 32-round (`xtea.rs`, validated against an independent textbook XTEA oracle + round-trip), RSA raw modpow (`rsa.rs`, `num-bigint-dig`, bundled OpenTibia key, decrypt validated against public-exponent oracle).
3. ✅ Framing — `net::frame` owns `[u16 LE len][bytes]` socket I/O (`read_frame`/`write_frame`, `MAX_FRAME`); `protocol::frame` owns the 4-byte LE Adler-32 checksum layer (`checksummed`/`verify`). XTEA-decrypt-after-handshake wiring lands with step 4's connection state.
4. ✅ Login packet parse — `protocol::login::parse(payload, &RsaPrivateKey) -> LoginRequest{os,version,xtea_key,account,password}`. RSA block decrypts to `[u8 0][u32x4 key][string account][string password]`. Added `rsa::encrypt_open_tibia_public` (client-side, for tests + sniff tool).
5. ✅ `persistence`: sqlx sqlite (`Store`), `accounts`/`players` schema (`migrations/0001_init.sql`), `authenticate` + idempotent `seed_test_account_if_empty` (test account `test`/`test`).
6. ✅ Login response — `protocol::charlist`: `CharacterList::encode` (MOTD 0x14, session key 0x28, char list 0x64, single world, premium trailer) + `build_error` (0x0B ≥1076 else 0x0A).
7. ✅ Wiring — `net::serve_with(proto, addr, handler)` (transport stays decoupled); `server::login_service::handle_login` ties framing + parse + auth + response, XTEA-encrypts the reply. `main.rs` opens the DB, seeds, serves login via the handler.
8. ✅ `sniff` + `probe` examples (`crates/server/examples/`): login-aware MITM hexdumps raw + decrypted frames; `probe` is a minimal client. Integration test `login_service::tests` replays a built login end-to-end (valid → char list, bad → error). Verified live over real sockets through the proxy.

Acceptance: protocol proven correct against the `probe` client (MOTD + char list). **Remaining: point the real OTClient Redemption at the server.** To run the proxy chain: server on an alt `login_port` (e.g. 7271), `cargo run -p server --example sniff -- 127.0.0.1:7171 127.0.0.1:7271`, then `cargo run -p server --example probe`.

## M2 plan

1. ✅ `formats::node` — generic OTB node-tree reader. `parse_tree(&[u8]) -> Node{kind, props, children}`. Markers `START 0xFE` / `END 0xFF` / `ESCAPE 0xFD`; props returned un-escaped. Validated against the real `forgotten.otbm`.
2. ✅ `formats::props` — `PropReader` LE cursor (`read_u8/u16/u32/string/skip`, `remaining`); mirrors TFS `PropStream` (string = `u16` len + bytes).
3. ✅ `formats::otb` — `parse(&[u8]) -> ItemsOtb{major,minor,build, items: Vec<ItemType{group,flags,server_id,client_id}>}`. Root version block + per-item attribute records. Real `items.otb` = v3.57, 26 282 items.
4. ✅ `formats::otbm` — `parse(&[u8]) -> OtbmMap{width,height,major_items,minor_items,description,spawn_file,house_file, tiles, towns, waypoints}`. Walks TILE_AREA→TILE/HOUSETILE (inline + child items), TOWNS, WAYPOINTS. Real map parses fully.
5. ✅ `mapinfo` example (`crates/formats/examples/mapinfo.rs`): loads both, prints versions/dims/file-refs/tile+item counts/per-floor distribution/town list. **M2 acceptance criterion.**

Run: `cargo run -p formats --example mapinfo [items.otb] [map.otbm]` (defaults to the bundled reference files).

## M3 plan

Design + plan: `docs/superpowers/specs/2026-06-06-m3-enter-game-design.md`, `docs/superpowers/plans/2026-06-06-m3-enter-game.md`.

1. ✅ `protocol::challenge` — encode `0x1F` `[u32 ts][u8 random]` (checksummed, NOT XTEA — first server→client packet).
2. ✅ `protocol::game_login` — `parse(payload, &RsaPrivateKey) -> GameLoginRequest`. Game packet = `[u8 0x0A id][u16 os][u16 version][skip 7][128-byte RSA block]`; block = `[u8 0][u32x4 xtea][u8 gamemaster][string sessionKey][string charName][u32 ts][u8 rnd]`. `sessionKey` = `account\npassword\ntoken\ntokenTime` (strict 4-part). `build_request` test helper.
3. ✅ `protocol::map_description` — `0x64` viewport (18×14, floors 7→0). **Exact port of TFS `GetMapDescription`/`GetFloorDescription`** skip-encoding; `GroundSource` trait keeps it map-agnostic. Round-trip tested against an OTClient-faithful decoder.
4. ✅ `protocol::enter_world` — burst encoders: `self_info 0x17` (29 B), `pending 0x0A`, `enter_world 0x0F`, `stats 0xA0` (53 B), `skills 0xA1`, `world_light 0x82`, `creature_light 0x8D`, `empty_inventory 0x79×11`, `basic_data 0x9F`, `icons 0xA2`, `magic_effect 0x83`, `extended 0x32`.
5. ✅ `world::map::StaticMap` (immutable ground lookup `server_id→client_id` + town-temple spawn, impl `GroundSource`) + `world::game::GameWorld` (tokio actor over mpsc/oneshot, owns the player registry; map shared via `Arc`).
6. ✅ `server::game_service` — `handle_game`: challenge → parse → echo+version validate → enableXTEA → `0x32` (OTClient) → `world.login` → burst. `main.rs` loads `items.otb`+`forgotten.otbm`, spawns the world, serves 7172 via `serve_with`. Integration replay test over `tokio::io::duplex`.
7. ✅ **Accepted live** — real OTClient renders the player on the Thais temple ground. Two bugs fixed during acceptance: the challenge frame needed the inner `[u16 length]` (TFS `onConnect:429`); and the session must stay open with a keep-alive ping (`0x1D` every 10 s of silence) or the client times out after 30 s.

Burst order (one XTEA frame): `0x17, 0x0A, 0x0F, 0x64 map, 0x83, 0x79×11, 0xA0, 0xA1, 0x82, 0x8D, 0x9F, 0xA2`.

**Deferred to M4:** per-connection random challenge (M3 uses a fixed `ts/rnd` — functionally fine since the client echoes it back, but no replay protection); items/creatures on tiles; underground floor walk (z≥8); real player persistence (M3 spawns every char at the temple).

## M4 plan

Design + plan: `docs/superpowers/specs/2026-06-06-m4-walk-design.md`, `docs/superpowers/plans/2026-06-06-m4-walk.md`. Scope: **core walk on one floor + visible creature**. Pure request/response over the world actor (no broadcast — single player; presence is M5).

1. ✅ `world::Direction` (wire bytes N0 E1 S2 W3 SW4 SE5 NW6 NE7, `delta`) + `Position::offset`.
2. ✅ `world::map::StaticMap` walkability — a `blocked` set derived at load from the `items.otb` `FLAG_BLOCK_SOLID` (bit 0); `is_walkable(pos)` = has ground AND not blocked.
3. ✅ `world::game` — `Move`/`Turn` actor commands, `MoveResult { outcome: Moved{from,to}|Blocked, facing }`; `PlayerState` gains a mutable `direction` (spawn faces South).
4. ✅ `protocol::creature` — byte-faithful `AddCreature`/`AddOutfit` port (1098). `0x0061` unknown / `0x0062` known forms; outfit, light, `speed/2`, shields, guild-emblem (unknown only), mark, helpers, walkthrough.
5. ✅ `protocol::map_description` — render creatures in the `0x64` (spliced after the ground item); refactor into a shared `get_map_description`; `encode_slice` for the directional row/column strips.
6. ✅ `protocol::walk` — `creature_move 0x6D`, `cancel_walk 0xB5`, `creature_turn 0x6B`, and `walk_update` (0x6D + slices; independent y/x blocks so a diagonal emits both).
7. ✅ `server::game_service` — render the player in the enter-world burst; `run_session` now decrypts each frame and dispatches walk (`0x65-0x68`, `0x6A-0x6D`) / turn (`0x6F-0x72`); `Moved`→`walk_update`, `Blocked`→`cancel_walk`. Integration replay walks east and asserts a `0x6D` comes back.
8. ✅ **Accepted live** — real OTClient renders the Test Knight with its outfit on the Thais temple ground and walks it around with arrow keys; walls stop it. Fixed during review: a bad-checksum frame now drops instead of killing the session.

**Deferred (later slice):** floor changes / stairs / underground (z≥8) walk; auto-walk / click-to-move pathfinding; diagonal corner-cut blocking; real player persistence; walkthrough byte fidelity (self currently `0x00`).

## Protocol gotchas (M4 walk)

- **Every item is `[u16 clientId][u8 0xFF MARK_UNMARKED]`** at 1098 (`networkmessage.cpp:86`). The `0xFF` is part of the item, not a tile terminator.
- **Tile thing order** (`GetTileDescription:583`): env `u16 0x0000`, ground item, top items, **creatures (reverse) via `AddCreature`**, down items. The tile ends at the next `[skip][0xFF]` flush — there is no per-tile terminator. Creature markers `0x0061`/`0x0062` are `< 0xFF00`, so the client reads them as things, not skip markers.
- **Diagonal steps send two slices, not a full `0x64`.** `sendMoveCreature`'s y and x checks are independent `if` blocks; a diagonal emits the applicable Y-slice and X-slice both.
- **Slice anchors** (`sendMoveCreature:2616-2630`, viewport 8×6): north/south anchor on `oldPos.x-8` and `newPos.y∓`; east/west anchor on `newPos.x±` and `newPos.y-6`. The slice still runs all 8 floors (`GetMapDescription`).
- **A lone creature on a ground-only tile has stackpos = 1** (ground = 0). `0x6D` / `0x6B` use the stackpos < 10 form.
- **Incoming vs outgoing opcode overlap is fine:** `0x6D` inbound = walk-NW, outbound = creature-move; `0x6B` inbound = walk-SE, outbound = creature-turn. Tibia opcodes are namespaced by direction.

## Protocol gotchas (M3 game-enter)

- **`ProtocolGame` id byte is `0x0A`** (vs `0x01` for login). Our frame payload keeps the protocol-id byte; `game_login::parse` reads it first. The game port skips only **7** bytes after version (`u32 clientVersion + u8 type + u16 datRevision`), unlike the login server's 17.
- **Skip-encoding must be a byte-faithful TFS port** (`protocolgame.cpp:633-680`): `skip` starts `-1` and persists across all 8 floors; on empty tile check `skip == 0xFE` **before** incrementing (`[0xFF][0xFF]` flush + reset `-1`); on a real tile flush `[skip][0xFF]` if `skip>=0`, then `skip=0`, write `[env 0x0000][item]`; final `[skip][0xFF]` flush. OTClient's `setFloorDescription` is the exact mirror — do NOT invent a self-consistent scheme.
- **Byte layouts are version-gated.** At 1098: `self_info 0x17` = 29 B; `stats 0xA0` = 53 B with health/mana as **u16** (u32 only from `GameDoubleHealth` ≥ 1300). Verify field sets against the OTClient parse, not just TFS send code.
- After parsing the game-login packet, **enable XTEA** for all subsequent traffic (the `0x32` ext-opcode and the whole burst are XTEA-encrypted + checksummed; the `0x1F` challenge is checksummed only).
- OTClient OS values ≥ 10 (`CLIENTOS_OTCLIENT_LINUX`) trigger a `0x32` extended-opcode init packet right after the key exchange.

## Format gotchas (.otb / .otbm)

- Both formats share the **same node-tree container** (`fileloader.cpp`): `[u8;4 identifier][0xFE root-type][root props]( child | 0xFF )*`. `0xFE` opens a child (next byte = type), `0xFF` closes, `0xFD` escapes the next byte so markers can appear in props. A node's props are the bytes between its type byte and its first child / its END, with escapes removed.
- **The root node's TYPE byte is `0x00`, NOT `OTBM_ROOTV1`(1).** TFS never validates `root.type`; it reads the `OTBM_root_header` straight from root props and only checks the single child is `OTBM_MAP_DATA`(2). Don't assert on root type.
- All integers little-endian; structs are `#pragma pack(1)`. `OTBM_root_header` = `u32 version, u16 width, u16 height, u32 majorItems, u32 minorItems` (16 bytes). `items.otb` root = `u32 flags, u8 ROOT_ATTR_VERSION(0x01), u16 len=140, VERSIONINFO{u32 major, u32 minor, u32 build, u8[128]}`.
- `items.otb` item node: type = `itemgroup_t`; props = `u32 flags` then `[u8 attr][u16 len][data]`. `ITEM_ATTR_SERVERID=0x10`, `CLIENTID=0x11` (both u16). Unknown attrs: skip `len` bytes.
- `.otbm` inline ground item (`OTBM_ATTR_ITEM=9` inside tile props) is **exactly a `u16` id** — `Item::CreateItem(PropStream)` reads only the id (no count in OTBM v2). Stacked items are separate child `OTBM_ITEM`(6) nodes; container contents nest as further child item nodes. Tile attrs: `TILE_FLAGS=3`(u32), `ITEM=9`. HOUSETILE(14) prepends a `u32 houseId` before attrs.
- Map-data attrs: `DESCRIPTION=1`, `EXT_SPAWN_FILE=11`, `EXT_HOUSE_FILE=13` (all strings). Tile coords are `base(u16 x,u16 y,u8 z)` per TILE_AREA + `(u8 dx,u8 dy)` per tile.
- Reference files (`reference/tfs/data/...`) are **gitignored** — format tests skip gracefully (`eprintln "skipping"`) when absent, so CI without the TFS tree stays green.

## Protocol gotchas (append as discovered)

- Reminder: on garbage/disconnect, suspect **checksum / XTEA padding / inner-vs-outer length** mismatch FIRST.
- Frame: `[u16 LE length][u32 Adler-32 of rest][payload]`. After login, all traffic XTEA-encrypted (32 rounds, 8-byte blocks, padding counts toward inner length).
- spr/dat are the **extended** variant (u32 sprite IDs) — irrelevant to the server; it only reads `items.otb` + `.otbm`.
- **First login packet is NOT XTEA-encrypted** (the key is inside its RSA block) but IS checksummed. Every packet *after* the handshake — including the login server's own response — is XTEA-encrypted. The login response carries the char list encrypted with the key the client just sent.
- **Send path**: prepend `[u16 inner_len]`, zero-pad the whole `[len][payload]` to a multiple of 8, XTEA-encrypt, prepend Adler-32, prepend `[u16 outer_len]`. **Recv path**: outer_len → verify checksum over the rest → XTEA-decrypt → read inner_len → take that many payload bytes. (`xtea::encrypt_message`/`decrypt_message`.)
- RSA block: TFS `RSA_decrypt` consumes the **leading zero byte** itself; XTEA key reads start at decrypted offset 1. Layout: `[u8 0][u32x4 key][string account][string password]`.
- Login header skip is version-dependent: `version >= 971` has a `u32 protocolVersion` field (skip 17 = 4+12 sigs+1 zero between version and RSA); older skips 12. We require ≥ 971.
- Error opcode: `0x0B` for client ≥ 1076, else `0x0A`.
- TFS `decrypt` loop `for i=63; i>0; i-=2` underflows in Rust `usize` at i=1 — use `(1..64).rev().step_by(2)`.
- The bundled `otclient-src/` is **obfuscated** (renamed symbols); TFS 1.4.2 is the layout oracle. A real-OTClient packet capture would harden the replay test.
