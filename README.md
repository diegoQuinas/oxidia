# Oxidia 🦀

![coverage](https://img.shields.io/badge/coverage-90%25-brightgreen)
![protocol](https://img.shields.io/badge/protocol-10.98-blue)
![unsafe](https://img.shields.io/badge/unsafe-forbidden-success)
![license](https://img.shields.io/badge/license-MIT-blue)

An Open Tibia server written in Rust — protocol **10.98**, targeting the **OTClient Redemption** client.

![Test Knight standing on the Thais temple ground in OTClient](docs/assets/ingame.png)

> A real OTClient connects, logs in, and renders a character on the actual `forgotten.otbm` map. No emulator tricks — genuine protocol, genuine map data.

## Why

A from-scratch, memory-safe Tibia server. Every crate is `#![forbid(unsafe_code)]`, the protocol is verified byte-for-byte against the real client, and TFS 1.4.2 is used only as a spec reference — never ported line by line.

## Why Rust over a C++ port

Rewriting instead of porting only pays off if the new architecture is *better*,
not just *different*. Each design call below was made after reading TFS 1.4.2's
actual implementation (`reference/tfs/`) and choosing an approach the language and
runtime make cleaner or safer. These are **architecture decisions**, verified
against TFS; the table notes what already ships vs. what the design lands in M5.

| Concern | TFS 1.4.2 (verified in source) | Oxidia | Status |
|---|---|---|---|
| **Shared mutable state** | Single dispatcher thread, manual discipline to stay lock-free | One `tokio` actor owns all world state; the type system *enforces* single-writer — no `Arc<Mutex>`, no torn reads possible | ✅ ships (M4) |
| **Memory safety** | C++; manual lifetimes, raw pointers in the quadtree/spectator paths | `#![forbid(unsafe_code)]` in every crate; no segfault/UAF class exists | ✅ ships |
| **Outbound latency** | Per-connection buffer flushed on a fixed **10 ms** tick (`outputmessage.cpp:25-38`) — up to 10 ms added to every packet, even with one player online | Per-session writer task **greedily drains** its channel: batches under load exactly like TFS, but writes immediately when idle — **no fixed-tick latency** | ✅ ships (M5) |
| **Slow-client safety** | Per-connection send queue grows unbounded (`connection.h:98`) — a stalled socket is a memory-pressure vector | Bounded channel + non-blocking `try_send`; a client that can't keep up is **reaped**, never buffered without limit, and **never stalls the game loop** | ✅ ships (M5) |
| **Packet correctness** | Byte layouts maintained by hand across the codebase | Every wire packet has a **byte-faithful round-trip test** against an OTClient-faithful decoder | ✅ ships |

The throughline: properties TFS holds by *convention and care* (lock-free game
logic, bounded memory, correct bytes), Oxidia holds by *construction* — the
compiler, the actor model, and the test suite enforce them so they can't quietly
regress.

## Quick start

```bash
cargo build && cargo test
RUST_LOG=info cargo run -p server -- config/server.toml
```

Then point OTClient Redemption at `127.0.0.1:7171` and log in with `test` / `test`.

## Roadmap

Ordered by ROI, weighted toward "fun to test live." Architecture: **Rust core +
embedded Lua (mlua)** for mutable content. Full plan in
[`docs/superpowers/specs/2026-06-06-roadmap-to-production.md`](docs/superpowers/specs/2026-06-06-roadmap-to-production.md).

| # | Goal | State |
|---|------|-------|
| M0 | Skeleton: workspace, listeners on 7171/7172, connection logs | ✅ |
| M1 | Login server: framing, Adler-32, RSA, XTEA, char list | ✅ |
| M2 | Formats: `.otb` + `.otbm` parsers | ✅ |
| M3 | Enter game: handshake, player load, render the real map | ✅ |
| **Phase A — Living World → pre-alpha #1** | | |
| M4 | Walk: movement, tile updates, floor changes, collision | ✅ |
| M5 | Multiplayer presence: spectators, see others walk | ✅ |
| M6 | Chat: say / whisper / yell + default channel | ✅ |
| M7 | Combat core + PvP melee: damage, death, respawn, protected zones | ⬜ |
| M8 | Persistence + accounts: per-friend characters, saved progress | ⬜ |
| **Phase B — Items & Inventory** | | |
| M9 | Ground items, stacks, look-at | ⬜ |
| M10 | Inventory & equipment: move, equip, containers, use | ⬜ |
| **Phase C — Scripting** | | |
| M11 | Lua runtime (mlua): hot-reloadable content hooks | ⬜ |
| **Phase D — PvE → pre-alpha #2** | | |
| M12 | Creatures & monsters: spawns, AI, A* pathfinding | ⬜ |
| M13 | Loot & corpses | ⬜ |
| M14 | Skills, XP, levels, vocations | ⬜ |
| M15 | Spells, runes, conditions | ⬜ |
| **Phase E — Social & Economy** | | |
| M16 | NPCs: dialogue + trade | ⬜ |
| M17 | Depot, bank, money | ⬜ |
| M18 | Parties: shared XP | ⬜ |
| M19 | Guilds + guild channel | ⬜ |
| **Phase F — World Systems** | | |
| M20 | Houses | ⬜ |
| M21 | Quests | ⬜ |
| M22 | Market | ⬜ |
| M23 | PvP systems: skulls, frags, war/PZ rules | ⬜ |
| **Phase G — Production Hardening** | | |
| M24 | GM/admin tools | ⬜ |
| M25 | Persistence robustness | ⬜ |
| M26 | Account management | ⬜ |
| M27 | Ops & stability | ⬜ |
| M28 | Configurability & deploy | ⬜ |

---

Built with strict TDD. See `PROGRESS.md` for the full milestone log and protocol notes.

## License

[MIT](LICENSE) © Diego Perez Giordán
