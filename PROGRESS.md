# PROGRESS — Oxidia

Open Tibia server in Rust, protocol **10.98**, client target **OTClient Redemption** (mehah/otclient).
Reference spec: **TFS 1.4.2** at `reference/tfs/` (read-only — never edit, never port line-by-line).

> Read this file first in every session.
> Repo layout: the OTClient test client lives in `../client/`; this Rust workspace **is** the server. Run every command below from inside `server/`. The server binary is `oxidia`.

## Current status

- **Milestone:** M6 ✅ **chat complete and accepted live**. M6.1 (stairs / floor changes) code-complete; underground floor-change desync fixes (teleport sloped stairs/ladders + boundary mover re-add) landed and **live-accepted**. **M7 (combat core + PvP melee) is code-complete, live acceptance pending** — branch `m7-combat`. **M8 ✅ persistence + accounts + outfit change/persist, accepted live** (load on login, save on logout; login never stacks on an occupied tile). M5 ✅ presence, M4 ✅ walk. M6.2 (ladders/holes, use-driven) is **folded into M11** — it is script-driven (`teleport.lua` `onUse`), not a data milestone, so it ships on the Lua runtime. Auto-walk remains deferred.
- **Build:** `cargo build` clean, `cargo test` green (workspace), `cargo clippy --all-targets -- -D warnings` clean.
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
| M5 | Multiplayer presence: spectator / known-creatures system, broadcast movement | ✅ done |
| M6 | Chat: say / whisper / yell + default channel | ✅ done |
| M6.1 | Floor changes & stairs (walk-driven): `items.xml` loader (`hasHeight` + `floorChange` dir), tile vertical semantics, walk up/down in `do_move`, `0xBE`/`0xBF` move-up/down, underground (z≥8) viewport + ±2 visibility band | ✅ code / live pending |
| M6.2 | Ladders & holes (use-driven) — **deferred to M11**: the behavior is script-driven (`teleport.lua` `onUse`), not data; ladders/grates carry no `items.xml` attribute. Belongs on the Lua runtime, not hardcoded in Rust. Research: `docs/superpowers/specs/2026-06-07-m6.2-ladders-design.md` | ⏸️ → M11 |
| M7 | Combat core + PvP melee: damage, HP sync, death, respawn, protected zones | ✅ code / live pending |
| M7.1 | Combat polish: death→logout flow (relog at temple, save-on-death), protection-zone client badge (ICON_PIGEON), blood-hit effect fix | ✅ code / live pending |
| M8 | Persistence + accounts: per-account characters, saved position/stats/outfit (load on login, save on logout via unbounded save channel); outfit change + persist (`0xD2` request → `0xC8` window, `0xD3` set → apply + `0x8E` broadcast); login never stacks on an occupied tile (`free_spawn_near`) | ✅ done |
| M8.1 | PvP justice — PK skull system: white skull on first unprovoked attack (`whiteSkullTime` 15 min) + yellow skull shown relationally to the victim; unjustified kills (victim was `SKULL_NONE`, not in war) count as frags → red skull (`killsToRedSkull` 3) / black skull (`killsToBlackSkull` 6); frag decay (`timeToDecreaseFrags` 24 h, `checkSkullTicks`); skull byte in `AddCreature` + `sendCreatureSkull` update; `getSkullClient` relational coloring. Depends on M7 (kills) + M8 (persist skull state + frag timestamps). Research: TFS `const.h` `Skulls_t`, `player.cpp` (`addUnjustifiedDead`/`checkSkullTicks`), `config.lua.dist` | ⬜ |
| **B** | **Items & Inventory** | |
| M9 | Ground items, stacks, look-at | ⬜ |
| M10 | Inventory & equipment: move, equip, containers, use | ⬜ |
| **C** | **Scripting** | |
| M11 | Lua runtime (mlua): hot-reloadable content hooks (onUse/onStepIn/onSay/…). Includes use-driven floor changes (M6.2: ladders/holes via `teleport.lua` `onUse`) — see `docs/superpowers/specs/2026-06-07-m6.2-ladders-design.md` | ⬜ |
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
| M23 | Guild war systems: war declarations, war PvP rules, war/PZ exceptions (basic PK skulls/frags moved to M8.1) | ⬜ |
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

