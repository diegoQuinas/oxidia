# M4 — Core Walk + Visible Creature (design)

Open Tibia server in Rust, protocol **10.98**, client target **OTClient Redemption**.
Reference spec: **TFS 1.4.2** at `reference/tfs/` (read-only — port byte-faithfully, never edit).

## Scope

M4 makes the player **visible** and **walkable on a single floor**, with collision.

In scope:
- Render the player creature (outfit) on its tile in the `0x64` map description — fixes the currently invisible knight.
- Directional walk: arrow keys (`0x65`–`0x68`) and diagonals (`0x6A`–`0x6D`).
- Turn in place (`0x6F`–`0x72`).
- Collision against blocking tiles (walls/water/void).
- `0x6D` creature move + map slice on tile crossing; `sendCancelWalk` on a blocked step.

Out of scope (deferred):
- Floor changes / stairs / underground (z ≥ 8) walk — next slice.
- Auto-walk / click-to-move pathfinding (`0x64` `parseAutoWalk`, A*).
- Other creatures, spectators, broadcast / push notifications (arrive with creatures, post-M4).
- Real player persistence of position (M4 still spawns at the temple every login).

Acceptance: real **OTClient Redemption** shows the **Test Knight** standing on the Thais
temple ground **with its outfit visible**, the arrow keys walk it around the temple, and
walls/water stop it (the client snaps back instead of clipping through).

## Architecture

No change to the actor model. The `world` actor (`GameWorld`) remains the single
authority over player position and is driven by **request/response** over `oneshot`
channels. The connection task (`game_service::run_session`) parses incoming client
opcodes, issues `Move`/`Turn` commands, awaits the `MoveResult`, then encodes and sends
the server→client packets itself.

No per-player back-channel and no broadcast. With a single player there are no spectators
to notify; push/fan-out is YAGNI until other creatures exist. This keeps M4 a pure
request/response extension of M3.

```
OTClient ──walk opcode──▶ run_session ──Command::Move{dir}──▶ GameWorld actor
                              ▲                                     │
                              └────────── MoveResult ◀──────────────┘
                              │
              encode 0x6D + map slice (or cancelWalk) ──▶ OTClient
```

## Components

### 1. Visible creature — `protocol/map_description.rs`

The `0x64` encoder currently writes only the ground item per tile
(`[u16 env][u16 clientId][u8 0xFF]`) and never serializes creatures, so OTClient draws
the floor with nothing on it → invisible knight.

Extend the tile serialization to write creatures standing on a tile **after** its ground
item (matching the TFS tile thing-stack order: ground, then items, then creatures). The
encoder gains a creature lookup alongside `GroundSource` — a small provider that yields
the creatures at a given `(x, y, z)`.

Creature bytes are a byte-faithful port of TFS `ProtocolGame::AddCreature`
(`reference/tfs/src/protocolgame.cpp`) for protocol 1098:

- Known-creature marker: `0x0061` (unknown) → `[u32 removeId][u32 creatureId][u8 creatureType][string name]`; `0x0062` (known) → `[u32 creatureId]`. The player enters **unknown** on first send (`removeId = 0`, known-creature set starts empty).
- Then: `u8 health%`, `u8 direction`, outfit (`u16 lookType` + `u8 head/body/legs/feet` + `u8 addons`; or `lookType == 0` → `u16 lookTypeEx` item), `u8 lightLevel`, `u8 lightColor`, `u16 speed`, `u8 skull`, `u8 partyShield`, and any further version-gated fields required by the 1098 OTClient parser.

