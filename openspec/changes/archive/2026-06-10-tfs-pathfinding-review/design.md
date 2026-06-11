# Design: TFS Pathfinding Review

## Technical Approach

Three independent fixes applied sequentially, each with its own test. Fix 1 is the highest priority — it fixes players arriving at wrong tiles after a blocked move.

## Architecture Decisions

### Fix 1: `last_pos` Cache Drift

| Option | Tradeoff | Decision |
|--------|----------|----------|
| Confirm cache after `move_player` response | Requires adding feedback channel for move results | ❌ Rejected — too invasive |
| Query authoritative position from actor | Adds latency (oneshot round-trip) | ❌ Rejected — breaks responsiveness |
| **Send raw 0x64 steps to actor** | No extra latency; actor derives target from `p.position` | ✅ **Chosen** |

**Rationale**: The `last_pos` cache in the reader loop is fundamentally unreliable because it updates optimistically before `world.move_player()` — a blocked move desyncs it permanently. The cleanest fix eliminates the cache for the 0x64 path entirely. Instead of deriving the target in the reader loop (where we lack authoritative state), pass the parsed `AutoWalkStep` vector to the actor via a new `Command::GoToSteps`. The actor's `do_go_to_steps` calls `walk::auto_walk_destination(p.position, &steps)` using the player's actual position, producing the correct target every time.

**Changes**:
- `Command::GoToSteps { id: u32, steps: Vec<protocol::walk::AutoWalkStep> }` replaces `Command::GoToPosition { target: Position }`
- `do_go_to_steps` in `game/mod.rs` derives target from `p.position + steps`, then runs same validation + A* as current `do_go_to_position`
- Reader loop sends steps directly instead of computing target from `last_pos`
- `last_pos` still updated for manual walk steps (harmless — only used for 0x64 derivation which no longer reads it)

### Fix 2: Redundant A* on Identical Target

| Option | Tradeoff | Decision |
|--------|----------|----------|
| Debounce timer in reader loop | Adds state, wrong timeout hurts UX | ❌ Rejected |
| **Idempotency check in `do_go_to_steps`** | Simple guard, zero overhead | ✅ **Chosen** |

**Rationale**: Rapid clicks on the same tile fire multiple 0x64 packets. All produce the same path. Guard: compare the DERIVED target against `go_to_position`. If equal AND `list_walk_dir` is non-empty, skip A* entirely. This is a cheap `Option<Position>::eq` check before the expensive pathfinding.

**Change**: At the start of `do_go_to_steps` (or `do_go_to_position`), after deriving the target:
```rust
if p.go_to_position == Some(target) && !p.list_walk_dir.is_empty() {
    return; // same target, path already computed
}
```

### Fix 3: Neighbor Pruning Mismatch with TFS

| Option | Tradeoff | Decision |
|--------|----------|----------|
| Keep current backward pruning | Produces different search trees = different paths | ❌ Rejected |
| **Replace tables with TFS `dirNeighbors`** | Exact path match with TFS 1.4.2 | ✅ **Chosen** |

**Rationale**: The current pruning removes 3 cells in the opposite hemisphere of travel. TFS uses an asymmetric 5-cell pruning table (`dirNeighbors[8][5][2]`). The Rust direction `ed` is FROM parent TO child, while TFS direction is TO parent FROM child — they are opposites. We must map Rust directions to the TFS table for the opposite direction.

**Mapping** (Rust direction → TFS parent direction → TFS table index):

| Rust `ed` | TFS table | Neighbor offsets |
|-----------|-----------|-----------------|
| `North` | `DIRECTION_SOUTH` [3] | (0,1),(1,0),(0,-1),(1,-1),(1,1) |
| `East` | `DIRECTION_WEST` [0] | (-1,0),(0,1),(1,0),(1,1),(-1,1) |
| `South` | `DIRECTION_NORTH` [2] | (-1,0),(1,0),(0,-1),(-1,-1),(1,-1) |
| `West` | `DIRECTION_EAST` [1] | (-1,0),(0,1),(0,-1),(-1,-1),(-1,1) |
| `NorthEast` | `DIRECTION_SOUTHWEST` [6] | (0,1),(1,0),(1,-1),(1,1),(-1,1) |
| `SouthEast` | `DIRECTION_NORTHWEST` [4] | (1,0),(0,-1),(-1,-1),(1,-1),(1,1) |
| `SouthWest` | `DIRECTION_NORTHEAST` [5] | (-1,0),(0,-1),(-1,-1),(1,-1),(-1,1) |
| `NorthWest` | `DIRECTION_SOUTHEAST` [7] | (-1,0),(0,1),(-1,-1),(1,1),(-1,1) |

**Change**: Replace the body of `neighbors_with_pruning()` with the mapped TFS tables above.

## Data Flow

```
Fix 1 (before):
  Client 0x64 → reader_loop → last_pos[DIRTY] + steps → target → world.goto_position(target)
                                                     ↑
  Manual walk ─────────────────────────→ last_pos += delta (even if blocked!)

Fix 1 (after):
  Client 0x64 → reader_loop → steps → world.goto_steps(steps)
                                          ↓
                                   do_go_to_steps:
                                     target = auto_walk_destination(p.position, steps)
                                     A* → list_walk_dir
```

## File Changes

| File | Action | Description |
|------|--------|-------------|
| `crates/server/src/game_service.rs` | Modify | Send raw `AutoWalkStep` to actor instead of deriving target from `last_pos` |
| `crates/world/src/game/mod.rs` | Modify | Add `Command::GoToSteps`, rename/refactor `do_go_to_position` → `do_go_to_steps`, add idempotency guard for Fix 2 |
| `crates/world/src/pathfinding.rs` | Modify | Replace `neighbors_with_pruning()` tables with TFS-mapped values for Fix 3 |

## Testing Strategy

| Layer | What to Test | Approach |
|-------|-------------|----------|
| Unit | Fix 1: target derived from authoritative position | New test: set `last_pos` incorrectly, send 0x64 — target must use actor position, not cache |
| Unit | Fix 2: same-target skip | New test: call `do_go_to_steps` twice with same target; verify A* runs only once (count node allocations) |
| Unit | Fix 3: TFS-aligned pruning | New test: assert `neighbors_with_pruning()` returns exact TFS offsets per direction |
| Regression | All existing pathfinding + movement tests | `cargo test` must pass unchanged |

## Open Questions

- None — all three fixes are well-understood from the exploration phase.