**Deferred (later slice):** floor changes / stairs / underground (z≥8) walk → **now scoped as M6.1 (stairs) + M6.2 (ladders/holes)**; auto-walk / click-to-move pathfinding; diagonal corner-cut blocking; real player persistence; walkthrough byte fidelity (self currently `0x00`).

## M5 plan

Design + plan: `docs/superpowers/specs/2026-06-06-m5-presence-design.md`, `docs/superpowers/plans/2026-06-06-m5-presence.md`. Scope: **full presence** — login appear, walk/turn broadcast, logout/disconnect remove, viewport in/out. Architecture chosen after verifying TFS 1.4.2 and improving on it (see README "Why Rust over a C++ port").

1. ✅ `protocol::tile_creature` — `add_tile_creature 0x6A` (`[0x6A][pos][stackpos][creature thing]`) and `remove_tile_thing 0x6C` (`[0x6C][pos][stackpos]`), byte-faithful round-trip tests.
2. ✅ `world::game` rewritten to **unified push**: the actor is the single builder of all outbound packets. `PlayerState` gains `outfit`, `push_tx: mpsc::Sender<Vec<u8>>`, `known: HashSet<u32>`. `Command` drops the Move/Turn reply channels and gains `Logout`; `Login` returns `LoginAck { snapshot, others }`. Spectators = iterate-all behind `spectators(pos, exclude)` (swappable to a quadtree later). `introduce()` owns the 0x61-full/0x62-short decision via the known-set. `push()` is non-blocking (`try_send` + reap) so the loop never stalls on a slow client.
3. ✅ `server::game_service` — per-session push pipeline: `handle_game` takes the stream **by value** (`Send + 'static`), splits it, **spawns a writer task** that greedily coalesces queued plaintext payloads into one XTEA frame and pings; a plain **reader loop** decodes inbound walk/turn into fire-and-forget commands. Reader/writer race via `select!` so a dead writer can't strand a blocked reader. (A single `select!` over `read_frame` + the channel was rejected as a cancel-safety bug.)
4. ✅ **Accepted live** — two OTClients: each sees the other spawn (teleport puff), walk, turn, and poof out on logout; no desync. Spectators get the teleport effect on both login and logout (the logout puff is a deliberate polish over TFS, which removes silently).

**Stackpos invariant (critical, found in final holistic review):** `0x6A`/`0x6C` are position+stackpos packets (no id-form for *add*), while `0x6D`/`0x6B` use the id-form. `StaticMap::creature_stackpos` is a *static* per-tile value, so two creatures on one tile would collide on add/remove → desync. Fix: keep **≤1 creature per tile** — `do_move` rejects a creature-occupied destination (collision) and `login` uses `free_spawn()` (nearest free tile when the temple is taken). Under that invariant the static stackpos is always correct. Co-occupancy has no path in M5 (logins serialize through the single actor; movement is blocked both ways; no teleport/summon).

**Deferred (YAGNI):** quadtree/sectored-grid spectators; known-set eviction cap (1300); multi-floor spectator band (±2 underground) — **now done in M6.1**; `0x6A`/`0x6C` stackpos≥10 id-form; proactive socket close on backpressure kick.

## M7 plan

Design + plan: `docs/superpowers/specs/2026-06-07-m7-combat-design.md`,
`docs/superpowers/plans/2026-06-07-m7-combat.md`. Scope: **PvP melee, end
to end** — `0xA1` attack target, 2 s auto-swing timer, TFS skill-based
fist damage, health-bar (`0x8C`) + self-stats (`0xA0`) sync, death window
(`0x28`), and temple respawn — with protection-zone attack rejection (`0xB4`).
Architecture: a **single global combat tick** (`CombatTick` command on the
existing actor mpsc) drives all in-progress fights, preserving the "one
writer, no locks" model. Pure feeders (wt-data damage math, wt-proto
combat packets) merged to main ahead of this spine.

