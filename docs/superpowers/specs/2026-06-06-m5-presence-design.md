# M5 — Multiplayer Presence — Design

> Oxidia, the from-scratch idiomatic Rust Open Tibia server. Protocol **10.98**,
> client **OTClient Redemption**. TFS 1.4.2 (`reference/tfs/`) is a **spec
> reference only** — verified, never ported line by line.

M5 is the keystone of Phase A: the moment a single-player demo becomes a server
your friends play on. You see each other walk, turn, log in, and log out **in
real time**.

## Scope (locked)

**Full presence.** All of:

- **Login** — a joining player appears (`0x61`) to everyone who can see their
  tile; their enter-world view already contains the players already in range.
- **Walk** — every spectator sees other players step (`0x6D` move), or appear /
  disappear as they cross a viewport edge.
- **Turn** — spectators see other players change facing (`0x6B`).
- **Logout / disconnect** — the leaving creature is removed (`0x6C`) for everyone
  who could see it.
- **Viewport in / out** — crossing into another player's view triggers an appear;
  crossing out triggers a remove.

Out of scope (later milestones): chat (M6), combat/HP sync (M7), persistence
(M8), ground-item rendering (M9).

## The core problem M5 solves

The actor is **request/response only** today. Every `Command` carries a
`oneshot` reply; the session reads the reply and writes it to its own socket.
There is **no path for the actor to push an unsolicited packet** to a session —
which is exactly what "see your friend walk" requires. M5 builds that path, once,
correctly, because every social and combat milestone wires into it.

## Architecture decision (locked) — evaluated against TFS

The model is **unified push + greedy coalescing + non-blocking actor**, chosen
after verifying TFS 1.4.2's real implementation and improving on it in three
places. The full TFS comparison and rationale live in
[engram `sdd/m5-presence/design`] and are summarized in `README.md`
("Why Rust over a C++ port"). The short version:

| Concern | TFS 1.4.2 (verified) | Oxidia M5 | Why ours is better / equal |
|---|---|---|---|
| Outbound path | Per-connection `OutputMessage` buffer, flushed every **10 ms** by `sendAll()` on the dispatcher thread (`outputmessage.cpp:25-38`) | Per-session **writer task** draining an `mpsc<Vec<u8>>`, **greedy drain** (`recv().await` then `try_recv()` loop), concatenate, encrypt once, one frame | Same batching under load; **zero added latency** at low load; no timer subsystem |
| Slow client | Unbounded per-connection queue grows (`connection.h:98`) | **Bounded** channel, actor uses `try_send`; on `Full` → kick the session | Protects the single-writer loop **and** removes an unbounded-memory DoS vector |
| Game logic | Single-threaded dispatcher, no locks | Single tokio actor, no locks | Equal — both correct by construction |
| Known-creatures | `unordered_set` per `ProtocolGame` connection (`protocolgame.h:305`) | `HashSet<u32>` in the actor's `PlayerState` | The actor is the **sole** packet builder, so it owns the `0x61`/`0x62` choice — no split state |
| Spectators | Quadtree leaf nodes 8×8, sectored-grid query (`map.cpp:330-384`); cache cleared on **every** add/remove | **Iterate-all-players** behind a `spectators(pos)` interface | YAGNI at friends-scale; TFS's own cache thrashes; interface lets us swap in a quadtree later untouched |

### Crypto placement

The `mpsc` channel carries **plaintext** `Vec<u8>` payloads. The per-session
writer task owns that session's XTEA round keys and encrypts **once per flushed
batch**, then writes the framed message. The actor never touches per-session
crypto. (TFS likewise encrypts once per flush in `Protocol::onSendMessage`,
`protocol.cpp:47-57`.)

### Why coalescing multiple game packets into one XTEA frame is valid

A Tibia "message" is one XTEA-encrypted body that the client decrypts whole and
parses as a sequence of opcodes. Concatenating several game-layer payloads into
one body is exactly what TFS does when it coalesces into a single
`OutputMessage`. Our greedy drain reproduces that without a fixed tick.

## Components

### 1. Actor — `crates/world/src/game.rs`

