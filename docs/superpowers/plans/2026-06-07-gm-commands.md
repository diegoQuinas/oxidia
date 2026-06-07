# Gamemaster Commands Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Give the "God Diego" character in-game chat commands to spawn items (`/item`) and teleport creatures (`/goto`, `/teleport`, `/teleportto`, `/bring`).

**Architecture:** Chat text starting with `/` is forwarded verbatim from the network reader loop to the world actor via a new `Command::GmCommand`. The actor owns the security gate (`PlayerState.gamemaster`), the parser, and dispatch. All five commands reduce to two primitives — `do_teleport(id, pos)` and `do_spawn_item(gm_id, pos, server_id, count)` — plus a `find_player_by_name` helper. Feedback to the GM goes through the existing `push_status_message` (`0xB4` MESSAGE_STATUS_SMALL).

**Tech Stack:** Rust, tokio actor (mpsc), custom Tibia 10.98 protocol crate.

**Testing note:** This project's convention (project memory: "No tests until manual validation") overrides the skill's TDD-first default. Tasks 1–5 implement and are validated **manually** by logging in as `diego`. Unit tests are Task 6, to be done only after manual validation passes.

**Reference patterns in the codebase (read before starting):**
- `crates/world/src/game.rs:1277` `do_move` — the spectator remove/add loop and the mover's own-view rebuild that `do_teleport` mirrors.
- `crates/world/src/game.rs:754` `do_move_thing` — the destination-insert + `broadcast_dest` path that `do_spawn_item` mirrors.
- `crates/world/src/game.rs:1000` `push_status_message` — the `0xB4` feedback helper.
- `crates/protocol/src/walk.rs:204` — why a teleport must send `remove_creature_by_id` + full `0x64` map (the incremental `0x6D` only shifts one tile and would desync a long jump).

---

## Task 1: Designate "God Diego" as gamemaster (with GM outfit)

The `gamemaster` flag already flows from login into `PlayerState`. We force it true when the character name is `diego`, and additionally give any gamemaster the classic Gamemaster outfit (looktype 75 — fixed across all Tibia versions including 10.98).

**Files:**
- Modify: `crates/server/src/game_service.rs` — looktype constant (near `knight_outfit`, :19) and the GM assignment (:234).

- [ ] **Step 1: Add the GM looktype constant**

Near `knight_outfit` (game_service.rs:19), add:

```rust
/// Classic Gamemaster outfit sprite id (TFS/Tibia `looktype 75`, stable across
/// versions). Gamemasters are forced into this look so they are visibly GMs.
const GM_LOOKTYPE: u16 = 75;
```

- [ ] **Step 2: Force GM by name + GM outfit**

Replace line 234:

```rust
    initial.gamemaster = req.gamemaster;
```

with:

```rust
    // "God Diego" is always a gamemaster, regardless of the login packet flag.
    // Hardcoded by name (no DB schema for access levels yet — see the GM design spec).
    initial.gamemaster = req.gamemaster || name.eq_ignore_ascii_case("diego");
    if initial.gamemaster {
        // Force the visible Gamemaster outfit (looktype 75). Colors are irrelevant
        // for this fixed-sprite outfit; only look_type matters on the wire.
        initial.outfit.look_type = GM_LOOKTYPE;
    }
```

- [ ] **Step 3: Build**

Run: `cargo build -p server`
Expected: compiles clean.

- [ ] **Step 4: Manual validation**

Log in as `diego`. Expected: the character renders as the Gamemaster outfit (looktype 75), visible to the GM and to any second observing client. Log in as `test` → normal outfit unchanged.

- [ ] **Step 5: Commit**

```bash
git add crates/server/src/game_service.rs
git commit -m "feat(gm): mark 'diego' as gamemaster and force the GM outfit (looktype 75)"
```

---

## Task 2: Command plumbing — `Command::GmCommand`, handle, network hook, gate + parser skeleton

Wire the full path end-to-end with an "unknown command" responder, so we can confirm the gate and feedback work before adding real subcommands.