1. ✅ `world::map` — `OTBM_TILEFLAG_PROTECTIONZONE` precomputed into a
   `HashSet` at load (mirrors `blocked`/`floor_change`); `is_protection_zone`
   and `temple_for` added.
2. ✅ `world::game` — `PlayerState` gains `health`, `max_health`,
   `fist_skill`, `attacking: Option<u32>`, `last_attack_ms`; `Command` gains
   `SetTarget` + `CombatTick`; `Game` gains a `StdRng` (entropy-seeded;
   seedable in tests). `do_set_target` enforces PZ rejection (push `0xB4`)
   and self-attack guard. `on_combat_tick` iterates fights, checks
   Chebyshev ≤ 1 same-floor range + `MELEE_ATTACK_INTERVAL_MS` (2000 ms),
   rolls `combat::fist_damage`, calls `apply_damage`. `apply_damage` pushes
   `0x8C` to spectators + `0xA0` to the victim and fires `do_death` on 0 HP.
   `do_death` pushes `0x28`, clears all fights targeting the victim,
   teleports the victim to the temple (remove+add pair — preserves M5
   ≤1-creature-per-tile stackpos invariant), restores HP, and sends a fresh
   map + `0xA0` to the respawned player. The combat tick task is started
   in `spawn`.
3. ✅ `server::game_service` — `reader_loop` intercepts `0xA1` (`parse_attack`
   → `world.set_target`) and drains `0xA2` (follow, ignored) before the
   walk/turn `opcode_action` dispatch.

Gate: `cargo test` **196 green** (whole workspace), `cargo clippy --all-targets
-- -D warnings` clean, `#![forbid(unsafe_code)]` intact.

**Protocol gotchas (M7):**
- **`0xA1` and `0xA2` are inbound AND outbound opcodes** — `0xA1` inbound =
  attack; `0xA2` inbound = follow; outbound `0xA1` = skills, `0xA2` = icons.
  No conflict — namespaced by direction, exactly as `0x6B`/`0x6D` in M4.
- **`0xB4` text message layout** (`sendTextMessage`, protocolgame.cpp:1411):
  `[0xB4][u8 type][u16-str text]`. For PZ rejection: `type = 21`
  (`MESSAGE_STATUS_SMALL`, const.h:190), no extra fields before the string
  (the switch has no case for this type — it falls through to `addString`).
