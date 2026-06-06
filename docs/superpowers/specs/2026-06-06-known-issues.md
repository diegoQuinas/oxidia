# Known Issues — live acceptance findings (2026-06-06)

Two defects observed during live OTClient (protocol 10.98) acceptance after the M4
core-walk merge. Both are recorded here to be scheduled, not yet fixed. Root causes
were traced in code, not guessed — see "Evidence" under each.

---

## ISSUE-1 — Map renders ground only: no walls, borders, or stacked items

**Status:** RESOLVED on branch `issue-1-tile-stack` (per-tile item stack +
correct stackpos). Design: `2026-06-06-issue-1-tile-stack-design.md`; plan:
`../plans/2026-06-06-issue-1-tile-stack.md`. Live OTClient acceptance pending.

**Severity:** High — the world is unrecognizable; collision against walls is also
absent because walls are never represented server-side.

### Symptom
The map looks "flat" and wrong: ground textures (grass, stone, dirt) render
correctly, but walls, wall-borders, columns, and any object stacked above the
ground are missing. Reported as "maybe the OTBM items aren't version 1098."

### Actual root cause (NOT a version mismatch)
The `server_id -> client_id` mapping via `items.otb` is correct — that is exactly
why ground tiles render with the right sprites. The real cause is that the world
model stores **only one item per tile** (the ground), and the wire encoder sends
only that one item.

### Evidence
- `crates/world/src/map.rs:19` — the world map is
  `ground: HashMap<(u16, u16, u8), u16>` — a single client id per coordinate.
- `crates/world/src/map.rs:37-39` — build step reads `tile.items.first()` only and
  inserts that one client id. Items `[1..]` (borders, walls, objects) are dropped.
- `crates/protocol/src/map_description.rs:147-150` — `add_item` writes exactly one
  `[u16 clientId][u8 0xFF]` per tile. There is no per-tile item loop.
- `GroundSource` trait (`map_description.rs:20`) exposes only
  `ground(x, y, z) -> Option<u16>` — the interface itself cannot express a stack.

### Direction for the fix (when scheduled)
- Replace the ground-only model with a per-tile **item stack** (`Vec<u16>` of client
  ids, in OTBM order), preserving items `[1..]`.
- Broaden the `GroundSource` abstraction to yield the full tile stack, not a single
  ground id. TFS encodes up to 10 items per tile (stackpos cap); mirror that.
- Update `map_description` / `walk` slice encoders to loop the stack per tile,
  keeping the byte-faithful skip-encoding intact (`protocolgame.cpp:633-680`).
- Block-solid flags already read from `items.otb` per item (`map.rs:42-44`); once the
  stack is stored, collision against walls falls out for free.

---

## ISSUE-2 — Walking under cover / floor transitions desync ("teleporting", map glitch)

**Severity:** High — movement becomes unreliable near any covered tile or z-change.

### Symptom
Walking south, the player reached what looked like a roof/ceiling. Walking *under*
it, the character began "teleporting" (snapping positions). Trying to walk back
north left the map inconsistent and glitched. Black region to the south in the
screenshot is the edge of the loaded/sent tiles.

### Root cause
Floor-change handling is **deferred** (documented in `PROGRESS.md`: "Floor changes /
underground (z>=8) and auto-walk are deferred"). The walk path assumes a single
floor and never reconciles a z-change with the client.

### Evidence
- `crates/protocol/src/walk.rs:55-81` — `walk_update` carries `nz` through the
  directional slices but emits no floor-up/floor-down sequence. There is no `0xBE`
  (move up) / `0xBF` (move down) handling anywhere.
- `crates/protocol/src/map_description.rs:42` — encoder is documented "Overground
  centers (z <= 7) only"; underground (z>=8) viewport stacking is not implemented.
- When the client steps onto a tile that should change z (stairs / hole / cover),
  the server keeps the player on the same floor while OTClient's covered-tile logic
  changes which floors it draws → client and server disagree on position → the
  "teleport" snap, then inconsistent slices on the return north.

### Direction for the fix (when scheduled)
- Implement floor changes as its own slice (already named as a deferred M4 follow-up
  in `PROGRESS.md`): detect z-change on step, send the floor-up/floor-down map
  description sequence TFS uses, and reposition authoritatively.
- Implement the underground (z>=8) viewport stacking in `map_description`.
- Until then, consider hard-blocking steps that would change z (treat stair/hole
  tiles as non-walkable) so the client never desyncs — a stopgap, not the fix.

---

## ISSUE-1b — Creature stackpos >= 10 sends the wrong move/turn wire form

**Status:** RESOLVED (commit `9012567`). Live testing of ISSUE-1 exposed a broader
stackpos problem — the server's items.otb stackpos disagreeing with OTClient's
`.dat` placement on *any* decorated tile, not just at 10 — so creature move
(`0x6D`) and turn (`0x6B`) now use the creature-id form `[0xFFFF][id]`, which
ignores stackpos entirely. The `>= 10` case below is covered by the same fix.

**Severity:** Low — unreachable on real M4 walkable tiles; deferred follow-up of
ISSUE-1, recorded so it is not silently dropped.

### Root cause
After ISSUE-1, `StaticMap::creature_stackpos` can return up to 10 (a tile with a
ground item plus nine always-on-top items). But `creature_move`
(`crates/protocol/src/walk.rs`) and `creature_turn` always emit the `stackpos < 10`
wire form `[opcode][pos][stackpos u8][...]`. TFS `sendMoveCreature`
(`reference/tfs/src/protocolgame.cpp:2600-2606`) branches at `oldStackPos >= 10` to
the extended form `[0x6D][0xFFFF][creatureID u32][newPos]`. A creature stepping off
(or turning on) a tile with nine always-on-top items would therefore desync the
client.

### Why it is deferred
Reaching stackpos 10 requires nine always-on-top items stacked on a single
**walkable** tile (block-solid tiles are never stood on). That configuration does
not occur in real Tibia maps. Correctness for the common decorated tile
(stackpos 1..9) is already byte-correct and tested.

### Direction for the fix (when scheduled)
- Thread the creature id into `creature_move` (it is available at the
  `walk_update` call site as `session.id`).
- When `stackpos >= 10`, emit `[0x6D][0xFFFF][creatureID u32][newPos]` (and the
  analogous extended form for `creature_turn`).
- Add a wire test asserting the `0xFFFF` form for `stackpos == 10`.

---

## Scheduling note
ISSUE-1 (tile stacks) is RESOLVED on `issue-1-tile-stack` — it restored the visible
world and wall collision in one change, and is a prerequisite for rendering
stairs/holes correctly, which ISSUE-2 depends on. Next: ISSUE-2 (floor changes),
with ISSUE-1b as a low-priority follow-up.
