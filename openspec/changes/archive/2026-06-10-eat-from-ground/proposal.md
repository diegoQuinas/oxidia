# Proposal: Eat from Ground with Red Console Message

## Intent

Players can eat food from inventory and containers but NOT from the ground, and receive no visual feedback when eating. This completes the food system by adding ground consumption and the red "Glup"/"Chomp" console message TFS players expect.

## Scope

### In Scope
- Ground consumption: clicking ground food picks up and consumes in one action
- Red console message via `MSG_CONSOLE_RED` (13) + `0xB4` packet on eating
- Lua `do_send_text_message(id, type, text)` builtin for future script use
- `decrement_food(Ground)` removes item from dynamic overlay + spectator broadcast
- `food.lua` updated with per-food message strings

### Out of Scope
- `TALKTYPE_MONSTER_SAY` / `0xAA` approach — too complex for identical visual result
- Stackable food count-decrement (deferred until needed by actual items)
- Differentiated food cooldown per source (current global cooldown matches TFS)

## Capabilities

### New Capabilities
- `food-consumption`: Eating food items from any source (slot, container, ground) with the red console feedback and regeneration condition

### Modified Capabilities
- None — existing specs (`player-auto-walk`, `gm-speed-command`) unaffected

## Approach

Approach 1 from exploration: Lua `do_send_text_message` builtin + ground decrement.

1. Add `MSG_CONSOLE_RED = 13` constant and `push_console_red()` to `mod.rs`
2. Add `GameAction::TextMessage { player_id, message_type, text }` in `lua.rs`
3. Register `do_send_text_message(id, type, text)` Lua builtin
4. Handle `ContainerSource::Ground` in `decrement_food` via `take_from_ground` + spectator broadcast
5. Update `food.lua` to call `do_send_text_message` with per-food message

## Affected Areas

| Area | Impact | Description |
|------|--------|-------------|
| `crates/world/src/game/mod.rs` | Modified | Add MSG_CONSOLE_RED, push_console_red() |
| `crates/world/src/game/lua.rs` | Modified | Add GameAction::TextMessage, do_send_text_message builtin |
| `crates/world/src/game/containers.rs` | Modified | Handle ContainerSource::Ground in decrement_food |
| `config/lua/scripts/food.lua` | Modified | Call do_send_text_message per food type |

## Risks

| Risk | Likelihood | Mitigation |
|------|------------|------------|
| Ground removal races with other actions | Low | Action queue serializes per player |
| MSG_CONSOLE_RED not rendered by client | Low | Verify `0xB4` with type 13 in OTClient |

## Rollback Plan

Revert Rust changes (`mod.rs`, `lua.rs`, `containers.rs`) and restore `food.lua` from git. No schema migrations. Single checkout per file.

## Dependencies

- `take_from_ground` in `crates/world/src/game/items.rs`
- `materialize()` for dynamic overlay before ground removal

## Success Criteria

- [ ] Player eats food from ground in one click — food removed from tile, regeneration applied
- [ ] Red "Glup"/"Chomp" message appears in player console on eating
- [ ] Existing inventory/container eating still works and also shows the message
- [ ] All existing tests pass (`cargo test`)
