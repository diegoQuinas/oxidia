# Proposal: Click-to-Move Pathfinding

## Intent

Players click a point on the map → character walks there using A* pathfinding. Currently `/follow` (0xA2) works but ad-hoc map clicks (0x64 auto-walk) are silently dropped.

## Scope

### In Scope
- Parse `0x64` auto-walk, compute A* path to clicked tile, drain per AI tick
- New `go_to_position: Option<Position>` on `PlayerState` (parallel to `follow_target`)
- Cancel auto-walk on manual movement, PZ entry, ESC (0xBE), or arrival
- Same-floor only — reject cross-floor clicks

### Out of Scope
- Cross-floor navigation (stairs, ladders, height ramps)
- Path visualization / client-side waypoints
- Click-through-walls optimization

## Capabilities

### New Capabilities
- `player-auto-walk`: auto-walk / goto pathfinding for player characters — receive `0x64` packet, compute A* path, drain per tick, cancel on PZ/manual/ESC

### Modified Capabilities
- None — no existing spec covers player movement behavior at spec level

## Approach

1. Add `Command::GoToPosition { id, target: Position }` + `go_to_position: Option<Position>` on `PlayerState`
2. Wire `0x64` in `game_service.rs` → `world.goto_position(id, destination)` → compute A* in `do_go_to_position()`, fill `list_walk_dir`
3. Extend `on_monster_ai_tick()` to also walk players with `go_to_position` set (repath on empty queue, PZ check, cancel on manual move)
4. On arrival (within 1 tile of target) or PZ enter: clear `go_to_position`, drain queue
5. On manual `do_move()`, ESC (0xBE): clear `go_to_position` + `list_walk_dir`

## Affected Areas

| Area | Impact | Description |
|------|--------|-------------|
| `crates/world/src/game/mod.rs` | Modified | Add `Command::GoToPosition`, `do_go_to_position()`, `go_to_position` field, extend AI tick |
| `crates/server/src/game_service.rs` | Modified | Wire `0x64` → `world.goto_position()` |
| `crates/protocol/src/walk.rs` | Modified | Add `auto_walk_destination()` to derive target from steps + start position |
| `crates/world/src/game/movement.rs` | Modified | Clear goto on manual `do_move()` |

## Risks

| Risk | Likelihood | Mitigation |
|------|------------|------------|
| PZ bypass — player clicks into PZ or walks from PZ | Low | Check `is_protection_zone` before A* + clear on PZ entry during tick |
| Path desync — client computed steps don't match server A* | Low | Ignore client steps; server computes own A* path from click position |
| Blocked path loops — infinite repathing against wall | Low | Limit repath attempts; clear goal if no path found after N ticks |

## Rollback Plan

Revert the `0x64` handler to the `tracing::debug!("not implemented")` stub. Remove `Command::GoToPosition` variant and `go_to_position` field from `PlayerState`. The AI tick change (walk any player with queue, not just follow) is safe to keep — empty queue = no-op.

## Dependencies

- None — all infrastructure exists (A*, `list_walk_dir`, AI tick, packet parser)

## Success Criteria

- [ ] Player clicks a walkable tile in view → character walks there using A*
- [ ] Player clicks an unwalkable tile (wall, water) → no movement, no crash
- [ ] Player reaches destination → character stops
- [ ] Manual step or ESC cancels auto-walk
- [ ] PZ entry cancels auto-walk
- [ ] `cargo test` + `cargo clippy -D warnings` passes