**Files:**
- Modify: `crates/world/src/game.rs` — `Command` enum (`:1448`), `handle` (`:306`), `WorldHandle` methods (`:1482`), new `do_gm_command`.
- Modify: `crates/server/src/game_service.rs` — reader loop say branch (`:346`).

- [ ] **Step 1: Add the `Command` variant**

In the `enum Command { ... }` block (game.rs:1448), after the `MoveThing` variant, add:

```rust
    /// Chat text beginning with `/` from a player. The actor gates on
    /// `PlayerState.gamemaster`, parses the verb, and dispatches to a GM primitive.
    GmCommand { id: u32, text: String },
```

- [ ] **Step 2: Route it in `handle`**

In `fn handle` (game.rs:306), after the `Command::MoveThing { .. } => ...` arm, add:

```rust
            Command::GmCommand { id, text } => self.do_gm_command(id, text),
```

- [ ] **Step 3: Add the `WorldHandle` method**

In `impl WorldHandle` (game.rs), after `move_thing` (around :1550), add:

```rust
    /// Forward a `/`-prefixed chat line to the world as a GM command. The actor
    /// validates that the sender is a gamemaster before doing anything.
    /// Fire-and-forget; feedback is pushed to the sender as a `0xB4` message.
    pub async fn gm_command(&self, id: u32, text: String) {
        let _ = self.tx.send(Command::GmCommand { id, text }).await;
    }
```

- [ ] **Step 4: Add `do_gm_command` (gate + parse + dispatch skeleton)**

In `impl Game`, near the other `do_*` handlers (e.g. just below `do_set_target`), add:

```rust
    /// Gate + parse + dispatch for `/`-prefixed GM commands. Non-gamemasters are
    /// silently ignored (their `/` line is simply dropped). Every parse/lookup
    /// failure replies to the sender via `push_status_message` and leaves the
    /// world untouched — no panics, no partial state.
    fn do_gm_command(&mut self, id: u32, text: String) {
        if !self.players.get(&id).map(|p| p.gamemaster).unwrap_or(false) {
            return; // not a GM: drop silently
        }
        let line = text.trim_start_matches('/').trim();
        let mut parts = line.split_whitespace();
        let Some(verb) = parts.next() else { return };
        let args: Vec<&str> = parts.collect();
        match verb {
            // real subcommands are added in Tasks 3–5
            other => self.push_status_message(
                id,
                format!("Unknown command: /{other}").as_bytes(),
            ),
        }
    }
```

- [ ] **Step 5: Hook the network reader loop**

In `game_service.rs`, the `OPCODE_CLIENT_SAY` branch (around :350), replace:

```rust
                if let Some((speak_type, text)) = chat::parse_say(&payload[1..]) {
                    world.say(id, speak_type, text).await;
                }
                continue;
```

with:

```rust
                if let Some((speak_type, text)) = chat::parse_say(&payload[1..]) {
                    // GM commands are chat lines beginning with '/'. The world
                    // actor owns the gamemaster gate; the network layer never trusts.
                    if text.starts_with('/') {
                        world.gm_command(id, text).await;
                    } else {
                        world.say(id, speak_type, text).await;
                    }
                }
                continue;
```

- [ ] **Step 6: Build**

Run: `cargo build -p world -p server`
Expected: compiles clean. The `match verb` has only the catch-all arm for now — that is intentional and warning-free.

- [ ] **Step 7: Manual validation**

Run the server, log in as `diego`, type `/foo` in chat.
Expected: a status message "Unknown command: /foo" appears. Log in as a non-GM (`test`), type `/foo` → nothing happens (no message, no crash).

- [ ] **Step 8: Commit**

```bash
git add crates/world/src/game.rs crates/server/src/game_service.rs
git commit -m "feat(gm): route /-prefixed chat to a gated GM command dispatcher"
```

---

## Task 3: `/item <id> [count]` — spawn an item on the GM's tile

**Files:**
- Modify: `crates/world/src/game.rs` — `do_gm_command` match, new `gm_item` + `do_spawn_item`.

- [ ] **Step 1: Add the `do_spawn_item` primitive**

In `impl Game`, near `do_move_thing`, add:

