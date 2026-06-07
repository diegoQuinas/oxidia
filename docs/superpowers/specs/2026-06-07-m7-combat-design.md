# M7 — Combat Core & PvP Melee — Design

> Oxidia, the from-scratch idiomatic Rust Open Tibia server. Protocol **10.98**,
> client **OTClient Redemption**. TFS 1.4.2 (`reference/tfs/`) is a **spec
> reference only** — verified, never ported line by line.

M7 is the pre-alpha #1 combat gate. Two friends, both in the game, can now **hit
each other with fists**: you target a player, your character swings on a timer,
damage lands, both health bars and your own stats update in real time, and when
someone hits 0 HP they die and respawn at the temple. It is the smallest
end-to-end PvP loop, built so that every later combat milestone (spells M15,
monsters M12, loot/corpses M13, skills/XP M14) wires into the same seam.

## Scope (locked)

**Melee PvP, end to end:**

- **Target a player** — the client's red attack-target request (`0xA1`) sets the
  attacker's target; the actor begins swinging.
- **Attack timer** — while a target is set, the attacker auto-swings on the TFS
  melee interval (no manual per-hit packet; Tibia melee is target-then-tick).
- **Melee damage** — TFS skill-based physical damage (fist, since there is no
  inventory until M10), with the real `getMaxWeaponDamage` formula and random
  spread.
- **HP sync** — two distinct outbound updates per hit: the **health-bar
  percent** (`0x8C`) to every spectator of the victim, and the victim's **own
  full stats** (`0xA0`) to the victim's session.
- **Death + respawn (minimal)** — on HP ≤ 0 the victim is teleported to its town
  temple with HP restored; spectators see the relocation; the dying player gets a
  death/relogin window (`0x28`). **No corpse, no loot, no XP/skill loss, no
  skull** — those are deferred (see "Out of scope").
