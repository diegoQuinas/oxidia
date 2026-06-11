# Proposal: GM Speed Command

## Intent

Add a GM command to modify a player's movement speed at runtime. GMs need to adjust player speed for testing, events, or moderation without restarting the server. Follows established patterns for GM commands and state broadcasting.

## Scope

### In Scope
- `speed: u16` field on `PlayerState`, default `220`
- `creature_speed()` reads `PlayerState.speed` instead of hardcoding `220`
- `/speed [player] <value>` command — no player targets self
- 0xA0 stats packet push to target player on change
- Remove+re-introduce target creature for spectator speed update
- Input validation: range `10..=2500`

### Out of Scope
- Persistence (speed resets to 220 on logout)
- Dedicated speed-change protocol packet
- Lua scripting API for speed changes
- Speed cap config or per-map speed rules

## Capabilities

### New Capabilities
- `gm-speed-command`: GM `/speed` command with optional-target pattern, player speed attribute, input validation, self-notification via 0xA0, and spectator broadcast via remove/re-introduce

### Modified Capabilities
- None

## Approach

1. Add `speed: u16 = 220` to `PlayerState` struct
2. Update `creature_speed()` to read from `PlayerState.speed` (fallback to `220` for monsters unchanged)
3. Add `GmVerb::Speed` variant with words `["speed", "spd"]`, usage `/speed [player] <value>`
4. `gm_speed()` handler: parse optional player name + u16 value, validate range, set speed, push 0xA0 stats to target, remove+re-introduce creature for spectators
5. Update all `PlayerState` construction sites in tests

## Affected Areas

| Area | Impact | Description |
|------|--------|-------------|
| `crates/world/src/game/mod.rs` | Modified | Add `speed` field to `PlayerState`, update `creature_speed()` |
| `crates/world/src/game/gm.rs` | Modified | Add `GmVerb::Speed`, implement `gm_speed()` handler |
| `crates/world/src/game/test_support.rs` | Modified | Add `speed: 220` to test helpers |
| `crates/world/src/game/session.rs` | Modified | Add `speed: 220` to session tests |
| `crates/world/src/game/movement.rs` | Modified | Add `speed: 220` to movement tests |

## Risks

| Risk | Likelihood | Mitigation |
|------|------------|------------|
| Extreme speed values crash client formula | Low | Validate input range `10..=2500` |
| Missed PlayerState construction sites | Low | Compiler-enforced (struct field) |
| Remove+re-introduce flickers for spectators | Low | Acceptable for admin command; matches `gm_ghost` pattern |

## Rollback Plan

Revert the single commit. All changes are additive/runtime — no data migrations. Remove `speed` field from `PlayerState`, restore `creature_speed()` hardcoded return, delete `GmVerb::Speed` and `gm_speed()`.

## Dependencies

None.

## Success Criteria

- [ ] `/speed 500` sets GM's own speed to 500 (visible in stats window)
- [ ] `/speed PlayerName 500` sets target player's speed to 500
- [ ] Spectators see the speed change (remove+re-introduce)
- [ ] Values outside `10..=2500` are rejected with error message
- [ ] Invalid player name returns error
- [ ] `cargo test` passes with all new and existing tests