```rust
    /// Place a fresh item on `pos` and broadcast a `0x6A` add to spectators.
    /// Mirrors the destination half of `do_move_thing`: materialize the tile,
    /// insert at the front of the down-items (newest on top), broadcast at the
    /// top down-item stackpos. Replies to `gm_id` on success or failure.
    fn do_spawn_item(&mut self, gm_id: u32, pos: Position, server_id: u16, count: u16) {
        let Some(meta) = self.map.item_meta(server_id) else {
            self.push_status_message(gm_id, format!("Unknown item id {server_id}.").as_bytes());
            return;
        };
        let client_id = meta.client_id;
        let animated = meta.animated;
        let stackable = meta.stackable;

        if !self.materialize(pos) {
            self.push_status_message(gm_id, b"You cannot create an item there.");
            return;
        }
        // TFS 10-thing-per-tile cap.
        let len = self.dynamic.get(&(pos.x, pos.y, pos.z)).map(|st| st.items.len()).unwrap_or(0);
        if len >= 10 {
            self.push_status_message(gm_id, b"This tile is full.");
            return;
        }

        let subtype = if stackable { Some(count.clamp(1, 100) as u8) } else { None };
        let wi = WireItem { client_id, subtype, animated };

        // creatures_on borrows &self immutably; compute before the &mut get_mut.
        let dest_creatures = self.creatures_on(pos).len();
        {
            let st = self.dynamic.get_mut(&(pos.x, pos.y, pos.z)).unwrap();
            let front = st.pre_creature_len; // first down-item slot
            st.items.insert(front, wi);
            st.server_ids.insert(front, server_id);
            st.counts.insert(front, subtype);
        }
        let front = self.dynamic.get(&(pos.x, pos.y, pos.z)).map(|st| st.pre_creature_len).unwrap_or(0);
        let dest_s = (front + dest_creatures).min(9) as u8;
        self.broadcast_dest(pos, dest_s, wi, false);

        self.push_status_message(gm_id, format!("Created item {server_id}.").as_bytes());
    }
```

- [ ] **Step 2: Add the `gm_item` parser wrapper**

In `impl Game`, near `do_gm_command`, add:

```rust
    /// `/item <server_id> [count]` — spawn an item on the GM's own tile.
    fn gm_item(&mut self, id: u32, args: &[&str]) {
        let Some(server_id) = args.first().and_then(|s| s.parse::<u16>().ok()) else {
            self.push_status_message(id, b"Usage: /item <id> [count]");
            return;
        };
        let count = args.get(1).and_then(|s| s.parse::<u16>().ok()).unwrap_or(1);
        let Some(pos) = self.players.get(&id).map(|p| p.position) else { return };
        self.do_spawn_item(id, pos, server_id, count);
    }
```

- [ ] **Step 3: Wire it into the dispatcher**

In `do_gm_command`'s `match verb`, add the arm above the catch-all:

```rust
            "item" => self.gm_item(id, &args),
```

- [ ] **Step 4: Build**

Run: `cargo build -p world`
Expected: compiles clean.

- [ ] **Step 5: Manual validation**

Log in as `diego`. Type `/item 2400` (a magic sword server id, or any valid id from your items.xml). Expected: the item appears on your tile for you AND a second observing client; status message "Created item 2400.". Try `/item 99999` → "Unknown item id 99999.". Try `/item` → usage message. Try a stackable like `/item <gold_id> 50` → a stack of 50 appears.

- [ ] **Step 6: Commit**

```bash
git add crates/world/src/game.rs
git commit -m "feat(gm): /item command spawns an item on the GM tile"
```

---

## Task 4: `do_teleport` primitive + `/goto <x> <y> <z>`

**Files:**
- Modify: `crates/world/src/game.rs` — new `do_teleport`, free `parse_pos`, `gm_goto`, dispatcher arm.

- [ ] **Step 1: Add the `do_teleport` primitive**

In `impl Game`, near `do_move`, add. This mirrors `do_move`'s spectator loop and own-view rebuild, minus the walkability check, and ALWAYS sends the mover a clean `remove + full 0x64` (never the incremental `0x6D`, which only shifts one tile):