- **Protected zones** — an attack whose **attacker tile** is flagged
  `PROTECTIONZONE` is rejected with a status message (`0xB4`); no target is set,
  no swing scheduled. (TFS rejects on the *target area* tile; for melee PvP at
  Chebyshev 1 the attacker and target tiles are adjacent, so we reject on the
  attacker's PZ tile, which is the player-facing rule "you can't fight in a
  temple". The target-tile variant lands when ranged combat does.)

Out of scope (later milestones): **corpses & loot** (M13); **XP / skill gain /
death penalty** (M14); **mana, spells, runes, conditions** (M15); **monsters /
NPC combat** (M12); **distance & wand weapons, real equipped weapon attack**
(needs inventory, M10); **follow (`0xA2`)** beyond consuming the opcode;
**shields / armor mitigation / block chance**, **defense**, **immunities**,
**skulls / frags / war PvP rules** (M23); **fight modes** (attack/balanced/
defensive — M7 hardcodes attack mode, `attackFactor = 1.0`); **logout-in-fight
block** (the `reader_loop` already has the TODO marker).

## The core problem M7 solves

Until now the actor only *reacts* to a client request (walk, turn, say) by
pushing back immediately. Combat is the first **server-driven, time-based**
behavior: once you set a target, the server keeps hitting on its own clock with
no further input. The single-actor model has no timer today. M7 adds a
**self-scheduling tick** that drives all in-progress fights through the same
actor that owns every other mutation — keeping the "one writer, no locks"
invariant intact (see "Attack scheduling in a single actor").

## Mechanics, verified against TFS 1.4.2

### 1. Inbound attack opcode `0xA1` (byte-pinned)

`ProtocolGame::parseAttack` (`protocolgame.cpp:972-977`):

```cpp
void ProtocolGame::parseAttack(NetworkMessage& msg) {
    uint32_t creatureId = msg.get<uint32_t>();
    addGameTask(&Game::playerSetAttackedCreature, player->getID(), creatureId);
}
```

**Wire layout (client → server):** `[0xA1][u32 creatureId]`. `creatureId == 0`
clears the target (stop attacking). The dispatch byte `0xA1` is read in the
opcode switch at `protocolgame.cpp:537`.

`0xA2` is **follow** (`parseFollow`, `protocolgame.cpp:979-984`), same body
(`[0xA2][u32 creatureId]`). M7 **consumes and ignores** `0xA2` (no movement-AI
follow yet); it must not fall through to the walk/turn dispatch.

> **Opcode-namespace gotcha:** `0xA1` and `0xA2` are also *outbound* opcodes
> elsewhere (`0xA1` skills, `0xA2` icons). Inbound vs outbound are namespaced by
> direction, exactly as M4 documented for `0x6B`/`0x6D`. No conflict.

### 2. Melee damage formula (byte-pinned, this is the wt-data feeder)

The skill-based maximum (`Weapons::getMaxWeaponDamage`, `weapons.cpp:135-138`):

```cpp
int32_t Weapons::getMaxWeaponDamage(uint32_t level, int32_t attackSkill,
                                    int32_t attackValue, float attackFactor) {
    return static_cast<int32_t>(std::round(
        (level / 5) +
        (((((attackSkill / 4.) + 1) * (attackValue / 3.)) * 1.03) / attackFactor)));
}
```

The fist (unarmed) attack (`Weapon::useFist`, `weapons.cpp` — confirmed body):

```cpp
float   attackFactor = player->getAttackFactor();      // FIGHTMODE_ATTACK -> 1.0
int32_t attackSkill  = player->getSkillLevel(SKILL_FIST);
int32_t attackValue  = 7;                              // fist constant
int32_t maxDamage    = Weapons::getMaxWeaponDamage(level, attackSkill, attackValue, attackFactor);
damage.primary.value = -normal_random(0, maxDamage);   // physical, uniform 0..max
```

**Critical integer-arithmetic faithfulness** (a port that uses float division
here silently inflates damage and desyncs from any TFS reference build):

- `level / 5` is **integer** division (C++ `uint32_t / int`).
- `attackSkill / 4.` and `attackValue / 3.` are **float** division (the `.`
  suffix). The Rust port MUST mirror this exactly: `(level / 5) as f64 +
  ((((attackSkill as f64 / 4.0) + 1.0) * (attackValue as f64 / 3.0)) * 1.03) /
  attackFactor`, then `.round()`.
- `normal_random(0, max)` in TFS is a **uniform** integer in `[0, max]`
  (`tools.cpp` `normal_random` is a misnomer — it is `uniform_int_distribution`
  for the melee path). M7 uses a uniform RNG over `0..=max`.

**M7 constants (no inventory / no progression yet):** `level = 1`,
`SKILL_FIST = 10` (TFS default starting fist), `attackValue = 7`,
`attackFactor = 1.0`. → `maxDamage = round((1/5) + ((((10/4)+1)*(7/3))*1.03)/1.0)
= round(0 + ((3.5 * 2.333…) * 1.03)) = round(8.408…) = 8`. So a level-1 fist hits
for a uniform `0..=8` physical damage per swing. (`vocation meleeDamageMultiplier`
is 1.0 for the no-vocation default and is folded in as a parameter for M14.)

The feeder exposes this as a **pure function** parameterized by all inputs so
M14 (real skills/levels) and M10 (equipped weapon `attackValue`) plug in without
touching the actor:

```rust
// crates/world/src/combat.rs  (wt-data, pure, no actor)
pub fn max_weapon_damage(level: u32, attack_skill: i32, attack_value: i32, attack_factor: f64) -> i32;
pub fn melee_damage(rng: &mut impl Rng, level: u32, attack_skill: i32, attack_value: i32, attack_factor: f64) -> i32; // returns the positive damage 0..=max
pub fn fist_damage(rng: &mut impl Rng, level: u32, fist_skill: i32) -> i32; // attack_value=7, factor=1.0
```

### 3. Attack interval (byte-pinned semantics)

`Player::doAttacking` (`player.cpp:3223-3264`) only swings when
`(now - lastAttack) >= getAttackSpeed()`, then reschedules a
`checkCreatureAttack` task at `delay = getAttackSpeed()`.
`Player::getAttackSpeed` (`player.cpp:351-358`) returns the vocation attack speed
when no weapon overrides it. **M7 uses the no-vocation default interval of
`2000 ms`** (TFS `vocations.xml` "None" `attackspeed`), a single tunable constant
`MELEE_ATTACK_INTERVAL_MS`. A target newly set fires on the **next tick**
(`lastAttack` initialized so the first eligible tick swings — mirrors
`player.cpp:3225-3226`).

### 4. Damage application & HP sync (two packets, two audiences — byte-pinned)

The TFS chain `drainHealth → changeHealth → executeDeath` and the player's
self-stats refresh:

- `Creature::changeHealth` (`creature.cpp`): clamps health to `[0, max]`, and if
  it changed calls `g_game.addCreatureHealth(this)`; if `health <= 0` it
  schedules `Game::executeDeath`.
- `Game::addCreatureHealth` (`game.cpp:4385-4399`): for every spectator that is a
  player, `sendCreatureHealth(target)` → the **health-bar** packet.
- `Player::drainHealth` (`player.cpp:1552-1556`): calls `Creature::drainHealth`
  **then `sendStats()`** → the victim's **own `0xA0`** refreshes its HP digits.

**`0x8C` health-bar (server → spectators)** — `sendCreatureHealth`
(`protocolgame.cpp:2339-2351`):

```cpp
msg.addByte(0x8C);
msg.add<uint32_t>(creature->getID());
msg.addByte(ceil((health / max(maxHealth,1)) * 100)); // 0..100, or 0x00 if hidden
```

Wire: `[0x8C][u32 creatureId][u8 healthPercent]`. The percent is
`ceil(health / max_health * 100)`, clamped via `max(maxHealth,1)`.

**`0xA0` self-stats (server → victim only)** — `AddPlayerStats`
(`protocolgame.cpp:3007-3048`). The leading fields are
`[0xA0][u16 health][u16 maxHealth][u32 freeCap][u32 cap][u64 exp]…`. This is the
**existing `enter_world::stats` encoder** (already 1098-faithful, 53-byte body);
M7 reuses it verbatim, passing the victim's new `health`. No new encoder needed
for self-stats — only the live `health` value flows through.

> **The split is the whole point of the wt-proto feeder:** `0x8C` is a brand-new
> tiny packet (health bar), `0xA0` already exists. Spectators get `0x8C` only;
> the victim gets `0x8C` (its own bar, since it is a spectator of its own tile)
> **and** `0xA0`. The attacker gets neither unless it is also a spectator of the
> victim's tile (it always is at melee range).

### 5. Death + respawn (minimal — byte-pinned window)

`Player::onDeath` in TFS handles corpse, loot, XP loss, skull, then
`g_game.removeCreature`. M7 takes the **minimal** path that is correct and
defers the rest:

1. On `health <= 0` (detected inside the actor right after applying damage):
   stop the victim's and any attackers-of-victim fights (clear targets).
2. Push the **death window `0x28`** to the dying player —
   `sendReLoginWindow` (`protocolgame.cpp:1376-1383`):
   `[0x28][u8 0x00][u8 unfairFightReduction]`. M7 sends `unfairFightReduction =
   0` (no unfair-fight math until M23). This is the client's "you are dead"
   modal.
3. **Respawn:** teleport the victim to its **town temple**
   (`player.cpp:1980 loginPosition = town->getTemplePosition()`,
   `player.cpp:2111 internalTeleport(getTemplePosition())`), restore
   `health = max_health`, and broadcast the relocation to spectators using the
   **existing M5 remove+add** machinery (the temple is almost always out of the
   death tile's viewport, so it is a clean `0x6C` at the death tile + `0x6A` at
   the temple, exactly like a teleport). Push a fresh enter-world-style map +
   `0xA0` to the respawned player.

> **Why no corpse desync:** M7 never spawns a corpse item, so the ≤1-creature-
> per-tile stackpos invariant is untouched — the dead creature is simply removed
> from its tile (a `0x6C`) and re-added at the temple (a `0x6A`). Corpses (M13)
> will be ground items on the death tile, handled by the M9 ground-item stackpos
> work, not here.

### 6. Protected zones (byte-pinned flag)

- **Source flag:** the OTBM tile flag `OTBM_TILEFLAG_PROTECTIONZONE = 1 << 0`
  (`iomap.h:60`), already parsed into `MapTile.flags` by the M2 OTBM loader
  (`TILE_FLAGS` attr). TFS maps it to the runtime `TILESTATE_PROTECTIONZONE =
  1 << 7` (`tile.h:33`, `iomap.cpp:270-271`).
- **Rejection:** `Combat::canDoCombat` / `combatTileCheck`
  (`combat.cpp:294-297`): `if (aggressive && tile->hasFlag(TILESTATE_PROTECTIONZONE))
  return RETURNVALUE_ACTIONNOTPERMITTEDINPROTECTIONZONE;`. The return value
  surfaces to the client as a status text via `sendCancelMessage` →
  `sendTextMessage` (`0xB4`, `protocolgame.cpp:1411-1414`).
- **M7 rule:** when a player sets a target, if the **attacker's** tile is PZ,
  reject: push `0xB4` "You may not attack a person while you are in a protection
  zone." and do **not** set the target. (`Combat::isProtected`,
  `combat.cpp:307-323`, adds level/vocation/skull guards — all deferred to M23;
  M7's only guard is the PZ tile flag, the one a friends-server actually needs so
  nobody gets ganked on the temple.)

