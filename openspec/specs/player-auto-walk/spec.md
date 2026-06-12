# Player Auto-Walk Specification

## Purpose

Allow players to click a map tile and have their character walk there via server-side A* pathfinding. The server receives `0x64` auto-walk, derives the destination from client-sent steps (ignores the path itself), computes its own A* path via `get_path_matching()`, and drains one direction per MonsterAiTick (100ms). Auto-walk cancels on manual movement, PZ entry, ESC (0xBE), or arrival.

## State Schema

- `go_to_position: Option<Position>` on `PlayerState` — tracks the goto target independently of `follow_target`
- `Command::GoToPosition { id: u32, position: Position }` — new command variant
- `WorldHandle::go_to_position(id, position)` — async method on the handle
- An `apply_auto_walk_steps(pos, &[AutoWalkStep]) -> Option<Position>` function derives destination from direction steps

## Requirements

### REQ-AW-01: Goto Destination State

The system MUST add `go_to_position: Option<Position>` to `PlayerState`. It MUST be cleared on arrival, manual move, PZ entry, ESC, or path-failure.

**Scenario: Goto set on click** — GIVEN a walkable tile is clicked, WHEN `0x64` is processed, THEN `go_to_position` MUST be set to the clicked tile.
**Scenario: Goto cleared on arrival** — GIVEN a player within 1 tile of destination, WHEN arrival is detected, THEN `go_to_position` MUST be cleared and walk stops.

### REQ-AW-02: Server-Side A* Path

The system MUST compute its own A* path via `get_path_matching()` from the player's current position. Client-sent direction steps MUST NOT be followed — only the destination is derived from them. Tiles with floor-change flags MUST be rejected as both path steps and destinations.

**Scenario: Path fills walk queue** — GIVEN a walkable same-floor destination, WHEN path is computed, THEN `list_walk_dir` MUST contain server-computed directions.
**Scenario: Unwalkable tile rejected** — GIVEN a destination is a wall or water, WHEN `is_walkable()` returns false, THEN `list_walk_dir` MUST NOT be modified and a `0xB4` status message MUST inform the player.
**Scenario: Floor-change destination rejected** — GIVEN the destination tile carries `FloorChange::DOWN`, WHEN A* runs, THEN the path MUST be empty AND auto-walk MUST stop per REQ-AW-09 with a status message.
**Scenario: Floor-change tile avoided mid-path** — GIVEN a floor-change tile lies between player and destination, WHEN A* evaluates the tile, THEN it MUST be rejected and A* MUST route around it.

### REQ-AW-03: Same-Floor Validation

The system MUST reject clicks on any floor other than the player's current z coordinate.

**Scenario: Cross-floor rejected** — GIVEN a player on z=7, WHEN the derived destination has z ≠ 7, THEN auto-walk MUST NOT start and a status message MUST be sent.

### REQ-AW-04: Tick Execution

The system MUST pop one direction from `list_walk_dir` per MonsterAiTick and call `do_move()`. When the queue empties before arrival, the system MUST recompute A* from the current position.

**Scenario: One step per tick** — GIVEN 3 queued directions, AFTER 1 tick, 2 MUST remain and the player MUST have moved.
**Scenario: Repath on empty queue** — GIVEN an empty queue but player not yet arrived, WHEN A* finds a new path, THEN `list_walk_dir` MUST be refilled.

### REQ-AW-05: Arrival Detection

Arrival is Chebyshev distance ≤ 1 from the destination. The system MUST send a `0xB4` MESSAGE_INFO_DESCR "You have arrived." and clear goto state.

**Scenario: Arrival message sent** — GIVEN a player within 1 tile of destination, WHEN the tick fires, THEN `go_to_position` MUST be None and the status message sent.

### REQ-AW-06: Manual Movement Cancellation

Any manual `do_move()` call (walk opcodes 0x65-0x6D) MUST clear `go_to_position`, `list_walk_dir`, AND `follow_target`.

**Scenario: Manual step cancels goto** — GIVEN a player auto-walking, WHEN 0x66 (walk east) is received, THEN goto and queue are cleared and the player steps east manually.

### REQ-AW-07: ESC Cancellation

0xBE (ESC) MUST clear `go_to_position`, `list_walk_dir`, and set attack target to 0.

**Scenario: ESC stops auto-walk** — GIVEN a player auto-walking, WHEN 0xBE is received, THEN `go_to_position` and `list_walk_dir` MUST be cleared.

### REQ-AW-08: PZ Protection

The system MUST NOT start auto-walk from or into a PZ tile. PZ entry mid-walk MUST immediately cancel auto-walk.

**Scenario: Entering PZ mid-walk** — GIVEN a player auto-walking outside PZ, WHEN the player steps onto a PZ tile, THEN goto and queue MUST be cleared.
**Scenario: Goto into PZ rejected** — GIVEN a player outside PZ, WHEN the destination is a protection zone, THEN auto-walk MUST NOT start and a status message MUST notify the player.

### REQ-AW-09: Path Blocked Handling

If A* returns empty (no route exists), the system MUST clear goto and notify the player. Three or more consecutive failed repaths MUST also terminate auto-walk.

**Scenario: No path stops auto-walk** — GIVEN an unreachable destination, WHEN A* returns empty, THEN `go_to_position` MUST be cleared and a status message sent.
**Scenario: Repath limit terminates** — GIVEN 3 consecutive repath failures, THEN `go_to_position` MUST be cleared and auto-walk terminated.

### REQ-AW-10: In-Viewport Validation

The destination MUST be within the player's current viewport (asymmetric 18×14). Out-of-viewport clicks MUST be rejected.

**Scenario: Out-of-range rejected** — GIVEN a tile outside the player's viewport, WHEN the `0x64` packet is processed, THEN auto-walk MUST NOT start.

## Test Scenarios

| ID | Test | Type | Coverage |
|----|------|------|----------|
| T01 | Player clicks walkable tile → walks there | Happy | REQ-AW-01,02,04,05 |
| T02 | Player clicks unwalkable tile → status message | Error | REQ-AW-02 |
| T03 | Player clicks cross-floor → rejected | Error | REQ-AW-03 |
| T04 | Player clicks PZ tile → rejected | Error | REQ-AW-08 |
| T05 | PZ entered mid-walk → stops | Error | REQ-AW-08 |
| T06 | Manual step cancels auto-walk | Cancellation | REQ-AW-06 |
| T07 | ESC cancels auto-walk | Cancellation | REQ-AW-07 |
| T08 | No path found → status + clear | Error | REQ-AW-09 |
| T09 | Path blocked mid-walk → repath → terminates | Error | REQ-AW-04,09 |
| T10 | Arrival at destination → message + stop | Happy | REQ-AW-05 |
| T11 | Same-tile click → no-op or immediate stop | Edge | REQ-AW-05 |
| T12 | Out-of-viewport click → rejected | Error | REQ-AW-10 |
