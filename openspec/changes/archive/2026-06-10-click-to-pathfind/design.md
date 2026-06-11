# Design: Click-to-Move Pathfinding

## Technical Approach

Wire `0x64` auto-walk → parse direction steps → derive target position → run server-side A* → fill `list_walk_dir` → drain per AI tick (extending the existing player-walk section that handles follow-target). New `go_to_position: Option<Position>` on `PlayerState` parallels `follow_target`. Cancellation on manual move, ESC, PZ entry, arrival, or blocked path.

## Architecture Decisions

| Option | Tradeoff | Decision |
|--------|----------|----------|
| Trust client path vs recompute | Client steps may be cheated | Derive target from steps, run server A* |
| Reuse follow_target field | Semantics tied to creature id | New `go_to_position: Option<Position>` |
| Separate tick vs extend existing | Same 100ms tick, same drain pattern | Extend existing `on_monster_ai_tick` player loop |
| Clear goto in do_move vs handle | do_move called by AI tick + keyboard | Clear in `handle()` for `Command::Move` only |
| ESC cancellation via new cmd vs chaining | ClearAutoWalk is self-documenting | Add `Command::ClearAutoWalk` + `WorldHandle` method |

## Data Flow

```
Client 0x64 → reader_loop → world.goto_position(id, dest)
  → do_go_to_position():
    validate same floor, walkable, in view
    collect creature coords, compute A* → list_walk_dir
    set go_to_position = Some(dest)
    cancel existing follow_target

AI tick (per player with go_to_position):
  if PZ                   → clear goto, continue
  if position == target   → clear goto, continue
  if queue empty          → recompute A*, skip if no path
  pop dir                 → do_move(id, dir)
  if do_move fails        → cancel goto + queue

Command::Move (keyboard) → clear goto/follow/queue → do_move
0xBE → set_target(0) + follow_target(0) + clear_auto_walk
```

## File Changes

| File | Action | Description |
|------|--------|-------------|
| `crates/protocol/src/walk.rs` | Modify | Add `auto_walk_destination()` — apply steps to start pos, return target |
| `crates/server/src/game_service.rs` | Modify | Wire `0x64` → `world.goto_position()`; extend `0xBE` to clear auto-walk |
| `crates/world/src/game/mod.rs` | Modify | Add `Command::GoToPosition`, `Command::ClearAutoWalk`, `go_to_position` field, `do_go_to_position()`, dispatch, AI tick player-loop extension |
| `crates/world/src/game/test_support.rs` | Modify | Add `go_to_position: None` to `add_player()` |

## Interfaces / Contracts

```rust
// --- Command enum (game/mod.rs) ---
Command::GoToPosition { id: u32, target: Position },
Command::ClearAutoWalk { id: u32 },

// --- PlayerState field ---
// Inserted after follow_target:
follow_target: Option<u32>,
go_to_position: Option<Position>,  // NEW
list_walk_dir: VecDeque<Direction>,

// --- WorldHandle methods ---
impl WorldHandle {
    pub async fn goto_position(&self, id: u32, target: Position) {
        let _ = self.tx.send(Command::GoToPosition { id, target }).await;
    }
    pub async fn clear_auto_walk(&self, id: u32) {
        let _ = self.tx.send(Command::ClearAutoWalk { id }).await;
    }
}

// --- Protocol (walk.rs) ---
/// Apply 0x64 direction steps to start position, return final Position.
/// Returns None if any step would overflow u16 bounds.
pub fn auto_walk_destination(start: Position, steps: &[AutoWalkStep]) -> Option<Position>;
```

## do_go_to_position() Logic

```
fn do_go_to_position(&mut self, id: u32, target: Position):
    p = self.players.get(&id)?
    if p.position.z != target.z: return               // same floor only
    if !self.map.is_walkable(target): push_cannot_move; return
    if !Self::can_see(p.position, target): return     // out of view
    if p.position == target: return                   // already there

    p.follow_target = None;
    p.go_to_position = Some(target);

    creatures = collect creatures on same z (exclude self)
    params = FindPathParams { full_search: false, max_search_dist: 20 }
    condition: move |pos| pos == target
    path = map.get_path_matching(p.position, target, &creatures, &params, condition)
    if !path.is_empty() { p.list_walk_dir = path; }
```

## Cancellation Rules

| Trigger | Effect |
|---------|--------|
| Manual arrow key (`Command::Move` handle) | Clear `follow_target`, `go_to_position`, `list_walk_dir` |
| ESC / 0xBE | `clear_auto_walk` clears goto + queue (alongside existing attack/follow clear) |
| PZ entry (AI tick) | Clear `go_to_position` + `list_walk_dir` |
| Arrival at target (AI tick) | `p.position == target` → clear goto |
| Path blocked (empty queue, no repath) | Clear goto (single repath attempt per tick) |
| Different floor (AI tick) | Clear goto |

## AI Tick Extension

In `on_monster_ai_tick()`, filter players to those with EITHER `follow_target.is_some()` OR `go_to_position.is_some()`. After the existing follow-target handling block, add a new `else if let Some(target) = p.go_to_position` block with identical structure: PZ check, arrival check (`p.position == target`), queue-empty repath, pop dir, `do_move`. On `do_move` failure, clear goto + queue.

## Testing Strategy

| Layer | What to Test | Approach |
|-------|-------------|----------|
| Unit | `auto_walk_destination` | Apply known steps, verify target pos; overflow returns None |
| Unit | `do_go_to_position` validation | Reject wrong floor, unwalkable, out-of-view, already-there |
| Unit | AI tick auto-walk step | Player with queue + goto → tick pops dir → position changes |
| Unit | AI tick arrival clear | Walk to target → tick detects arrival → clears goto |
| Unit | AI tick PZ clear | Player in PZ + goto → tick clears goto |
| Unit | Manual move cancellation | Goto set → send `Command::Move` → goto cleared |
| Unit | ESC cancellation | Goto set → `clear_auto_walk` → goto cleared |
| Integration | Full cycle | Login, send 0x64 → player walks to destination |

## Open Questions

- None. All decisions map to existing patterns (follow-target AI tick, PZ checks, A* API).

## Migration / Rollout

No migration required. `go_to_position` is runtime-only (defaults to `None` on login). All PlayerState constructions explicitly set `go_to_position: None`.