## Attack scheduling in a single actor (the architecture decision)

TFS drives combat with a global scheduler posting `checkCreatureAttack` tasks per
fighter. Our actor has **no scheduler and no timer** — adding per-fight tokio
timers would reintroduce concurrency into the single-writer loop. The locked
design keeps the actor single-threaded and lock-free:

| Concern | TFS 1.4.2 | Oxidia M7 | Why ours holds the invariant |
|---|---|---|---|
| Attack clock | Per-creature `SchedulerTask` re-posted at `getAttackSpeed()` | **One** `tokio::time::interval` (the combat tick) feeding `Command::CombatTick` into the same mpsc the actor already drains | The actor stays the sole mutator; the tick is just another command, processed serially between walk/say/etc. |
| Fight state | `attackedCreature` pointer on each Creature | `attacking: Option<u32>` + `last_attack_ms: u64` on `PlayerState` | Pure data on the existing struct; no new task per fight |
| Per-hit eligibility | `(now - lastAttack) >= attackSpeed` | identical check inside the tick handler | Byte-faithful interval semantics, no float drift |

**The combat tick:** `spawn` starts a single
`tokio::time::interval(TICK_PERIOD)` task (e.g. 250 ms — finer than the 2000 ms
attack interval so timing granularity is good, but coarse enough to be cheap)
that sends `Command::CombatTick { now_ms }` to the actor. The actor's
`on_combat_tick` iterates players with `attacking.is_some()`, and for each whose
`now - last_attack >= MELEE_ATTACK_INTERVAL_MS` and whose target is still
adjacent/alive, computes one swing. **The tick never blocks** (it is fire-and-
forget like every other command); a backpressured actor simply processes the
next tick late, which is benign for a 2 s swing.

