# game.rs Module Split — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Split the 6367-line `crates/world/src/game.rs` god-file into a `game/` submodule folder, one file per concern, with each concern's tests living alongside its code — without changing any behavior.

**Architecture:** `game.rs` becomes `game/mod.rs`, which keeps the public API (`WorldHandle`, the DTO structs, `spawn`, `push_channel`), the core data model (`Game`, `PlayerState`, and the embedded item/container types), the actor dispatch (`Command`, `handle`), and the widely-shared `impl Game` helper methods. Each domain concern (containers, movement, combat, gm, chat, look, items, session) moves to its own child file as additional `impl Game` blocks. Shared test fixtures move to a `test_support` module; each concern file carries its own `#[cfg(test)] mod tests`.

**Tech Stack:** Rust 2021, `cargo` workspace, crate `world`. No new dependencies.

---

## The one Rust rule that governs this whole refactor

**A private item is visible in its defining module AND all descendant modules.** `Game` and `PlayerState` are defined in `game/mod.rs` (module `game`). Every child file (`game/containers.rs` = module `game::containers`, etc.) is a *descendant* of `game`, so its `impl Game` methods can read `self.players`, `self.map`, `self.rng`, every `PlayerState` field, and every embedded private type **with no visibility annotations**.

The only things that need widened visibility are **methods called from OUTSIDE the module that defines them**:
- A method moved to a child file but called from `handle`/`spawn` (which stay in the parent `mod.rs`) → parent calling child → needs `pub(super)`.
- A method moved to a child file but called from a *sibling* child file → needs `pub(super)` (`pub(super)` = visible to parent `game` and therefore to all `game` descendants).
- A method that stays in `mod.rs` and is called from children → no change (children are descendants; they see parent privates).

`pub(super)` never leaks anything outside the `game` module, so over-applying it is harmless. The compiler emits `E0624: method is private` for every miss — treat that as the authoritative checklist, not these tables.

**Public-path preservation:** `lib.rs` declares `pub mod game;` with **no** crate-root re-exports, so external crates use `world::game::WorldHandle`, `world::game::InitialState`, etc. Therefore every `pub` item MUST remain reachable as `world::game::*`. We keep all `pub` API in `mod.rs` — zero public-path churn. Do NOT move `pub` types into child modules.

---

## Target file layout

```
crates/world/src/
  lib.rs                  (unchanged — still `pub mod game;`)
  game/
    mod.rs                core: Game, PlayerState, embedded types, Command,
                          handle, WorldHandle, spawn, shared helpers, view consts
    session.rs            login, logout, save_all, do_change_outfit, do_request_outfit
    containers.rs         the entire container engine + container free-helpers
    movement.rs           do_turn, resolve_vertical, do_teleport, do_move
    combat.rs             do_set_target, apply_damage, do_death, on_combat_tick
    gm.rs                 GmVerb + impl, tokenize_args, parse_pos, do_gm_command, gm_*
    chat.rs               do_say
    look.rs               do_look, do_look_battle, describe_tile_item, describe_creature
    items.rs              do_move_thing, do_move_inventory, take/add ground, broadcasts,
                          push_inventory_slot, item_wire_stackpos
    test_support.rs       #[cfg(test)] shared fixtures (add_player, maps, outfits, drain…)
  combat.rs               (unchanged — pure damage math)
  map.rs                  (unchanged)
  outfit_catalog.rs       (unchanged)
```

### What STAYS in `game/mod.rs` (the core — do not move these)

**Public API (must keep the `world::game::*` path):**
- `pub struct PlayerSnapshot`, `pub struct LoginAck`, `pub struct InitialState`, `pub struct SaveRecord`
- `pub struct WorldHandle` + its entire `impl` (all the async forwarder methods)
- `pub fn push_channel`, `pub fn spawn`
- `pub const MELEE_ATTACK_INTERVAL_MS` (referenced by tests and combat; keep central)

