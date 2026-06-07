# M6 — Chat (say / whisper / yell) — Design

> Oxidia, the from-scratch idiomatic Rust Open Tibia server. Protocol **10.98**,
> client **OTClient Redemption**. TFS 1.4.2 (`reference/tfs/`) is a **spec
> reference only** — verified, never ported line by line.

M6 is the social glue of Phase A: players talk to each other in the game world.
It is cheap because the M5 spectator system already exists — chat is "compute who
hears it, push the speech packet," the same shape as movement broadcast.

## Scope (locked)

**TFS-faithful local chat**: `say` / `whisper` / `yell`, position-based, with the
exact TFS semantics. The roadmap's "default channel" means **local positional
speech** (the OTClient Local Chat tab renders position-based `0xAA` packets with
no channel handshake) — NOT a joinable channel system.

Out of scope (later milestones): joinable channels (`0x97`/`0x98`/`0xAB`/`0xAC`,
channel-id `0xAA`), private messages (`TALKTYPE_PRIVATE_*`), yell cooldown,
multi-floor voice, NPC/monster speech.

## Wire protocol (verified in `reference/tfs/`)

### Inbound — client says (`0x96`, `parseSay`, `protocolgame.cpp:922-951`)

For `say`/`whisper`/`yell` the body after the opcode is:

```
[0x96]            opcode        u8
[type]            speak type    u8   (1=SAY, 2=WHISPER, 3=YELL)
[len u16][bytes]  message       u16-LE-prefixed string
```

`TALKTYPE_PRIVATE_TO`(5)/`PRIVATE_RED_TO`(16) carry a receiver-name string and
`CHANNEL_Y`(7)/`CHANNEL_R1`(14) carry a `channelId u16` before the message — M6
does **not** support these; the parser returns `None` and the reader drains them.

### Outbound — creature says (`0xAA`, `sendCreatureSay`, `protocolgame.cpp:2199-2225`)

```
[0xAA]            opcode        u8
[stmt_id]         statement id  u32 LE   (monotonic counter; any value works)
[len u16][bytes]  speaker name  string
[level]           speaker level u16 LE   (M6 sends 1; real level at M14)
[type]            speak type    u8       (1=SAY, 2=WHISPER, 3=YELL)
[x u16][y u16][z u8]  position  speaker's tile
[len u16][bytes]  message       string
```

(`sendToChannel` swaps the position for a `channelId u16`; M6 never uses it.)

## Semantics (verified in `game.cpp`)

| Type | Spectator range | Text delivered | Transform |
|---|---|---|---|
| **say** | ±8x / ±6y, same floor (`internalCreatureSay`:3641) | full text to all | none |
| **whisper** | ±8x / ±6y query (`playerWhisper`:3502) | full text if Chebyshev ≤1, else `"pspsps"` (still type WHISPER) | none |
| **yell** | ±18x / ±14y (`internalCreatureSay`:3646) | full text to all | **UPPERCASE** |

The speaker always hears their own message (TFS includes self in the spectator
set; our `spectators()` excludes the speaker, so we push to the speaker
explicitly with the full/uppercased text).

## Components

### 1. `crates/protocol/src/chat.rs` (new)

```rust
pub enum SpeakType { Say, Whisper, Yell }   // 1 / 2 / 3
impl SpeakType { fn from_u8(b: u8) -> Option<Self>; fn to_u8(self) -> u8; }

/// Parse the body of an inbound 0x96 (the bytes AFTER the opcode). Returns the
/// speak type + message for say/whisper/yell; None for unsupported types or a
/// malformed/empty body.
pub fn parse_say(body: &[u8]) -> Option<(SpeakType, String)>;

/// Build a 0xAA creature-say (position form).
pub fn creature_say(
    statement_id: u32, name: &[u8], level: u16,
    speak_type: SpeakType, pos: (u16, u16, u8), text: &[u8],
) -> Vec<u8>;
```

`parse_say` reads `type u8` then a string via `MessageReader::read_string`; maps
`1/2/3 → SpeakType`, returns `None` otherwise. Rejects an empty message.

### 2. `crates/world/src/game.rs`

- `Command::Say { id: u32, speak_type: SpeakType, text: String }`;
  `WorldHandle::say(id, speak_type, text)` (fire-and-forget, like turn).