**`PlayerState`** gains two fields:

```rust
struct PlayerState {
    name: String,
    position: Position,
    direction: Direction,
    push_tx: mpsc::Sender<Vec<u8>>, // plaintext payloads to this session's writer task
    known: HashSet<u32>,            // creature ids already introduced to this client
}
```

**`Command`** changes from pure request/response to push-driven:

```rust
enum Command {
    Login { name: String, push_tx: mpsc::Sender<Vec<u8>>, reply: oneshot::Sender<LoginAck> },
    Logout { id: u32 },                      // fire-and-forget
    Move { id: u32, direction: Direction },  // no reply — actor pushes the result
    Turn { id: u32, direction: Direction },  // no reply — actor pushes the result
}
```

`Login` still replies once — the joining session needs its own creature id and
spawn snapshot to build the enter-world burst. After that, everything is push.

**New behavior in the actor loop:**

- `spectators(pos: Position) -> Vec<u32>` — iterate `players`, keep those whose
  position is within client viewport range (`±8` x, `±6` y) on the same floor.
  This is the **only** place visibility is computed; callers never inline a range
  check. Swappable to a sectored-grid/quadtree later without touching callers.
- `push(&mut self, id, payload)` — look up the player's `push_tx`, `try_send`.
  On `Err(TrySendError::Full | Closed)` mark the session dead and schedule
  `Logout`. Never `.await` on a send — the game loop must not block on one client.
- `introduce(viewer_id, target) -> Vec<u8>` — pick `0x61` (full) vs `0x62`
  (short) by consulting and updating the viewer's `known` set; emit the bytes.

### 2. Session — `crates/server/src/game_service.rs`

On entering the game (post-login):

1. `let (rd, wr) = tokio::io::split(stream);`
2. Create `let (push_tx, push_rx) = mpsc::channel::<Vec<u8>>(CAP);`
3. Spawn the **writer task** owning `wr` + round keys + `push_rx`:
   - `select!` between `push_rx.recv()` and a **ping ticker**.
   - On a payload: drain `push_rx.try_recv()` greedily, concatenate, encrypt the
     batch once, write one frame.
   - On ticker: write a ping.
   - On channel closed: flush remaining, then exit (session ended).
4. Send `Command::Login { name, push_tx, reply }`, await the ack, build and push
   the enter-world burst (now including in-range players via the existing
   `walk_update` / map-description creature slice).
5. The **read loop** becomes fire-and-forget: read frame → decrypt → decode
   opcode → send `Command::Move/Turn` to the actor. No reply handling.
6. When the read loop returns (EOF / error / kicked): send `Command::Logout`,
   drop `push_tx` (closes the writer task).

### 3. Protocol — `crates/protocol/src/remove_creature.rs` (new)

`remove_creature(pos, stackpos) -> Vec<u8>` → opcode `0x6C` + map position +
stackpos, matching the OTClient-faithful layout. Byte-faithful round-trip test
against a decoder, same pattern as the existing `walk.rs` packets. Add the `mod`
to `protocol/src/lib.rs` (append-only).

## Data flow

### Login

```
session: split stream → spawn writer → Command::Login{push_tx} → ack(id, spawn)
actor:   insert PlayerState; for each s in spectators(spawn): push(s, introduce(s, newPlayer))
session: build enter-world burst incl. spectators(spawn) creatures → push to self
```

### Walk (the heart of M5)

```
session: Command::Move{id, dir}
actor:   validate against map collision (unchanged from M4)
         if blocked: push(id, cancel_walk(dir)); done
         from, to = old/new pos
         specs = union(spectators(from), spectators(to))
         for s in specs (excluding the mover):
             sees_from, sees_to = can_see(s, from), can_see(s, to)
             both        → push(s, creature_move(id, to))            // 0x6D
             only to     → push(s, introduce(s, mover) at to)        // appear 0x61/0x62
             only from   → push(s, remove_creature(from, stackpos))  // 0x6C
         push(id, self move 0x6D + revealed slice WITH other creatures in new tiles)
```

### Turn