> **Why one global tick, not per-player timers:** N timers = N tasks racing to
> mutate shared state → exactly the locking the architecture forbids. One tick =
> one serialized command stream = the model we already proved in M5. This is the
> same "improve on TFS" move as M5's greedy-coalescing writer.

## Components

### 1. Feeder — `crates/world/src/combat.rs` (new, wt-data, pure)

Pure damage math (§2): `max_weapon_damage`, `melee_damage`, `fist_damage`,
parameterized by `(level, attack_skill, attack_value, attack_factor)` and an
injected `Rng` so tests are deterministic. Unit-tested against hand-computed TFS
values (the level-1 fist → `0..=8`, plus the integer-vs-float division edge
cases). **No actor, no protocol, no I/O.** Lives in `world` (not `protocol`)
because it is game logic, not wire format; it is the one piece of the spine crate
a feeder owns, isolated in its own file so it never collides with `game.rs`.

### 2. Feeder — `crates/protocol/src/combat_packets.rs` (new, wt-proto)

```rust
pub const OP_CREATURE_HEALTH: u8 = 0x8C;
pub const OP_DEATH_WINDOW: u8 = 0x28;

/// 0x8C health-bar: [0x8C][u32 id][u8 percent]. percent = ceil(hp/max*100), 0..=100.
pub fn creature_health(creature_id: u32, percent: u8) -> Vec<u8>;

/// ceil(hp / max(max,1) * 100) clamped to 0..=100 — the TFS percent helper.
pub fn health_percent(health: i32, max_health: i32) -> u8;

/// 0x28 death/relogin window: [0x28][u8 0x00][u8 unfair_fight_reduction].
pub fn death_window(unfair_fight_reduction: u8) -> Vec<u8>;
```

