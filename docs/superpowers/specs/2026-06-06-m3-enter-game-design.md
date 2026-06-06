# M3 — Enter Game (design)

The Crustacean Server · Open Tibia · protocol 10.98 · client target OTClient Redemption (1098).
Reference oracle: TFS 1.4.2 at `reference/tfs/` (read-only).

## Goal & acceptance

A real OTClient Redemption client, after passing the login server's character list (M1),
selects a character, connects to the game port (7172), completes the game handshake, and is
rendered **standing on real ground tiles** of `forgotten.otbm`, with stats, skills, world light
and an empty inventory shown.

**Acceptance criterion:** the real OTClient renders the player on the actual Forgotten world
(a town temple tile), no client-side disconnect, stats/skills/light visible.

**Explicitly out of scope for M3** (deferred to M4+):
- Walking / movement packets, tile updates, floor changes (M4).
- Items stacked on tiles, containers, ground items beyond the single ground item.
- Creatures (other players, NPCs, monsters), known-creature set management.
- Multi-floor underground rendering nuance beyond the standard overground 7→0 walk.
- Combat, Lua, spawns.

## Map decision

Render onto the **real `forgotten.otbm`**, ground layer only, single conceptual floor (the
client still receives the overground floor walk z=7→0, but only the player's floor carries a
ground tile in M3). This reuses the M2 parser and keeps the milestone honest. The synthetic
flat-map alternative was rejected: it would make "render map" fake and defer the otb client-id
lookup we need anyway.

## Architecture (Approach A — minimal authoritative world loop)

