# Proposal: Pathfinding Should Avoid Holes

## Intent

The A* pathfinder treats hole tiles (`floor_change=DOWN`) as walkable, so auto-walking players fall through and monsters pathfind into holes with no fall resolution. Mirror TFS `queryAdd(FLAG_PATHFINDING)` which returns `NOTPOSSIBLE` for `FLOORCHANGE` tiles.

## Scope

### In Scope
- Reject `floor_change` tiles in A* walkability closure (`get_path_matching`)
- Reject `floor_change` tiles in `do_move_monster` validation
- Update existing test fixtures that pathfind through floor_change tiles
- New tests: pathfinding routes around holes, monsters avoid holes

### Out of Scope
- Changing `ChunkManager::is_walkable()` — manual single-step through a hole still works
- Monster floor-change resolution (monsters don't use stairs — defer to separate change)
- Pathfinding cost penalty for floor_change (outright block, per TFS)

## Capabilities

### New Capabilities
- `pathfinding`: A* walkability rules shared by player auto-walk and monster AI path planning

### Modified Capabilities
- `player-auto-walk`: REQ-AW-02 (destination validation) must clarify that floor_change tiles cannot be A* path steps or destinations

## Approach

1. In `ChunkManager::get_path_matching()` (map.rs:587–617): add `&& self.floor_change_at(x.into(), y.into(), z.into()).is_empty()` to the inner walkability closure
2. In `do_move_monster()` (movement.rs:461–463): add `&& self.chunks.floor_change_at(d.x.into(), d.y.into(), d.z.into()).is_empty()` to the walk filter
3. Fix test fixtures that expect pathfinding to route through floor_change tiles — replace with plain ground tiles so assertions remain meaningful

## Affected Areas

| Area | Impact | Description |
|------|--------|-------------|
| `crates/world/src/map.rs` | Modified | `get_path_matching` walkability closure |
| `crates/world/src/game/movement.rs` | Modified | `do_move_monster` walk filter |
| `crates/world/src/game/mod.rs` | None | `do_go_to_position` calls same closure |
| `crates/world/src/pathfinding.rs` | None | A* core unchanged |

## Risks

| Risk | Likelihood | Mitigation |
|------|------------|------------|
| Stairs/ladders also blocked for pathfinding | Certain | Matches TFS — correct behavior |
| Existing pathfinding tests fail on floor_change tiles | High | Update fixtures before submitting |

## Rollback Plan

Revert the floor_change check from the two locations. All tests return to green.

## Dependencies

None

## Success Criteria

- [ ] Player auto-walk rejects holes as destinations (A* returns empty path)
- [ ] Player auto-walk routes around floor_change tiles on intermediate steps
- [ ] Monsters reject floor_change tiles in `do_move_monster`
- [ ] Manual step onto a hole still works (resolve_vertical handles it)
- [ ] `cargo test` passes
