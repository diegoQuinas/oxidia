# PROGRESS — The Crustacean Server

Open Tibia server in Rust, protocol **10.98**, client target **OTClient Redemption** (mehah/otclient).
Reference spec: **TFS 1.4.2** at `reference/tfs/` (read-only — never edit, never port line-by-line).

> Read this file first in every session.
> Repo layout: the OTClient test client lives in `../client/`; this Rust workspace **is** the server. Run every command below from inside `server/`. The server binary is `crustacean-server`.

## Current status

- **Milestone:** M2 ✅ format parsers complete end-to-end → next is **M3 (enter game)**.
- **Build:** `cargo build` clean, `cargo test` green (55 tests), `cargo clippy` clean.
- **Toolchain:** Rust 1.96, edition 2024, `#![forbid(unsafe_code)]` in every crate.
- **Accepted (M1):** real **OTClient Redemption** (protocol 1098) connects to `127.0.0.1:7171` with `test`/`test` and shows the MOTD + character list. M1 acceptance criterion fully met.
- **Accepted (M2):** `cargo run -p formats --example mapinfo` parses the real `items.otb` (v3.57, 26 282 items) and `forgotten.otbm` (2048×2048, 340 594 tiles, 429 031 items, 5 towns) — full tree walk, no unknown nodes/attrs.

## Milestones

| # | Goal | State |
|---|------|-------|
| M0 | Skeleton: workspace compiles, tests green, server listens on 7171/7172, logs connections | ✅ done |
| M1 | Login server: framing, Adler-32, RSA, XTEA, NetworkMessage, login parse, char list, sniff tool | ✅ done |
| M2 | Formats: `.otb` + `.otbm` parsers, `mapinfo` example | ✅ done |
| M3 | Enter game: game handshake (challenge), player load, initial packet sequence, render map | ⬜ |
| M4 | Walk: movement packets, tile updates, floor changes, collision | ⬜ |

(Combat / Lua scripting / creatures are planned *after* M4.)

## Workspace layout

```
../client/             OTClient Redemption test client (C++) + downloads — NOT the server
server/                this Rust workspace (The Crustacean Server)
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
RUST_LOG=info cargo run -p server -- config/server.toml   # binary name: crustacean-server
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