```rust
    /// Relocate creature `id` to `to`, bypassing walkability. Spectators get a
    /// clean remove/add (a teleport can span any distance, so the incremental
    /// `0x6D` move is never used). The mover gets `remove_creature_by_id` + a full
    /// `0x64` map centered on the landing tile, which carries the landing position
    /// explicitly. Mirrors `do_move` + the teleport branch of `walk::walk_update`.
    fn do_teleport(&mut self, id: u32, to: Position) {
        let from = match self.players.get(&id) {
            Some(p) => p.position,
            None => return,
        };
        if from == to { return; }
        if let Some(p) = self.players.get_mut(&id) { p.position = to; }

        // PZ badge: resend icons if we crossed a protection-zone boundary.
        if self.map.is_protection_zone(from) != self.map.is_protection_zone(to) {
            let mask = if self.map.is_protection_zone(to) { enter_world::ICON_PIGEON } else { 0 };
            self.push(id, enter_world::icons(mask));
        }

        // Spectators of either endpoint: clean remove/add.
        let mut seen: HashSet<u32> = HashSet::new();
        for s in self.spectators(from, id) { seen.insert(s); }
        for s in self.spectators(to, id) { seen.insert(s); }
        for spec in seen {
            let Some(svpos) = self.players.get(&spec).map(|p| p.position) else { continue };
            let sees_from = Self::can_see(svpos, from);
            let sees_to = Self::can_see(svpos, to);
            if sees_to {
                if sees_from {
                    self.push(spec, walk::remove_creature_by_id(id));
                    if let Some(s) = self.players.get_mut(&spec) { s.known.remove(&id); }
                }
                if let Some(bytes) = self.introduce(spec, id) {
                    let sp = self.creature_stackpos_on(to, id);
                    self.push(spec, tile_creature::add_tile_creature((to.x, to.y, to.z), sp, &bytes));
                }
            } else if sees_from {
                self.push(spec, walk::remove_creature_by_id(id));
                if let Some(s) = self.players.get_mut(&spec) { s.known.remove(&id); }
            }
        }

        // Prune the mover's known-set of creatures no longer in view.
        let left_view: Vec<u32> = self.visible_from(from, id).into_iter()
            .filter(|oid| self.players.get(oid).is_some_and(|p| !Self::can_see(to, p.position)))
            .collect();
        for oid in left_view {
            if let Some(mover) = self.players.get_mut(&id) { mover.known.remove(&oid); }
        }

        // Mover's own view: full 0x64 carrying every in-range player plus self.
        // Build creatures (introduce = &mut self) BEFORE borrowing self.merged().
        let mut wire_creatures: Vec<PlacedCreature> = self.visible_from(to, id).into_iter()
            .filter_map(|oid| {
                let opos = self.players.get(&oid)?.position;
                let bytes = self.introduce(id, oid)?;
                Some(PlacedCreature { x: opos.x, y: opos.y, z: opos.z, bytes })
            })
            .collect();
        if let Some(bytes) = self.introduce(id, id) {
            wire_creatures.push(PlacedCreature { x: to.x, y: to.y, z: to.z, bytes });
        }
        let mut pkt = walk::remove_creature_by_id(id);
        {
            let merged = self.merged();
            pkt.extend(protocol::map_description::encode(
                protocol::map_description::Center { x: to.x, y: to.y, z: to.z },
                &merged,
                &wire_creatures,
            ));
        }
        self.push(id, pkt);
    }
```

- [ ] **Step 2: Add a free `parse_pos` helper**

At module scope (e.g. just above `enum Command`), add:

```rust
/// Parse `<x> <y> <z>` from the front of a GM command's args. `None` if any
/// coordinate is missing or out of range.
fn parse_pos(args: &[&str]) -> Option<Position> {
    let x = args.first()?.parse::<u16>().ok()?;
    let y = args.get(1)?.parse::<u16>().ok()?;
    let z = args.get(2)?.parse::<u8>().ok()?;
    Some(Position::new(x, y, z))
}
```

- [ ] **Step 3: Add `gm_goto`**

In `impl Game`, near the other `gm_*` wrappers:

