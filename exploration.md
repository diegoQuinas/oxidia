## Exploration: Food/Eating Bug — Control+Click on Meat Does Nothing

### Current State

The server has **no food consumption system**. When control+click is performed on a food item:

1. **Client** sends opcode `0x82` (use-item) to the server, with the item's position and sprite id.
2. **Server** (`game_service.rs:447-452`) routes `0x82` to `world.use_item()` → `Game::do_use_item()`.
3. **`do_use_item()`** (`containers.rs:135-267`) resolves the item's location and checks its metadata:
   - **Is it a container?** No → skip the open-container path.
   - **Is it registered in `actions.xml`?** Only items 1386, 1391, 384 are registered (all teleport items) → meat (2666) is NOT registered.
   - Since neither condition is met → **returns silently** at line 204 with no action taken.
4. Nothing happens — no packet is sent back, no state change occurs.

### Affected Areas

- `server/crates/world/src/game/containers.rs` (lines 135-267) — `do_use_item()` is the handler, but it has no food-specific branch.
- `server/crates/world/src/game/lua.rs` — `GameAction` enum only has `Teleport`; needs a `Feed` action.
- `server/crates/world/src/game/condition.rs` — `ConditionRegeneration` already exists and supports health-over-time; needs a way to be added from Lua actions.
- `server/crates/world/src/game/xml_registry.rs` — Already works; just needs food items registered.
- `server/config/lua/actions.xml` — Only registers teleport items; needs food entries.
- `server/config/lua/scripts/` — Only has `teleport.lua`; needs a `food.lua` script.
- `server/crates/formats/src/items_xml.rs` — Item XML parser; could optionally parse a `food`/`nutrition` attribute, but TFS uses action registration (not item attributes) to identify food.

### Approaches

1. **Lua-based food system (recommended — matches TFS)**
   - Add a `Feed` variant to `GameAction` in `lua.rs` with fields: `player_id`, `health_gain`, `interval_ms`, `duration_ms`, `total_heal_cap`.
   - Drain the `Feed` action in `do_use_item()` (or in the Lua dispatch area) to add a `ConditionRegeneration` to the player's `conditions` vec.
   - Create `config/lua/scripts/food.lua` with an `onUse(args)` function that calls the builtin `do_feed(...)` function.
   - Register food item ranges in `config/lua/actions.xml`: e.g. `<action fromid="2666" toid="2691" script="food.onUse"/>`.
   - **Pros**: Faithful to TFS behavior; Lua makes it easy to configure custom food values; no schema changes needed.
   - **Cons**: Requires implementing the feed action drain + Lua binding.
   - **Effort**: Medium (new Lua binding, action drain in Rust, new Lua script, config changes).

2. **Attribute-based food system**
   - Add a `food_nutrition` field to `ItemXmlAttrs` and `ItemMeta`.
   - Parse `<attribute key="food" value="N">` in `items_xml.rs`.
   - Add a `do_feed()` method to `Game` that checks `ItemMeta.food_nutrition` > 0 and adds a regeneration condition.
   - **Pros**: Self-describing items; no Lua dependency for basic food.
   - **Cons**: Doesn't match TFS convention; actual TFS items.xml has no `food` attribute — food is identified by being registered in actions.xml, not by item flags.
   - **Effort**: Medium (schema change + parser change + logic).

3. **Hybrid (recommended for production)**
   - Use Lua actions to identify food items (approach 1), but implement the core `do_feed()` Rust function so the Lua script is thin (just nutrition values).
   - **Pros**: Best of both; TFS-compatible registration path, Rust-native health-over-time handling.
   - **Effort**: Medium-High.

### Recommendation

**Approach 1 (Lua-based food system)** is the right fit for this codebase's current architecture. The server already has:
- The `LuaRuntime` with dispatch mechanism
- The `ConditionRegeneration` struct ready to use
- The `XmlRegistry` for item-to-script mapping
- The `GameAction` pattern for Lua-to-Rust communication

The missing pieces are:
1. A `Feed` variant in `GameAction`
2. Drain logic in `do_use_item()` to apply `ConditionRegeneration` from a `Feed` action
3. A `food.lua` script
4. `actions.xml` entries for food item ranges

### Risks

- The `GameAction::Feed` action must be drained in the Lua dispatch section of `do_use_item()` (`containers.rs:184-204`), alongside the existing `Teleport` drain.
- The `ConditionRegeneration` struct is minimal and already tested; the feed system must calculate nutrition-based health regen values.
- Item removal on consumption must be implemented (the food item must be removed from the inventory/ground after eating).
- No `mana_gain` field exists in `ConditionRegeneration` — only HP regen. Mana food (e.g. brown mushrooms) would need a separate condition or an extended struct.

### Ready for Proposal

Yes — the root cause is clear and well-understood. The orchestrator should present these findings to the user and confirm the approach before proceeding to design.
