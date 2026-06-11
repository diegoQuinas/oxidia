## Exploration: Click-to-Move Pathfinding

### Current State

The server already has most of the infrastructure needed for click-to-move pathfinding:

**Pathfinding (A\*):**
- `crates/world/src/pathfinding.rs` — full A\* with `get_path_matching()`, supports cardinal+diagonal, creature penalty, `FindPathParams` (max search dist, full search flag), `FrozenPathingConditionCall`
- `StaticMap::get_path_matching()` wrapper at `crates/world/src/map.rs:620` resolves walkability via `is_walkable()`
- Used by: monster AI chasing, player follow-target (`do_follow_target`)

**PlayerState fields (already exist):**
- `list_walk_dir: VecDeque<Direction>` — queued walk directions populated by A\*
- `follow_target: Option<u32>` — creature being followed

**Player auto-walk loop (already exists):**
- `on_monster_ai_tick()` at `crates/world/src/game/mod.rs:974` pops from `list_walk_dir` and calls `do_move()` per tick (100ms intervals)
- BUT: currently gated to only process players where `p.follow_target.is_some()` — see line 1072-1075
- When `list_walk_dir` runs out, if a `follow_target` exists and not adjacent, recomputes A\* path

**Protocol layer:**
- Client sends `0x64` auto-walk packet with pre-computed direction steps — `parse_auto_walk()` at `crates/protocol/src/walk.rs:31` already parses it
- Server receives it at `crates/server/src/game_service.rs:467-473` but currently logs "not implemented" and ignores it

**Movement system:**
- `do_move()` at `crates/world/src/game/movement.rs:139` handles collision, stairs, vertical mechanics, spectator broadcast, known-set management. Works per step.
- `do_follow_target()` at `crates/world/src/game/mod.rs:893` shows the exact pattern: compute A\* path, fill `list_walk_dir`

**What's missing:**
1. No handler connects the `0x64` auto-walk packet to A\* path computation
2. No `Command::GoToPosition` variant in the command enum
3. No `do_go_to_position()` handler in `Game`
4. `on_monster_ai_tick` doesn't walk players who have `list_walk_dir` but no `follow_target`
5. No method to derive destination tile from the direction steps the client sent
6. No tests for click-to-move / auto-walk

### Affected Areas

- `crates/protocol/src/walk.rs` — Add `apply_auto_walk_steps(pos, &[AutoWalkStep]) -> Option<Position>` to compute destination from steps (or extend `parse_auto_walk` to return dest)
- `crates/world/src/game/mod.rs` — Add `Command::GoToPosition { id, position }` variant; add `do_go_to_position()` handler; extend `on_monster_ai_tick` walk logic to handle players without `follow_target` but with a goto goal
- `crates/world/src/game/mod.rs` (PlayerState) — Add `go_to_position: Option<Position>` field for goto target tracking
- `crates/server/src/game_service.rs` — Wire `0x64` auto-walk packet to the new command: parse steps → compute destination via `apply_auto_walk_steps` → call `world.go_to_position()`
- `crates/world/src/game/movement.rs` or `crates/world/src/game/mod.rs` — In `on_monster_ai_tick`, when player has `go_to_position` and `list_walk_dir` is empty, check if arrived (adjacent) and clear + notify; else recompute A\* path
- `crates/world/src/game/test_support.rs` — Add test maps and fixtures for auto-walk
- `crates/world/src/game/mod.rs` (tests) — Add auto-walk tests
- `crates/server/src/game_service.rs` (tests) — Add integration test for `0x64` packet handling

### Approaches

1. **Minimal: derive destination from client steps, fill list_walk_dir** — When `0x64` received, apply steps to current position to find destination, run A\* server-side, fill `list_walk_dir`. Modify AI tick to drain `list_walk_dir` for any player (not just follow_target). No new PlayerState fields needed.
   - Pros: Minimum new code, reuses all existing infrastructure, anti-cheat via server-side A\*
   - Cons: Client sends steps, server re-computes (wasted bandwidth/steps), no goto position tracking (can't recalc when player is blocked or pushed)
   - Effort: Low

2. **Full: separate go_to_position field + Command** — Add `go_to_position: Option<Position>` to `PlayerState`, new `Command::GoToPosition`, handler `do_go_to_position()`. AI tick iterates players with `go_to_position.is_some()` OR `follow_target.is_some()`. Recomputes path when blocked or queue empties. Clears goto on arrival. Player gets `0xB4` "you have arrived" message.
   - Pros: Full state tracking, can recompute on obstruction, separates concerns from follow target, standard TFS behavior, extensible for distance checking
   - Cons: Slightly more code, new PlayerState field
   - Effort: Medium

3. **Full + PZ + floor-change** — Everything in Approach 2 plus: check PZ before starting auto-walk (stop in PZ like follow target does), handle stairs in pathfinding (resolve_vertical-aware A\* condition)
   - Pros: Complete feature, production-ready
   - Cons: Stair-aware A\* is nontrivial (z-level pathfinding is complex), more code, bigger change
   - Effort: High

### Recommendation

**Approach 2 (Full: separate go_to_position)**. The existing follow-target code at `on_monster_ai_tick:1068-1189` already shows the exact pattern for re-computing paths when the queue empties and handling arrival (adjacency check). Approach 2 reuses this pattern cleanly without over-complicating the initial implementation. Approach 1 would leave players stuck when their path is obstructed (no re-path logic without follow_target). Approach 3 (stairs) should be a separate follow-up.

The implementation steps:
1. Add `apply_auto_walk_steps` to protocol to derive destination from direction steps
2. Add `go_to_position: Option<Position>` to `PlayerState`
3. Add `Command::GoToPosition` + `do_go_to_position()` handler
4. Modify `on_monster_ai_tick` to also process players with `go_to_position`
5. Wire `0x64` auto-walk packet in `game_service.rs`
6. Add tests (unit + integration)

### Risks

- **Same-floor only**: A\* doesn't handle z-level changes (stairs). Clicking on a different floor tile will either silently fail or produce wrong results. MUST validate same-floor and reject cross-floor clicks with a status message.
- **PZ bypass**: Without PZ check, players could auto-walk in protection zones (attack-enabled zones in future). Must check `self.map.is_protection_zone(pos)` before starting auto-walk and stop if entered PZ, matching the follow-target behavior at line 1106.
- **Client expectation**: The 10.98 OTClient expects the `0x64` auto-walk to be an immediate step-by-step execution. If the server responds differently (e.g., just starts walking), the client might desync. Need to test with actual OTClient.
- **Race with manual movement**: If the player presses a directional key while auto-walking, the manual move should cancel auto-walk (standard TFS behavior). Need to clear `go_to_position` / `list_walk_dir` on any manual `Command::Move`.

### Ready for Proposal

Yes. Approach 2 is well-understood, all infrastructure is in place, and the change is well-scoped.