Byte-faithful round-trip tests against an OTClient-faithful decoder (the
`walk.rs` / `tile_creature.rs` pattern). The inbound `0xA1` parse is a one-liner
(`u32` LE at offset 1) and lives **in the session reader** (like the walk opcode
decode), not as a packet builder — but the *constant* `OP_ATTACK = 0xA1` and a
tiny `parse_attack(body) -> Option<u32>` helper go here for testability and to
keep the magic byte out of `game_service.rs`. **0xA0 self-stats is NOT here** —
it is reused from `enter_world::stats`.

### 3. Spine — `crates/world/src/map.rs` (PZ flag surfacing)

`StaticMap` gains a `protection_zone: HashSet<(u16,u16,u8)>` precomputed at load
from `MapTile.flags & OTBM_TILEFLAG_PROTECTIONZONE`, plus
`pub fn is_protection_zone(&self, pos: Position) -> bool`. Mirrors the existing
`blocked` / `floor_change` precompute pattern. (This is the one `map.rs` edit;
per the parallelization plan `map.rs` is spine-owned for tile state.)

### 4. Spine — `crates/world/src/game.rs` (the integration)

- `PlayerState` gains `health: u16`, `max_health: u16`, `fist_skill: i32`,
  `attacking: Option<u32>`, `last_attack_ms: u64`. (Health lives here now, not
  baked into the enter-world burst; the burst reads it from the snapshot.)
- `Command` gains `SetTarget { id, target_id }` (from `0xA1`) and
  `CombatTick { now_ms }` (from the interval task).
- `do_set_target(id, target_id)`: if `target_id == 0` clear `attacking`;
  else PZ-reject if attacker tile is PZ (push `0xB4`, return); else validate the
  target exists and set `attacking = Some(target_id)`, prime `last_attack_ms` so
  the next tick swings.
- `on_combat_tick(now_ms)`: for each attacker with a live, adjacent
  (Chebyshev ≤ 1, same floor) target whose interval elapsed: roll
  `combat::fist_damage`, apply via a new `apply_damage` helper, set
  `last_attack_ms = now_ms`. Out-of-range/dead targets clear the fight (TFS
  `setAttackedCreature(nullptr)` semantics).
- `apply_damage(victim, amount)`: clamp `health`; push **`0x8C`** to
  `spectators(victim.pos)` (including the victim and attacker) and **`0xA0`** to
  the victim; if `health == 0` call `do_death(victim)`.
- `do_death(victim)`: push `0x28` to the victim, clear all fights targeting it,
  teleport to `map.temple_for(victim)` (reuse `free_spawn` semantics at the
  temple), restore `health`, broadcast remove+add to spectators (M5 path), push
  a fresh map + `0xA0` to the respawned victim.
- The combat hit also pushes a **melee magic effect** (`enter_world::magic_effect`
  with the physical-hit effect id) on the victim's tile, the visible "blood"
  feedback — reusing the existing effect encoder.

### 5. Spine — `crates/server/src/game_service.rs` (opcode wiring + tick)

- `reader_loop` intercepts `0xA1` (parse `u32` target id → `world.set_target`)
  and **drains `0xA2`** (follow — consumed, ignored) before the walk/turn
  dispatch, exactly as `0x96`/`0x14` are handled today.
- The world's `spawn` (in `game.rs`) starts the single combat-tick interval task;
  `game_service.rs` needs no per-session timer.
- `WorldHandle` gains `set_target(id, target_id)`.

## Data flow — one PvP swing

```
session A: reader reads 0xA1 [target=B]  -> world.set_target(A, B)
actor:     do_set_target: A.pos not PZ, B exists -> A.attacking = Some(B); prime clock
... (combat tick fires every TICK_PERIOD) ...
actor:     on_combat_tick(now): A.attacking==B, B adjacent & alive, interval elapsed
           dmg = combat::fist_damage(rng, A.level, A.fist_skill)   // 0..=8
           apply_damage(B, dmg):
               B.health -= dmg (clamp 0)
               for s in spectators(B.pos): push(s, 0x8C health_percent(B))
               push(B, 0xA0 stats with new health)
               push(B.pos, magic_effect blood)   // to spectators
               if B.health == 0: do_death(B)
           A.last_attack_ms = now
actor:     do_death(B): push(B, 0x28); clear fights on B; teleport B to temple;
           B.health = max; spectators get remove(deathTile)+add(temple); push(B, map+0xA0)
```

