# Design: Pathfinding Should Avoid Holes

## Technical Approach

Block floor-change tiles in the A* walkability closure so the pathfinder never
routes through holes (or any vertical-transition tile). Two targeted changes:

1. `ChunkManager::get_path_matching()` — add `floor_change_at` check to the
   `is_walkable` closure so all pathfinding callers (player GoTo, follow,
   monster AI) reject floor-change tiles as intermediate steps.
2. `do_move_monster()` — add `floor_change_at` check to the step validation so
   monsters never step onto a hole tile (they have no vertical resolution).

Mirrors TFS `Tile::queryAdd(FLAG_PATHFINDING)` which returns `NOTPOSSIBLE` for
`TILESTATE_FLOORCHANGE | TILESTATE_TELEPORT` tiles (`tile.cpp:488-489`).

## Architecture Decisions

### Decision: Block ALL floor-change tiles, not just DOWN

| Option | Tradeoff | Decision |
|--------|----------|----------|
| Block only DOWN (holes) | UP-stairs still walkable but TFS blocks them in pathfinding too | Rejected — diverges from TFS behavior |
| Block ALL floor_change (`.is_empty()`) | Matches TFS `FLAG_PATHFINDING`, simpler impl | **Chosen** |

The up-stairs case is irrelevant for pathfinding because a player/monster
already on a higher floor would never need to pathfind onto the small stair
edge tile — they just walk. The TFS behavior is the safe reference.

### Decision: Do NOT modify `ChunkManager::is_walkable()` itself

| Option | Tradeoff | Decision |
|--------|----------|----------|
| Change `is_walkable()` | Breaks manual step-to-hole (M6.1 resolve_vertical) | **Rejected** |
| Change only the pathfinding closure | Manual step still works, only A* rejects floor-change | **Chosen** |

Per scope: manual single-step onto a hole still works (`resolve_vertical`
handles it). The change is only in the A* pathfinding code path.

### Decision: Do NOT modify `StaticMap::get_path_matching()`

`StaticMap::get_path_matching()` (map.rs:1531) is dead code — zero callers in
production or tests. Out of scope.

## Data Flow

```
Player Click (GoTo) / Follow Target / Monster AI
    │
    ▼
ChunkManager::get_path_matching()  ← also used by monster AI
    │
    ├── is_walkable?  now: && floor_change_at(x,y,z).is_empty()
    │                      └── blocks DOWN, NORTH, SOUTH, EAST, WEST, ALT flags
    │
    └── pathfinding::get_path_matching()
            │
            └── A* expands neighbors → skip if !is_walkable(nx, ny)

Monster AI tick → do_move_monster()
    │
    └── step validation: is_walkable(d) && !tile_occupied(d, id)
                          && floor_change_at(d).is_empty()   ← NEW
```

## File Changes

| File | Action | Description |
|------|--------|-------------|
| `crates/world/src/map.rs` | Modify | Add `floor_change_at` check in `ChunkManager::get_path_matching()` closure (line ~616) |
| `crates/world/src/game/movement.rs` | Modify | Add `floor_change_at` check in `do_move_monster()` step filter (line ~463) |
| `openspec/changes/pathfinding-avoid-holes/specs/` | Create | Add spec scenarios covering hole avoidance |

## Interfaces / Contracts

No new interfaces. Uses existing `FloorChange::is_empty()` API:

```rust
// FloorChange API (formats::items_xml)
impl FloorChange {
    pub fn is_empty(self) -> bool;  // true if NONE (no flags set)
    pub fn contains(self, other: Self) -> bool;
}
```

## Testing Strategy

| Layer | What | Approach |
|-------|------|----------|
| Unit (pathfinding) | `get_path_matching` rejects floor-change tiles | New `get_path_matching` test with mock floor-change tiles on intermediate step |
| Unit (movement) | `do_move_monster` rejects hole step | New test: monster on tile adjacent to hole, step toward hole is blocked |
| Integration | Player GoTo routes around hole | Map with a hole tile between start and target — path must go around |
| Integration | Player follow routes around hole | Similar to GoTo but with follow target |

Existing test fixtures that pathfind through `FloorChange::DOWN` tiles will
need their fixture maps updated (remove the floor-change item from the tile
or add alternate routing).

## Migration / Rollout

No migration required. Feature flag not needed — the change is purely additive
to the pathfinding closure (stricter rejection). Existing paths through holes
are bugs, not expected behavior.

## Open Questions

- [ ] Are there any existing test fixtures that pathfind through stair/hole tiles
      and will break? Need to audit `stair_map()`, `walk_map()`, and test maps.
