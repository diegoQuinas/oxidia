# Delta for Player Auto-Walk

## MODIFIED Requirements

### Requirement: REQ-AW-02: Server-Side A* Path

The system MUST compute its own A* path via `get_path_matching()` from the player's current position. Client-sent direction steps MUST NOT be followed — only the destination is derived from them. Tiles with floor-change flags MUST be rejected as both path steps and destinations.
(Previously: no floor-change rejection in pathfinding walkability closure)

**Scenario: Path fills walk queue** — GIVEN a walkable same-floor destination, WHEN path is computed, THEN `list_walk_dir` MUST contain server-computed directions.

**Scenario: Unwalkable tile rejected** — GIVEN a destination is a wall or water, WHEN `is_walkable()` returns false, THEN `list_walk_dir` MUST NOT be modified and a `0xB4` status message MUST inform the player.

**Scenario: Floor-change destination rejected** — GIVEN the destination tile carries `FloorChange::DOWN`, WHEN A* runs, THEN the path MUST be empty AND auto-walk MUST stop per REQ-AW-09 with a status message.

**Scenario: Floor-change tile avoided mid-path** — GIVEN a floor-change tile lies between player and destination, WHEN A* evaluates the tile, THEN it MUST be rejected and A* MUST route around it.
