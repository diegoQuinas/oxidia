# Roadmap to Production — Oxidia

> A from-scratch, idiomatic Rust Open Tibia server. Protocol **10.98**, client target
> **OTClient Redemption**. TFS 1.4.2 (`reference/tfs/`) is a **spec reference only** —
> never ported line by line.

This document is the high-level milestone plan. Each milestone gets its own
design → plan → implementation cycle when we reach it. Milestones are ordered by
**ROI, weighted toward "fun to test live"** — every milestone ends in something
playable you can accept in a live session.

## Architecture decision (locked)

**Rust core + embedded Lua (mlua) for mutable content.**

- **Rust (native):** networking, world state, the single authoritative game loop,
  combat math, movement, pathfinding, persistence, anything performance-critical or
  stable. `#![forbid(unsafe_code)]` everywhere, strict TDD.
- **Lua (scripted, hot-reloadable):** content that changes often — spell logic,
  monster special behaviors, NPC dialogue, quests, custom events. No recompile to
  iterate on content.
- **Data files (TOML/RON):** static definitions — monster stats/loot tables, item
  attributes, spawns, vocations.

Rule of thumb: *the less you need to modify it, the more it belongs in Rust.*

## Content policy (locked)

TFS content is **not one thing** — it splits into *data* and *behavior*, and each maps
to a different home. Reference scale in `reference/tfs/data/`: **742 monster XMLs** (pure
data), **513 Lua scripts** (236 of them spells, mostly the same template), and
**~3700 LoC of `lib/`** (the binding API those scripts assume).

| Content kind | Examples | Lives in | Idiomatic |
|---|---|---|---|
| **Data** | Monster stats/loot, item attributes, spawns, vocations (the 742 monsters) | TOML/RON + native Rust loader | 100% ✅ |
| **Common patterns** | Damage-formula evaluator, standard monster AI, loot rolls, melee | Native Rust, parameterized by data | 100% ✅ |
| **Bespoke long tail** | Quests, weird NPCs, multi-stage/summon/teleport spells, custom events | Lua (mlua), hot-reload | thin scripting layer |

**The key insight:** most TFS spells are the same `Combat` + parameters + damage-formula
shape. Model the formula *as data* (coefficients) with a native Rust evaluator — a handful
of formula types cover ~90% of spells with zero scripting. Only the genuinely bespoke
tail needs Lua.

**On reusing TFS Lua:** do **not** run TFS scripts as-is. They are useless without
re-porting TFS's entire Lua binding API (`Combat`, `Player`, `Creature`, `COMBAT_PARAM_*`,
the userdata types, the ~3700-line `lib/`), which would couple our clean core to TFS
internals and chase bug-for-bug compatibility — breaking "100% idiomatic." Instead use TFS
Lua **two cheap ways**: (a) as a **spec** — read the formula/behavior, reimplement in
Rust or our thin Lua layer; (b) **mine the data** out of them (loot tables, mana costs,
cooldowns), which *is* portable.

## Hot-reload policy (locked)

GMs reload content live (`/reload <subsystem>`) without restarting or recompiling —
exactly like TFS reloads XML/Lua. **The reload boundary is the same data/Lua vs native-Rust
line as the content policy** (not a coincidence — same frontier):

| Tier | Live-reloadable | Mechanism |
|---|---|---|
| Data (TOML/RON) | ✅ trivial | Re-parse file → atomically swap the in-memory registry |
| Lua (mlua) | ✅ yes | Re-init the script registry |
| Native Rust (netcode, combat core, AI base, formula evaluator) | ❌ no | Recompile + restart (cheap in dev; rarely changes) |

This emerges for free from the existing single-actor world design — no bolt-on subsystem.

**Two non-negotiable rules:**

1. **Validate before swap.** Parse the new file into a *new* registry; on failure (bad
   TOML, Lua syntax error) keep the old one and report the error to the GM. Never swap a
   broken state. In Rust: build the `Result`, swap the `Arc` only on `Ok`. Fail-safe.
2. **The `/reload` command is native, not Lua.** Otherwise reloading Lua via a Lua command
   is chicken-and-egg. It's a native GM talkaction gated by group permissions.

**Mechanism (leans on the actor):** `world::game::GameWorld` is a single tokio actor over
mpsc, so `/reload` enters as one more channel message — the swap happens *between ticks*,
no locks, no torn reads. Atomicity comes from the design we already have.

**Per-subsystem decision** (not a blocker): when definitions reload, do live entities
update? *Lookup-by-reference* (live monsters re-read stats → instant) vs *snapshot-at-spawn*
(reload affects only new spawns). Decided per registry; TFS mixes both.

**Roadmap placement** — a cross-cutting principle, not a standalone milestone:

- **Every data/Lua-backed registry exposes a validated, atomic `reload()` from day one.**
- Reload capability is built incrementally with each content milestone (M11 Lua, M12
  monsters, M21 quests/raids).
- The `/reload <subsystem>` GM command lands in **M24 (GM tools)**, built on the
  per-subsystem `reload()` hooks that already exist by then.

## Ordering principles

1. **Every milestone is testable and delivers a "moment."** The live-acceptance loop
   is the reward.