**Core data model (embedded in `Game`/`PlayerState`, so they live with them):**
- `struct InvItem`, `struct ContainerItem` + `impl ContainerItem { wire() }`, `enum ContainerSource`, `struct OpenContainer`
- `struct PlayerState`, `struct Game`
- `enum Command`

**Shared `impl Game` helpers (called from many children → keep in parent so children inherit access):**
`new`, `new_seeded`, `merged`, `materialize`, `can_see`, `spectators_in_range`, `spectators`, `visible_from`, `introduce`, `push`, `handle`, `tile_occupied`, `creature_stackpos_on`, `free_spawn`, `free_spawn_near`, `creatures_on`, `push_cannot_move`, `merged_server_id`, `merged_count`, `merged_pre_creature_len`, `push_status_message`, `push_info_descr`, `push_console_blue`.

**Constants:** `PUSH_CAPACITY`, `COMBAT_TICK_MS`, `MSG_STATUS_SMALL`, `MSG_INFO_DESCR`, `MSG_CONSOLE_BLUE`, `VIEW_LEFT/RIGHT/UP/DOWN`.

---

## Conventions for every extraction task

Each child file follows this skeleton:

```rust
//! <concern> behavior for the game actor.

use super::*; // pulls in Game, PlayerState, the data types, helpers, and the crate imports re-exported by mod.rs

impl Game {
    // ... moved methods ...
}

#[cfg(test)]
mod tests {
    use super::super::*;          // Game, public API, data types
    use super::super::test_support::*; // shared fixtures
    // ... moved tests ...
}
```

Notes:
- `use super::*;` at the top of a child file resolves `Game`, `PlayerState`, the embedded types, and any `pub`/`pub(super)`/private-but-inherited items from `mod.rs`. If a specific protocol import (e.g. `protocol::chat::SpeakType`) is not visible through `super::*`, add an explicit `use protocol::...;` line to that child file — do NOT add it to `mod.rs` just for the child.
- Free helper functions that are private and used only within one concern (`matches_source`, `in_close_range`, `tokenize_args`, `parse_pos`, `find_player_by_name`) move WITH that concern and stay private.
- When you move a method that is called from `handle`, `spawn`, or another child file, change its signature from `fn name(` to `pub(super) fn name(`. The per-task tables below list the known ones; **also obey every `E0624` the compiler raises.**

### Verification ritual (run after EVERY task, before committing)

```bash
cargo build -p world
cargo test -p world
cargo clippy -p world -- -D warnings
```
Expected after every task: build OK, **all tests pass with the same count as the task-0 baseline**, clippy clean. The test suite is the behavior contract — a green run means the move was behavior-preserving. If the test count drops, you dropped a test; restore it.

---

## Task 0: Scaffold the `game/` folder (rename only — no logic moves)

**Files:**
- Move: `crates/world/src/game.rs` → `crates/world/src/game/mod.rs`
- Create (empty stubs): `crates/world/src/game/{session,containers,movement,combat,gm,chat,look,items,test_support}.rs`

- [ ] **Step 1: Record the baseline test count**

Run: `cargo test -p world 2>&1 | tail -5`
Write down the reported number of passing tests (the "baseline count"). Every later task must match it.

- [ ] **Step 2: Move the file with git (preserve history)**

```bash
cd crates/world/src
mkdir game
git mv game.rs game/mod.rs
```

- [ ] **Step 3: Create empty child-module files**

Create each of these files with a single doc-comment line so they are valid empty modules:

`crates/world/src/game/session.rs`:
```rust
//! Session lifecycle (login, logout, save, outfit) for the game actor.
```
Repeat with the matching doc comment for `containers.rs`, `movement.rs`, `combat.rs`, `gm.rs`, `chat.rs`, `look.rs`, `items.rs`. For `test_support.rs`:
```rust
//! Shared test fixtures for the game module's per-file test suites.
```

- [ ] **Step 4: Declare the child modules at the top of `game/mod.rs`**

Add immediately after the existing `use` block in `game/mod.rs`:
```rust
mod chat;
mod combat;
mod containers;
mod gm;
mod items;
mod look;
mod movement;
mod session;
#[cfg(test)]
mod test_support;
```
(Leave the modules empty for now — empty modules compile fine.)

