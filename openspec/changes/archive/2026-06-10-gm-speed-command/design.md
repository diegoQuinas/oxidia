# Design: GM Speed Command

## Technical Approach

Add `speed: u16` to `PlayerState`, wire `creature_speed()` to read it, implement `/speed [player] <value>` following `gm_setlooktype` for optional-target parsing and `gm_ghost` for spectator re-introduction. All runtime stats pushes read `p.speed` instead of hardcoded 220.

## Architecture Decisions

| Option | Tradeoff | Decision |
|--------|----------|----------|
| `u16` vs `u32` for speed | Wire uses u16; TFS base_speed is u16 | `u16` |
| Field on PlayerState vs separate map | inline matches ghost/noclip; simpler to maintain | `PlayerState.speed` |
| Validate range in handler vs type | handler gives precise error messages | handler validates 10..=2500 |
| Self-target by omitting player vs keyword | matches setlooktype ergonomics | omit player → target self |
| Spectator update via remove+re-intro vs dedicated packet | same proven pattern as ghost; no new protocol code | remove+re-introduce |

## Data Flow

```
/speed 150                    → self-target
/speed "Alice" 150            → other-target

gm_speed():
  parse → (optional name, value)
  validate 10..=2500 else push error
  set p.speed = value
  push 0xA0 stats to target  ← base_speed = value
  for each spectator:
    walk::remove_creature_by_id
    known.remove(&id)
    introduce → add_tile_creature  ← CreatureView.speed = value
  push_status_message to caller

Stats push sites (regen tick, combat):
  base_speed: p.speed         ← reads live value, was hardcoded 220
```

## File Changes

| File | Action | Description |
|------|--------|-------------|
| `crates/world/src/game/mod.rs` | Modify | Add `speed: u16` to PlayerState struct; update `creature_speed()` to read it; update `on_regen_tick` stats to use `p.speed` |
| `crates/world/src/game/gm.rs` | Modify | Add `GmVerb::Speed` variant, `["speed", "spd"]`, usage/description, dispatch, `gm_speed()` handler |
| `crates/world/src/game/test_support.rs` | Modify | Add `speed: 220` to `add_player()` |
| `crates/world/src/game/session.rs` | Modify | Add `speed: 220` to `login()` and 6 inline test PlayerState constructions |
| `crates/world/src/game/combat.rs` | Modify | Use `p.speed` instead of `220` in stats push |
| `crates/world/src/game/look.rs` | Modify | Add `speed: 220` to inline test PlayerState |
| `crates/server/src/game_service.rs` | No change | Login burst hardcoded 220 is correct (speed not persisted) |

## Interfaces / Contracts

```rust
// --- PlayerState ---
struct PlayerState {
    // ... existing fields ...
    noclip: bool,
    speed: u16,               // NEW: runtime speed override, default 220
    inventory: [Option<InvItem>; 10],
    // ...
}

// --- GmVerb ---
enum GmVerb {
    // ...
    Speed,                    // NEW variant
}

impl GmVerb {
    fn words(self) -> &'static [&'static str] {
        match self {
            // ...
            Self::Speed => &["speed", "spd"],
        }
    }
    fn usage(self) -> &'static str {
        // ...
        Self::Speed => "/speed <value> | /speed \"player\" <value>",
    }
    fn description(self) -> &'static str {
        // ...
        Self::Speed => "Set movement speed on yourself or another player (10-2500).",
    }
}
```

## Testing Strategy

| Layer | What to Test | Approach |
|-------|-------------|----------|
| Unit | GmVerb::Speed variant registration | extend `gmverb_registry_is_complete_and_resolvable` |
| Unit | gm_speed without target (self) | create GM, run `/speed 500`, assert p.speed == 500, assert 0xA0 pushed |
| Unit | gm_speed with named target | create GM + target, run `/speed "target" 500`, assert target.speed |
| Unit | Range validation | test 9 and 2501 produce error messages, 10 and 2500 succeed |
| Unit | Spectator sees new speed via re-intro | create GM + target + spectator, change speed, assert spectator receives remove+add |
| Unit | creature_speed reads from PlayerState | add player, set speed, call creature_speed, assert match |
| Unit | Stats pushes reflect live speed | set speed, trigger regen tick or combat, assert 0xA0 has base_speed = p.speed |

## Migration / Rollout

No migration required. `speed` is runtime-only (defaults to 220 on login), not persisted. All PlayerState constructions explicitly set `speed: 220`.

## Open Questions

- None.