```rust
    /// `/goto <x> <y> <z>` — teleport the GM to a position.
    fn gm_goto(&mut self, id: u32, args: &[&str]) {
        let Some(pos) = parse_pos(args) else {
            self.push_status_message(id, b"Usage: /goto <x> <y> <z>");
            return;
        };
        if !self.map.has_ground(pos) {
            self.push_status_message(id, b"There is no tile there.");
            return;
        }
        self.do_teleport(id, pos);
        self.push_status_message(id, format!("Teleported to {}, {}, {}.", pos.x, pos.y, pos.z).as_bytes());
    }
```

- [ ] **Step 4: Wire it into the dispatcher**

In `do_gm_command`'s `match verb`, add above the catch-all:

```rust
            "goto" => self.gm_goto(id, &args),
```

- [ ] **Step 5: Build**

Run: `cargo build -p world`
Expected: compiles clean.

- [ ] **Step 6: Manual validation**

Log in as `diego`. Note your current position (use look-at on yourself, which shows position for GMs). Type `/goto <x> <y> <z>` to a known nearby tile, then to a far tile, then to a different floor (e.g. a known underground temple). Expected each time: your client recenters on the new tile, you can walk normally afterward (no "unable to remove creature" desync), and a second observing client sees you vanish from the old spot and appear at the new one if in view. Try `/goto 0 0 0` (no ground) → "There is no tile there.". Try `/goto 100` → usage message.

- [ ] **Step 7: Commit**

```bash
git add crates/world/src/game.rs
git commit -m "feat(gm): do_teleport primitive + /goto self-teleport"
```

---

## Task 5: `/teleport`, `/teleportto`, `/bring` + `find_player_by_name`

**Files:**
- Modify: `crates/world/src/game.rs` — `find_player_by_name`, three `gm_*` wrappers, dispatcher arms.

- [ ] **Step 1: Add `find_player_by_name`**

In `impl Game`:

```rust
    /// Find an online player's creature id by name (case-insensitive).
    fn find_player_by_name(&self, name: &str) -> Option<u32> {
        self.players.iter()
            .find(|(_, p)| p.name.eq_ignore_ascii_case(name))
            .map(|(&id, _)| id)
    }
```

- [ ] **Step 2: Add the three wrappers**

In `impl Game`, near the other `gm_*` wrappers:

```rust
    /// `/teleport <name> <x> <y> <z>` — teleport another player to a position.
    fn gm_teleport(&mut self, id: u32, args: &[&str]) {
        let Some(name) = args.first() else {
            self.push_status_message(id, b"Usage: /teleport <name> <x> <y> <z>");
            return;
        };
        let Some(pos) = parse_pos(&args[1..]) else {
            self.push_status_message(id, b"Usage: /teleport <name> <x> <y> <z>");
            return;
        };
        let Some(target) = self.find_player_by_name(name) else {
            self.push_status_message(id, format!("Player '{name}' not found.").as_bytes());
            return;
        };
        if !self.map.has_ground(pos) {
            self.push_status_message(id, b"There is no tile there.");
            return;
        }
        self.do_teleport(target, pos);
        self.push_status_message(id, format!("Teleported {} to {}, {}, {}.", name, pos.x, pos.y, pos.z).as_bytes());
    }

    /// `/teleportto <name>` — teleport the GM to another player's tile.
    fn gm_teleportto(&mut self, id: u32, args: &[&str]) {
        let Some(name) = args.first() else {
            self.push_status_message(id, b"Usage: /teleportto <name>");
            return;
        };
        let Some(target) = self.find_player_by_name(name) else {
            self.push_status_message(id, format!("Player '{name}' not found.").as_bytes());
            return;
        };
        let Some(pos) = self.players.get(&target).map(|p| p.position) else { return };
        self.do_teleport(id, pos);
        self.push_status_message(id, format!("Teleported to {name}.").as_bytes());
    }

    /// `/bring <name>` — teleport another player to the GM's tile.
    fn gm_bring(&mut self, id: u32, args: &[&str]) {
        let Some(name) = args.first() else {
            self.push_status_message(id, b"Usage: /bring <name>");
            return;
        };
        let Some(target) = self.find_player_by_name(name) else {
            self.push_status_message(id, format!("Player '{name}' not found.").as_bytes());
            return;
        };
        let Some(pos) = self.players.get(&id).map(|p| p.position) else { return };
        self.do_teleport(target, pos);
        self.push_status_message(id, format!("Brought {name} to you.").as_bytes());
    }
```

