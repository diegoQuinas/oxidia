# Food Consumption Specification

## Purpose

Players eat food from inventory slots, containers, or ground tiles with red console feedback ("Glup"/"Chomp") and regeneration. Covers `MSG_CONSOLE_RED` (13), `do_send_text_message` Lua builtin, ground source handling, and per-food message strings.

## Requirements

### REQ-FC-01: Red Console Message

The system MUST define `MSG_CONSOLE_RED = 13` and `push_console_red(id, text)` sending `0xB4` packet with type 13. Text MUST truncate to 255 bytes.

**Scenario: Red message on food eat** — GIVEN food consumption succeeds, WHEN the action handler processes `GameAction::TextMessage`, THEN a `0xB4` packet with type 13 and per-food text MUST be pushed to the player.

**Scenario: Message truncation** — GIVEN text > 255 bytes, WHEN `push_console_red` builds the packet, THEN only bytes `[0..255)` MUST be written.

### REQ-FC-02: Lua TextMessage Builtin

The system MUST add `GameAction::TextMessage { player_id, message_type: u8, text: String }` and register `do_send_text_message(id, type, text)` as a Lua global. The Lua builtin MUST push a `GameAction::TextMessage` onto the action queue.

**Scenario: Builtin queues action** — GIVEN `food.lua` calls `do_send_text_message(pid, 13, "Glup")`, WHEN `drain_actions()` runs, THEN a `GameAction::TextMessage` with matching fields MUST be present.

**Scenario: Action handler pushes packet** — GIVEN a drained `GameAction::TextMessage` with type 13, WHEN the action handler matches it in containers.rs, THEN `push_console_red` MUST be called with the player_id and text.

### REQ-FC-03: Ground Food Source

The system MUST handle `ContainerSource::Ground(pos)` in `decrement_food` by materializing the tile, calling `take_from_ground`, and broadcasting `0x6C` (full remove) or `0x6B` (count update) to spectators.

**Scenario: Eat single food from ground** — GIVEN non-stackable food at position P, WHEN a player eats it, THEN the item MUST be removed from the tile and a `0x6C` remove packet broadcast to all spectators of P.

**Scenario: Partial stack decrement** — GIVEN stackable food with count > 1 on the ground, WHEN one unit is eaten, THEN count MUST decrement and a `0x6B` update packet broadcast to spectators.

**Scenario: Race-safe ground removal** — GIVEN ground item was already taken between `materialize` and `take_from_ground`, WHEN `take_from_ground` returns `None`, THEN `decrement_food` MUST return silently without panic.

### REQ-FC-04: Per-Food Console Messages

`food.lua` MUST call `do_send_text_message(id, 13, msg)` with per-food message strings matching TFS 1.4.2.

**Scenario: Meat family "Glup"** — GIVEN item_id 2666 (meat) is eaten, WHEN `food.onUse` succeeds, THEN `do_send_text_message` MUST be called with message "Glup".

**Scenario: Fish family "Chomp"** — GIVEN item_id 2676 (salmon) is eaten, WHEN `food.onUse` succeeds, THEN `do_send_text_message` MUST be called with message "Chomp".

**Scenario: Cheese "Munch"** — GIVEN item_id 2679 (cheese) is eaten, WHEN `food.onUse` succeeds, THEN `do_send_text_message` MUST be called with message "Munch".

### REQ-FC-05: Inventory and Container Regression

Eating from inventory slots and nested containers MUST continue working. Both paths MUST also show the red console message after eating.

**Scenario: Eat from inventory slot** — GIVEN food in inventory slot 4, WHEN eaten, THEN slot 4 count MUST decrement AND a red console message MUST appear.

**Scenario: Eat from nested container** — GIVEN food inside an open container (cid=0, slot=2), WHEN eaten, THEN item MUST be removed from the container AND a red console message MUST appear.

**Scenario: Global cooldown from any source** — GIVEN the player ate food 1s ago from the ground, WHEN they try to eat from inventory, THEN `food.onUse` MUST return false and no action MUST be queued.

### REQ-FC-06: Lua Error Resilience

Lua script errors during message dispatch MUST NOT crash the game actor.

**Scenario: Invalid argument logged** — GIVEN `do_send_text_message` is called with invalid args, WHEN the Lua runtime returns an error, THEN `tracing::error` MUST log the error and the actor MUST continue processing.
