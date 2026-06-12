## Exploration: Pathfinding Should Avoid Holes (so creatures don't fall through)

### Current State

The A* pathfinder (crates/world/src/pathfinding.rs) treats tiles as walkable based on a closure `is_walkable(nx, ny)` passed by `ChunkManager::get_path_matching()`. That closure currently calls `ChunkManager::is_walkable(pos)` which only checks:

1. The tile has a ground item (tile exists in the chunk)
2. The tile does NOT have a block-solid item (`FLAG_BLOCK_SOLID`)

Hole tiles (e.g. items 383, 432-433, 469, 470, etc. in TFS items.xml) are ground tiles with `floorchange="down"` — they have ground and are NOT block-solid. So `is_walkable()` returns `true`, and the pathfinder happily routes creatures across them.

When a player auto-walks (GoTo/follow) through a hole:
- `do_move()` calls `resolve_vertical()` → `resolve_floor_change()` which detects `FloorChange::DOWN`
- The landing position resolves one floor below
- The player unexpectedly falls through the hole to an unintended location

Monsters don't call `resolve_vertical` at all (comment: "No floor-change resolution — monsters don't use stairs yet"), so a monster pathing through a hole would just step onto it and stay there — they can't fall through, but they're now standing on a tile they shouldn't be.

### How TFS Handles This

In TFS `tile.cpp:488-489`:
```cpp
if (hasBitSet(FLAG_PATHFINDING, flags) && hasFlag(TILESTATE_FLOORCHANGE | TILESTATE_TELEPORT)) {
    return RETURNVALUE_NOTPOSSIBLE;
}
```

The pathfinding check (`map.cpp:637-638`) calls `tile->queryAdd(0, creature, 1, FLAG_PATHFINDING | FLAG_IGNOREFIELDDAMAGE)`. When the tile has `TILESTATE_FLOORCHANGE` (or `TILESTATE_TELEPORT`), `queryAdd` returns `NOTPOSSIBLE`, making the tile blocked for pathfinding.

Additionally for monsters (tile.cpp:496-498), floor change tiles are always blocked, not just during pathfinding.

The Rust code has no equivalent check — the tile's `floor_change` flags are never consulted during walkability evaluation for pathfinding.

### Affected Areas

- `crates/world/src/map.rs:574-626` — `ChunkManager::get_path_matching()`: the `is_walkable` closure needs to also check for `floor_change` flags
- `crates/world/src/game/movement.rs:452-510` — `do_move_monster()`: monster movement validation needs to reject floor_change tiles (monsters can never step onto them)
- `crates/world/src/game/mod.rs:1074-1500` — All pathfinding call sites (player follow, player GoTo, monster AI path refresh)
- `crates/world/src/pathfinding.rs` — The A* algorithm itself is fine; the gap is in what the calling code defines as "walkable"

### Approaches

1. **Modify `is_walkable` closure in `get_path_matching`** (recommended) — Add a `!fc.is_empty()` check after `is_walkable()` returns true. Tiles with any floor_change flag are blocked for pathfinding. This mirrors TFS `queryAdd(FLAG_PATHFINDING)`. Player manual steps still work (resolve_vertical handles them). Monsters need a separate fix in `do_move_monster` since they also walk manually.
   - Pros: Minimal change, matches TFS exactly, all pathfinding routes avoid holes
   - Cons: Monsters still need separate `do_move_monster` fix
   - Effort: Low

2. **Change `ChunkManager::is_walkable()` itself** — Add floor_change check to the core walkability function. This applies to EVERYTHING.
   - Pros: Single change point, protects everything automatically
   - Cons: Breaks manual player movement onto holes (which should work for intentional falls)
   - Effort: Low (but wrong approach)

3. **Penalty-based avoidance** — Instead of fully blocking, add a high path cost penalty for hole tiles (like CREATURE_PENALTY but higher). The pathfinder prefers routes around holes but can still route through them if there's no alternative.
   - Pros: More graceful degradation for forced-choice scenarios
   - Cons: Over-engineered; TFS doesn't do this; potential for unexpected path behavior
   - Effort: Medium

### Recommendation

**Approach 1.** Modify the `is_walkable` closure in `ChunkManager::get_path_matching()` to reject tiles with any `floor_change` flags. Additionally, modify `do_move_monster()` to reject floor_change tiles so monsters never step onto them (even without pathfinding).

This closely mirrors TFS behavior:
- `FLAG_PATHFINDING` → block FLOORCHANGE + TELEPORT tiles
- Monsters never step onto FLOORCHANGE tiles

### Risks

- If the ONLY path to a destination goes through a hole tile, the pathfinder returns "no path found" and the GoTo fails with "There is no way." This matches TFS behavior. The player must manually step onto the hole if they want to fall through.
- All staircase tiles are also blocked for pathfinding (they also have `floor_change` flags). This is correct — players must manually step onto/off stairs.
- Existing tests that assume holes are walkable for pathfinding will need to be updated.

### Ready for Proposal

Yes. The gap is well-understood, the TFS reference is clear, and the fix is contained to two changes:
1. Add floor_change check to the pathfinding `is_walkable` closure in `map.rs`
2. Block monster manual movement on floor_change tiles in `movement.rs`