2. **Foundations before content.** The spectator/known-creatures system, the combat
   actor, and the Lua engine are load-bearing — build them right once.
3. **Lua arrives when content needs it** (monsters/spells/NPCs), not before. PvP melee
   is native Rust.
4. **Foundations-first over quick-ship** (decided): the spectator/event system is
   touched by every social and combat milestone, so it must be solid from day one.

## Status

- Done: **M0** skeleton, **M1** login, **M2** formats, **M3** enter game (all accepted live).
- In progress: **M4** walk.

---

## Phase A — Living World → PRE-ALPHA #1

Highest-ROI stretch. Turns the single-player demo into a server friends play on.

| # | Milestone | Live payoff | Why here |
|---|-----------|-------------|----------|
| M4 | Walk *(in progress)* | Move your character; tile updates, floor changes, collision | Absolute foundation. |
| M5 | Multiplayer presence | See your friends walking in real time | The "multiplayer!" moment. Spectator / known-creatures system + movement broadcast. Keystone for everything social. |
| M6 | Chat | say / whisper / yell + default channel | Cheap once the spectator system exists. Social glue. |
| M7 | Combat core + PvP melee | Players hit each other, see damage, die, respawn; protected zones (no-PvP temple) | Pre-alpha gate. Native combat actor, HP sync, death/respawn, basic regen. |
| M8 | Persistence + accounts | Each friend has an account/character and keeps progress (position, stats) | Turns "demo" into a real server. Enables the ship. |

**➡️ SHIP PRE-ALPHA #1** — shared world: walk + chat + PvP, persisted characters.

> Ops note (not a milestone): bind address + network reachability — port forward or a
> tunnel so friends can connect.

---

## Phase B — Items & Inventory

Foundation for gear, loot, trade, depot — everything downstream depends on it.

| # | Milestone | Live payoff | Why |
|---|-----------|-------------|-----|
| M9 | Ground items | See items on tiles, stacks, *look* (examine) | Already parsed from `.otbm`; needs rendering + look-at. |
| M10 | Inventory & equipment | Move items, equip slots, open backpack, use items | Enables gear-based PvP and is prerequisite for loot/trade/depot. |

---

## Phase C — Scripting Engine (Lua)

| # | Milestone | Live payoff | Why here |
|---|-----------|-------------|----------|
| M11 | Lua runtime (mlua) | Reload a script without recompiling; `onUse`/`onStepIn`/`onSay` hooks in a sandbox | Materializes "Rust core + Lua content". Lands right before the content explosion. First hot-reloadable subsystem — Lua registry exposes validated atomic `reload()`. |

---

## Phase D — PvE: The Classic Loop → PRE-ALPHA #2

The "now it's really Tibia" milestone. Maximum fun.

| # | Milestone | Live payoff | Why |
|---|-----------|-------------|-----|
| M12 | Creatures & monsters | Monsters spawn, chase you, fight | Spawns + AI (idle/wander/target/attack/flee) + A* pathfinding in Rust; stats/loot/spells in data+Lua. Monster/spawn registries get validated atomic `reload()`. |
| M13 | Loot & corpses | Kill them, they drop loot, you pick it up | Reuses inventory + combat. |
| M14 | Skills, XP, levels, vocations | Level up, skills improve | The progression hook. |
| M15 | Spells, runes, conditions | Cast spells, use runes, poison/haste/etc. | Instant spells via Lua; conditions native. |

**➡️ SHIP PRE-ALPHA #2** — full PvE loop with friends: hunt, loot, level up.

---

## Phase E — Social & Economy

| # | Milestone | Live payoff |
|---|-----------|-------------|
| M16 | NPCs | Dialogue (Lua) + buy/sell |
| M17 | Depot, bank, money | Store items and gold |
| M18 | Parties | Group + shared XP |
| M19 | Guilds | Guilds + guild channel |

---

## Phase F — World Systems

| # | Milestone | Live payoff |
|---|-----------|-------------|
| M20 | Houses | Ownership, doors, beds, persisted house items |
| M21 | Quests | Quest log + rewards (mostly Lua) |
| M22 | Market | Auction-house economy (late/optional) |
| M23 | PvP systems | Skulls, frags, war/PZ rules |

---

## Phase G — Production Hardening 🏁

The "production-ready" gate. Closes `PROGRESS.md`.

| # | Milestone | Covers |
|---|-----------|--------|
| M24 | GM/admin tools | `/reload <subsystem>` (native, permission-gated), commands, broadcast, kick, ban, teleport, mute |
| M25 | Persistence robustness | Periodic saves, crash recovery, graceful shutdown |
| M26 | Account management | In-protocol account/character creation, security |
| M27 | Ops & stability | Metrics, logging, rate-limit, reconnection, load test |
| M28 | Configurability | Rates, world type, packaging, deploy |

---

## Ship gates summary

| Gate | After | You get |
|------|-------|---------|
| **Pre-alpha #1** | M8 | Shared world: walk, chat, PvP, persistence |
| **Pre-alpha #2** | M15 | Full PvE loop: monsters, loot, levels, spells |
| **Production** | M28 | Complete OTServer, hardened for live operation |
