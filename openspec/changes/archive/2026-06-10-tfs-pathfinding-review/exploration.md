# Exploration: TFS Pathfinding System Review

## Executive Summary

The TFS pathfinding system is a synchronous A* implementation with octile heuristic, bounds of 512 nodes, and diagonal cost 25 vs cardinal cost 10 (matching TFS 1.4.2 constants exactly). Three distinct problems were identified: (1) a **last_pos cache drift bug** in `game_service.rs` that causes 0x64 GoTo target derivation to use stale positions, (2) **neighbor pruning differs from TFS reference** which produces different search paths, and (3) **no rapid-click debounce** causing pathfinding recompute storms. The first is high-severity and the likely root cause of the user's "wrong destination" and "unresponsive" complaints.

## Files Examined

| File | Role |
|------|------|
| `crates/world/src/pathfinding.rs` | Core A* implementation (104 lines) |
| `crates/world/src/map.rs` | Map integration (get_path_matching bridge) |
| `crates/world/src/game/mod.rs` | Game actor: auto-walk commands, tick processing, state management |
| `crates/world/src/game/movement.rs` | do_move, do_teleport, do_move_monster + exhaustive tests |
| `crates/protocol/src/walk.rs` | Auto-walk parse, walk_update packet builder, tests |
| `crates/server/src/game_service.rs` | Session reader loop, 0x64 handler, last_pos cache, opcode routing |
| `reference/tfs/src/map.cpp` | TFS AStarNodes, getPathMatching, cost constants |
| `reference/tfs/src/map.h` | TFS AStarNode, MAX_NODES, MAP_NORMALWALKCOST, MAP_DIAGONALWALKCOST |
| `reference/tfs/src/creature.h` | TFS FindPathParams, FrozenPathingConditionCall |
| `reference/tfs/src/creature.cpp` | TFS onWalk, getNextStep, startAutoWalk, condition evaluation |
| `reference/tfs/src/protocolgame.cpp` | TFS parseAutoWalk (0x64 handler) |
| `reference/tfs/src/game.cpp` | TFS playerAutoWalk |

## Current Architecture

### Data flow

```
Client click → 0x64 GoTo (direction steps)
  → game_service.rs reader_loop
    → walk::parse_auto_walk(&payload[1..])     // parse client directions
    → walk::auto_walk_destination(last_pos, steps)  // derive target from cache
    → world.goto_position(id, target).await     // fire-and-forget command
      → Game::do_go_to_position(id, target)     // sync in actor
        → map.get_path_matching()               // A* sync in actor
        → player.list_walk_dir = path           // fill queue
    → (next MonsterAiTick, bucket processes player)
      → list_walk_dir.pop_front()               // pop next direction
      → do_move(id, direction)                  // execute step
```

### Key types

- `FindPathParams` — `full_search`, `clear_sight`, `max_search_dist` (no `allowDiagonal`, always on implicitly)
- `AStarNodes` — fixed-capacity `Vec<AStarNode>` (max 512) + `HashMap<(u16,u16),usize>` for O(1) lookup
- `PlayerState` — `follow_target`, `go_to_position`, `list_walk_dir: VecDeque<Direction>`, `last_walk_ms`, `failed_repaths`
- `Command::GoToPosition` / `Command::Move` / `Command::ClearAutoWalk` — mpsc channel commands

### Tick model

Auto-walk for players is not handled by a dedicated scheduler task per player (as TFS does). Instead, the `MonsterAiTick` (100ms interval, 10 buckets) processes ALL players with `follow_target || go_to_position` in section 2 of `on_monster_ai_tick`. Step timing is gated by `last_walk_ms + step_time(speed)`.

## Algorithm Analysis

### A* with octile heuristic — matches TFS constants

```
CARDINAL_COST  = 10   (TFS: MAP_NORMALWALKCOST = 10)
DIAGONAL_COST  = 25   (TFS: MAP_DIAGONALWALKCOST = 25)
CREATURE_PENALTY = 30 (TFS: MAP_NORMALWALKCOST * 3 = 30)
MAX_NODES      = 512  (TFS: MAX_NODES = 512)
```