- [ ] **Step 3: Wire them into the dispatcher**

In `do_gm_command`'s `match verb`, add above the catch-all:

```rust
            "teleport" => self.gm_teleport(id, &args),
            "teleportto" => self.gm_teleportto(id, &args),
            "bring" => self.gm_bring(id, &args),
```

- [ ] **Step 4: Build**

Run: `cargo build -p world`
Expected: compiles clean.

- [ ] **Step 5: Manual validation (needs two clients)**

Log in `diego` (GM) and `test` (target) on two clients.
- `diego` types `/teleportto test` → diego appears on/next to test's tile.
- `diego` types `/bring test` → test is yanked to diego's tile; test's client recenters correctly and can walk afterward.
- `diego` types `/teleport test <x> <y> <z>` → test is moved to those coords.
- `diego` types `/teleport ghost 100 100 7` → "Player 'ghost' not found.".

- [ ] **Step 6: Commit**

```bash
git add crates/world/src/game.rs
git commit -m "feat(gm): /teleport, /teleportto, /bring player-targeted teleports"
```

---

## Task 6 (after manual validation passes): unit tests

Per project convention, write these **only after** Tasks 1–5 are manually validated. Cover the pure logic that does not need a live socket: the arg parser and player lookup.

**Files:**
- Modify: `crates/world/src/game.rs` — `#[cfg(test)] mod tests` (existing module if present, else add one).

- [ ] **Step 1: Test `parse_pos`**

```rust
    #[test]
    fn parse_pos_reads_three_coords() {
        assert_eq!(parse_pos(&["100", "200", "7"]), Some(Position::new(100, 200, 7)));
    }

    #[test]
    fn parse_pos_rejects_short_or_bad_input() {
        assert_eq!(parse_pos(&["100", "200"]), None);
        assert_eq!(parse_pos(&["x", "200", "7"]), None);
        assert_eq!(parse_pos(&[]), None);
    }
```

- [ ] **Step 2: Test `find_player_by_name` is case-insensitive**

Build a `Game` (use `Game::new_seeded(map, 0)` per the existing test pattern at game.rs:171), log in a player named "Diego" through the existing test login helper, then:

```rust
    // (inside a test that has registered a player named "Diego" with id `pid`)
    assert_eq!(game.find_player_by_name("diego"), Some(pid));
    assert_eq!(game.find_player_by_name("DIEGO"), Some(pid));
    assert_eq!(game.find_player_by_name("nobody"), None);
```

Match the exact login/registration helper the existing tests use — read the `#[cfg(test)] mod tests` block in `game.rs` first and follow its setup pattern rather than inventing one.

- [ ] **Step 3: Run tests**

Run: `cargo test -p world`
Expected: all pass.

- [ ] **Step 4: Commit**

```bash
git add crates/world/src/game.rs
git commit -m "test(gm): cover parse_pos and find_player_by_name"
```

---

## Self-review checklist (done by the plan author)

- **Spec coverage:** `/item` → Task 3; `/goto` → Task 4; `/teleport`+`/teleportto`+`/bring` → Task 5; GM-by-name → Task 1; parse-in-actor + gate → Task 2; `0xB4` feedback → used in every command; the two primitives → Tasks 3 & 4; occupancy simplification (land on exact tile) → `do_teleport` lands on `to` with no adjacent search, as the spec's v1 simplification states. ✓
- **Type consistency:** `do_teleport(id, to)`, `do_spawn_item(gm_id, pos, server_id, count)`, `find_player_by_name(&str) -> Option<u32>`, `parse_pos(&[&str]) -> Option<Position>` are referenced identically across all call sites. ✓
- **No placeholders:** every step ships real code or a real command. The `match verb` grows one arm per task — intentional and noted, not a placeholder. ✓
