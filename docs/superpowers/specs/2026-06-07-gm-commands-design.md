# Gamemaster Commands — Design

**Date:** 2026-06-07
**Branch:** m10.2-equipment (or a dedicated `gm-commands` branch)
**Status:** Approved design, pre-implementation

## Goal

Give a designated gamemaster character in-game chat commands to spawn items and
teleport creatures. First batch:

| Command | Effect |
|---|---|
| `/item <server_id> [count]` | Spawn an item on the GM's own tile |
| `/goto <x> <y> <z>` | Teleport self to a position |
| `/teleport <name> <x> <y> <z>` | Teleport another player to a position |
| `/teleportto <name>` | Teleport self next to/at another player |
| `/bring <name>` | Teleport another player to the GM's position |

## GM designation

A character is a gamemaster iff its name matches a hardcoded value
(`"diego"`, case-insensitive). This is the "God Diego" rule — simple, no schema
change, no in-game promotion flow.

- The `gamemaster: bool` flag already exists on `PlayerState` (game.rs:136) and
  `InitialState` (game.rs:79). It is **not** persisted in the DB; it is set at
  login time.
- Change: in `game_service::handle_game` (around line 234), set
  `initial.gamemaster = name.eq_ignore_ascii_case("diego") || req.gamemaster;`
  so the hardcoded god is always GM, without removing the existing login-packet
  path.

Tiered access levels are explicitly out of scope. The binary `gamemaster` flag is
the only privilege.

## Architecture

### Decision: parse inside the world actor (not the network layer)

Incoming chat text that starts with `/` is forwarded verbatim to the world
actor as a single command. The actor owns the security gate AND the parser.

Rationale:

- The gate (`gamemaster == true`) and the parser live in one place.
- GM commands need actor-only state: player-by-name lookup, tile materialization,
  spectator sets. The network layer has none of this.
- The network layer stays dumb and never trusts the client: "starts with `/` →
  `world.gm_command(id, text)`". The world decides everything else.

### Data flow

```
client 0x96 "say"  →  reader_loop (game_service.rs:346)
                         │
                         ├─ text.starts_with('/') ──→ world.gm_command(id, text)
                         │                               │
                         └─ else ──────────────────→ world.say(id, type, text)
                                                         │
                                  Game actor: Command::GmCommand { id, text }
                                                         │
                                              do_gm_command(id, text)
                                                ├─ gate: is players[id].gamemaster?  (no → 0xB4 reply, stop)
                                                ├─ parse verb + args
                                                └─ dispatch to a primitive
```

## The two primitives

All five commands reduce to two operations plus a name-lookup helper.

### Primitive 1 — `do_teleport(player_id, to: Position)`

Moves a creature to an arbitrary position, bypassing walkability checks. Mirrors
the far-jump branch of `walk::walk_update` (walk.rs:179).

Steps:

1. Capture `from = players[player_id].position`.
2. To spectators of `from` who can no longer see the creature: send
   `walk::remove_creature_by_id(id)` → `0x6C`.
3. Set `players[player_id].position = to`.
4. To the moved player: send a full map re-description centered on `to`
   (`map_description::encode(Center{to}, ...)` → `0x64`).
5. To spectators of `to` who could not see the creature before: send
   `tile_creature::add_tile_creature(to, stackpos, &bytes)` → `0x6A`.

Notes:
- Reuse existing spectator/known-set bookkeeping (`PlayerState.known`) so we only
  add/remove where visibility actually changes, consistent with the walk path.
- No floor-change/stair resolution: teleport lands exactly on `to`.

Wrappers:
- `/goto x y z` → `do_teleport(self_id, pos)`
- `/teleport <name> x y z` → `do_teleport(find(name)?, pos)`
- `/teleportto <name>` → `do_teleport(self_id, find(name)?.position)`
- `/bring <name>` → `do_teleport(find(name)?, players[self_id].position)`

Occupancy simplification (v1): `do_teleport` lands the creature on the target
position exactly, without searching for a free adjacent tile. For `/teleportto`
and `/bring` this means the moved creature lands on the target player's own tile.
Tibia normally allows only one creature per tile; we accept the visual overlap in
v1 since teleport already bypasses walkability. Adjacent-tile resolution is a
later refinement, not part of this batch.

### Primitive 2 — `do_spawn_item(pos, server_id, count)`

Places an item on a tile and notifies spectators. Reuses the `dynamic` overlay
COW path already used by item movement.

Steps:

1. Look up `ItemMeta` by `server_id` in `StaticMap::item_meta` (map.rs:146). If
   absent → error reply, stop.
2. `self.materialize(pos)` to get a mutable `TileStack` in `dynamic`.
3. Build `WireItem { client_id: meta.client_id, subtype, animated: meta.animated }`
   where `subtype = if meta.stackable { Some(count.clamp(1,100)) } else { None }`.
4. Insert into the tile stack at the correct stack position (above ground, below
   creatures — same insertion rule as `do_move_thing`'s destination handling),
   updating `items`, `server_ids`, and `counts` in parallel.
5. `broadcast_dest(pos, stackpos, &wire_item, false)` → `0x6A add_tile_item` to
   all spectators of `pos`.

Notes:
- Respect `MAX_TILE_THINGS` (10): if the tile is full, error reply, stop.
- `count` defaults to 1 and only matters for stackables; ignored otherwise.

### Helper — `find_player_by_name(name) -> Option<u32>`

Case-insensitive scan of the actor's player map. Returns the creature id or
`None`. Used by `/teleport`, `/teleportto`, `/bring`.

## Parsing

`do_gm_command` splits `text` on whitespace after stripping the leading `/`:

- `item <id> [count]` — `id: u16`, optional `count: u16` (default 1)
- `goto <x> <y> <z>` — three `u16`/`u8` coords
- `teleport <name> <x> <y> <z>`
- `teleportto <name>`
- `bring <name>`

Unknown verb or malformed args → error reply. Parsing is total: no `unwrap`,
every failure path produces a `0xB4` message and leaves the world untouched.

## Feedback to the GM

Every command replies to the GM with a white console text message via `0xB4`
TextMessage (the same opcode used for floating damage numbers — see the
floating-damage memo). Examples:

- Success: `"Created item 2400 on your tile."`, `"Teleported to (1000, 1000, 7)."`
- Error: `"Player 'pepe' not found."`, `"Unknown item id 99999."`,
  `"Usage: /goto <x> <y> <z>"`, `"This tile is full."`

Pick the TextMessage subtype that renders as a status/console line (not a
combat number); confirm the exact subtype byte against the floating-damage
implementation when wiring it.

## New / changed pieces

1. `game_service.rs` — set `initial.gamemaster` by name; add the `/`-prefix hook
   in `reader_loop` before `world.say()`.
2. `game.rs` — new `Command::GmCommand { id, text }` variant; `WorldHandle::gm_command`;
   `Game::do_gm_command` (gate + parse + dispatch); `do_teleport`, `do_spawn_item`,
   `find_player_by_name`; an outbound `0xB4` console-message helper if one does not
   already exist.

## Out of scope (YAGNI)

- Tiered access levels / groups.
- Persisting GM status in the DB.
- In-game GM promotion.
- Spawning items into inventory/containers (only onto the GM's tile for now).
- Item subtypes beyond stackable count (fluids, charges).
- Cross-floor teleport stair resolution (teleport lands exactly on target).

## Testing

Per project convention (memory: "No tests until manual validation"), validate
manually first: log in as `diego`, run each command, confirm item appears /
creature teleports for both the GM and a second observing client. Add unit tests
for the parser and the two primitives only after manual validation passes.