## Feeder vs spine split

Each piece is either **pure feeder** (builds in a parallel worktree, merges to
`main` ahead of the spine, independently green) or **spine** (only `wt-spine`
touches it, serial). This is the M5/M6/M9 pattern applied to combat.

- **wt-data feeder** — `world/src/combat.rs`: the damage math. Pure functions,
  deterministic-RNG unit tests, hand-verified against TFS `getMaxWeaponDamage`.
  No actor, no protocol. Finishes first; the spine just calls
  `combat::fist_damage`.
- **wt-proto feeder** — `protocol/src/combat_packets.rs`: `0x8C` health-bar,
  `0x28` death window, `OP_ATTACK 0xA1` + `parse_attack`. Byte-faithful
  round-trip tests against an OTClient-faithful decoder. The `0xA0` self-stats is
  **not** rebuilt — it is the existing `enter_world::stats`.
- **wt-spine (serial, sole writer of the actor)** — `world/src/game.rs` combat
  state + tick + apply/death, `server/src/game_service.rs` `0xA1`/`0xA2` wiring +
  tick startup, `world/src/map.rs` PZ set. These call the two feeders; the
  feeders never call back.

### File-ownership table (for mechanical fan-out)

| Worktree | File | New / edit | Owns | Depends on |
|---|---|---|---|---|
| **wt-data** | `crates/world/src/combat.rs` | **new** | melee damage math (`max_weapon_damage`, `melee_damage`, `fist_damage`) | nothing (pure) |
| **wt-data** | `crates/world/src/lib.rs` | edit (append `mod combat; pub use`) | module registration | — |
| **wt-data** | `crates/world/Cargo.toml` | edit (add `rand`) | RNG dep | — |
| **wt-proto** | `crates/protocol/src/combat_packets.rs` | **new** | `0x8C`, `0x28`, `OP_ATTACK`, `parse_attack`, `health_percent` | `message::MessageWriter` |
| **wt-proto** | `crates/protocol/src/lib.rs` | edit (append `pub mod combat_packets;`) | module registration | — |
| **wt-spine** | `crates/world/src/map.rs` | edit | `protection_zone` set + `is_protection_zone` | `MapTile.flags` (M2) |
| **wt-spine** | `crates/world/src/game.rs` | edit | combat state on `PlayerState`, `SetTarget`/`CombatTick` commands, `do_set_target`, `on_combat_tick`, `apply_damage`, `do_death`, tick task in `spawn` | `combat::fist_damage` (wt-data), `combat_packets::*` + `enter_world::stats` (wt-proto) |
| **wt-spine** | `crates/server/src/game_service.rs` | edit | `0xA1`/`0xA2` reader dispatch, `WorldHandle::set_target` wiring | `combat_packets::parse_attack` |
| **docs** | `PROGRESS.md` | edit (spine only) | M7 status after live acceptance | — |

> Shared touch-points are append-only and trivially mergeable: `protocol/src/lib.rs`
> (one `mod` line), `world/src/lib.rs` (one `mod` line), the two `Cargo.toml`
> dep additions. Append at the end; sort later. **Only wt-spine edits
> `game.rs`, `game_service.rs`, `map.rs`, and `PROGRESS.md`** — the golden rule.

## Invariants preserved

- **≤1 creature per tile (M5).** Combat adds **no** new tile occupant: damage
  never moves a creature, and death is a remove (death tile) + add (temple) —
  the same atomic pair M5 uses for logout/login, so the static stackpos stays
  correct. No corpse means no second thing on the death tile.
- **Stackpos rules (M5).** Death respawn uses `tile_creature::remove_tile_thing`
  / `add_tile_creature` at stackpos < 10, identical to logout/login.
- **Single writer, no locks.** The combat tick is one more `Command` on the
  existing mpsc; nothing else gains write access to the world.
- **Same-floor melee.** A swing requires Chebyshev ≤ 1 **on the same floor**
  (`Position::areInRange<1,1>` + equal `z`, TFS `useFist`). Cross-floor melee is
  impossible, so combat never interacts with the M6.1 vertical band.