Heuristic: `min(dx,dy) * 25 + |dx-dy| * 10` — exactly equals the true minimum path cost, making it **admissible but tight** (no under-estimation slack). With `DIAGONAL_COST=25 > 2*CARDINAL_COST=20`, diagonal steps are deliberately penalized: the algorithm prefers two cardinal steps over one diagonal step unless obstacles force the diagonal. This is a design choice that matches TFS but produces cardinal-heavy paths that may feel "unnatural" to players expecting Euclidean movement.

### Neighbor pruning (5 of 8 neighbors per node)

When a node has a parent (i.e., the search entered from a specific direction), only 5 of 8 neighbors are explored. The Rust implementation prunes **backward-facing** neighbors (the 3 neighbors in the opposite hemisphere from the entry direction). The TFS reference prunes **asymmetrically** — for example when moving East, TFS prunes all northward neighbors (NORTH, NORTHEAST, NORTHWEST) while Rust prunes all westward neighbors (WEST, NORTHWEST, SOUTHWEST). This is a structural difference that produces different search trees.

| Entry Direction | TFS pruned | Rust pruned |
|----------------|------------|-------------|
| East | NORTH, NORTHEAST, NORTHWEST | WEST, NORTHWEST, SOUTHWEST |
| West | SOUTH, SOUTHEAST, SOUTHWEST | EAST, NORTHEAST, SOUTHEAST |
| North | EAST, SOUTHEAST, NORTHEAST | SOUTH, SOUTHWEST, SOUTHEAST |
| South | WEST, NORTHWEST, SOUTHWEST | NORTH, NORTHWEST, NORTHEAST |
| NE | WEST, SOUTHWEST, NORTHWEST | SOUTH, SOUTHEAST, SOUTHWEST |
| NW | EAST, SOUTHEAST, NORTHEAST | SOUTH, SOUTHWEST, SOUTHEAST |
| SE | NORTH, NORTHEAST, NORTHWEST | WEST, NORTHWEST, SOUTHWEST |
| SW | NORTH, NORTHEAST, EAST | EAST, NORTHEAST, SOUTHEAST |

### Path consumption

TFS pops from `listWalkDir` **back** (reverse order, path built backward from end to start). Rust pops from **front** (forward order, path built forward from start to end). Functionally equivalent but the direction order in TFS starts from the END node toward the START.

## Problem Analysis

### Problem 1: Character doesn't reach destination correctly

**Root cause**: `last_pos` cache drift (`game_service.rs:491-497`).

The `reader_loop` maintains a `last_pos: Option<(u16,u16,u8)>` cache initialized from the login snapshot position. Every time a move command is sent, `last_pos` is optimistically updated **before** the actor processes the move:

```rust
if let Some(pos) = last_pos {
    let (dx, dy) = direction.delta();
    let nx = i32::from(pos.0) + dx;
    let ny = i32::from(pos.1) + dy;
    if (0..=i32::from(u16::MAX)).contains(&nx) && (0..=i32::from(u16::MAX)).contains(&ny) {
        last_pos = Some((nx as u16, ny as u16, pos.2));
    }
}
world.move_player(id, direction).await;  // may be BLOCKED — but last_pos already changed!
```

When a move is blocked (wall, creature, obstacle), `last_pos` is **already updated** before the actor returns a `cancel_walk`. The cache is now desynced from the server's true position. Subsequent 0x64 GoTo clicks derive the target from this stale `last_pos`, producing a wrong destination.

**Scenario**: Player at (100,100) tries to walk east into a wall. `last_pos` becomes (101,100). Player then clicks to GoTo (105,100). The server computes path from (100,100) to (104,100) [because `last_pos` was (101,100), not (100,100)], arriving one tile short. Or worse, the path from the wrong origin may clip through walls.

### Problem 2: Unnecessary diagonal movements

**Root cause**: Neighbor pruning mismatch with TFS reference.

