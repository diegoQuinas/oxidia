# Tasks: Eat from Ground with Red Console Message

## Review Workload Forecast

| Field | Value |
|-------|-------|
| Estimated changed lines | ~150-200 |
| 400-line budget risk | Low |
| 800-line budget risk | Low |
| Chained PRs recommended | No |
| Delivery strategy | auto-forecast |
| Chain strategy | pending |

Decision needed before apply: No
Chained PRs recommended: No
Chain strategy: pending
400-line budget risk: Low

## Phase 1: Foundation — push_console_red

- [x] EAT-01: RED test — `push_console_red` builds 0xB4 packet with type 13 (`mod.rs`)
- [x] EAT-02: Add `MSG_CONSOLE_RED = 13` + `push_console_red(id, &str)` (`mod.rs`)
- [x] EAT-03: RED test — message truncation at 255 bytes (`mod.rs`)

## Phase 2: Lua Builtin + Action Handler

- [x] EAT-04: RED test — `do_send_text_message` queues `GameAction::TextMessage` (`lua.rs`)
- [x] EAT-05: Add `TextMessage{player_id, message_type, text}` variant + remove `Copy` (`lua.rs`)
- [x] EAT-06: Register `do_send_text_message(id, type, text)` Lua global (`lua.rs`)
- [x] EAT-07: RED test — `TextMessage` arm calls `push_console_red` (`containers.rs`)
- [x] EAT-08: Add `TextMessage` match arm in `drain_actions` (`containers.rs`)

## Phase 3: Ground Source Handling

- [x] EAT-09: RED test — `decrement_food(Ground)` removes item + broadcasts 0x6C/0x6B (`containers.rs`)
- [x] EAT-10: Implement `Ground(pos)` arm: materialize → `take_from_ground` → `broadcast_source` (`containers.rs`)
- [x] EAT-11: RED test — race-safe ground removal returns silently when item missing (`containers.rs`)

## Phase 4: Script + Integration

- [x] EAT-12: RED integration test — full Lua→Feed+TextMessage→decrement→push_console_red (e2e)
- [x] EAT-13: Update `food.lua` — `do_send_text_message(pid, 13, msg)` per food type (Glup/Chomp/Munch)
- [x] EAT-14: Verify inventory/container regression: eat from slot and container still works
