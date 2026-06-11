# Design: Eat from Ground with Red Console Message

## Technical Approach

Extend the existing food pipeline (Lua → `GameAction::Feed` → `decrement_food`) with two additions:
1. A `GameAction::TextMessage` variant that Lua scripts push to queue red console messages (`0xB4` type 13) on successful eat
2. Handle `ContainerSource::Ground(pos)` in `decrement_food` by reusing `take_from_ground` + `broadcast_source` for COW removal and spectator broadcast

Ground decrement mirrors the existing `do_move_thing` pattern: materialize the dynamic overlay, find the item by `server_id`, call `take_from_ground`, then broadcast `0x6C`/`0x6B` via `broadcast_source`.

## Architecture Decisions

| Option | Tradeoffs | Decision |
|--------|-----------|----------|
| New `push_console_red` vs. generic `push_msg(type)` | Generic saves one method but obscures call sites; explicit per-type is the codebase pattern (`push_console_blue`, `push_info_descr`) | **New `push_console_red`** following existing pattern |
| `GameAction::TextMessage { text: String }` vs. static string ID | String gives Lua full flexibility for per-food messages; static IDs would need a message registry | **String variant** — matches TFS flexibility, `GameAction` loses `Copy` (becomes `Clone`) |
| Ground item search by `server_id` vs. stored stack index | `server_id` is always available from `decrement_food` arg; stack index would need to be threaded through the Lua action | **Search by `server_id`** — simple linear scan, items per tile are tiny (typically <10) |

## Data Flow

```
Client click (0x82) on ground food
  → do_use_item resolves ContainerSource::Ground(pos)
  → Lua runtime: food.onUse(args)
    → if cooldown OK: do_feed(pid, ...) + do_send_text_message(pid, 13, "Glup")
    → queues [GameAction::Feed, GameAction::TextMessage]
  → drain_actions():
    → Feed → apply/extend ConditionRegeneration
           → decrement_food(pid, sid, Ground(pos))
             → materialize(pos)
             → take_from_ground(pos, idx, 1, stackable?) → COW remove/split
             → broadcast_source(pos, stackpos, removed_fully, idx)
               → 0x6C (full remove) or 0x6B (count update) to spectators
    → TextMessage → push_console_red(pid, "Glup")
      → 0xB4 packet: [0xB4][13][u16 len][text ≤255 bytes]
```

## File Changes

| File | Action | Description |
|------|--------|-------------|
| `crates/world/src/game/mod.rs` | Modify | Add `MSG_CONSOLE_RED = 13`, `push_console_red()` following `push_console_blue` pattern |
| `crates/world/src/game/lua.rs` | Modify | Add `GameAction::TextMessage { player_id, message_type: u8, text: String }`, register `do_send_text_message` builtin, remove `Copy` derive |
| `crates/world/src/game/containers.rs` | Modify | Handle `ContainerSource::Ground(pos)` in `decrement_food`: materialize → find sid → `take_from_ground` → `broadcast_source`. Add `TextMessage` arm in action drain |
| `config/lua/scripts/food.lua` | Modify | Add `do_send_text_message(pid, 13, msg)` call per food type (Glup/Chomp/Munch) |

## Interfaces / Contracts

```rust
// In mod.rs — new 0xB4 push following push_console_blue pattern
const MSG_CONSOLE_RED: u8 = 13;
fn push_console_red(&mut self, id: u32, text: &str) {
    let bytes = text.as_bytes();
    let mut w = MessageWriter::new();
    w.write_u8(0xB4);
    w.write_u8(MSG_CONSOLE_RED);
    w.write_string(&bytes[..bytes.len().min(255)]);
    self.push(id, w.into_bytes());
}

// In lua.rs — new GameAction variant
#[derive(Debug, Clone)]
pub(crate) enum GameAction {
    Teleport { player_id: u32, landing: Position },
    Feed { player_id: u32, health_gain: i32, interval_ms: u64, duration_ms: u64, total_heal_cap: i32 },
    TextMessage { player_id: u32, message_type: u8, text: String },
}

// In lua.rs — new builtin registration (mirrors do_feed pattern)
let text_actions = Arc::clone(&actions);
let do_send_text_message = lua
    .create_function(move |_, (id, msg_type, text): (u32, u8, String)| {
        text_actions.lock().unwrap().push(GameAction::TextMessage {
            player_id: id,
            message_type: msg_type,
            text,
        });
        Ok(())
    })
    .expect("create_function for do_send_text_message must not fail");
lua.globals().set("do_send_text_message", do_send_text_message)
    .expect("set do_send_text_message global must not fail");
```

## Testing Strategy

| Layer | What to Test | Approach |
|-------|-------------|----------|
| Unit (lua.rs) | `do_send_text_message` queues `GameAction::TextMessage` with correct fields | Create `LuaRuntime` with a script that calls it, `drain_actions()`, match variant — same pattern as `do_feed_pushes_feed_action_with_correct_params` |
| Unit (containers.rs) | Ground food decrement removes item from overlay, broadcasts 0x6C/0x6B | Set up food on tile via `dynamic`, call `decrement_food` with `Ground(pos)`, verify overlay + spectator packets |
| Unit (containers.rs) | Race-safe ground: missing item returns silently | Call `decrement_food(Ground(pos))` for a tile with no food — no panic, no crash |
| Integration | Full flow: Lua → Feed + TextMessage → decrement + push_console_red | End-to-end test with real `LuaRuntime` dispatching `food.onUse`, drain both `Feed` and `TextMessage` actions, verify packet output |
| Unit (mod.rs) | `push_console_red` builds correct 0xB4 packet | Compare byte layout against `push_console_blue` pattern with type=13, verify truncation at 255 bytes |

## Migration / Rollout

No migration required. Ground food did not work before — existing data is unaffected. Existing inventory/container eating continues unchanged and now also shows the red message.

## Open Questions

None.
