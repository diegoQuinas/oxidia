# Pathfinding Specification

## Purpose

Shared A* walkability rules used by player auto-walk and monster AI to reject floor-change tiles (holes, pits) as valid path steps or destinations, mirroring TFS `queryAdd(FLAG_PATHFINDING)`.

## Requirements

### REQ-PF-01: Floor-Change Tile Rejection in A*

The A* walkability closure in `ChunkManager::get_path_matching` MUST reject tiles carrying non-empty `FloorChange` flags.

| # | Scenario | GIVEN | WHEN | THEN |
|---|----------|-------|------|------|
| 1 | Hole tile skipped | A* search hits a `FloorChange::DOWN` tile | Walkability closure evaluates | Tile rejected, search continues around |
| 2 | Stair tile also blocked | A* search hits a directional `FloorChange` tile | Walkability closure evaluates | Tile rejected (per TFS `FLAG_PATHFINDING`) |
| 3 | Normal tile unaffected | A* hits a tile with no floor change | Walkability closure evaluates | Accepted if otherwise walkable |

### REQ-PF-02: Floor-Change Rejection in Monster Movement

`do_move_monster` MUST reject destination tiles with non-empty `FloorChange` flags before committing movement.

| # | Scenario | GIVEN | WHEN | THEN |
|---|----------|-------|------|------|
| 1 | Monster avoids hole | Monster pathfinds toward a player, next step is `FloorChange::DOWN` | Step evaluates | Monster stays in place, step skipped |
| 2 | Monster walks normally | Monster has a clear walkable tile ahead | Step evaluates | Movement proceeds |

### REQ-PF-03: Manual Step Unaffected

`is_walkable()` MUST NOT be modified. Floor-change rejection applies only in A* pathfinding and monster movement — manual steps via walk opcodes still trigger `resolve_floor_change`.

| # | Scenario | GIVEN | WHEN | THEN |
|---|----------|-------|------|------|
| 1 | Step into hole still works | Player walks toward a hole via walk opcode | `do_move()` calls `is_walkable()` | Step succeeds, `resolve_floor_change` handles the fall |
