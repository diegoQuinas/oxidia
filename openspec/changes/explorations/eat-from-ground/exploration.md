## Exploration: Eat from Ground with Red Message

### Current State

The server **already has a food consumption system** built via Lua scripting:

1. `config/lua/actions.xml` maps items **2666-2691** (meat, fish, bread, etc.) to `food.onUse`
2. `config/lua/scripts/food.lua` defines per-item food stats `{health_gain, interval_ms, duration_ms, total_heal_cap}` and calls `do_feed(...)`
3. The Rust side (containers.rs `do_use_item`) routes non-container items through Lua dispatch
4. The `do_feed` Lua builtin pushes a `GameAction::Feed` to the action queue
5. The Feed handler applies/extends `ConditionRegeneration` on the player and calls `decrement_food`

**What currently works:**
- Eating from inventory slot (`ContainerSource::Slot`) — food decremented, regeneration applied
- Eating from container (`ContainerSource::Nested`) — food decremented, regeneration applied

**What does NOT work (the gaps this change must fill):**

- `decrement_food(ContainerSource::Ground(_))` is an **explicit no-op** — ground food is NOT removed
- No red text message is sent at all — `food.lua` only calls `do_feed`, with no text output
- No Lua builtin exists to send text messages back to the player

### TFS Reference (food.lua)

In TFS, food consumption works as follows:
```lua
-- food[2] is the message string ("Chomp.", "Glup.", etc.)
player:say(food[2], TALKTYPE_MONSTER_SAY)
player:feed(food[1] * 12)
item:remove(1)
```

The **red message** uses `TALKTYPE_MONSTER_SAY = 14` which sends a `0xAA` creature-say packet with speak type 14 — the client renders this as red text in the console. This is distinct from a `0xB4` text message.

### Message Types (from TFS `const.h`)

| Constant | Value | Visual |
|----------|-------|--------|
| `MESSAGE_STATUS_CONSOLE_BLUE` | 4 | Blue console |
| `MESSAGE_STATUS_CONSOLE_RED` | 13 | Red console |
| `MESSAGE_STATUS_DEFAULT` | 17 | White bottom + console |
| `MESSAGE_STATUS_WARNING` | 18 | Red game window + console |
| `MESSAGE_STATUS_SMALL` | 21 | White bottom bar |
| `MESSAGE_INFO_DESCR` | 22 | Green game window + console |

Current code defines: `MSG_STATUS_SMALL = 21`, `MSG_INFO_DESCR = 22`, `MSG_CONSOLE_BLUE = 4`. **`MSG_CONSOLE_RED = 13` is NOT defined yet.**

### Affected Areas

- `crates/world/src/game/containers.rs` — `decrement_food()` has no-op for `ContainerSource::Ground`; needs ground-item removal
- `crates/world/src/game/mod.rs` — Needs `MSG_CONSOLE_RED = 13` constant; needs a `push_console_red()` method
- `crates/world/src/game/lua.rs` — `GameAction` enum needs a `TextMessage` variant (or `MonsterSay`); needs a `do_send_text_message` builtin
- `crates/protocol/src/chat.rs` — `SpeakType` enum only has Say/Whisper/Yell; needs `MonsterSay = 14` if following TFS approach
- `config/lua/scripts/food.lua` — Needs to send text message after consuming
- `crates/world/src/game/items.rs` — Ground removal helpers already exist (`take_from_ground`) — may be reusable

### Approaches

#### 1. Lua `do_send_text_message` builtin + ground decrement (recommended — cleanest)

- Add `MSG_CONSOLE_RED = 13` constant in `mod.rs`
- Add a `push_console_red()` method that sends `0xB4` with type `MSG_CONSOLE_RED`
- Add `GameAction::TextMessage { player_id, message_type: u8, text: Vec<u8> }` to `lua.rs`
- Register a Lua builtin `do_send_text_message(player_id, type, text)` that pushes the action
- Extend `decrement_food` to handle `ContainerSource::Ground` by calling `take_from_ground` — removing the item from the dynamic overlay and broadcasting removal to spectators
- Update `food.lua` to call `do_send_text_message` with the message string

**Pros:**
- Clean separation: Rust handles protocol, Lua handles game logic
- Reuses existing `take_from_ground` machinery
- Red message via `0xB4` is simpler than monster-speak approach
- Extensible for future Lua scripts that need to show messages

**Cons:**
- Slightly different from TFS which uses `TALKTYPE_MONSTER_SAY` (monster-speak in chat channel)
- The red via `0xB4` vs monster-speak `0xAA` is a visual difference

**Effort: Medium** (2-3 files changed in Rust, 1 Lua script updated)

#### 2. TFS-faithful: `TALKTYPE_MONSTER_SAY` + ground decrement

- Add `MonsterSay = 14` to `SpeakType` enum in `chat.rs`
- Add `do_monster_say` Lua builtin that queues a `GameAction::MonsterSay`
- Extend Feed handler (or separate action handler) to broadcast `0xAA` creature-say with speak type 14
- Extend `decrement_food` to handle `ContainerSource::Ground`

**Pros:**
- Byte-identical to TFS behavior
- Shows in the chat console (not just status bar)
- More authentic Tibia feel

**Cons:**
- `SpeakType` enum needs extension — affects all chat dispatch paths
- Creature-say format requires statement_id, name, level, position — more protocol overhead
- More moving parts

**Effort: Medium-High** (chat.rs, lua.rs, containers.rs, mod.rs, food.lua)

#### 3. All-Rust: Hardcode food behavior in `do_use_item`

- Instead of Lua dispatch for food, add a special case in `do_use_item` that checks `server_id` against a food range
- Apply `ConditionRegeneration` directly, remove from ground, push red message

**Pros:**
- No Lua changes needed
- Single-file change

**Cons:**
- Breaks the Lua-scripting pattern the project established
- Hardcodes item IDs in Rust code
- Not extensible

**Effort: Low** (but wrong architecture)

### Recommendation

**Approach 1 (Lua `do_send_text_message` + ground decrement).** It fits the existing architecture (Lua scripts drive item behavior, Rust handles protocol and state). The `0xB4` red console message achieves the same visual effect as TFS's monster-say, with less protocol complexity. The ground-decrement should reuse `take_from_ground` from `items.rs` and broadcast the removal the same way `do_move_thing` does.

### Risks

- **Ground item removal**: Must ensure the dynamic overlay is materialized before removing. `materialize()` already does COW from static map. Need to broadcast `0x6C` (remove) or `0x6B` (update count) to spectators, and the food may be stackable — must handle count decrement vs full removal.
- **Food from container and ground simultaneously**: If the player has the same food in a container and on the ground, we must ensure we only decrement the correct source.
- **Adjacency check**: `do_use_item` already checks adjacency for ground items (line 229-233). Ground food must be within 1 tile to eat — already enforced.
- **Lua cooldown**: The current `food.lua` has a 2-second per-player cooldown using `os.time()`. This doesn't differentiate between inventory and ground sources. If the player picks up food and eats from inventory within 2 seconds, it's blocked — which is correct TFS behavior.

### Ready for Proposal

**Yes.** The exploration is complete and the requirements are well-understood. The orchestrator should tell the user: "We have a food system that works from inventory/containers but has two gaps: (1) `decrement_food` has a no-op for ground sources, and (2) no red text message is sent. The solution involves adding a Lua text-message builtin, extending the ground decrement path, and updating the food.lua script. Recommend proceeding to `sdd-propose`."