The Rust neighbor pruning (`neighbors_with_pruning`) explores a different set of neighbors than TFS for the same entry direction. This means the search tree is structurally different. For a given start/end pair, the Rust A* might find a path that includes a diagonal where TFS would have found a cardinal-only path, or vice versa.

Combined with the penalty-heavy diagonal cost (25 vs 10), paths that include diagonals are counter-intuitive: A* avoids them unless forced by obstacles, but the wrong neighbor set might force unnecessary diagonals by pruning cardinal alternatives that TFS would have kept.

**Secondary factor**: The GoTo destination derivation starts from `last_pos` (which may be wrong), so even if A* produces an optimal path for the given origin, it's the wrong origin.

### Problem 3: Rapid clicking makes it unresponsive

**Root causes** (two interacting):

**3a. No debounce on 0x64 GoTo** (`game_service.rs:476-484`):
Every client 0x64 packet fires `world.goto_position(id, target).await` through the mpsc channel. Each call triggers a synchronous A* search (up to 512 nodes) inside the actor's `handle()` method. While 512-node A* is fast in Rust (~microseconds), the actor is **blocked** during the search — no tick processing, no other commands. Rapid clicks queue commands in the mpsc channel (capacity 64), and each is processed sequentially with a full A* each time.

**3b. Full path reset on every GoTo** (`do_go_to_position`):
Each GoTo command clears `go_to_position`, sets a new target, runs A*, and fills `list_walk_dir`. This replaces any existing path. If the player clicks 10 times quickly:
1. The first click starts the player walking
2. Click 2+ immediately stop the current walk and restart with a new path
3. The player appears to "stutter" or stop responding because each click throws away progress

**3c. No alignment with TFS event model**:
TFS uses a per-player scheduled walk event (`eventWalk`) with a guard (`if eventWalk != 0 return;`) that prevents stacking walk events. Rapid clicks replace the path in `startAutoWalk` but the single event mechanism smooths execution. The Rust implementation has no equivalent guard — every 0x64 fires a new command that resets state and runs A*.

### Comparison with Expected TFS Behavior

