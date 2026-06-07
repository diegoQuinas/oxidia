# M5–M9 Worktree Parallelization Plan — Oxidia

> How to attack milestones M5 through M9 across parallel git worktrees without
> stepping on each other. Read alongside the roadmap
> (`docs/superpowers/specs/2026-06-06-roadmap-to-production.md`).

## Verdict up front

The bulk of the roadmap **cannot** be parallelized. The architecture is a
**single authoritative tokio actor** (`world::game::GameWorld`) — by design,
single-writer. Milestones M5–M28 all mutate the same three files
(`world/src/game.rs`, `server/src/game_service.rs`, `PROGRESS.md`) and form a
hard dependency chain (M5 presence is the keystone every social/combat milestone
wires into).

What *can* run in parallel is the **pure, additive work** that never touches the
actor: byte-faithful protocol encoders, pure combat math, static data files, and
the isolated `persistence` crate. The model is **1 serial spine + 3 parallel
feeders**, not "M5–M9 at once."

## Topology

```
                    ┌──────────────────────────────────────────────────┐
   wt-spine ───────►│ M5 broadcast → M6 int → M7 int → M8 int → M9 int  │  serial; only writer of game.rs
                    └───▲────────▲───────────▲──────────▲───────────▲───┘
                        │ merge  │           │          │           │
   wt-proto  ───────────┴────────┴───────────┴──────────┼───────────┘   pure packets (1 file each)
   wt-data   ─────────────────────────────────────────────┘            pure math + TOML/RON
   wt-persist ────────────────────────────────────────────────────►     isolated persistence crate
```

**Golden rule:** only `wt-spine` edits `world/src/game.rs` and
`server/src/game_service.rs`. Everything else lives in its own file.

## File ownership (collision-free)

| Worktree | Milestone(s) | Creates / edits | Why it does not collide |
|---|---|---|---|
| **wt-spine** | M5→M9 integration | `world/src/game.rs`, `server/src/game_service.rs`, `world/src/map.rs` (tile state, M9), `PROGRESS.md` | Sole writer of the actor. Serial by design. |
| **wt-proto** | M5, M6, M7, M9 | `protocol/src/remove_creature.rs` (M5), `chat.rs` (M6), `combat_packets.rs` (M7), `ground_items.rs` + look-at parse (M9) | Each packet is its own file with byte-faithful round-trip tests. |
| **wt-data** | M7, M9 | `world/src/combat.rs` (pure damage formula), `config/items*.toml`, `config/damage_coeffs.toml`, look-at text builder | Pure functions parameterized by data. No actor. |
| **wt-persist** | M8 | `persistence/migrations/0002_player_state.sql`, `persistence/src/lib.rs` (save/load player) | Own crate, own numbered migration. |

> Opcodes above are indicative; exact byte layouts get pinned in each milestone's
> own design phase (the M-series pattern), not invented here.

## The split that makes it safe

Each milestone = **fat spine piece + thin feeder piece**:

- **M5** is ~95% spine — the keystone: known-creatures set per player, spectator
  list, a per-session outbound channel for unsolicited push. Feeder contributes
  only `remove_creature 0x6C`.
- **M6 / M7 / M9** — the feeder pre-builds *all* pure encoders/parsers and math;
  the spine then does **only the integration** (route say/attack/look-at through
  the M5 broadcast).
- **M8** — feeder lands schema + queries; spine only wires load-on-login /
  save-on-logout.

Feeders are **independently verifiable**: each packet file round-trips against an
OTClient-faithful decoder, combat math has pure unit tests, persistence runs
against sqlite. None need the actor to go green.

## Synchronization points (the real barriers)

1. **M5 is a hard barrier.** No M6/M7/M9 *integration* lands in the spine until
   M5 broadcast is accepted live. Feeders may run *during* M5 (they are pure);
   their integration waits.
2. **Each feeder merges to main before the spine reaches its milestone.** Since
   feeders are fast and pure, they finish well ahead of the spine. The feeder is
   never the bottleneck.
3. **Critical path = the spine, serial:** M5→M6→M7→M8→M9. Feeders do not shorten
   the critical path — they *thin* it, reducing each spine milestone to "just the
   integration."

## Merge discipline (non-negotiable)

- **Only the spine edits `PROGRESS.md`** — it owns milestone state. Feeders
  document in their PR; the spine records live acceptance.
- Feeders merge to `main` often, in small one-file PRs → near-zero conflicts.
- Sole shared touch-points: `protocol/src/lib.rs` (the `mod` list) and
  `Cargo.toml` (deps) — append-only, trivial conflicts. Append at the end, sort
  later.
- The spine runs `git pull main` before each integration so feeder pieces are
  available.

## What this is NOT

This does **not** deliver "M5–M9 in parallel." Real parallelism = 3 worktrees
pre-building pure pieces while 1 worktree advances the actor serially. The
wall-clock win is concrete but bounded — you remove work from the critical path,
you don't divide it. M5 stays a bottleneck no one can skip.

## Cost note

Running N concurrent worktrees (each driven by an agent) multiplies token burn
roughly linearly with N. A 4-worktree fan-out costs ~4× a serial pass. Budget
accordingly: M5 is the bottleneck and **cannot** be parallelized anyway, so doing
M5 serially first — then fanning out the feeders when budget is full — is the
economical sequence.