- `Game` gains `next_statement_id: u32` (starts at 1).
- Generalize visibility: `spectators_in_range(pos, exclude, rx, ry) -> Vec<u32>`;
  `spectators(pos, exclude)` becomes `spectators_in_range(pos, exclude, 8, 6)`.
  Yell uses `spectators_in_range(pos, exclude, 18, 14)`.
- `do_say(id, speak_type, text)`:
  1. Look up speaker `position` + `name` (return if unknown).
  2. Validate/cap text (drop if empty; cap 255 bytes).
  3. `stmt = self.next_statement_id; self.next_statement_id += 1;`
  4. Build the speaker's own packet (full text, uppercased for yell) and `push`
     it to `id`.
  5. Per type:
     - **Say**: for each `spectators_in_range(pos, id, 8, 6)`, push the full-text
       `0xAA`.
     - **Whisper**: for each `spectators_in_range(pos, id, 8, 6)`, push `0xAA`
       with full text if `chebyshev(speaker_pos, spec_pos) <= 1` else `"pspsps"`.
     - **Yell**: uppercase the text; for each `spectators_in_range(pos, id,
       18, 14)`, push the full uppercased `0xAA`.

The actor remains the single packet builder (M5 invariant). Level is sent as `1`.

### 3. `crates/server/src/game_service.rs`

In `reader_loop`, before the `opcode_action` fallthrough, add:

```rust
if opcode == OPCODE_CLIENT_SAY {            // 0x96
    if let Some((speak_type, text)) = chat::parse_say(&payload[1..]) {
        world.say(id, speak_type, text).await;
    }
    continue;
}
```

`OPCODE_CLIENT_SAY = 0x96`. No other change to the reader.

## Data flow

```
client 0x96 → reader_loop → chat::parse_say → Command::Say
actor do_say: stmt id; build 0xAA; push to speaker (full/UPPER) +
              push to spectators_in_range (full / "pspsps" / UPPER per type)
→ writer task coalesces → client renders speech bubble + Local Chat line
```

## Error handling

- Empty message or unsupported speak type → dropped silently (no packet).
- Message > 255 bytes → capped to 255 (TFS limit) before broadcast.
- Unknown speaker id → no-op (the actor ignores it, as for move/turn).
- A bad/short `0x96` body → `parse_say` returns `None` → drained.
- `#![forbid(unsafe_code)]`, clippy `-D warnings` stay clean.

## Testing strategy (TDD, subagent-driven)

Pure, actor-free:
- `parse_say` round-trips say/whisper/yell; returns `None` for a channel type
  (7), a private type (5), an empty message, and a truncated body.
- `creature_say` byte-faithful layout (opcode, stmt id, name, level, type,
  position, message), exact length.

Actor:
- **say** pushes a `0xAA` (full text) to an in-range spectator and to the speaker.
- **yell** uppercases the text and reaches a spectator that is outside ±8 but
  within ±18 (proving the wider range), and does NOT reach one outside ±18.
- **whisper** pushes full text to an adjacent (≤1) spectator and `"pspsps"` to a
  spectator that is in view (±8/±6) but more than 1 tile away.
- speaker always receives their own message.

Reader:
- a `0x96` say frame drives a `Command::Say` (the existing integration test
  harness can assert a `0xAA` comes back).

**Live acceptance (gate):** two OTClients — say is seen nearby; walk apart and
say is no longer heard; whisper only the adjacent client reads (the far one sees
"pspsps"); yell is heard far away and arrives uppercased.

## Files touched

| File | Change |
|---|---|
| `crates/protocol/src/chat.rs` (new) | `SpeakType`, `parse_say`, `creature_say` + tests |
| `crates/protocol/src/lib.rs` | `pub mod chat;` |
| `crates/world/src/game.rs` | `Command::Say`, `WorldHandle::say`, `next_statement_id`, `spectators_in_range`, `do_say` |
| `crates/server/src/game_service.rs` | `reader_loop` 0x96 branch + `OPCODE_CLIENT_SAY` |
| `PROGRESS.md` / `README.md` | M6 status (after live acceptance) |

## Out of scope / deferred (YAGNI)

- Joinable channels (`0x97`/`0x98`/`0x99`/`0xAB`/`0xAC`, channel-id `0xAA`).
- Private messages (`TALKTYPE_PRIVATE_*`), GM red text.
- Yell cooldown (30 s in TFS), anti-spam/flood control.
- Multi-floor yell (consistent with M5's same-floor spectators).
- Real speaker level (sent as `1` until M14 progression exists).
- NPC/monster speech (`TALKTYPE_MONSTER_*`), arrives with M12.
