# Backlog — future work

Short, deliberately deferred items. Each notes the rationale and where the work
would land. Not a roadmap — just things we chose not to do yet, captured so they
are not lost.

## Real chat channels

**What:** Implement Tibia chat channels (open-channel handshake `0xB2` / channel
list `0xAB` / close `0xAC`, and channel-scoped `0xAA` speech).

**Why deferred:** The GM `/help` output currently uses a `0xB4`
`MESSAGE_STATUS_CONSOLE_BLUE` message pushed only to the sender's session —
private and scrollable, which is enough for command output. Full channels are a
larger feature (handshake + per-channel membership) and nothing yet needs them.

**Where:** `crates/protocol/src/chat.rs` (currently `parse_say` rejects
channel/private speak types), the reader loop in `crates/server/src/game_service.rs`,
and the world actor in `crates/world/src/game.rs`. Marked with `TODO(future)` near
`Game::gm_help`.

## Per-character home town

**What:** Store a `town_id` per character so the no-argument `/temple` command
teleports to the character's own town temple instead of the server's spawn temple.

**Why deferred:** No character currently carries a `townId` (see
`StaticMap::temple_for`, which notes this awaits M8 persistence). Until then,
no-arg `/temple` falls back to `StaticMap::spawn()`.

**Where:** `crates/persistence` (add `town_id` to `PlayerSave` + schema),
`crates/world/src/game.rs` `InitialState` / `PlayerState` and `Game::gm_temple`.
Marked with `TODO(future)` near `Game::gm_temple`.