- [ ] **Step 5: Verify nothing broke (pure rename)**

Run the verification ritual. Build, all baseline tests pass, clippy clean. No code moved yet, so this MUST be green.

- [ ] **Step 6: Commit**

```bash
git add -A
git commit -m "refactor(world): scaffold game/ module folder"
```

---

## Task 1: Extract shared test fixtures into `test_support.rs`

**Why first:** every concern's tests reuse these fixtures. Centralizing them once lets later tasks move tests cleanly.

**Files:**
- Modify: `crates/world/src/game/mod.rs` (cut fixtures out of `mod tests`)
- Modify: `crates/world/src/game/test_support.rs`

**Fixtures to move** (locate by name with `rg 'fn <name>' crates/world/src/game/mod.rs`; original line hints in parens):
`stair_map` (~3195), `walk_map` (~3397), `knight` (~3422), `default_initial` (~3428), `add_player` (~3443), `combat_map` (~3864), `wide_combat_map_with_pz` (~4127), `look_map` (~5544), `recv_look_text` (~5610), `move_map` (~5771), `drain` (~5850), `has_op` (~5856), `inv_pos` (~6343), `outfit_window_looktypes` (~5400), `drain_find_icons` (~5445), `count_sid_in_overlays` (~6053).

Leave the movement-only ClientSim cluster (`underground_room`, `server_floor8_ids`, `seed_floor8`, `decode_band_into`, `apply_walk_update`, `underground_multifloor`, `server_floor8_ids_z`, `ClientSim` + its impl, `sim_apply`, `stair_multifloor`, `seed_initial`, `first_band_mismatch`, `replay`, `deep_stair_multifloor`, `run_scenario`) in place for now — those move with movement tests in Task 8.

- [ ] **Step 1: Write the `test_support` module shell**

Replace the contents of `crates/world/src/game/test_support.rs`:
```rust
//! Shared test fixtures for the game module's per-file test suites.

use super::*;
// (add any extra `use` lines the moved fixtures need, e.g. protocol decoders,
//  as the compiler reports unresolved names)
```

- [ ] **Step 2: Move the shared fixtures**

Cut each fixture function listed above from the `#[cfg(test)] mod tests` block in `game/mod.rs` and paste it into `test_support.rs`. Change each one's visibility from `fn` to `pub(super)` (sibling test modules must see them). Move any `const`/helper structs they depend on alongside them.

- [ ] **Step 3: Point `mod.rs`'s remaining tests at the fixtures**

At the top of the `#[cfg(test)] mod tests` block still in `game/mod.rs`, add:
```rust
use super::test_support::*;
```

- [ ] **Step 4: Verify**

Run the verification ritual. Baseline test count unchanged.

- [ ] **Step 5: Commit**

```bash
git add -A
git commit -m "refactor(world): extract shared test fixtures into game::test_support"
```

---

## Task 2: Extract `gm.rs` (most self-contained — safe first concern)

**Files:**
- Modify: `crates/world/src/game/mod.rs`
- Modify: `crates/world/src/game/gm.rs`

**Items to move:**
- `enum GmVerb` + `impl GmVerb` (`ALL`, `words`, `usage`, `description`, `from_word`) (~2863–2945)
- free fns `tokenize_args` (~2950), `parse_pos` (~2983) — stay private
- `impl Game` methods: `do_gm_command` (~2091), `gm_help` (~2126), `find_player_by_name` (~2134, stays private), `gm_goto` (~2142), `gm_temple` (~2175), `gm_item` (~2203), `gm_teleport` (~2238), `gm_teleportto` (~2260), `gm_bring` (~2275), `gm_changesex` (~2289), `gm_setlooktype` (~2314), `do_spawn_item` (~2352)

**`pub(super)` required (called from outside `gm`):**
- `do_gm_command` — called by `handle` in `mod.rs`.