```
session: Command::Turn{id, dir}
actor:   update facing; for s in spectators(pos): push(s, creature_turn(id, dir)) // 0x6B
         push(id, creature_turn(id, dir))   // mover sees own turn
```

### Logout / disconnect

```
read loop returns → Command::Logout{id}
actor:  pos = player.position; remove from registry
        for s in spectators(pos): push(s, remove_creature(pos, stackpos)) // 0x6C
        (dropping PlayerState drops push_tx → writer task exits)
```

## Error handling

- **Slow client** — `try_send` returns `Full` → mark dead, schedule `Logout`,
  kick. The game loop never blocks.
- **Abrupt disconnect** — read loop errors → `Logout`. Spectators get `0x6C`.
- **Closed push channel** — treat as a dead session: `Logout`.
- **Bad opcode** — unchanged from M4 (logged, ignored), never panics the actor.
- `#![forbid(unsafe_code)]` and strict clippy stay green.

## Testing strategy (TDD, subagent-driven — the M4 pattern)

Pure, actor-free pieces (independently verifiable):

- `remove_creature` `0x6C` byte-faithful round-trip.
- `spectators(pos)` — in/out of range on the same floor, different floor excluded,
  exact viewport edges (`±8` / `±6`).
- Form selection — first sighting yields `0x61` and inserts into `known`; second
  yields `0x62`; a removed-then-reseen creature yields `0x61` again.
- Walk transition matrix — `both → move`, `only-to → appear`, `only-from →
  remove` for a spectator at a fixed position as another creature steps.

Actor wiring:

- Login pushes an appear to an in-range existing player and vice versa.
- Move pushes the correct packet to each of two spectators in different
  visibility relations.
- Logout pushes `0x6C` to remaining spectators.

**Live acceptance (gate):** two OTClient sessions connected to one server — each
sees the other spawn, walk, turn, and disappear on logout, with no desync.

## Files touched

| Worktree role | File | Change |
|---|---|---|
| spine | `crates/world/src/game.rs` | `Command` enum, `PlayerState` fields, `spectators`, `push`, `introduce`, broadcast logic |
| spine | `crates/server/src/game_service.rs` | split stream, writer task, fire-and-forget read loop, login/logout wiring |
| feeder | `crates/protocol/src/remove_creature.rs` (new) | `0x6C` encoder + round-trip test |
| feeder | `crates/protocol/src/lib.rs` | append `mod remove_creature;` |
| docs | `README.md` | "Why Rust over a C++ port" — the 3 verified wins |
| docs | `PROGRESS.md` | M5 status (spine records live acceptance) |

## Out of scope / deferred (YAGNI)

- **Quadtree / sectored-grid spectators** — deferred behind the `spectators()`
  interface until profiling at real player counts demands it.
- **Known-set eviction (cap 1300)** — never triggers at friends-scale; the cap +
  TFS-style out-of-viewport eviction is a trivial later addition.
- **Multi-floor spectator band** — M5 broadcasts same-floor; the floor band
  (`±2` underground) lands when it visibly matters.

## Addendum (post-implementation): the stackpos invariant

The final holistic review surfaced a desync the unified-push design glossed over:
`0x6A`/`0x6C` carry **position + stackpos** (no id-form for *add*), while `0x6D`/`0x6B`
use the id-form. `StaticMap::creature_stackpos` is a *static* per-tile value, so two
creatures sharing a tile would collide on add/remove → "no thing at pos" desync
(the ISSUE-1b class of bug). A "creature-aware stackpos" fix is fragile because the
id-form move lets the client choose stack order on the destination tile.

Resolution (implemented): keep **≤1 creature per tile** so the static stackpos is
always correct. Two additions, both authentic Tibia behavior:
1. **Creature collision** — `do_move` rejects a destination occupied by another
   creature (blocked → `cancel_walk`).
2. **Free-tile login placement** — `Game::free_spawn()` places a joining player on
   the nearest walkable, unoccupied tile when the temple spawn is taken.

Under this invariant (and with no teleport/summon in M5, plus logins serialized by
the single actor) co-occupancy has no path, so every `0x6A`/`0x6C` stackpos is
correct. This is a small, intended scope addition over the original design.
