## Exploration: GM Speed Command

### Current State

**GM Commands**: Defined as a `GmVerb` enum in `crates/world/src/game/gm.rs`. Each variant declares `words()`, `usage()`, `description()`, and is listed in `GmVerb::ALL`. Dispatch is a match in `do_gm_command()` (line 157). Arguments are tokenized quote-aware by `tokenize_args()`. The GM gate checks `PlayerState.gamemaster` — non-GMs are silently dropped.

**Player Speed**: `PlayerState` (mod.rs line 221) has NO speed field. The function `creature_speed()` (mod.rs line 629) hardcodes `220` for all players. Only `MonsterState` carries a `speed` field. Speed is communicated to clients via:
- `creature::add_creature()` — `view.speed / 2` in creature thing bytes (spectator view)
- `enter_world::stats()` — `base_speed / 2` in 0xA0 stats packet (player's own stat window)
- `enter_world::self_info()` — 3 speed formula doubles (A=857.36, B=261.29, C=-4795.01), these are constants unaffected by player speed

**Existing pattern for optional-player commands**: `gm_setlooktype` (line 384) uses the pattern:
- No player name → modify self
- Player name + value → lookup by name, modify that player
- This is the exact pattern needed for `/speed`

**State-change broadcasting**: `do_change_outfit` broadcasts 0x8E to spectators. For speed, there's no dedicated broadcast packet — the speed field is embedded in the creature thing bytes. To update speed for spectators, the approach is remove+re-introduce (same pattern used in `gm_ghost` for visibility changes).

### Affected Areas

- `crates/world/src/game/gm.rs` — Add `GmVerb::Speed` variant, implement `gm_speed()` handler
- `crates/world/src/game/mod.rs` — Add `speed: u16` field to `PlayerState`, update `creature_speed()` to read it instead of hardcoding 220
- `crates/world/src/game/test_support.rs` — Add `speed: 220` to all `PlayerState` construction sites (add_player, login tests, etc.)
- `crates/world/src/game/session.rs` — Add `speed: 220` to all `PlayerState` construction sites in tests
- `crates/protocol/src/enter_world.rs` — Stats struct already has `base_speed: u16`, no change needed
- `crates/server/src/game_service.rs` — `build_enter_world_burst()` Stats uses `base_speed: 220` constant, no change needed (speed always starts at 220)

### Approaches

1. **Minimal: Add speed field + GM command, re-introduce for spectators** — Full approach matching existing patterns.
   - Add `speed: u16` (default 220) to `PlayerState`
   - Update `creature_speed()` to read from `PlayerState.speed`
   - Create `/speed [player] <value>` following `gm_setlooktype` pattern
   - Push 0xA0 stats to target player (self-notification of speed change)
   - For spectators: remove + re-introduce target creature at same position (like ghost toggle)
   - Pros: Complete, matches existing patterns, both self and spectators see the change
   - Cons: Remove+add for spectators may cause brief client flicker (acceptable for GM tool)
   - Effort: Medium

2. **Minimal: Add speed field + GM command, stats-only notification** — Only push 0xA0 to the target; skip spectator re-introduce.
   - Same as above but skip the remove+re-introduce for spectators
   - Pros: Simpler, less code
   - Cons: Spectators don't see the speed change until the creature walks out/in view, inconsistent with other commands
   - Effort: Low

3. **Add dedicated speed change protocol packet** — Add a new protocol encoder for a speed-change broadcast packet and use it instead of remove+add.
   - Pros: Cleaner client-side behavior, no flicker
   - Cons: Requires protocol research (TFS 10.98 wire format), new encoder+parser, more complex
   - Effort: High

### Recommendation

**Approach 1** (Full approach). It follows the exact pattern set by `gm_setlooktype` for argument parsing and `gm_ghost` for spectator notification. It's the minimal correct implementation — the remove+add for spectators is the established pattern in this codebase and is acceptable for an admin command. No protocol research needed.

### Risks

- **PlayerState construction**: Every test file that constructs `PlayerState` inline (test_support.rs, session.rs tests) must add `speed: 220` — this is mechanical but easy to miss. The compiler will flag missing fields.
- **Stats packet staleness**: The `build_enter_world_burst()` in game_service.rs hardcodes `base_speed: 220` in the Stats struct. This is only used at login, so it's fine — the real speed will be read from PlayerState after login. But if speed changes before logout, the saved speed must be persisted → `SaveRecord` and `PlayerSave` need a `speed` field for future persistence (out of scope for this command, which sets runtime speed only).
- **Speed formula**: The client applies the speed formula (A/B/C doubles from 0x17 self-info) to `base_speed`. The formula is: `client_speed = floor(A * log2(B * base_speed + C) / 2)`. Values below ~10 or above ~2500 may produce NaN or crash — input validation needed.

### Ready for Proposal

Yes — the approach is clear, follows established patterns, and the scope is well-defined. The orchestrator should proceed with `sdd-propose`.