## Error handling

- **Target gone / logged out** — `on_combat_tick` finds no `PlayerState` for the
  target → clear `attacking`; no panic.
- **Target out of melee range** — fight stays set (TFS keeps the target; the
  player would walk toward it), but **no swing** that tick. M7 has no auto-walk
  (M4 deferral), so a target that walks away simply stops taking hits until
  re-adjacent. This is acceptable for the minimal loop and matches "target set,
  not in range" TFS behavior (no damage, no error).
- **Attack into / from PZ** — rejected at `do_set_target` with `0xB4`; never
  schedules a swing.
- **Self-target / target == attacker** — rejected (TFS `playerSetAttackedCreature`
  ignores self); no fight set.
- **Slow client / dead session** — unchanged: `push` reaps on a full channel
  (M5); a reaped attacker's fight vanishes with its `PlayerState`.
- `#![forbid(unsafe_code)]` and `cargo clippy --all-targets -- -D warnings` stay
  green.

## Testing strategy (TDD, subagent-driven — the M4/M5/M6.1 pattern)

Pure, independently verifiable (feeders):

- **combat math** — `max_weapon_damage(1, 10, 7, 1.0) == 8`; the integer
  `level/5` vs float `skill/4.` edges; `fist_damage` stays within `0..=max` over
  many seeded rolls; a deterministic RNG yields a known sequence.
- **`0x8C`** — `creature_health(id, pct)` byte layout; `health_percent` =
  `ceil`, clamped (e.g. `1/150 -> 1`, `0/150 -> 0`, `150/150 -> 100`).
- **`0x28`** — `death_window(0)` == `[0x28, 0x00, 0x00]`.
- **`parse_attack`** — `[0xA1][u32 LE id]` → `Some(id)`; `id==0` → clear; short
  body → `None`.

Actor / integration:

- **set target** — `0xA1` sets `attacking`; `0xA1 [0]` clears it.
- **swing applies damage** — drive `CombatTick` past the interval; assert the
  victim's `health` dropped, a `0x8C` reached a spectator, and a `0xA0` reached
  the victim.
- **death + respawn** — enough swings to reach 0 HP: assert `0x28` to the victim,
  the victim teleported to the temple, `health == max_health`, spectators got
  remove+add.
- **PZ rejection** — attacker on a PZ tile: `set_target` pushes `0xB4` and leaves
  `attacking` unset; no `0x8C` ever flows.
- **out-of-range** — target 2 tiles away: tick produces no damage; step adjacent:
  next tick hits.
- **follow drained** — `0xA2` does not move/turn the player and is not
  misread as a walk.

**Live acceptance (gate):** two OTClient sessions — A right-clicks B to attack,
B's health bar visibly drains on both screens and B's own HP digits drop;
continued attacks kill B; B sees the death window and respawns at the Thais
temple with full HP; A sees B vanish and reappear at the temple; nobody can
attack while standing on a temple PZ tile. Flip M7 to ✅ once this passes.

## Out of scope / deferred (YAGNI, with the milestone that owns each)

- **Corpses & loot** → M13 (death tile gets a corpse ground item; needs M9
  ground-item stackpos).
- **XP, skill advance, death penalty (level/skill loss)** → M14.
- **Mana, spells, runes, conditions (poison/fire/etc.)** → M15.
- **Monsters / NPC combat, target-following auto-walk** → M12.
- **Equipped weapons, real `attackValue`, distance/wand, ammo, element damage,
  armor/shield mitigation, block chance, defense** → M10 (inventory) then M15.
- **Fight modes (attack/balanced/defensive), `0xA0` fight-mode parse** → folds in
  with M14 progression; M7 hardcodes attack mode.
- **Skulls, frags, war PvP, unfair-fight reduction, NO-PVP / PVP-zone rules** →
  M23 (M7 sends `unfairFightReduction = 0`).
- **Logout-while-in-fight block** → wire into the existing `reader_loop` TODO
  when combat state exists (M7 leaves the marker; enforcing it is a one-liner
  once `attacking`/`last_hit` are queryable — a small follow-up).
- **Real target-tile PZ check (ranged)** → M7 rejects on the attacker's PZ tile;
  the target-area variant lands with distance combat.