- **`0x8C` goes to all spectators including the victim AND attacker** (they
  are both spectators of the victim's tile at melee range). `0xA0` goes only
  to the victim.
- **`last_attack_ms = 0` priming**: a newly set target fires on the first
  eligible tick whose `now_ms >= MELEE_ATTACK_INTERVAL_MS` (not the very
  first tick, since the tick task consumes the immediate `tick().await`).
  This mirrors TFS `player.cpp:3225-3226`.
- **Death respawn is a remove+add pair** (not a move): the death tile and the
  temple are almost always out of each other's viewport, so `0x6D` would
  deref a wrong stackpos on the spectators of the old tile. The remove at
  the death tile + add at the temple is the same atomic pair as logout/login.
- **No corpse in M7** — no second tile occupant, so the M5 ≤1-creature-per-tile
  invariant is untouched. Corpses land in M13 with the M9 ground-item stackpos.

**Deferred:** corpses/loot (M13); XP/skill/death-penalty (M14); mana/spells/
conditions (M15); monsters (M12); equipped weapons / real `attackValue` (M10);
fight modes / skulls / frags (M23); auto-walk follow; logout-in-fight block
(TODO marker already in `reader_loop`); unfair-fight reduction (M23).

**Final holistic review** caught four bugs fixed post-implementation (all TDD — red then green):
- **W1 (respawn render):** `do_death` only put the victim in `placed`; players standing near the temple were invisible to the respawned victim. Fixed: `placed` now includes all creatures `visible_from(respawn_pos)`, introduced one-by-one (mirrors `do_move`'s `others_in_range` rebuild).
- **W2 (known-set prune):** `do_death` never pruned the victim's `known` set after the respawn teleport. Stale ids from the death tile would be sent short `0x62` form for creatures the client already discarded. Fixed: drop from `victim.known` every id not visible from `respawn_pos` (mirrors `do_move`'s left-view prune).
- **W3 (PZ per-tick):** `on_combat_tick` checked range but not protection zones; a victim who fled into the temple kept taking hits. Fixed: if either party is in PZ, clear `attacker.attacking` and skip — matches TFS `canTargetCreature` (combat.cpp:221-229) clearing the fight, not just suppressing damage.
- **S1 (drawblood effect id):** `EFFECT_DRAWBLOOD = 2` was wrong; TFS `CONST_ME_DRAWBLOOD = 1`, wire = TFS − 1 = `0`. Fixed: constant corrected and moved to `enter_world.rs` next to `EFFECT_TELEPORT` for consistency.

**Live acceptance — PENDING (manual gate):** two OTClient Redemption sessions:
A right-clicks B → B's HP bar drains on both screens; B's own HP digits drop;
continued attacks kill B → B sees `0x28`, respawns at Thais temple with full
HP; A sees B vanish + reappear; standing on a PZ tile, A cannot attack
(status message). Flip M7 to ✅ once this passes.

## M7.1 plan

Design + plan: `docs/superpowers/specs/2026-06-07-m7.1-combat-polish-design.md`,
`docs/superpowers/plans/2026-06-07-m7.1-combat-polish.md`. Scope: **TFS-faithful
M7 polish found during live testing.** Built in worktree `m7.1-combat-polish`
(rebased onto main after M8 landed).

1. ✅ `protocol::enter_world` — blood-hit effect: `EFFECT_DRAWBLOOD` 0→1. TFS
   `sendMagicEffect` sends the effect byte directly (protocolgame.cpp:2326) and
   `CONST_ME_DRAWBLOOD = 1`; wire `0` is dropped by the client as "no effect".
2. ✅ `protocol::enter_world` — `icons(mask: u16)` + `ICON_PIGEON` (`1<<14`, TFS
   const.h:343), replacing the static `icons()`.
3. ✅ `world::game` (`do_move`) — push `0xA2` with/without `ICON_PIGEON` when the
   mover crosses a protection-zone boundary (TFS `getClientIcons`).
4. ✅ `server::game_service` — the enter-world burst carries `ICON_PIGEON` when the
   spawn tile is a PZ (`map.is_protection_zone(center)`).
5. ✅ `world::game` (`do_death`) — **death is now a logout, not an in-world
   respawn.** Send `0x28`, clear fights, id-form remove at the death tile, then
   remove the victim from the world and emit a `SaveRecord` at the **temple** with
   full HP. Dropping the victim's `push_tx` closes the session → the client shows
   the death window and returns to character select; the relog spawns at the temple
   (M8 `login` restores the saved position). Mirrors TFS `onDeath` →
   `sendReLoginWindow` + `removeCreature` (player.cpp:2070, 2197). **Supersedes the
   M7 in-world respawn** — the W1/W2 respawn-render + known-set prune + `free_spawn_near`
   are removed (moot once death logs out).

Gate: `cargo test` green (225), `cargo clippy --all-targets -- -D warnings` clean,
`#![forbid(unsafe_code)]` intact.

**Deferred (confirmed with roadmap owner):** floor blood splat (`ITEM_SMALLSPLASH`
+ decay) → M9; corpse body → M13; PK skull system → M8.1.

**Live acceptance — PENDING (manual gate):** die in PvP → death window → character
select → relog spawns at the temple with full HP; standing in the temple shows the
PZ dove badge (clears on leaving); hits draw a visible blood animation.

## M6.1 plan

Design + plan: `docs/superpowers/specs/2026-06-06-m6.1-stairs-design.md`,
`docs/superpowers/plans/2026-06-06-m6.1-stairs.md`. Scope: **walk-driven vertical
movement** — stairs up/down, underground (z≥8), multi-floor presence. Built in an
isolated worktree (`m6.1-stairs` branch) via subagent-driven TDD. Two TFS-verified
mechanics: **height slopes** (`game.cpp:792-820`, `hasHeight(3)` from the `.otb`)
and **`floorChange` staircase tiles** (`tile.cpp::queryDestination`, direction from
`items.xml`).

1. ✅ `formats::items_xml` — `FloorChange` bitmask + `ItemType.has_height`/`floor_change`; `FLAG_HAS_HEIGHT` (bit 3) from the `.otb`.
2. ✅ `formats::items_xml` — complete `items.xml` loader (`roxmltree`), `floorChange` string→flags, `fromid/toid` ranges, `merge_items_xml` into `ItemType`. Real-file parse tested.
3. ✅ `world::map` — per-tile `floor_change`/`tile_height` precomputed at load; `resolve_floor_change` (faithful `queryDestination` port — verified line-by-line) + `triggers_up`.
4. ✅ `protocol::map_description` — `get_map_description` refactored into per-floor `floor_description` (shared `skip`) + the underground `z-2..=z+2` band (`floor_range`). Overground stays byte-identical.
5. ✅ `protocol::walk` — `0xBE` move-up / `0xBF` move-down (faithful `MoveUpCreature`/`MoveDownCreature` ports, byte-verified) + z-aware `walk_update` (id-form remove at the 7→8 boundary).
6. ✅ `world::game` — vertical `do_move`: height mechanic A then floorChange mechanic B; stair/height landings reached with TFS `FLAG_NOLIMIT` semantics (block-solid on the landing is ignored; the ≤1-creature-per-tile invariant kept).
7. ✅ `world::game` — multi-floor presence: `can_see` ±2 band **with the per-floor `offsetz` x/y projection** (matches the encoder), plus a remove+add at the 7→8 boundary for other-creature broadcasts (TFS `sendMoveCreature` 2633-2649).
8. ✅ `server::main` — load + `merge_items_xml` at boot; the live world has real floor-change data.

Gate: `cargo test` **129 green**, `cargo clippy --all-targets -- -D warnings` clean, `#![forbid(unsafe_code)]` intact.

**Final holistic review** caught two integration gaps the per-task reviews missed (both fixed): `can_see` lacked the floor `offsetz` (cross-floor spectator desync), and the spectator broadcast lacked the 7→8 remove+add. Solo mechanics + the mover's own camera were verified TFS-faithful throughout.

**Live acceptance — PENDING (manual gate):** real OTClient descends the Thais
temple staircase into the underground and climbs back, rendering correctly; a
second client one floor away sees the crossing — no desync. Flip M6.1 to ✅ once
this passes. **Known untested-in-prod:** full underground map description
(`encode` with z>7) has no live path yet (login always spawns at z=7); it's
unit-tested but not exercised until relog/teleport-underground exists.

## M6 plan

Design + plan: `docs/superpowers/specs/2026-06-06-m6-chat-design.md`, `docs/superpowers/plans/2026-06-06-m6-chat.md`. Scope: **TFS-faithful local chat** — say/whisper/yell, position-based. The roadmap's "default channel" = local positional speech (OTClient Local Chat tab); NO joinable-channel system. Cheap because it reuses the M5 spectator + push machinery.

1. ✅ `protocol::chat` — `parse_say(body) -> Option<(SpeakType, String)>` (inbound `0x96` = `[type u8][msg str]`; rejects unsupported types / empty / malformed) and `creature_say` (outbound `0xAA` = `[stmt u32][name str][level u16][type u8][x u16][y u16][z u8][msg str]`). `SpeakType { Say=1, Whisper=2, Yell=3 }`.
2. ✅ `world::game` — `Command::Say` + `do_say`. The actor (single packet builder) reads the speaker's current pos+name, allocates a statement id, and broadcasts `0xAA`. `spectators` generalized to `spectators_in_range(pos, exclude, rx, ry)`. Ranges: say ±8/±6, yell ±18/±14 + UPPERCASE, whisper ±8/±6 with full text to Chebyshev ≤1 and `"pspsps"` to in-view-but-far. Speaker always hears own (pushed explicitly, since `spectators` excludes self).
3. ✅ `server::game_service` — `reader_loop` intercepts `0x96` before the walk/turn dispatch, `chat::parse_say(&payload[1..])` → `world.say(...)`; unsupported/malformed dropped.
4. ✅ **Accepted live** — two OTClients: say heard nearby (not when far); whisper full only to the adjacent client (far-in-view sees `pspsps`); yell heard far in UPPERCASE. Off-screen yell appears in the chat console but shows no floating bubble — correct: `addStaticText` is positional, so a bubble only renders when the speaker is on the recipient's screen (matches TFS/OTClient).

**Key seam (confirmed clean in final holistic review):** chat depends on **no** M5 presence state. `0xAA` carries the speaker NAME (string) + POSITION, not a creature id — so a yell reaching a player who never had the speaker introduced (off their viewport, not in their known-set) still renders. `do_say` only READS positions; it never touches stackpos, the known-set, or the ≤1-creature-per-tile invariant.

**Deferred (YAGNI):** joinable channels (`0x97`/`0x98`/`0xAB`/`0xAC`); private messages (`TALKTYPE_PRIVATE_*`); yell cooldown + anti-spam; multi-floor yell; real speaker level (sent as `1` until M14); NPC/monster speech (M12). Over-255 messages are truncated (TFS drops them) — a deliberate, documented divergence.

## Protocol gotchas (M6 chat)

- **Inbound `0x96` body for say/whisper/yell is just `[type u8][msg str]`** (`parseSay`, protocolgame.cpp:922). Private (5/16) carries a receiver-name string and channel (7/14) a `channelId u16` BEFORE the message — M6 rejects those in `parse_say`.
- **Outbound `0xAA` is name+position based, not creature-id based** (`sendCreatureSay`, protocolgame.cpp:2199). It does not depend on the client knowing the creature, so far yells and off-screen speech render fine.
- **`sendToChannel` is the same `0xAA` but with a `channelId u16` instead of the position** — that's the joinable-channel path, deferred.
- **Ranges (game.cpp):** say/whisper query = client viewport ±8/±6; whisper full text only within Chebyshev 1 (3×3), else literal `"pspsps"` (same WHISPER type); yell = ±18/±14 (TFS multifloor; we are same-floor) and text uppercased.
- **One statement id per utterance**, shared by all recipients (TFS `lastStatementId`); ours is a `u32` `wrapping_add` counter starting at 1.

## Protocol gotchas (M5 presence)

- **`0x6A` add-tile-creature wraps the creature thing**: `[0x6A][pos x:u16 y:u16 z:u8][stackpos:u8]` then the `AddCreature` bytes (`0x61`/`0x62`). The raw creature thing alone is not enough — the client needs the tile + stackpos. (`protocolgame.cpp:2517`)
- **`0x6C` short form is `[0x6C][pos][stackpos:u8]`** for stackpos < 10 (`protocolgame.cpp:3101`); the id-form (`0xFFFF`+id) is only for stackpos ≥ 10 and is deferred.
- **Walk broadcast transition matrix** (spectator union of `from`+`to`): sees both → `0x6D` move; only `to` → `0x6A` appear; only `from` → `0x6C` remove (and drop the id from that spectator's known-set so re-entry re-introduces with `0x61`).
- **The mover's own view** still uses `walk_update` (0x6D + revealed slices); other in-range players are spliced into the new slices via `PlacedCreature`. The client auto-culls creatures that scroll off the edge, so the mover gets no explicit removes.
- **Coalescing multiple game packets into one XTEA frame is legal** — the client decrypts the whole body and parses opcodes sequentially (TFS coalesces into one `OutputMessage` too). The writer drains the channel greedily, so batching happens under load with zero added latency when idle (vs TFS's fixed 10 ms autosend tick).

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