State lives in an authoritative `world` actor, as PROGRESS already intends ("single
authoritative game loop over channels"). M3 stands it up minimally so M4 (walking) extends it
without a refactor.

### Layer split

- **`protocol`** — pure encoders/decoders, zero game logic. New modules below.
- **`world`** — owns the loaded map + the player registry; the `GameWorld` actor.
- **`persistence`** — loads the character record by name.
- **`server`** — `game_service` wiring (mirror of `login_service`) + startup wiring in `main.rs`.

### `protocol` new modules

- `challenge.rs` — encode `0x1F` challenge: `[u8 0x1F][u32 LE timestamp][u8 random]`.
  Checksummed but NOT XTEA-encrypted (first server→client packet).
- `game_login.rs` — `parse(payload, &RsaPrivateKey) -> GameLoginRequest`.
  Outer plaintext: `[u16 os][u16 version][skip 7 bytes: u32 clientVersion + u8 clientType + u16 datRevision]`.
  Then a 128-byte RSA block decrypting to:
  `[u8 0][u32x4 xtea_key][u8 gamemaster][string sessionKey][string characterName][u32 challengeTimestamp][u8 challengeRandom]`.
  `GameLoginRequest { os, version, xtea_key, gamemaster, session_key, character_name, challenge_timestamp, challenge_random }`.
  Reuses `rsa`, `message`. Account/password are parsed out of `sessionKey`
  (`"account\npassword\ntoken\ntokenTime"`) — for M3 we accept account+password, ignore token.
- `enter_world.rs` — byte-exact encoders for the login burst:
  - `0x17` self-info: `playerId u32`, `beatDuration u16=50`, three `addDouble(prec=3)` speed
    fields (`[u8 3][u32 encoded]` each, encoded = `value*1000 + i32::MAX`), `canReportBugs u8=0`,
    `canChangePvpFraming u8=0`, `expertMode u8=0`, `storeImagesUrl` empty string `u16=0`,
    `premiumCoinPkg u16=25`.
  - `0x0A` pending-state, `0x0F` enter-world (opcode only).
  - `0xA0` stats, `0xA1` skills (7 combat skills ×5 bytes + 6 special skills ×4 bytes) — byte
    layout per the TFS spec section 4.
  - `0x82` world light `[u8 level][u8 color]`, `0x8D` creature light `[u32 id][u8 level][u8 color]`.
  - `0x78`/`0x79` inventory item present/absent per slot; M3 sends `0x79` (empty) for slots 1..=11.
  - `0x9F` basic data `[u8 isPremium][u32 premiumEndsAt][u8 vocClientId][u16 0x00FF][255×u8 spellIds]`.
    M3 emits 255 sequential/zero spell ids (placeholder).
  - `0xA2` icons `[u16 0]`.
  - `0x83` magic effect `[u16 x][u16 y][u8 z][u8 effect=10]` (CONST_ME_TELEPORT), login only.
  - `0x32` OTClient extended-opcode init `[u8 0x00][u16 0x0000]`, sent only when OS >= OTClient.
- `map_description.rs` — `0x64`:
  `[u8 0x64][u16 x][u16 y][u8 z]` then the tile stream.
  - Floor walk: overground `z<=7` iterate floors 7→0; (underground handling stubbed/deferred).
  - Viewport: anchor `(x-8, y-6)`, width 18, height 14, X outer / Y inner loop.
  - Skip encoding: count consecutive empty tiles; emit `[u8 skip][u8 0xFF]` before a real tile and
    as a final flush; emit `[0xFF][0xFF]` when skip hits 0xFE.
  - `add_tile`: `[u16 0x0000]` (env effects) + `add_item(ground)`.
  - `add_item`: `[u16 clientId][u8 0xFF mark]` (+ `[u8 count]` if stackable — not needed for M3 ground).
  - Takes already-resolved client IDs from the caller (no otb lookup inside `protocol`).

### `world` crate

- Loads `OtbmMap` (from `formats`) at startup + builds a `server_id -> client_id` table from
  `items.otb` (`ItemType.server_id` -> `client_id`).
- `GameWorld` actor: a tokio task owning `{ map, item_client_ids, players: HashMap<PlayerId, PlayerState> }`.
  Commands over `mpsc`, replies over `oneshot`:
  - `Login { character_name, reply }` -> assigns a `PlayerId`, sets spawn = first town's temple
    position, inserts `PlayerState`, returns `PlayerSnapshot { id, position, name, stats, outfit }`.
  - `Viewport { center, reply }` -> returns the ground client-id grid for the 18×14×floors window
    (for each tile, the ground item's client id or "empty").
- `PlayerState`: `{ id, name, position, health, max_health, mana, max_mana, level, outfit }` with
  sane M3 defaults.
- A thin handle (`WorldHandle`) wrapping the mpsc sender, cloneable into each connection task.

### `persistence`

- Load the character by name (from the `players`/`characters` table seeded in M1). If the schema
  has no stored position, default the spawn to the town temple. Schema is confirmed during apply.

### `server`

- `game_service.rs` (mirror of `login_service.rs`):
  1. On connect, send `0x1F` challenge (checksummed, not XTEA); remember `(timestamp, random)`.
  2. On first frame: `game_login::parse` (RSA). Validate challenge echo (mismatch -> silent
     disconnect), version in 1097..=1098 (else `0x14`), authenticate (else `0x14`).
  3. `enableXTEA` with the parsed key for all subsequent traffic.
  4. If OS >= OTClient, send `0x32` extended-opcode init.
  5. `WorldHandle.login(character_name)` -> snapshot.
  6. Build the burst in order, in a single buffer:
     `[0x17, 0x0A, 0x0F, 0x64 map(viewport), 0x83, 0x79×11, 0xA0, 0xA1, 0x82, 0x8D, 0x9F, 0xA2]`.
  7. XTEA-encrypt + checksum + send (reuse `xtea::encrypt_message`, `protocol::frame`).
- `main.rs`: load `items.otb` + `forgotten.otbm` at startup, spawn the `GameWorld` task, serve the
  game port via `net::serve_with` with the game handler. Login service stays as-is.

## Data flow

```
connect
  -> S: 0x1F challenge (checksum, no xtea)
  <- C: game-login packet (outer + RSA block, checksum, no xtea)
  -> parse RSA, validate echo + version + auth
  -> enableXTEA(key)
  -> S: 0x32 (only if OTClient OS)
  -> WorldHandle.login(name) -> PlayerSnapshot
  -> WorldHandle.viewport(snapshot.position) -> ground grid
  -> encode burst [0x17,0x0A,0x0F,0x64,0x83,0x79x11,0xA0,0xA1,0x82,0x8D,0x9F,0xA2]
  -> S: one XTEA-encrypted + checksummed buffer
```

## Error handling

| Condition | Response |
|---|---|
| Challenge echo mismatch | silent disconnect (TFS behavior) |
| Version not in 1097..=1098 | `0x14` disconnect + version string |
| Auth fails | `0x14` disconnect |
| Character not found / not loadable | `0x14` "Your character could not be loaded." |

Pre-key errors are checksummed only; post-key errors are XTEA-encrypted + checksummed.

## Testing (strict TDD — RED before GREEN)

- `challenge::encode` — exact bytes.
- `game_login::parse` — build a known RSA block via `rsa::encrypt_open_tibia_public`, round-trip
  parse, assert every field; short/EOF inputs error cleanly; bad padding byte errors.
- Each `enter_world` encoder — byte-exact unit test against the TFS layout (stats, skills, lights,
  inventory empty slot, basic data, icons, self-info incl. the speed `addDouble` encoding).
- `map_description` — synthetic small grid: (a) all-empty floor produces only a trailing skip
  flush, (b) a single ground tile at center produces the right skip + `add_item` bytes, (c) the
  `0xFE` skip-rollover emits `[0xFF][0xFF]`.
- `world::GameWorld` — a tiny in-memory map (NOT the real 2048²): `Login` assigns id + temple
  position; `Viewport` returns expected ground client ids around the center.
- `server::game_service` — integration replay (like M1 `login_service::tests`): feed a built
  challenge-response packet, decrypt the reply, assert the burst opcodes appear in the exact order.
- Acceptance (manual): point real OTClient Redemption at the server and confirm the player renders.

## Risks / unknowns to confirm during apply

- `formats::OtbmMap` must expose town temple positions — verify the `towns` field carries the
  temple coordinate; if not, extend the M2 parser minimally.
- `players` table schema — whether a position is stored; if absent, default to temple.
- Exact speed constants / `addDouble` formula — replicate TFS `protocolgame.cpp` precisely.
- Underground floor walk (z>=8) is deferred; M3 assumes the temple is overground (z<=7).

## Milestone steps (for the implementation plan)

1. `protocol::challenge` — encode `0x1F`.
2. `protocol::game_login` — parse outer + RSA block.
3. `protocol::enter_world` — burst encoders (opcode-only, stats, skills, lights, inventory, basic
   data, icons, magic effect, `0x32`).
4. `protocol::map_description` — `0x64` viewport + skip encoding + `add_item`.
5. `world::GameWorld` — actor: map + otb client-id table + player registry; `Login` + `Viewport`.
6. `persistence` — load character by name (default temple position).
7. `server::game_service` — handshake + burst assembly + XTEA; `main.rs` startup wiring.
8. Integration replay test + manual OTClient acceptance.