**Cross-module calls this concern makes** (all resolve fine; listed so you don't panic):
- `do_teleport` — still in `mod.rs` until Task 8; child→parent, OK. After Task 8 it becomes a sibling and gets `pub(super)` there.
- `materialize`, `creatures_on`, `broadcast_dest` — `materialize`/`creatures_on` are core (mod.rs), OK. `broadcast_dest` moves to `items.rs` in Task 6 and is marked `pub(super)` there.

**Tests to move into `gm.rs`'s `mod tests`:** `tokenize_args_groups_quoted_segments` (~3460), `parse_pos_reads_three_coords` (~3469), `find_player_by_name_is_case_insensitive` (~3477), `gmverb_registry_is_complete_and_resolvable` (~3487).

- [ ] **Step 1: Move the code**

Fill `game/gm.rs` using the child-file skeleton. Paste the `GmVerb` enum/impl, the two free fns, and the `impl Game { ... }` block with the 12 methods. Mark `do_gm_command` as `pub(super)`.

- [ ] **Step 2: Move the tests**

Add the `#[cfg(test)] mod tests` block (with `use super::super::*;` and `use super::super::test_support::*;`) and paste the 4 GM tests, cutting them from `mod.rs`.

- [ ] **Step 3: Delete the moved code from `mod.rs`**

Ensure every moved item is removed from `game/mod.rs` (no duplicates).

- [ ] **Step 4: Verify**

Run the verification ritual. Fix any `E0624` by adding `pub(super)` to the named method. Baseline test count unchanged.

- [ ] **Step 5: Commit**

```bash
git add -A
git commit -m "refactor(world): extract GM commands into game::gm"
```

---

## Task 3: Extract `chat.rs`

**Files:** modify `game/mod.rs`, `game/chat.rs`.

**Items to move:** `impl Game` method `do_say` (~1214–1262). Add `use protocol::chat::{self, SpeakType};` to `chat.rs` if `super::*` doesn't surface it.

**`pub(super)` required:** `do_say` — called by `handle`.

**Tests to move:** `say_broadcasts_to_spectator_and_speaker` (~3807), `say_does_not_reach_beyond_viewport` (~3824), `yell_uppercases_and_reaches_far_spectator` (~3833), `whisper_full_to_adjacent_pspsps_to_far_in_view` (~3845).

- [ ] **Step 1: Move `do_say` into `game/chat.rs`** (skeleton + `pub(super)`).
- [ ] **Step 2: Move the 4 chat tests into `chat.rs`'s `mod tests`.**
- [ ] **Step 3: Delete moved code from `mod.rs`.**
- [ ] **Step 4: Verify** (ritual; fix `E0624`; baseline count).
- [ ] **Step 5: Commit**

```bash
git commit -am "refactor(world): extract chat into game::chat"
```

---

## Task 4: Extract `look.rs`

**Files:** modify `game/mod.rs`, `game/look.rs`.

**Items to move:** `do_look` (~1271), `do_look_battle` (~1301), `describe_tile_item` (~1936), `describe_creature` (~1991).

Keep in `mod.rs` (shared, do NOT move): `creatures_on`, `merged_server_id`, `merged_count`, `merged_pre_creature_len`, `push_status_message`, `push_info_descr`.

**`pub(super)` required:** `do_look`, `do_look_battle` — called by `handle`. (`describe_*` are only called by `do_look*`, so they can stay private in `look.rs`.)

**Tests to move:** all `do_look_*` tests (~5618–5747): `do_look_ground_item_adjacent_shows_article_name_and_weight`, `do_look_ground_item_far_away_omits_weight`, `do_look_non_pickupable_item_no_weight_line`, `do_look_stackable_item_with_count_shows_count_and_plural`, `do_look_other_player_shows_name_level_and_pronoun`, `do_look_self_shows_yourself`, `do_look_gamemaster_item_appends_item_id_and_position`, `do_look_non_gamemaster_no_debug_suffix`, `do_look_out_of_viewport_pushes_nothing`. These use `look_map` and `recv_look_text` (already in `test_support`).

- [ ] **Step 1: Move the 4 methods into `game/look.rs`** (mark the two dispatched ones `pub(super)`).
- [ ] **Step 2: Move the 9 look tests into `look.rs`'s `mod tests`.**
- [ ] **Step 3: Delete moved code from `mod.rs`.**
- [ ] **Step 4: Verify** (ritual; fix `E0624`; baseline count).
- [ ] **Step 5: Commit**

```bash
git commit -am "refactor(world): extract look/describe into game::look"
```

---

## Task 5: Extract `combat.rs` (actor combat behavior)

**Files:** modify `game/mod.rs`, `game/combat.rs`.

> Note: this is `game/combat.rs` (actor behavior). The existing top-level `crates/world/src/combat.rs` (pure damage math) is untouched and is still imported via `use crate::combat;` in `mod.rs`.

**Items to move:** `do_set_target` (~2054), `apply_damage` (~2396), `do_death` (~2492), `on_combat_tick` (~2556). Add `use protocol::combat_packets;` to `combat.rs` if needed.

Keep `pub const MELEE_ATTACK_INTERVAL_MS` in `mod.rs`.

**`pub(super)` required:** `do_set_target`, `on_combat_tick` (called by `handle`/`spawn`), `do_death` and `apply_damage` if called cross-module (`apply_damage` is called by `on_combat_tick` — same module — but also verify; `do_death` is called by `apply_damage`). Apply `pub(super)` per `E0624`.

**Cross-module call:** `do_death` calls `export_container_items` — still in `mod.rs` until Task 6 (child→parent OK); after Task 6 it's a sibling in `containers.rs` and gets `pub(super)` there.

**Tests to move** (~3891–4188): `set_target_sets_attacking_and_clear_resets_it`, `set_target_self_is_ignored`, `set_target_from_pz_tile_rejects_and_pushes_0xb4`, `combat_tick_deals_damage_to_adjacent_target`, `combat_tick_sends_stats_to_victim`, `combat_tick_spectator_receives_health_bar`, `combat_tick_no_damage_when_target_out_of_melee_range`, `combat_tick_respects_interval_no_damage_before_due`, `death_sends_window_removes_victim_and_saves_at_temple`, `death_with_full_client_buffer_still_saves_at_temple`, `death_clears_attacker_fight`, `death_remove_uses_id_form_for_coocc_safety`, `tick_clears_target_when_target_logs_out`, `combat_tick_clears_fight_when_target_enters_pz`. These use `combat_map` and `wide_combat_map_with_pz` (in `test_support`).

- [ ] **Step 1: Move the 4 methods into `game/combat.rs`.**
- [ ] **Step 2: Move the 14 combat tests into `combat.rs`'s `mod tests`.**
- [ ] **Step 3: Delete moved code from `mod.rs`.**
- [ ] **Step 4: Verify** (ritual; fix `E0624`; baseline count).
- [ ] **Step 5: Commit**

```bash
git commit -am "refactor(world): extract combat behavior into game::combat"
```

---

## Task 6: Extract `containers.rs` (the container engine)

**Files:** modify `game/mod.rs`, `game/containers.rs`.

> Keep the DATA types (`InvItem`, `ContainerItem` + `wire()`, `ContainerSource`, `OpenContainer`) in `mod.rs` — they are embedded in `PlayerState`. Only behavior + container-only free helpers move.

**Free helpers to move (stay private in `containers.rs`):** `matches_source` (~173), `in_close_range` (~184).

**`impl Game` methods to move:** `restore_containers` (~690), `export_container_items` (~743), `next_free_cid` (~764), `push_open_container` (~775), `do_use_item` (~797), `close_container_tree` (~906), `close_orphaned_nested_container` (~930), `do_close_container` (~954), `do_up_arrow` (~965), `push_item_to_container` (~993), `pop_item_from_container` (~1020), `nested_dest_cid` (~1042), `rekey_container_source` (~1082), `auto_close_ground_containers` (~1097), `do_move_container` (~1669).

**`pub(super)` required (called from siblings / parent):**
- `do_use_item`, `do_close_container`, `do_up_arrow` — called by `handle`.
- `restore_containers` — called by `login` (session.rs, Task 7).
- `export_container_items` — called by `logout`/`save_all` (session.rs) and `do_death` (combat.rs, Task 5 — now a sibling).
- `rekey_container_source`, `auto_close_ground_containers` — called by `items.rs` (Task 6/7) and `movement.rs` (Task 8).
- `do_move_container` — called by `do_move_thing` (items.rs).

**Tests to move** (~5861–6136): `throwing_open_inventory_container_follows_to_ground_with_contents`, `walking_away_closes_ground_container_keeps_inventory_open`, `drop_onto_nested_bag_opened_before_parent_shift_is_not_lost`, `drop_item_onto_nested_bag_routes_inside_and_is_retrievable`, `do_use_item_on_open_container_toggles_closed`.

- [ ] **Step 1: Move the 2 free helpers + 15 methods into `game/containers.rs`** with the listed `pub(super)` markings.
- [ ] **Step 2: Move the 5 container tests into `containers.rs`'s `mod tests`.**
- [ ] **Step 3: Delete moved code from `mod.rs`.**
- [ ] **Step 4: Verify** (ritual; fix `E0624`; baseline count). Expect to confirm `pub(super)` on `export_container_items` now that `do_death` is a sibling.
- [ ] **Step 5: Commit**

```bash
git commit -am "refactor(world): extract container engine into game::containers"
```

---

## Task 7: Extract `items.rs` (ground/inventory item movement) and `session.rs`

These two are grouped because `session` depends on `containers` (already moved) and `items` depends on `containers` (already moved). Do `items.rs` first, then `session.rs`, each with its own commit.

### 7a — `items.rs`

**Files:** modify `game/mod.rs`, `game/items.rs`.

**Items to move:** `push_inventory_slot` (~1337), `item_wire_stackpos` (~1352), `take_from_ground` (~1388), `add_to_ground_front` (~1412), `do_move_thing` (~1451), `do_move_inventory` (~1535), `broadcast_dest` (~1896), `broadcast_source` (~1913).

Keep in `mod.rs` (shared): `creatures_on`, `push_cannot_move`, `materialize`, `merged_*`.

**`pub(super)` required:**
- `do_move_thing` — called by `handle`.
- `broadcast_dest`, `add_to_ground_front` — called by `do_spawn_item` (gm.rs, sibling).

**Tests to move** (~6061–6443): `do_move_thing_multi_hop_never_duplicates_including_on_tile`, `do_move_thing_from_eq_to_is_noop`, `do_move_thing_non_moveable_is_rejected_with_status_push`, `do_move_thing_out_of_reach_is_rejected`, `do_move_thing_full_move_removes_item_from_source`, `do_move_thing_stackable_split_source_keeps_remainder`, `do_move_thing_stackable_clamps_to_available`, `do_move_thing_spectator_receives_tile_update`, `do_move_thing_dest_insert_front_of_down_items`, `equip_ground_item_into_matching_slot`, `equip_into_wrong_slot_is_rejected`, `unequip_returns_item_to_the_ground`. These use `move_map`, `drain`, `has_op`, `inv_pos`, `count_sid_in_overlays` (in `test_support`).

- [ ] **Step 1: Move the 8 methods into `game/items.rs`** with the listed `pub(super)`.
- [ ] **Step 2: Move the 12 item/equip tests into `items.rs`'s `mod tests`.**
- [ ] **Step 3: Delete moved code from `mod.rs`.**
- [ ] **Step 4: Verify** (ritual; fix `E0624`; baseline count).
- [ ] **Step 5: Commit**

```bash
git commit -am "refactor(world): extract item movement into game::items"
```

### 7b — `session.rs`

**Files:** modify `game/mod.rs`, `game/session.rs`.

**Items to move:** `login` (~502), `logout` (~612), `save_all` (~655), `do_change_outfit` (~1135), `do_request_outfit` (~1157).

Keep in `mod.rs`: the `pub` DTO structs (`PlayerSnapshot`, `LoginAck`, `InitialState`, `SaveRecord`) and the spawn helpers (`free_spawn`, `free_spawn_near`, `creature_stackpos_on`, `tile_occupied`, `introduce`, `can_see`) — they are shared core.

**`pub(super)` required:** `login`, `logout`, `save_all` (called by `handle`/`spawn`/`shutdown`), `do_change_outfit`, `do_request_outfit` (called by `handle`).

**Tests to move** (~3588–5521, the session/login/outfit/save group): `login_pushes_appear_to_existing_spectator`, `second_login_sees_first_in_ack_others`, `relogin_map_description_includes_dynamic_ground_items`, `logout_pushes_remove_to_spectator`, `shutdown_and_save_persists_online_players_then_stops_actor`, `second_login_on_occupied_spawn_gets_free_tile`, `login_on_occupied_saved_position_gets_free_adjacent_tile`, `login_with_initial_position_places_player_at_that_position`, `login_with_no_position_falls_back_to_free_spawn`, `logout_with_save_tx_emits_save_record`, `save_all_emits_one_record_per_player_without_removing_them`, `save_all_with_no_save_tx_is_a_noop`, `push_to_dead_channel_reap_also_emits_save_record`, `change_outfit_updates_player_state`, `change_outfit_broadcasts_0x8e_to_player_and_spectator`, `change_outfit_unknown_id_is_noop`, `request_outfit_sends_0xc8_to_requester_only`, `request_outfit_male_gets_male_catalog`, `request_outfit_female_gets_female_catalog`, `sex_is_set_from_initial_state_on_login`, `sex_is_emitted_in_save_record_on_logout`. Uses `walk_map`, `default_initial`, `add_player`, `knight`, `wizard_outfit`, `outfit_window_looktypes` (move `wizard_outfit` to `test_support` in this task if not already there).

- [ ] **Step 1: Move the 5 methods into `game/session.rs`** with `pub(super)`.
- [ ] **Step 2: Move the ~21 session/outfit tests into `session.rs`'s `mod tests`.** Move `wizard_outfit` to `test_support` (`pub(super)`).
- [ ] **Step 3: Delete moved code from `mod.rs`.**
- [ ] **Step 4: Verify** (ritual; fix `E0624`; baseline count).
- [ ] **Step 5: Commit**

```bash
git commit -am "refactor(world): extract session lifecycle into game::session"
```

---

## Task 8: Extract `movement.rs` (largest, most map-entangled — last)

**Files:** modify `game/mod.rs`, `game/movement.rs`.

**Items to move:** `do_turn` (~1115), `resolve_vertical` (~1174), `do_teleport` (~2617), `do_move` (~2689).

**`pub(super)` required:**
- `do_turn`, `do_move` — called by `handle`.
- `do_teleport` — called by `handle` AND by all the `gm_*` teleport commands (gm.rs, sibling).

**Cross-module calls (already `pub(super)` from earlier tasks):** `rekey_container_source`, `auto_close_ground_containers` (containers.rs).

**Tests to move — including the ClientSim simulator cluster.** This is the big one. Move into `movement.rs`'s `mod tests`:
- Movement/stair/floor tests (~3221–3586): `walking_onto_a_down_stair_drops_a_floor`, `mover_is_readded_on_its_landing_when_crossing_to_underground`, `down_stair_lands_even_when_landing_is_block_solid`, `down_stair_lands_even_when_landing_is_occupied_by_creature`, `walking_off_a_raised_tile_climbs_a_floor`, `same_floor_spectator_sees_climb_as_move_not_remove`, `spectator_gets_remove_then_add_when_mover_crosses_to_underground`, `mover_forgets_creatures_that_leave_its_own_viewport`, `move_pushes_creature_move_to_spectator`, `move_out_of_view_pushes_remove_to_spectator`, `move_into_view_pushes_appear_to_spectator`, `cannot_move_onto_tile_occupied_by_creature`, `moving_across_pz_boundary_pushes_icons`.
- Underground/desync tests (~4314–5126): `underground_walk_out_and_back_keeps_floor8_consistent`, `underground_walk_east_west_keeps_full_band_consistent`, `floorchange_descend_then_ascend_1tile_diagonal_keeps_player_attached`, `floorchange_geometry_battery_reports_first_divergence`, `deeper_underground_descend_1tile_step_mover_splice_probe`, `simulator_detects_a_forced_detach`.
- The movement-only fixtures (move these from `mod.rs` into `movement.rs`'s `mod tests`, NOT `test_support`, since only movement uses them): `stair_map` may already be in `test_support` (shared with no one else now — leave it there, it's harmless), plus `underground_room`, `server_floor8_ids`, `seed_floor8`, `decode_band_into`, `apply_walk_update`, `underground_multifloor`, `server_floor8_ids_z`, `ClientSim` struct + impl, `sim_apply`, `stair_multifloor`, `seed_initial`, `first_band_mismatch`, `replay`, `deep_stair_multifloor`, `run_scenario`.

- [ ] **Step 1: Move the 4 methods into `game/movement.rs`** with `pub(super)`.
- [ ] **Step 2: Move the movement tests + the ClientSim fixture cluster into `movement.rs`'s `mod tests`.**
- [ ] **Step 3: Delete moved code from `mod.rs`.**
- [ ] **Step 4: Verify** (ritual; fix `E0624`; baseline count).
- [ ] **Step 5: Commit**

```bash
git commit -am "refactor(world): extract movement into game::movement"
```

---

## Task 9: Final sweep — confirm `mod.rs` is the thin core

**Files:** review `crates/world/src/game/mod.rs` and all child files.

- [ ] **Step 1: Confirm what remains in `mod.rs`**

Run: `rg -n '^\s*(pub(\(.*\))? )?(fn|struct|enum|impl|const) ' crates/world/src/game/mod.rs`

Expected: only the core listed in "What STAYS in `game/mod.rs`" — public API, data model, `Command`, `handle`, `WorldHandle` impl, `spawn`, `push_channel`, shared helpers, constants. No domain behavior (`do_say`, `do_move`, `gm_*`, container ops, etc.) should remain.

- [ ] **Step 2: Confirm the mod.rs `#[cfg(test)] mod tests` block is now empty or only holds core-helper tests**

The viewport/spectator/introduce tests (`spectators_within_client_viewport`, `viewport_is_asymmetric_like_tfs`, `spectators_are_the_dual_of_can_see`, `introduce_uses_full_then_short_form`, `underground_spectator_sees_within_two_floors`, `overground_viewer_sees_all_upper_floors_but_not_underground`) legitimately test core `mod.rs` helpers — they stay in `mod.rs`'s `mod tests`. Everything else should have moved.

- [ ] **Step 3: Check file sizes are now reasonable**

Run: `eza -l crates/world/src/game/`

Expected: no single file dominates the way the old 6367-line `game.rs` did; `mod.rs` is the largest but is now core + WorldHandle + spawn, not everything.

- [ ] **Step 4: Full verification**

```bash
cargo build -p world
cargo test -p world          # MUST equal the Task-0 baseline count
cargo clippy -p world -- -D warnings
cargo fmt -p world -- --check
```

- [ ] **Step 5: Final commit (only if Step 4 produced changes, e.g. fmt)**

```bash
git commit -am "refactor(world): tidy game module split"
```

---

## Self-review checklist (run before declaring the plan done)

- **Coverage:** every non-test item from the manifest has a home (Task 0 lists what stays; Tasks 2–8 list what moves). ✔
- **Tests:** all ~100 test fns are assigned to exactly one concern file; shared fixtures live in `test_support`; the ClientSim cluster lives with movement tests. ✔
- **Public paths:** all `pub` items stay in `mod.rs`, so `world::game::*` paths are preserved — no external crate edits needed. ✔
- **Visibility:** fields stay private (descendants inherit access); only cross-module-called methods get `pub(super)`; `E0624` is the catch-all. ✔
- **Behavior preservation:** no logic is rewritten; the unchanged test suite (same baseline count every task) is the contract. ✔
- **Bisectability:** every task commit builds and passes tests on its own. ✔