The exact field set and order is verified against the OTClient `getCreature` parser, not
just TFS send code (same discipline as M3's `0xA0`/`0x17`). The Test Knight uses a fixed
outfit for now.

A small per-connection **known-creatures set** tracks which creature ids have already been
sent as `0x0061`, so later sends (after `0x6D`, slices) can use `0x0062`. For M4 with one
self-creature this is trivial but keeps the encoder honest.

### 2. Walkability — `world/map.rs`

`StaticMap` currently stores only `ground: (x,y,z) -> client_id`. Collision needs to know
which tiles block movement.

At map load, derive a blocking set: a tile is **walkable** iff it has a ground item **and**
none of the items on it (ground + stacked items from the `.otbm`) carry the `items.otb`
`block-solid` flag. The `items.otb` flags were already parsed in M2 (`ItemType.flags`);
cross the `.otbm` tile's server-ids against those flags at load time.

`StaticMap` gains `is_walkable(pos: Position) -> bool` (out-of-bounds and groundless tiles
are not walkable). The blocking data is precomputed once at load (a `HashSet` of blocked
coords or a per-tile flag), so move validation is an O(1) lookup.

### 3. `Move` / `Turn` commands — `world/game.rs`

- `PlayerState.position` becomes mutable; add `direction: Direction` (default South, the spawn facing).
- New `Direction` enum: `North, East, South, West, NorthEast, SouthEast, SouthWest, NorthWest` with a `delta() -> (dx, dy)` helper and a `to_byte()` for the protocol.
- New commands:
  - `Move { id: u32, direction: Direction, reply: oneshot::Sender<MoveResult> }`
  - `Turn { id: u32, direction: Direction, reply: oneshot::Sender<MoveResult> }`
- `MoveResult { outcome: Moved { from: Position, to: Position } | Blocked, facing: Direction }`.
- `Move` handling: compute `dest = pos + direction.delta()` (same z); if `map.is_walkable(dest)` → update position **and** facing, return `Moved`; else update facing only (the creature still turns toward a wall) and return `Blocked`.
- `Turn` handling: update facing only; always returns a turn outcome (no position change).
- `WorldHandle` gains `move_player(id, dir)` / `turn_player(id, dir)` async helpers wrapping the oneshot round-trip.

### 4. Opcode dispatch — `server/game_service.rs`

`run_session` stops discarding frames. Per received frame:

1. `xtea::decrypt_message` the payload, read the inner `[u16 length]`, take the opcode byte.
2. Dispatch:
   - `0x65→North, 0x66→East, 0x67→South, 0x68→West` → `Move`
   - `0x6A→NorthEast, 0x6B→SouthEast, 0x6C→SouthWest, 0x6D→NorthWest` → `Move`
   - `0x6F→North, 0x70→East, 0x71→South, 0x72→West` → `Turn`
   - `0x1E` (pong) → ignore
   - `0x14` (logout) → close session cleanly
   - anything else → drain (log at trace, no crash)
3. For a `Move`/`Turn`, issue the command, await `MoveResult`, encode the response (below), and `send_encrypted`.

The 10 s keep-alive ping (`0x1D` on idle) is preserved exactly as in M3.

### 5. Server→client encoders — `protocol/`

- **`0x6D` creature move** — `[0x6D][oldPos x,y,z][u8 oldStackPos][newPos x,y,z]`. `oldStackPos` is the creature's stack index on the old tile (ground = 0, so a lone player creature = 1; compute from the tile contents).
- **Map slice on a step** — port of TFS `sendMoveCreature` viewport correction:
  - North step → `0x65` + a `18 × 1` description of the newly revealed top row.
  - South step → `0x67` + a `18 × 1` description of the new bottom row.
  - East step → `0x66` + a `1 × 14` description of the new right column.
  - West step → `0x68` + a `1 × 14` description of the new left column.
  - Diagonal step → send a full `0x64` map description (TFS sends the full description for diagonals rather than two slices).
  - The slice encoders reuse the same skip-encoding + creature serialization as the full `0x64`; the only difference is the rectangle bounds. The exact edge offsets are ported byte-for-byte from `protocolgame.cpp` and validated against an OTClient-faithful slice decoder.
- **`sendCancelWalk`** — `[0xB5][u8 direction]` (verify the exact opcode against `protocolgame.cpp`). Sent on a `Blocked` result so the client cancels its predicted walk and faces the attempted direction.

## Data flow

1. Login (M3 path, unchanged) → burst now renders the player creature on its tile via the extended `0x64`.
2. Client sends a walk opcode → `run_session` decrypts, dispatches `Move`.
3. Actor validates against `is_walkable`:
   - **Moved** → connection encodes `0x6D` + the appropriate slice (or full `0x64` for diagonal), faces updated, sends one encrypted frame.
   - **Blocked** → connection encodes `sendCancelWalk`, position unchanged.
4. Turn opcode → actor updates facing → connection sends a creature update (turn); position unchanged.

## Error handling

- Blocked / out-of-bounds / groundless destination → `Blocked` → `sendCancelWalk`, no position change.
- Frame fails to decrypt or has a malformed inner length → log + drop the frame, keep the session alive (do not panic).
- Unknown opcode → drain silently (trace log), as M3 did, but now after parsing the opcode byte.
- Actor `oneshot` drop (player gone) → connection task ends its loop cleanly.

## Testing

**`world`**
- Move onto a walkable tile updates position and facing.
- Move into a block-solid tile returns `Blocked`, position unchanged, facing updated.
- Move off-map / onto groundless tile returns `Blocked`.
- Turn updates facing without moving.

**`world/map`**
- A tile carrying a block-solid item is not walkable.
- Plain ground tile is walkable.
- Void / no-ground tile is not walkable.

**`protocol`**
- `0x6D` creature move round-trips (encode → decode old/stack/new).
- Map slice (`18×1` and `1×14`) encodes match an OTClient-faithful slice decoder (extends M3's `decode_stream` to a rectangle).
- `AddCreature` serialization round-trips against an OTClient-faithful creature decoder (asserts field order/values for 1098).
- `sendCancelWalk` byte layout.

**`server/game_service` (integration over `tokio::io::duplex`)**
- Login burst → feed a `0x66` (walk east) frame → assert a `0x6D` + `0x66` slice come back.
- Feed a walk into a wall → assert `sendCancelWalk` comes back and no `0x6D`.

**Live acceptance**
- Real OTClient Redemption: Test Knight visible with its outfit on the Thais temple ground; arrow keys walk it; walls/water stop it with a client snap-back.

## Protocol gotchas (to confirm during implementation)

- `AddCreature` field set is **version-gated**; verify each field against the 1098 OTClient `getCreature` parser, not just TFS send order. Wrong field count desyncs the whole tile stream.
- Tile thing-stack order in the `0x64` stream: ground, then items, then creatures. The creature must come **after** the ground item bytes, before the tile's trailing run marker.
- `oldStackPos` in `0x6D` must match where the client thinks the creature sits on the old tile, or the client move animation desyncs.
- For diagonal steps TFS does **not** send two slices — it sends a full `sendMapDescription`. Don't invent a two-slice scheme.
- The map slice encoders must reuse the **exact** skip-encoding of the full `0x64` (skip starts at `-1`, persists across the single floor) — a `1×N` strip is still skip-encoded.
- `sendCancelWalk` opcode value and payload — confirm against `protocolgame.cpp` for 1098.