| Aspect | TFS Reference | Rust Implementation | Match? |
|--------|---------------|---------------------|--------|
| Cost constants (10/25/512) | `MAP_NORMALWALKCOST=10, MAP_DIAGONALWALKCOST=25, MAX_NODES=512` | Same | ✅ Identical |
| Heuristic | Octile with diagonal/cardinal costs | Same | ✅ Identical |
| Neighbor pruning | Asymmetric (hemisphere) | Symmetric (backward-facing) | ❌ Different |
| Path direction order | Reverse (pop back) | Forward (pop front) | ✅ Equivalent |
| Walk event scheduling | Per-player scheduled event, `eventWalk` guard | Tick-based (MonsterAiTick), no per-player event | ❌ Different |
| GoTo target derivation | Direct from player Position object | From best-effort `last_pos` cache | ❌ Buggy |
| Rapid click handling | `startAutoWalk` replaces path, single event guard | `do_go_to_position` runs A* each time, no guard | ❌ Different |
| Creature penalty | `MAP_NORMALWALKCOST * 3 = 30` | `CREATURE_PENALTY = 30` | ✅ Identical |
| Diagonal allowed | Controlled by `allowDiagonal` field | Always on (no `allowDiagonal` field) | ⚠️ Always-on |
| Sight check | `clearSight` condition checked in path search | Unused (params exists but condition doesn't check) | ⚠️ Not implemented |

### Code Snippets (critical parts)

**Snippet 1 — last_pos cache drift bug** (`game_service.rs:488-499`):
```rust
// Update best-effort position cache for 0x64 derivation.
if let Some(pos) = last_pos {
    let (dx, dy) = direction.delta();
    let nx = i32::from(pos.0) + dx;
    let ny = i32::from(pos.1) + dy;
    if (0..=i32::from(u16::MAX)).contains(&nx) && (0..=i32::from(u16::MAX)).contains(&ny) {
        last_pos = Some((nx as u16, ny as u16, pos.2));
    }
}
world.move_player(id, direction).await; // <-- last_pos updated BEFORE validation
```

**Snippet 2 — Neighbor pruning mismatch** (`pathfinding.rs:20-31`):
```rust
// Rust: prunes backward-facing (intuitive but differs from TFS)
Some(Direction::East) => &[(0,-1),(1,-1),(1,0),(1,1),(0,1)],
// TFS equivalent (entry from WEST, moving EAST) would be:
// {{-1, 0}, {0, 1}, {1, 0}, {1, 1}, {-1, 1}}  // west, south, east, southeast, southwest
```

**Snippet 3 — No 0x64 debounce** (`game_service.rs:476-484`):
```rust
if opcode == 0x64 {
    if let Some(steps) = walk::parse_auto_walk(&payload[1..]) {
        if let Some(start) = last_pos {
            if let Some(target) = walk::auto_walk_destination(start, &steps) {
                world.goto_position(id, Position::new(target.0, target.1, target.2)).await;
                // Every click fires this — no debounce, no ignore-if-same-target
            }
        }
    }
    continue;
}
```

**Snippet 4 — step_time gate updated before do_move** (`game/mod.rs:1413-1437`):
```rust
let dir = {
    let p = match self.players.get_mut(&id) {
        Some(p) => p,
        None => continue,
    };
    p.list_walk_dir.pop_front()
};
if let Some(direction) = dir {
    if let Some(p) = self.players.get_mut(&id) {
        p.last_walk_ms = self.now_ms;  // <-- updated BEFORE do_move
    }
    // ... PZ check ...
    self.do_move(id, direction);  // <-- may be blocked, but timer already reset
}
```

## Risks & Recommendations

### Priority 1 (HIGH) — Fix `last_pos` cache drift
**Problem**: The position cache desyncs on blocked moves, corrupting all subsequent GoTo targets.
**Options**:
1. Remove the cache; query the actor for position before deriving GoTo target (authoritative, adds latency)
2. Only update cache on successful move confirmation (requires actor feedback for each move)
3. Use the world actor's position directly by not trusting client direction steps for GoTo (derive target from client click coordinates, not from cached position + direction offsets)
**Recommendation**: Option 3 is cleanest — the 0x64 GoTo should resolve the target from the client's intention (click coordinates in client map space), not from direction offsets applied to a cached position. This aligns with how OTClient/Tibia sends 0x64 — the client already computes the full path; we can derive the target from the final position. If that's not feasible, Option 2 (confirmation-based cache update) is the minimal fix.

### Priority 2 (MEDIUM) — Add rapid-click protection
**Problem**: Each 0x64 click fires a full A* recomputation, and identical-target clicks should be idempotent.
**Fix**: In `do_go_to_position`, skip pathfinding if target equals existing `go_to_position` AND `list_walk_dir` is non-empty. Only recompute path when the target changes or the queue drains.

### Priority 3 (MEDIUM) — Align neighbor pruning with TFS reference
**Problem**: Different neighbor sets produce different paths.
**Fix**: Change `neighbors_with_pruning()` to match TFS `dirNeighbors` table exactly. This ensures paths match TFS output for identical start/end positions.

### Priority 4 (LOW) — Consider removing `last_pos` cache entirely
**Problem**: Even with fix #1, the cache is a maintenance hazard.
**Fix**: Resolve GoTo target using the actor's authoritative position via a oneshot reply channel, or use the final coordinate from the client's path (which already has the target in screen-relative terms).

### Priority 5 (LOW) — Move `last_walk_ms` update after `do_move` success
**Problem**: Timer reset on blocked steps wastes a step-time cycle.
**Fix**: Only update `last_walk_ms` when `do_move` succeeds, not before.

## Ready for Proposal

**Yes** — sufficient information exists to write targeted fixes. The three highest-value fixes are:

1. **Fix `last_pos` cache drift** — stop using stale position for GoTo target derivation
2. **Skip redundant A* on identical GoTo target** — stop flooding the actor with recomputes
3. **Match TFS neighbor pruning** — align pruning tables for path-identical behavior

The orchestrator should propose these three as separate changes, with the first being the priority fix for the user's reported "wrong destination" issue.
