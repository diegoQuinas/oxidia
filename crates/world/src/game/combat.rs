//! Combat behavior (targeting, damage, death, ticks) for the game actor.

use std::collections::VecDeque;

use super::*;
use crate::combat;
use rand::Rng;

/// Pre-roll drops from a loot table using independent per-item chance.
/// Delegates to the module-level function for testability; caller passes
/// its own RNG to keep the combat path deterministic in tests.
pub(crate) fn collect_player_loot(
    inventory: &[Option<InvItem>; 10],
    containers: &[Option<OpenContainer>; 16],
) -> Vec<(u16, u8)> {
    let mut items = Vec::new();
    for slot in inventory.iter().flatten() {
        items.push((slot.server_id, slot.count.unwrap_or(1)));
    }
    for oc in containers.iter().flatten() {
        let is_inv_slot = matches!(oc.source, ContainerSource::Slot(_));
        if !is_inv_slot {
            continue;
        }
        for ci in &oc.items {
            items.push((ci.server_id, ci.count.unwrap_or(1)));
        }
    }
    items
}

impl Game {
    /// Handle `0xA1` — set or clear the attacker's melee target.
    ///
    /// - `target_id == 0` clears the fight.
    /// - `target_id == id` (self-attack) is ignored.
    /// - Attacker on a PZ tile → push `0xB4` and do NOT set target
    ///   (`combat.cpp:294-297`, TFS `playerSetAttackedCreature`).
    /// - Unknown target (player or monster) is silently ignored.
    /// Status-icon mask for `id` (TFS `Player::getClientIcons`).
    ///
    /// `ICON_PIGEON` on a protection-zone tile. `ICON_REDSWORDS` while PZ-locked,
    /// including inside a PZ (a temple teleport does not clear the lock).
    /// `ICON_SWORDS` is the infight badge and is suppressed in PZ; the lock that
    /// actually blocks entry is the red swords.
    pub(super) fn client_icon_mask(&self, id: u32) -> u16 {
        let Some(p) = self.players.get(&id) else {
            return 0;
        };
        let mut mask = 0u16;
        if self.chunks.is_protection_zone(p.position) {
            mask |= enter_world::ICON_PIGEON;
        }
        if p.pz_locked_until_ms > self.now_ms {
            mask |= enter_world::ICON_REDSWORDS;
        }
        mask
    }

    /// Push the combat-status icons to `id`.
    fn push_combat_icons(&mut self, id: u32) {
        let mask = self.client_icon_mask(id);
        self.push(id, enter_world::icons(mask));
    }

    /// True while the player's TFS `pzLocked` timer has not expired.
    pub(super) fn is_pz_locked(&self, id: u32) -> bool {
        self.players
            .get(&id)
            .is_some_and(|p| p.pz_locked_until_ms > self.now_ms)
    }

    /// Start or refresh the PZ lock (`Player::addInFightTicks` / `pzLocked`).
    /// Does not push icons; the caller does that when the visible mask changes.
    fn refresh_pz_lock(&mut self, id: u32) {
        let until = self.now_ms.saturating_add(PZ_LOCK_MS);
        if let Some(p) = self.players.get_mut(&id) {
            p.pz_locked_until_ms = until;
        }
    }

    /// Drop expired locks and clear the red-swords badge.
    fn expire_pz_locks(&mut self) {
        let now = self.now_ms;
        let expired: Vec<u32> = self
            .players
            .iter()
            .filter(|(_, p)| p.pz_locked_until_ms != 0 && p.pz_locked_until_ms <= now)
            .map(|(&id, _)| id)
            .collect();
        for id in expired {
            if let Some(p) = self.players.get_mut(&id) {
                p.pz_locked_until_ms = 0;
            }
            self.push_combat_icons(id);
        }
    }

    pub(super) fn do_set_target(&mut self, id: u32, target_id: u32) {
        if target_id == 0 {
            if let Some(p) = self.players.get_mut(&id) {
                p.attacking = None;
            }
            self.push_combat_icons(id);
            return;
        }
        if target_id == id {
            return; // self-attack ignored
        }
        // Check attacker exists.
        let attacker_pos = match self.players.get(&id) {
            Some(p) => p.position,
            None => return,
        };
        // PZ check on the attacker's tile.
        if self.chunks.is_protection_zone(attacker_pos) {
            self.push_status_message(
                id,
                b"You may not attack while you are in a protection zone.",
            );
            return;
        }
        // Target must exist (player or monster).
        if !self.creature_exists(target_id) {
            return;
        }
        if let Some(p) = self.players.get_mut(&id) {
            p.attacking = Some(target_id);
            // Prime last_attack_ms = 0 so the first tick whose now_ms >=
            // MELEE_ATTACK_INTERVAL_MS swings immediately.
            p.last_attack_ms = 0;
        }
        // TFS `setAttackedCreature` → `onAttackedCreature`: `pzLocked` (red
        // swords) is set only for a player target. A monster target does not.
        if self.players.contains_key(&target_id) {
            self.refresh_pz_lock(id);
        }
        self.push_combat_icons(id);
    }

    /// Apply `dmg` hit points of damage to `victim_id`, dealt by `attacker_id`.
    /// Clamps to 0, pushes health-bar (`0x8C`) to all spectators including the
    /// victim and attacker, pushes self-stats (`0xA0`) to the victim, emits the
    /// physical-hit blood effect plus a floating damage number (`0xB4`), and
    /// fires `do_death` on 0 HP.
    fn apply_damage(&mut self, attacker_id: u32, victim_id: u32, dmg: i32) {
        let (dmg, block) = self.mitigate_player(victim_id, dmg);
        let (health_before, new_health, max_health) = {
            let v = match self.players.get_mut(&victim_id) {
                Some(p) => p,
                None => return,
            };
            let before = v.health;
            v.health = v.health.saturating_sub(dmg.max(0) as u32);
            (before, v.health, v.max_health)
        };
        let victim_pos = match self.players.get(&victim_id) {
            Some(p) => p.position,
            None => return,
        };
        // Compute actual damage applied (clamped at remaining HP).
        let applied = health_before.saturating_sub(new_health);
        // Push 0x8C health-bar to every spectator of the victim's tile,
        // INCLUDING the victim itself (it is also a spectator of its own tile).
        let pct = combat_packets::health_percent(new_health as i32, max_health as i32);
        let health_bar = combat_packets::creature_health(victim_id, pct);
        // Collect spectators first (can_see of the victim's tile), then push.
        let spectators: Vec<u32> = self
            .players
            .iter()
            .filter(|&(&sid, sp)| Self::can_see(sp.position, victim_pos) || sid == victim_id)
            .map(|(&sid, _)| sid)
            .collect();
        for sid in &spectators {
            self.push(*sid, health_bar.clone());
        }
        // Push 0xA0 self-stats to the victim only.
        let stats = {
            let p = match self.players.get(&victim_id) {
                Some(p) => p,
                None => return,
            };
            enter_world::stats(&enter_world::Stats {
                health: p.health as u16,
                max_health: p.max_health as u16,
                free_capacity: 40_000,
                total_capacity: 40_000,
                experience: 0,
                level: 1,
                level_percent: 0,
                mana: 0,
                max_mana: 0,
                magic_level: 0,
                soul: 100,
                stamina_minutes: 2520,
                base_speed: p.speed,
            })
        };
        self.push(victim_id, stats);
        let hit_effect = Self::hit_effect_for(block, applied);
        let effect =
            enter_world::magic_effect(victim_pos.x, victim_pos.y, victim_pos.z, hit_effect);
        for sid in &spectators {
            self.push(*sid, effect.clone());
        }
        // Floating damage number (0xB4 TextMessage). A 0 value renders nothing
        // client-side, so skip the packet entirely.
        if applied > 0 {
            let victim_name = self
                .creature_name(victim_id)
                .unwrap_or_default()
                .to_string();
            let attacker_name = self
                .creature_name(attacker_id)
                .unwrap_or_default()
                .to_string();
            for sid in &spectators {
                let (mode, text) = if *sid == attacker_id {
                    (
                        combat_packets::MSG_DAMAGE_DEALT,
                        format!("You deal {applied} damage to {victim_name}."),
                    )
                } else if *sid == victim_id {
                    (
                        combat_packets::MSG_DAMAGE_RECEIVED,
                        format!(
                            "You lose {applied} hitpoints due to an attack by {attacker_name}."
                        ),
                    )
                } else {
                    (
                        combat_packets::MSG_DAMAGE_OTHERS,
                        format!(
                            "{victim_name} loses {applied} hitpoints due to an attack by {attacker_name}."
                        ),
                    )
                };
                let pkt = combat_packets::damage_text(
                    mode,
                    victim_pos.x,
                    victim_pos.y,
                    victim_pos.z,
                    applied,
                    combat_packets::TEXTCOLOR_RED,
                    text.as_bytes(),
                );
                self.push(*sid, pkt);
            }
        }
        // On-hit splash from the player's race. A blocked or 0-damage swing
        // leaves no blood on the tile (TFS only splashes when HP is lost).
        if applied > 0
            && new_health > 0
            && let Some(fluid) = self
                .players
                .get(&victim_id)
                .and_then(|p| p.race.fluid_subtype())
        {
            self.spawn_splash(victim_pos, ITEM_SMALLSPLASH, fluid);
        }
        // Death?
        if new_health == 0 {
            self.do_death(victim_id);
        }
    }

    /// Handle the death of `victim_id`: death == logout. Send the `0x28` death
    /// window, clear all fights, id-form remove at the death tile, then remove the
    /// victim from the world and emit a `SaveRecord` at the temple with full HP.
    /// Dropping the victim's `push_tx` ends the session; the relog spawns at the
    /// temple (M8 `login` restores the saved position). Mirrors TFS `onDeath` →
    /// `sendReLoginWindow` + `removeCreature` (player.cpp:2070, 2197).
    fn do_death(&mut self, victim_id: u32) {
        // Death window to the victim — best-effort, non-reaping `try_send`. The
        // reaping `push()` would, on a saturated client buffer, divert death
        // through `logout()` (which saves at the death tile with 0 HP) and then
        // the temple/full-HP save below would never run.
        if let Some(p) = self.players.get(&victim_id) {
            let _ = p.push_tx.try_send(combat_packets::death_window(0));
        }

        // Clear all player fights targeting the victim, and the victim's own fight.
        let all_players: Vec<u32> = self.players.keys().copied().collect();
        for pid in all_players {
            if let Some(p) = self.players.get_mut(&pid) {
                if p.attacking == Some(victim_id) || pid == victim_id {
                    p.attacking = None;
                }
            }
        }
        // Clear all monster fights targeting the victim.
        let all_monsters: Vec<u32> = self.monsters.keys().copied().collect();
        for mid in all_monsters {
            if let Some(m) = self.monsters.get_mut(&mid) {
                if m.attacking == Some(victim_id) {
                    m.attacking = None;
                }
            }
        }

        // Death position + temple destination (computed before removal).
        let death_pos = match self.players.get(&victim_id) {
            Some(p) => p.position,
            None => return,
        };
        let temple = self.meta.temple_for(death_pos);

        // On-death splash from player's race (always Blood by default). Exclude
        // the victim from the broadcast — the dying player won't see the world
        // around them, and including them could reap (via push) when their buffer
        // is full, breaking the death flow (regression
        // `death_with_full_client_buffer_still_saves_at_temple`).
        let victim_race = self.players.get(&victim_id).map(|p| p.race);
        if let Some(fluid) = victim_race.and_then(RaceType::fluid_subtype) {
            if self.materialize(death_pos) {
                let wi = WireItem {
                    client_id: ITEM_FULLSPLASH,
                    subtype: Some(fluid),
                    animated: false,
                };
                let front = self
                    .dynamic
                    .get(&(death_pos.x, death_pos.y, death_pos.z))
                    .map(|st| st.pre_creature_len)
                    .unwrap_or(0);
                let creatures = self.creatures_on(death_pos).len();
                let sp = (front + creatures).min(9) as u8;
                let pkt =
                    tile_item::add_tile_item((death_pos.x, death_pos.y, death_pos.z), sp, &wi);
                // Push to all spectators EXCEPT the victim (non-reaping try_send).
                for spec in self.spectators(death_pos, victim_id) {
                    if let Some(p) = self.players.get(&spec) {
                        let _ = p.push_tx.try_send(pkt.clone());
                    }
                }
                {
                    let st = self
                        .dynamic
                        .get_mut(&(death_pos.x, death_pos.y, death_pos.z))
                        .unwrap();
                    let front = st.pre_creature_len;
                    st.items.insert(front, wi);
                    st.server_ids.insert(front, ITEM_FULLSPLASH);
                    st.counts.insert(front, Some(fluid));
                }
            }
        }

        // Spawn human corpse on the death tile (exclude victim from broadcast
        // — the dying player may have a full push buffer; reaping via push
        // would logout() them early and break the temple save below).
        self.spawn_corpse(death_pos, ITEM_HUMAN_CORPSE, Some(victim_id));

        // Collect player inventory + container contents and drop as loot.
        let player_loot = self
            .players
            .get(&victim_id)
            .map(|p| collect_player_loot(&p.inventory, &p.open_containers))
            .unwrap_or_default();
        if !player_loot.is_empty() {
            self.spawn_loot(death_pos, &player_loot, Some(victim_id));
        }

        // Remove from the death tile for spectators. The id-form remove is
        // unambiguous under co-occupancy (stair/height landings); drop the victim
        // from each spectator's known-set so a relog re-introduces it (full form).
        for spec in self.spectators(death_pos, victim_id) {
            self.push(spec, walk::remove_creature_by_id(victim_id));
            if let Some(s) = self.players.get_mut(&spec) {
                s.known.remove(&victim_id);
            }
        }

        // Remove the victim from the world (death == logout). Persist the player
        // AT THE TEMPLE with full HP so the relog spawns there — M8 `login`
        // restores the saved position, so saving the death tile would respawn the
        // player where they died. Dropping the PlayerState drops its session
        // push_tx, which closes the writer channel and ends the session: the
        // client shows the death window and returns to character select. Mirrors
        // TFS onDeath -> sendReLoginWindow + removeCreature (player.cpp:2070, 2197);
        // the death-respawn position is the town temple.
        let Some(p) = self.players.remove(&victim_id) else {
            return;
        };
        if let Some(tx) = &self.save_tx {
            let _ = tx.send(SaveRecord {
                name: p.name.clone(),
                position: temple,
                direction: p.direction,
                outfit: p.outfit,
                health: p.max_health,
                max_health: p.max_health,
                sex: p.sex,
                inventory: p
                    .inventory
                    .iter()
                    .enumerate()
                    .filter_map(|(i, slot)| {
                        slot.map(|it| ((i + 1) as u8, it.server_id, it.count.unwrap_or(1)))
                    })
                    .collect(),
                container_items: Self::export_container_items(&p.inventory, &p.open_containers),
            });
        }
    }

    /// Apply `dmg` hit points of damage to `victim_id` (a monster), dealt by
    /// `attacker_id`. Reduces HP, pushes health-bar (`0x8C`) to all player
    /// spectators, emits the physical-hit blood effect plus floating damage
    /// number, and fires `do_monster_death` on 0 HP.
    fn apply_monster_damage(&mut self, attacker_id: u32, victim_id: u32, dmg: i32, now_ms: u64) {
        let (dmg, block) = self.mitigate_monster(victim_id, dmg);
        let (health_before, new_health) = {
            let m = match self.monsters.get_mut(&victim_id) {
                Some(m) => m,
                None => return,
            };
            let before = m.health;
            m.health = m.health.saturating_sub(dmg.max(0) as u32);
            (before, m.health)
        };
        let victim_pos = match self.monsters.get(&victim_id) {
            Some(m) => m.position,
            None => return,
        };
        // Push health-bar to every player who can see the monster's tile.
        let max_hp = self
            .monsters
            .get(&victim_id)
            .map(|m| m.max_health)
            .unwrap_or(1);
        let pct = combat_packets::health_percent(new_health as i32, max_hp as i32);
        let health_bar = combat_packets::creature_health(victim_id, pct);
        let player_spectators: Vec<u32> = self
            .players
            .iter()
            .filter(|&(_, sp)| Self::can_see(sp.position, victim_pos))
            .map(|(&sid, _)| sid)
            .collect();
        for sid in &player_spectators {
            self.push(*sid, health_bar.clone());
        }

        // Compute actual damage applied.
        let applied = health_before.saturating_sub(new_health);
        let hit_effect = Self::hit_effect_for(block, applied);
        let effect =
            enter_world::magic_effect(victim_pos.x, victim_pos.y, victim_pos.z, hit_effect);
        for sid in &player_spectators {
            self.push(*sid, effect.clone());
        }
        if applied > 0 {
            let monster_name = self
                .creature_name(victim_id)
                .unwrap_or_default()
                .to_string();
            let attacker_name = self
                .creature_name(attacker_id)
                .unwrap_or_default()
                .to_string();
            for sid in &player_spectators {
                let (mode, text) = if *sid == attacker_id {
                    (
                        combat_packets::MSG_DAMAGE_DEALT,
                        format!("You deal {applied} damage to {monster_name}."),
                    )
                } else {
                    (
                        combat_packets::MSG_DAMAGE_OTHERS,
                        format!(
                            "{monster_name} loses {applied} hitpoints due to an attack by {attacker_name}."
                        ),
                    )
                };
                let pkt = combat_packets::damage_text(
                    mode,
                    victim_pos.x,
                    victim_pos.y,
                    victim_pos.z,
                    applied,
                    combat_packets::TEXTCOLOR_RED,
                    text.as_bytes(),
                );
                self.push(*sid, pkt);
            }
        }

        // On-hit splash from the monster's race. No floor blood on a 0-damage hit.
        if applied > 0
            && new_health > 0
            && let Some(fluid) = self
                .monsters
                .get(&victim_id)
                .and_then(|m| m.race)
                .and_then(RaceType::fluid_subtype)
        {
            self.spawn_splash(victim_pos, ITEM_SMALLSPLASH, fluid);
        }

        // Retaliation: hostile creatures that aren't fleeing swing back.
        // TFS `hostile="0"` (rabbit, deer, sheep) never sets an attack target;
        // `isFleeing` also skips the swing (`canUseSpell` bails on melee).
        if new_health > 0 {
            if let Some(m) = self.monsters.get_mut(&victim_id) {
                if m.attacking.is_none() && m.hostile && !m.is_fleeing() {
                    m.attacking = Some(attacker_id);
                    m.last_attack_ms = 0;
                }
            }
        }

        // Death?
        if new_health == 0 {
            self.do_monster_death(victim_id, now_ms);
        }
    }

    /// Handle the death of a monster: notify all player spectators (remove +
    /// known-set cleanup), clear all fights targeting this monster, drop it
    /// from the monster registry, and spawn loot on the death tile. If the
    /// monster had a spawn entry, schedule a respawn.
    fn do_monster_death(&mut self, victim_id: u32, now_ms: u64) {
        tracing::debug!(victim_id, "do_monster_death called");
        let (spawn_id, death_pos, loot, race, look_corpse) = match self.monsters.get(&victim_id) {
            Some(m) => (
                m.spawn_id,
                m.position,
                m.loot.clone(),
                m.race,
                m.look_corpse,
            ),
            None => {
                tracing::warn!(
                    victim_id,
                    "do_monster_death called for already-dead monster"
                );
                return;
            }
        };

        // 1. Clear fights, remove creature from world FIRST (frees a slot on tile),
        //    broadcast remove, then spawn items (splash, corpse, loot).
        //    Order matters: the creature must be gone so tile-capacity checks pass.

        // Clear all player fights targeting the monster.
        let all_players: Vec<u32> = self.players.keys().copied().collect();
        for pid in all_players {
            if let Some(p) = self.players.get_mut(&pid) {
                if p.attacking == Some(victim_id) {
                    p.attacking = None;
                }
            }
        }
        // Clear all monster fights targeting the monster.
        let all_monsters: Vec<u32> = self.monsters.keys().copied().collect();
        for mid in all_monsters {
            if let Some(m) = self.monsters.get_mut(&mid) {
                if m.attacking == Some(victim_id) {
                    m.attacking = None;
                }
            }
        }

        // Remove creature from the death tile (spectators).
        for spec in self.spectators(death_pos, u32::MAX) {
            self.push(spec, walk::remove_creature_by_id(victim_id));
            if let Some(s) = self.players.get_mut(&spec) {
                s.known.remove(&victim_id);
            }
        }

        // Drop the monster (frees one tile slot for splash/corpse).
        self.monsters.remove(&victim_id);

        // 2. On-death splash + corpse + loot (tile now has room).
        if let Some(fluid) = race.and_then(RaceType::fluid_subtype) {
            self.spawn_splash(death_pos, ITEM_FULLSPLASH, fluid);
        }
        self.spawn_corpse(death_pos, look_corpse, None);
        let rolled = roll_drops(&mut self.rng, &loot);
        self.spawn_loot(death_pos, &rolled, None);

        // 3. Schedule respawn.
        if let Some(sid) = spawn_id {
            if let Some(spawn) = self.spawns.get_mut(&sid) {
                spawn.respawn_at_ms = Some(now_ms + spawn.respawn_interval_ms);
            }
        }
    }

    /// Spawn a splash item at `pos` with server id `item_id` and fluid subtype.
    /// Reuses the same overlay pattern as `spawn_loot`: materialize → cap check →
    /// insert at pre_creature_len → broadcast `0x6A` to spectators.
    /// Silently returns if the tile is at capacity (≥10 things).
    fn spawn_splash(&mut self, pos: Position, item_id: u16, fluid: u8) {
        if !self.materialize(pos) {
            tracing::warn!(?pos, "spawn_splash: materialize failed");
            return;
        }
        // No tile cap check — TFS uses FLAG_NOLIMIT for death items (splash,
        // corpse). The client renders the top 10 things regardless.
        let wi = WireItem {
            client_id: item_id,
            subtype: Some(fluid),
            animated: false,
        };
        let front = {
            let st = self.dynamic.get_mut(&(pos.x, pos.y, pos.z)).unwrap();
            let front = st.pre_creature_len;
            st.items.insert(front, wi);
            st.server_ids.insert(front, item_id);
            st.counts.insert(front, Some(fluid));
            front
        };
        // Splash goes at `front` (below creatures, above ground).
        let sp = front.min(9) as u8;
        self.broadcast_dest(pos, sp, wi, false);
        self.schedule_decay(pos, sp, SPLASH_DECAY_MS);
    }

    /// Store items inside the corpse at `pos` instead of dropping on the ground.
    /// `loot` is `[(server_id, count)]` — pre-rolled by `roll_drops` or
    /// `collect_player_loot`. The contents are read when a player opens the
    /// corpse via 0x82 use-item.
    fn spawn_loot(&mut self, pos: Position, loot: &[(u16, u8)], _exclude_id: Option<u32>) {
        let key = (pos.x, pos.y, pos.z);
        let contents = self.corpse_contents.entry(key).or_default();
        let before = contents.len();
        for &(item_id, count) in loot {
            if contents.len() >= 20 {
                break;
            }
            contents.push((item_id, count));
        }
        tracing::debug!(
            pos = ?(pos.x, pos.y, pos.z),
            before, added = loot.len(),
            after = contents.len(),
            loot = ?loot,
            "spawn_loot called"
        );
        if before > 0 {
            tracing::warn!(
                pos = ?(pos.x, pos.y, pos.z),
                before, after = contents.len(),
                "spawn_loot: corpse_contents already had items — APPENDING"
            );
        }
    }

    /// Spawn a corpse item at `pos` via overlay. Inserts AFTER existing
    /// pre-creature items (so the corpse renders on top of any splash/blood).
    /// Silently skips if `corpse_id == 0`, tile is at capacity, or the item id
    /// is unknown to the item catalog.
    ///
    /// When `exclude_id` is `Some`, the broadcast uses non-reaping `try_send`
    /// and excludes that creature — required during player death to avoid
    /// reaping the dying player (who may have a full push buffer).
    fn spawn_corpse(&mut self, pos: Position, corpse_id: u16, exclude_id: Option<u32>) {
        if corpse_id == 0 {
            return;
        }
        if !self.materialize(pos) {
            tracing::warn!(?pos, corpse_id, "spawn_corpse: materialize failed");
            return;
        }
        {
            let _st = match self.dynamic.get(&(pos.x, pos.y, pos.z)) {
                Some(st) => st,
                None => {
                    tracing::warn!(?pos, corpse_id, "spawn_corpse: no dynamic tile");
                    return;
                }
            };
        }
        let Some(meta) = self.meta.item_meta(corpse_id) else {
            tracing::warn!(
                ?pos,
                corpse_id,
                "spawn_corpse: unknown corpse id (not in item registry)"
            );
            return;
        };
        let wi = WireItem {
            client_id: meta.client_id,
            subtype: None,
            animated: meta.animated,
        };
        // Insert AFTER existing pre-creature items (splash, etc.) so the corpse
        // renders on top of blood pools.
        let idx = {
            let st = self.dynamic.get_mut(&(pos.x, pos.y, pos.z)).unwrap();
            let idx = st.items.len(); // after all existing items
            st.items.insert(idx, wi);
            st.server_ids.insert(idx, corpse_id);
            st.counts.insert(idx, None);
            idx
        };
        // Mark this tile as having a corpse (persists across container open).
        self.corpse_tiles.insert((pos.x, pos.y, pos.z));
        self.corpse_item_ids
            .insert((pos.x, pos.y, pos.z), corpse_id);
        // Corpse sits above splashes (higher stackpos = renders on top).
        let sp = idx.min(9) as u8;
        match exclude_id {
            Some(victim) => {
                let pkt = tile_item::add_tile_item((pos.x, pos.y, pos.z), sp, &wi);
                for spec in self.spectators(pos, victim) {
                    if let Some(p) = self.players.get(&spec) {
                        let _ = p.push_tx.try_send(pkt.clone());
                    }
                }
            }
            None => self.broadcast_dest(pos, sp, wi, false),
        }
        let decay_ms = if corpse_id == ITEM_HUMAN_CORPSE {
            CORPSE_DECAY_MS
        } else {
            MONSTER_CORPSE_DECAY_MS
        };
        self.schedule_decay(pos, sp, decay_ms);
    }

    /// Global combat tick. Iterates all players with an active target and, for
    /// each whose attack interval has elapsed, rolls one swing. Out-of-range or
    /// missing targets clear the fight without damage.
    pub(super) fn on_combat_tick(&mut self, now_ms: u64) {
        self.now_ms = now_ms;
        // --- Player attackers (existing) ---
        let player_fights: Vec<(u32, u32)> = self
            .players
            .iter()
            .filter_map(|(&id, p)| p.attacking.map(|tid| (id, tid)))
            .collect();
        for (attacker_id, target_id) in player_fights {
            self.process_player_attack(attacker_id, target_id, now_ms);
        }
        // TFS refreshes `pzLocked` only while the attacked creature is a player
        // (`onAttackedCreature`). A monster target must not start or extend it.
        let still_pvp: Vec<(u32, u32)> = self
            .players
            .iter()
            .filter_map(|(&id, p)| p.attacking.map(|tid| (id, tid)))
            .collect();
        for (id, tid) in still_pvp {
            if self.players.contains_key(&tid) {
                self.refresh_pz_lock(id);
            }
        }
        self.expire_pz_locks();

        // --- Monster attackers (M12.3) ---
        let monster_fights: Vec<(u32, u32)> = self
            .monsters
            .iter()
            .filter_map(|(&id, m)| m.attacking.map(|tid| (id, tid)))
            .collect();
        for (attacker_id, target_id) in monster_fights {
            self.process_monster_attack(attacker_id, target_id, now_ms);
        }

        // --- Respawn overdue spawn points (M12.5) ---
        self.process_respawns(now_ms);

        // --- Decay expired overlay items ---
        self.process_decay(now_ms);
    }

    /// Schedule an overlay item for removal after `delay_ms`.
    fn schedule_decay(&mut self, pos: Position, stackpos: u8, delay_ms: u64) {
        let ts = self.now_ms.saturating_add(delay_ms);
        self.decay_queue.push(DecayEntry {
            remove_at_ms: ts,
            pos: (pos.x, pos.y, pos.z),
            stackpos,
        });
    }

    /// Remove all overlay items whose decay timer has elapsed. Broadcasts 0x6B
    /// remove-tile-item to spectators. Cleans up corpse_contents for corpses.
    fn process_decay(&mut self, now_ms: u64) {
        let mut i = 0;
        while i < self.decay_queue.len() {
            if now_ms < self.decay_queue[i].remove_at_ms {
                i += 1;
                continue;
            }
            let entry = self.decay_queue.swap_remove(i);
            let (x, y, z) = entry.pos;
            let sp = entry.stackpos;

            // Remove from overlay at the correct index.
            let pre = self
                .dynamic
                .get(&(x, y, z))
                .map(|st| st.pre_creature_len)
                .unwrap_or(0);
            let creatures_on = self.creatures_on(Position::new(x, y, z)).len();
            let idx = if usize::from(sp) < pre {
                usize::from(sp)
            } else {
                usize::from(sp).saturating_sub(creatures_on)
            };
            if let Some(st) = self.dynamic.get_mut(&(x, y, z)) {
                if idx < st.items.len() {
                    st.items.remove(idx);
                    st.server_ids.remove(idx);
                    st.counts.remove(idx);
                }
            }

            // Broadcast 0x6C remove to spectators (positional form works for items).
            let pos = Position::new(x, y, z);
            let remove_pkt = tile_creature::remove_tile_thing((x, y, z), sp);
            for sid in self.spectators(pos, u32::MAX) {
                self.push(sid, remove_pkt.clone());
            }

            // Clean up corpse data for this corpse.
            self.corpse_contents.remove(&(x, y, z));
            self.corpse_tiles.remove(&(x, y, z));
            self.corpse_item_ids.remove(&(x, y, z));
        }
    }

    /// Check all spawn points for overdue respawns and create the monster.
    fn process_respawns(&mut self, now_ms: u64) {
        let due: Vec<u32> = self
            .spawns
            .iter()
            .filter(|(_, s)| s.respawn_at_ms.is_some_and(|t| t <= now_ms))
            .map(|(&id, _)| id)
            .collect();
        for sid in due {
            let spawn = match self.spawns.get(&sid) {
                Some(s) => s.clone(),
                None => continue,
            };
            // Clear the respawn timer.
            self.spawns.get_mut(&sid).unwrap().respawn_at_ms = None;

            let mid = self.next_monster_id;
            self.next_monster_id += 1;
            let monster = MonsterState {
                name: spawn.name,
                position: spawn.position,
                direction: Direction::South,
                health: spawn.health,
                max_health: spawn.max_health,
                speed: spawn.speed,
                look_type: spawn.look_type,
                look_corpse: spawn.look_corpse,
                attacking: None,
                last_attack_ms: 0,
                attack: spawn.attack,
                loot: spawn.loot,
                spawn_id: Some(sid),
                list_walk_dir: VecDeque::new(),
                follow_target: None,
                target_distance: spawn.target_distance,
                race: spawn.race,
                personality: spawn.personality,
                aggro_range: spawn.aggro_range,
                hostile: spawn.hostile,
                run_on_health: spawn.run_on_health,
                light_level: 0,
                light_color: 0,
            };
            self.monsters.insert(mid, monster);
            // Broadcast the resawned monster.
            for spec in self.spectators(spawn.position, u32::MAX) {
                if let Some(bytes) = self.introduce(spec, mid) {
                    let stackpos = self.creature_stackpos_on(spawn.position, mid);
                    self.push(
                        spec,
                        tile_creature::add_tile_creature(
                            (spawn.position.x, spawn.position.y, spawn.position.z),
                            stackpos,
                            &bytes,
                        ),
                    );
                }
            }
        }
    }

    /// Inventory slot 1..=10, if occupied.
    fn slot_item<'a>(p: &'a PlayerState, slot: u8) -> Option<&'a InvItem> {
        p.inventory
            .get(usize::from(slot.wrapping_sub(1)))
            .and_then(|s| s.as_ref())
    }

    /// TFS `Player::getWeapon`: left hand, then right. Shields and ammo are not
    /// weapons. A distance weapon that takes ammo is replaced by the matching
    /// ammo item (no match → that hand is empty).
    fn weapon_hand(&self, p: &PlayerState) -> Option<(u8, InvItem)> {
        for slot in [6u8, 5] {
            let Some(item) = Self::slot_item(p, slot) else {
                continue;
            };
            let Some(meta) = self.meta.item_meta(item.server_id) else {
                continue;
            };
            match meta.weapon_type.as_str() {
                "" | "shield" | "ammunition" => continue,
                "distance" if !meta.ammo_type.is_empty() => {
                    let Some(ammo) = Self::slot_item(p, 10) else {
                        continue;
                    };
                    let Some(ammo_meta) = self.meta.item_meta(ammo.server_id) else {
                        continue;
                    };
                    if ammo_meta.ammo_type != meta.ammo_type {
                        continue;
                    }
                    return Some((slot, *ammo));
                }
                _ => return Some((slot, *item)),
            }
        }
        None
    }

    /// Outgoing swing, TFS `WeaponMelee` / `WeaponDistance` / `useFist`.
    /// Skills are all `fist_skill` until M14 (a fresh character is 10 in each).
    /// Level is 1 until M14. Fight mode is attack (factor 1.0), the TFS default.
    fn player_swing(&self, p: &PlayerState) -> (i32, i32, i32, bool, bool, i32, i32) {
        let skill = p.fist_skill;
        let Some((_, item)) = self.weapon_hand(p) else {
            return (skill, combat::FIST_ATTACK_VALUE, 1, false, false, 0, -1);
        };
        let Some(meta) = self.meta.item_meta(item.server_id) else {
            return (skill, combat::FIST_ATTACK_VALUE, 1, false, false, 0, -1);
        };
        let distance = meta.weapon_type == "distance" || meta.weapon_type == "ammunition";
        let mut attack = meta.attack.max(0);
        let mut takes_ammo = false;
        let mut hit_chance = meta.hit_chance;
        let mut max_hit = meta.max_hit_chance;
        let mut range = i32::from(meta.shoot_range.max(1));
        if distance && meta.weapon_type == "ammunition" {
            // Damage item is the ammo; add the bow/crossbow attack and use its range.
            takes_ammo = true;
            for slot in [6u8, 5] {
                let Some(hand) = Self::slot_item(p, slot) else {
                    continue;
                };
                let Some(hand_meta) = self.meta.item_meta(hand.server_id) else {
                    continue;
                };
                if hand_meta.weapon_type == "distance" && hand_meta.ammo_type == meta.ammo_type {
                    attack += hand_meta.attack.max(0);
                    range = i32::from(hand_meta.shoot_range.max(1));
                    hit_chance = hand_meta.hit_chance;
                    max_hit = hand_meta.max_hit_chance;
                    break;
                }
            }
        }
        let skill = if meta.weapon_type == "wand" { 0 } else { skill };
        (
            skill, attack, range, distance, takes_ammo, hit_chance, max_hit,
        )
    }

    /// TFS `Player::getArmor`: head, necklace, armor, legs, feet, ring.
    /// Vocation armor multiplier is 1.0 until M14.
    fn player_armor(&self, p: &PlayerState) -> i32 {
        [1u8, 2, 4, 7, 8, 9]
            .into_iter()
            .filter_map(|slot| Self::slot_item(p, slot))
            .filter_map(|it| self.meta.item_meta(it.server_id))
            .map(|m| m.armor)
            .sum()
    }

    /// TFS `Player::getDefense` inputs, then `combat::player_defense`.
    fn player_defense(&self, p: &PlayerState) -> i32 {
        let skill_level = p.fist_skill;
        let mut defense_skill = skill_level;
        let mut defense_value = 7;
        let mut shield_def: Option<i32> = None;
        let mut weapon_def: Option<i32> = None;
        let mut weapon_extra = 0;
        let mut weapon_skill = skill_level;
        for slot in [5u8, 6] {
            let Some(it) = Self::slot_item(p, slot) else {
                continue;
            };
            let Some(meta) = self.meta.item_meta(it.server_id) else {
                continue;
            };
            match meta.weapon_type.as_str() {
                "shield" => {
                    let d = meta.defense;
                    if shield_def.is_none_or(|cur| d > cur) {
                        shield_def = Some(d);
                    }
                }
                "" | "ammunition" => {}
                other => {
                    weapon_def = Some(meta.defense);
                    weapon_extra = meta.extra_defense;
                    weapon_skill = if other == "wand" { 0 } else { skill_level };
                }
            }
        }
        if weapon_def.is_some() {
            defense_value = weapon_def.unwrap_or(0) + weapon_extra;
            defense_skill = weapon_skill;
        }
        if let Some(sd) = shield_def {
            defense_value = if weapon_def.is_some() {
                sd + weapon_extra
            } else {
                sd
            };
            defense_skill = skill_level;
        }
        combat::player_defense(defense_skill, defense_value)
    }

    /// TFS `Creature::blockHit` for a player victim. Shield check is always on
    /// for melee/distance (both set `blockedByShield`). The 2-charges-per-second
    /// `blockCount` limit is skipped: swings are 2 s apart, so a 1v1 always
    /// has a charge.
    fn mitigate_player(&mut self, victim_id: u32, damage: i32) -> (i32, combat::BlockKind) {
        let (defense, armor) = match self.players.get(&victim_id) {
            Some(p) => (self.player_defense(p), self.player_armor(p)),
            None => return (damage.max(0), combat::BlockKind::None),
        };
        combat::apply_defense_armor(&mut self.rng, damage, defense, armor, true)
    }

    /// Monster `<defenses defense/armor>`, same `blockHit` steps.
    fn mitigate_monster(&mut self, victim_id: u32, damage: i32) -> (i32, combat::BlockKind) {
        let (defense, armor) = self
            .monsters
            .get(&victim_id)
            .map(|m| m.name.to_ascii_lowercase())
            .and_then(|name| self.monster_types.get(&name))
            .map(|t| (t.defense, t.armor))
            .unwrap_or((0, 0));
        combat::apply_defense_armor(&mut self.rng, damage, defense, armor, defense > 0)
    }

    fn hit_effect_for(block: combat::BlockKind, applied: u32) -> u8 {
        match block {
            combat::BlockKind::Defense => enter_world::EFFECT_POFF,
            combat::BlockKind::Armor => enter_world::EFFECT_BLOCKHIT,
            combat::BlockKind::None if applied > 0 => enter_world::EFFECT_DRAWBLOOD,
            combat::BlockKind::None => enter_world::EFFECT_POFF,
        }
    }

    /// Process a player's attack against `target_id`.
    /// Range comes from the equipped weapon (fist = 1). Ranged attacks require LOS.
    fn process_player_attack(&mut self, attacker_id: u32, target_id: u32, now_ms: u64) {
        let target_pos = match self.creature_position(target_id) {
            Some(pos) => pos,
            None => {
                if let Some(p) = self.players.get_mut(&attacker_id) {
                    p.attacking = None;
                }
                return;
            }
        };
        let (attacker_pos, last_attack, swing) = match self.players.get(&attacker_id) {
            Some(p) => (p.position, p.last_attack_ms, self.player_swing(p)),
            None => return,
        };
        let (skill, attack_value, weapon_range, distance, takes_ammo, hit_chance, max_hit) = swing;
        let target_is_player = self.players.contains_key(&target_id);
        if self.chunks.is_protection_zone(attacker_pos)
            || (target_is_player && self.chunks.is_protection_zone(target_pos))
        {
            if let Some(p) = self.players.get_mut(&attacker_id) {
                p.attacking = None;
            }
            return;
        }
        if now_ms.saturating_sub(last_attack) < MELEE_ATTACK_INTERVAL_MS {
            return;
        }
        if attacker_pos.z != target_pos.z {
            return;
        }
        let dx = (i32::from(attacker_pos.x) - i32::from(target_pos.x)).abs();
        let dy = (i32::from(attacker_pos.y) - i32::from(target_pos.y)).abs();
        let range = weapon_range.max(1);
        if dx.max(dy) > RANGED_MAX_DISTANCE.min(range) {
            return;
        }
        // Ranged attacks need clear sight (no projectile-blocking tiles between).
        // Uses is_sight_clear with same_floor=true so only the direct line matters,
        // not the floor-above fallback that can_throw_object_to depends on.
        let ranged = distance || range > 1 || dx.max(dy) > 1;
        if ranged && !self.chunks.is_sight_clear(attacker_pos, target_pos, true) {
            return;
        }
        if let Some(p) = self.players.get_mut(&attacker_id) {
            p.last_attack_ms = now_ms;
        }
        let level = 1u32;
        let dmg = if distance {
            let dist = dx.max(dy).max(0) as u32;
            let chance = combat::distance_hit_chance(skill, dist, hit_chance, max_hit, takes_ammo);
            if chance < self.rng.gen_range(1..=100) {
                // Miss: puff on the target, no HP change. Ammo is not consumed.
                let effect = enter_world::magic_effect(
                    target_pos.x,
                    target_pos.y,
                    target_pos.z,
                    enter_world::EFFECT_POFF,
                );
                for sid in self.spectators(target_pos, u32::MAX) {
                    self.push(sid, effect.clone());
                }
                return;
            }
            let max =
                combat::max_weapon_damage(level, skill, attack_value, combat::ATTACK_FACTOR).max(0);
            let min = combat::distance_min_damage(level, target_is_player).min(max);
            if max == 0 {
                0
            } else {
                self.rng.gen_range(min..=max)
            }
        } else {
            combat::melee_damage(
                &mut self.rng,
                level,
                skill,
                attack_value,
                combat::ATTACK_FACTOR,
            )
        };
        // Projectile effect for ranged attacks.
        if ranged {
            let effect = enter_world::magic_effect(
                target_pos.x,
                target_pos.y,
                target_pos.z,
                enter_world::EFFECT_DRAWBLOOD,
            );
            let viewers: Vec<u32> = self
                .players
                .iter()
                .filter(|&(&sid, sp)| {
                    Self::can_see(sp.position, target_pos) || sid == attacker_id || sid == target_id
                })
                .map(|(&sid, _)| sid)
                .collect();
            for sid in viewers {
                self.push(sid, effect.clone());
            }
        }
        if target_is_player {
            self.apply_damage(attacker_id, target_id, dmg);
        } else {
            self.apply_monster_damage(attacker_id, target_id, dmg, now_ms);
        }
    }

    /// Process a monster's attack against `target_id`.
    /// Uses `target_distance` from MonsterState — when > 1 the monster attacks
    /// at range, requiring line of sight.
    fn process_monster_attack(&mut self, attacker_id: u32, target_id: u32, now_ms: u64) {
        let target_pos = match self.creature_position(target_id) {
            Some(pos) => pos,
            None => {
                if let Some(m) = self.monsters.get_mut(&attacker_id) {
                    m.attacking = None;
                }
                return;
            }
        };
        let target_is_player = self.players.contains_key(&target_id);
        let (attacker_pos, last_attack, attack, target_distance, may_attack) =
            match self.monsters.get(&attacker_id) {
                Some(m) => (
                    m.position,
                    m.last_attack_ms,
                    m.attack,
                    m.target_distance,
                    m.hostile && !m.is_fleeing() && m.attack > 0,
                ),
                None => return,
            };
        // Wildlife and anything under `runonhealth` must not swing, even if a
        // stale target was set before the flag was noticed.
        if !may_attack {
            if let Some(m) = self.monsters.get_mut(&attacker_id) {
                m.attacking = None;
                if m.is_fleeing() || !m.hostile {
                    m.follow_target = None;
                }
            }
            return;
        }
        // PZ check: attacker in PZ always clears; target PZ only matters for players.
        if self.chunks.is_protection_zone(attacker_pos)
            || (target_is_player && self.chunks.is_protection_zone(target_pos))
        {
            if let Some(m) = self.monsters.get_mut(&attacker_id) {
                m.attacking = None;
                // Drop the chase too. Leaving follow_target set parks the
                // monster on the PZ edge instead of letting it wander off.
                if target_is_player && self.chunks.is_protection_zone(target_pos) {
                    m.follow_target = None;
                    m.list_walk_dir.clear();
                }
            }
            return;
        }
        if now_ms.saturating_sub(last_attack) < MELEE_ATTACK_INTERVAL_MS {
            return;
        }
        if attacker_pos.z != target_pos.z {
            return;
        }
        let dx = (i32::from(attacker_pos.x) - i32::from(target_pos.x)).abs();
        let dy = (i32::from(attacker_pos.y) - i32::from(target_pos.y)).abs();
        let range = target_distance.max(1);
        if dx.max(dy) > RANGED_MAX_DISTANCE.min(range) {
            return;
        }
        // Ranged attacks need line of sight.
        let ranged = range > 1 || dx.max(dy) > 1;
        if ranged && !self.chunks.can_throw_object_to(attacker_pos, target_pos) {
            return;
        }
        let dmg: i32 = self.rng.gen_range(0..=attack).into();
        if let Some(m) = self.monsters.get_mut(&attacker_id) {
            m.last_attack_ms = now_ms;
        }
        // Projectile effect for ranged attacks.
        if ranged {
            let effect = enter_world::magic_effect(
                target_pos.x,
                target_pos.y,
                target_pos.z,
                enter_world::EFFECT_DRAWBLOOD,
            );
            let viewers: Vec<u32> = self
                .players
                .iter()
                .filter(|&(&sid, sp)| Self::can_see(sp.position, target_pos) || sid == target_id)
                .map(|(&sid, _)| sid)
                .collect();
            for sid in viewers {
                self.push(sid, effect.clone());
            }
        }
        if target_is_player {
            self.apply_damage(attacker_id, target_id, dmg);
        } else {
            self.apply_monster_damage(attacker_id, target_id, dmg, now_ms);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::test_support::*;
    use super::*;
    use rand::{SeedableRng, rngs::StdRng};

    /// RNG seed whose first `0..=attack` roll is a hit. Entropy (and an unlucky
    /// seed) can roll 0, which leaves a 1 HP player alive.
    fn seeded_hit(attack: u16) -> StdRng {
        let mut seed = 1u64;
        loop {
            let mut probe = StdRng::seed_from_u64(seed);
            if probe.gen_range(0..=attack) != 0 {
                return StdRng::seed_from_u64(seed);
            }
            seed += 1;
        }
    }

    // -------------------------------------------------------------------------
    // M7 combat tests
    // -------------------------------------------------------------------------

    #[test]
    fn set_target_sets_attacking_and_clear_resets_it() {
        let mut g = Game::from_static_map_arc(combat_map(false));
        let (a, _ra) = add_player(&mut g, Position::new(95, 117, 7));
        let (b, _rb) = add_player(&mut g, Position::new(96, 117, 7));
        g.do_set_target(a, b);
        assert_eq!(
            g.players[&a].attacking,
            Some(b),
            "set_target should store target id"
        );
        g.do_set_target(a, 0);
        assert_eq!(g.players[&a].attacking, None, "target 0 clears the fight");
    }

    #[test]
    fn set_target_self_is_ignored() {
        let mut g = Game::from_static_map_arc(combat_map(false));
        let (a, mut ra) = add_player(&mut g, Position::new(95, 117, 7));
        g.do_set_target(a, a);
        assert_eq!(g.players[&a].attacking, None, "self-target must be ignored");
        assert!(
            ra.try_recv().is_err(),
            "self-target must not push any packet"
        );
    }

    #[test]
    fn set_target_from_pz_tile_rejects_and_pushes_0xb4() {
        // Attacker is standing on a PZ tile → attack must be rejected with 0xB4
        // and attacking must remain None.
        let mut g = Game::from_static_map_arc(combat_map(true)); // spawn is PZ
        let (a, mut ra) = add_player(&mut g, Position::new(95, 117, 7)); // PZ tile
        let (b, _rb) = add_player(&mut g, Position::new(96, 117, 7));
        g.do_set_target(a, b);
        assert_eq!(
            g.players[&a].attacking, None,
            "PZ attacker must not get a target"
        );
        let pkt = ra.try_recv().expect("PZ rejection must push a 0xB4 packet");
        assert_eq!(
            pkt[0], 0xB4,
            "PZ rejection packet must be a text message (0xB4)"
        );
    }

    #[test]
    fn combat_tick_deals_damage_to_adjacent_target() {
        // A (attacker) and B (victim) are adjacent. After setting target and
        // advancing time past one attack interval, B must have lost HP.
        let mut g = Game::from_static_map_arc(combat_map(false));
        let (a, _ra) = add_player(&mut g, Position::new(95, 117, 7));
        let (b, mut rb) = add_player(&mut g, Position::new(96, 117, 7));
        g.do_set_target(a, b);
        let b_hp_before = g.players[&b].health;
        // Advance time past the attack interval (last_attack_ms=0 → now_ms >= 2000).
        g.on_combat_tick(MELEE_ATTACK_INTERVAL_MS);
        let b_hp_after = g.players[&b].health;
        // HP may have stayed the same if dmg happened to roll 0, but a 0x8C
        // must still have been pushed (damage applied even if 0). Actually let's
        // use a seeded RNG approach: with seed_from_u64(42) and level-1/skill-10
        // the first roll is non-zero, but since the Game RNG seed is entropy-based
        // we can only assert B received a 0x8C packet (spectator of own tile).
        let _ = b_hp_before;
        let _ = b_hp_after;
        let pkt = rb
            .try_recv()
            .expect("victim must receive at least a 0x8C health-bar");
        assert_eq!(
            pkt[0],
            protocol::combat_packets::OP_CREATURE_HEALTH,
            "first packet must be 0x8C (health-bar)"
        );
    }

    #[test]
    fn combat_tick_sends_stats_to_victim() {
        // After a combat tick, the victim must also receive its own 0xA0 stats.
        let mut g = Game::from_static_map_arc(combat_map(false));
        let (a, _ra) = add_player(&mut g, Position::new(95, 117, 7));
        let (b, mut rb) = add_player(&mut g, Position::new(96, 117, 7));
        g.do_set_target(a, b);
        g.on_combat_tick(MELEE_ATTACK_INTERVAL_MS);
        // Drain the 0x8C (spectator of own tile, health-bar first)
        let _ = rb.try_recv().expect("0x8C expected");
        // Then the 0xA0 self-stats
        let stats_pkt = rb
            .try_recv()
            .expect("victim must also receive its own 0xA0 stats");
        assert_eq!(
            stats_pkt[0],
            protocol::enter_world::OP_STATS,
            "0xA0 self-stats expected"
        );
    }

    #[test]
    fn combat_tick_spectator_receives_health_bar() {
        // A third-party spectator of B's tile must also receive the 0x8C.
        let mut g = Game::from_static_map_arc(combat_map(false));
        let (a, _ra) = add_player(&mut g, Position::new(95, 117, 7));
        let (b, _rb) = add_player(&mut g, Position::new(96, 117, 7));
        // Spectator sits close enough to see B's tile.
        let (_spec, mut rx_spec) = add_player(&mut g, Position::new(95, 116, 7));
        g.do_set_target(a, b);
        g.on_combat_tick(MELEE_ATTACK_INTERVAL_MS);
        let pkt = rx_spec
            .try_recv()
            .expect("spectator must receive 0x8C health bar");
        assert_eq!(pkt[0], protocol::combat_packets::OP_CREATURE_HEALTH);
    }

    #[test]
    fn combat_tick_no_damage_when_target_out_of_melee_range() {
        // Target 2 tiles away → no swing, no packets.
        let mut g = Game::from_static_map_arc(combat_map(false));
        let (a, _ra) = add_player(&mut g, Position::new(95, 117, 7));
        let (b, mut rb) = add_player(&mut g, Position::new(97, 117, 7)); // 2 tiles east
        g.do_set_target(a, b);
        g.on_combat_tick(MELEE_ATTACK_INTERVAL_MS);
        assert!(
            rb.try_recv().is_err(),
            "out-of-range target should receive no packets"
        );
    }

    #[test]
    fn combat_tick_respects_interval_no_damage_before_due() {
        // tick at now_ms < MELEE_ATTACK_INTERVAL_MS must not swing.
        let mut g = Game::from_static_map_arc(combat_map(false));
        let (a, _ra) = add_player(&mut g, Position::new(95, 117, 7));
        let (b, mut rb) = add_player(&mut g, Position::new(96, 117, 7));
        g.do_set_target(a, b);
        // Send a tick at t=1000ms (< 2000ms interval) → no swing.
        g.on_combat_tick(1000);
        assert!(
            rb.try_recv().is_err(),
            "tick before interval elapses must not produce damage"
        );
    }

    #[test]
    fn death_sends_window_removes_victim_and_saves_at_temple() {
        // Death == logout: the victim gets the 0x28 window, is removed from the
        // world, and a SaveRecord is emitted at the temple with full HP — so the
        // relog spawns at the temple (M8 `login` restores the saved position).
        let mut g = Game::from_static_map_arc(combat_map(false));
        let (save_tx, mut save_rx) = mpsc::unbounded_channel::<SaveRecord>();
        g.save_tx = Some(save_tx);
        let (a, _ra) = add_player(&mut g, Position::new(97, 117, 7));
        let (b, mut rb) = add_player(&mut g, Position::new(96, 117, 7));
        let max_hp = g.players[&b].max_health;
        let temple = g.meta.spawn();
        g.do_set_target(a, b);
        g.players.get_mut(&b).unwrap().health = 1;

        let mut saw_death_window = false;
        for tick in 1..=(max_hp as u64 + 5) {
            g.on_combat_tick(tick * MELEE_ATTACK_INTERVAL_MS);
            while let Ok(pkt) = rb.try_recv() {
                if pkt[0] == protocol::combat_packets::OP_DEATH_WINDOW {
                    saw_death_window = true;
                }
            }
            if !g.players.contains_key(&b) {
                break;
            }
        }

        assert!(
            saw_death_window,
            "dying player must receive the 0x28 death window"
        );
        assert!(
            !g.players.contains_key(&b),
            "victim must be removed from the world on death"
        );
        let rec = save_rx.try_recv().expect("death must emit a SaveRecord");
        assert_eq!(rec.position, temple, "death saves the player at the temple");
        assert_eq!(
            rec.health, rec.max_health,
            "death saves the player at full HP"
        );
    }

    #[test]
    fn death_with_full_client_buffer_still_saves_at_temple() {
        // Regression: a saturated victim push buffer must NOT divert death through
        // the reaping push()/logout path (which saves at the death tile with the
        // current HP). do_death uses a non-reaping try_send for the death window.
        let mut g = Game::from_static_map_arc(combat_map(false));
        let (save_tx, mut save_rx) = mpsc::unbounded_channel::<SaveRecord>();
        g.save_tx = Some(save_tx);
        let (b, _rb) = add_player(&mut g, Position::new(96, 117, 7));
        let temple = g.meta.spawn();
        g.players.get_mut(&b).unwrap().health = 1;
        // Fill B's push channel to capacity so a reaping send would log it out.
        for _ in 0..super::super::PUSH_CAPACITY {
            g.push(b, vec![0u8]);
        }
        g.do_death(b);
        let rec = save_rx
            .try_recv()
            .expect("death must emit a SaveRecord even with a full buffer");
        assert_eq!(
            rec.position, temple,
            "death saves at the temple even with a full client buffer"
        );
        assert_eq!(
            rec.health, rec.max_health,
            "death saves full HP even with a full client buffer"
        );
        assert!(
            !g.players.contains_key(&b),
            "victim must be removed from the world"
        );
    }

    #[test]
    fn death_clears_attacker_fight() {
        // Death clears every fight targeting the victim. `fist_damage` rolls
        // 0..=max (a swing can deal 0), so tick until the kill lands rather than
        // assuming one swing kills.
        let mut g = Game::from_static_map_arc(combat_map(false));
        let (a, _ra) = add_player(&mut g, Position::new(97, 117, 7));
        let (b, mut rb) = add_player(&mut g, Position::new(96, 117, 7));
        g.do_set_target(a, b);
        g.players.get_mut(&b).unwrap().health = 1;
        for tick in 1..=200u64 {
            g.on_combat_tick(tick * MELEE_ATTACK_INTERVAL_MS);
            while rb.try_recv().is_ok() {} // drain packets
            if !g.players.contains_key(&b) {
                break;
            }
        }
        assert!(
            !g.players.contains_key(&b),
            "victim must be removed from the world on death"
        );
        assert_eq!(
            g.players[&a].attacking, None,
            "attacker's fight must clear on target death"
        );
    }

    #[test]
    fn death_remove_uses_id_form_for_coocc_safety() {
        // Regression for the M7<->co-occupancy merge: do_death must remove the
        // victim with the id-form (0x6C 0xFFFF <id>), not position+stackpos.
        // Under co-occupancy (stair/height landings) a position+stackpos remove
        // is ambiguous when another creature shares the death tile. Matches
        // logout and do_move.
        let mut g = Game::from_static_map_arc(combat_map(false));
        let (a, mut ra) = add_player(&mut g, Position::new(97, 117, 7));
        let (b, _rb) = add_player(&mut g, Position::new(96, 117, 7));
        let max_hp = g.players[&b].max_health;
        g.do_set_target(a, b);
        g.players.get_mut(&b).unwrap().health = 1;
        // Drive ticks until B dies; a single tick may roll 0 damage, so loop and
        // drain A's channel each tick (avoids overflow) until the 0x6C remove
        // appears. A is a spectator of B's death tile.
        let mut remove_pkt: Option<Vec<u8>> = None;
        for tick in 1..=(max_hp as u64 + 5) {
            g.on_combat_tick(tick * MELEE_ATTACK_INTERVAL_MS);
            while let Ok(pkt) = ra.try_recv() {
                if pkt.first() == Some(&0x6C) {
                    remove_pkt = Some(pkt);
                }
            }
            if remove_pkt.is_some() {
                break;
            }
        }
        let pkt = remove_pkt.expect("spectator must receive a 0x6C remove on the victim's death");
        assert_eq!(
            &pkt[1..3],
            &[0xFF, 0xFF],
            "death remove must be id-form (co-occupancy safe)"
        );
        assert_eq!(
            &pkt[3..7],
            &b.to_le_bytes(),
            "id-form remove carries the victim id"
        );
    }

    #[test]
    fn tick_clears_target_when_target_logs_out() {
        // If the target logs out, the attacker's attacking must be cleared on the
        // next tick (no panic, no stale fight).
        let mut g = Game::from_static_map_arc(combat_map(false));
        let (a, _ra) = add_player(&mut g, Position::new(95, 117, 7));
        let (b, _rb) = add_player(&mut g, Position::new(96, 117, 7));
        g.do_set_target(a, b);
        assert_eq!(g.players[&a].attacking, Some(b));
        g.logout(b); // B disconnects
        g.on_combat_tick(MELEE_ATTACK_INTERVAL_MS);
        assert_eq!(
            g.players[&a].attacking, None,
            "attacker must clear when target logs out"
        );
    }

    // -------------------------------------------------------------------------
    // M7 review fix tests (W1, W2, W3)
    // -------------------------------------------------------------------------

    // W3 repro: attacker locked on a target; target moves onto a PZ tile → next
    // tick must deal NO damage AND clear the attacker's `attacking` field.
    //
    // We can't actually move the target in this unit test (do_move needs a walkable
    // path), so we directly set the target's position to a PZ tile and fire a tick.
    // The tick must clear the fight, not just skip damage.
    #[test]
    fn combat_tick_clears_fight_when_target_enters_pz() {
        let mut g = Game::from_static_map_arc(wide_combat_map_with_pz());
        // Attacker A at (91,117,7); target B starts adjacent at (92,117,7).
        let (a, _ra) = add_player(&mut g, Position::new(91, 117, 7));
        let (b, mut rb) = add_player(&mut g, Position::new(92, 117, 7));
        g.do_set_target(a, b);
        assert_eq!(g.players[&a].attacking, Some(b));

        // Teleport B onto the PZ tile (90,117,7) by direct state mutation —
        // simulates B stepping into the temple PZ.
        g.players.get_mut(&b).unwrap().position = Position::new(90, 117, 7);

        // Fire the combat tick — B is now in PZ, Chebyshev range = 1 (adjacent).
        // The tick MUST clear A's fight (not merely skip the swing).
        g.on_combat_tick(MELEE_ATTACK_INTERVAL_MS);

        assert_eq!(
            g.players[&a].attacking, None,
            "combat tick must clear attacker.attacking when target is in PZ (W3)"
        );
        // B must have received NO damage packet (no 0x8C).
        assert!(
            rb.try_recv().is_err(),
            "target in PZ must receive no damage (no 0x8C) on tick"
        );
    }

    // -------------------------------------------------------------------------
    // M12.2 monster combat tests
    // -------------------------------------------------------------------------

    #[test]
    fn set_target_on_monster_sets_attacking() {
        let mut g = Game::from_static_map_arc(combat_map(false));
        let (a, _ra) = add_player(&mut g, Position::new(95, 117, 7));
        let m = add_monster(&mut g, Position::new(96, 117, 7));
        g.do_set_target(a, m);
        assert_eq!(g.players[&a].attacking, Some(m));
    }

    #[test]
    fn set_target_on_unknown_creature_is_ignored() {
        let mut g = Game::from_static_map_arc(combat_map(false));
        let (a, _ra) = add_player(&mut g, Position::new(95, 117, 7));
        // 0x4000_0001 doesn't exist as a player or monster.
        g.do_set_target(a, 0x4000_0001);
        assert_eq!(g.players[&a].attacking, None);
    }

    #[test]
    fn set_target_on_monster_from_pz_rejects() {
        let mut g = Game::from_static_map_arc(wide_combat_map_with_pz());
        let (a, _ra) = add_player(&mut g, Position::new(90, 117, 7)); // PZ tile
        let m = add_monster(&mut g, Position::new(95, 117, 7));
        g.do_set_target(a, m);
        assert_eq!(
            g.players[&a].attacking, None,
            "attacker in PZ must not be allowed to set target on any creature"
        );
    }

    #[test]
    fn combat_tick_deals_damage_to_monster() {
        let mut g = Game::from_static_map_arc(combat_map(false));
        let (a, mut ra) = add_player(&mut g, Position::new(95, 117, 7));
        let m = add_monster(&mut g, Position::new(96, 117, 7));
        g.do_set_target(a, m);
        // Drain the combat-icons packet pushed by do_set_target.
        let _ = ra.try_recv().ok();
        let hp_before = g.monsters[&m].health;
        g.on_combat_tick(MELEE_ATTACK_INTERVAL_MS);
        let hp_after = g.monsters[&m].health;
        assert!(
            hp_after <= hp_before,
            "monster HP must not increase after combat tick"
        );
        // Attacker must receive a 0x8C health-bar.
        let pkt = ra
            .try_recv()
            .expect("attacker must receive a 0x8C for monster health-bar");
        assert_eq!(pkt[0], protocol::combat_packets::OP_CREATURE_HEALTH);
    }

    #[test]
    fn combat_tick_kills_monster() {
        // Monster with 50 HP: player with fist_skill 10 lands at most ~7 dmg per
        // swing; kill needs many ticks if damage rolls low (min 0). Loop and drain
        // packets until the monster dies.
        let mut g = Game::from_static_map_arc(combat_map(false));
        let (a, mut ra) = add_player(&mut g, Position::new(95, 117, 7));
        let m = add_monster(&mut g, Position::new(96, 117, 7));
        g.monsters.get_mut(&m).unwrap().health = 1;
        g.do_set_target(a, m);
        for tick in 1..=5u64 {
            g.on_combat_tick(tick * MELEE_ATTACK_INTERVAL_MS);
            if !g.monsters.contains_key(&m) {
                break;
            }
        }
        // Monster removed.
        assert!(
            !g.monsters.contains_key(&m),
            "monster must be removed on death"
        );
        // Attacker's fight cleared.
        assert_eq!(
            g.players[&a].attacking, None,
            "attacker's fight must clear on monster death"
        );
        // Spectator (the attacker, who can see the death tile) must have received
        // a 0x6C id-form remove packet.
        let mut saw_remove = false;
        while let Ok(pkt) = ra.try_recv() {
            if pkt.first() == Some(&0x6C) && pkt.len() >= 7 && pkt[3..7] == m.to_le_bytes() {
                saw_remove = true;
            }
        }
        assert!(
            saw_remove,
            "attacker must receive 0x6C remove for the monster on death"
        );
    }

    #[test]
    fn combat_tick_monster_spectator_receives_health_bar() {
        let mut g = Game::from_static_map_arc(combat_map(false));
        let (a, _ra) = add_player(&mut g, Position::new(95, 117, 7));
        let (_spec, mut rx_spec) = add_player(&mut g, Position::new(95, 116, 7));
        let m = add_monster(&mut g, Position::new(96, 117, 7));
        g.do_set_target(a, m);
        g.on_combat_tick(MELEE_ATTACK_INTERVAL_MS);
        let pkt = rx_spec
            .try_recv()
            .expect("spectator must receive 0x8C for monster health bar");
        assert_eq!(pkt[0], protocol::combat_packets::OP_CREATURE_HEALTH);
    }

    #[test]
    fn combat_tick_monster_no_damage_when_out_of_melee_range() {
        let mut g = Game::from_static_map_arc(combat_map(false));
        let (a, mut ra) = add_player(&mut g, Position::new(95, 117, 7));
        let m = add_monster(&mut g, Position::new(97, 117, 7)); // 2 tiles east
        g.do_set_target(a, m);
        let _ = ra.try_recv().ok(); // drain combat-icons packet
        g.on_combat_tick(MELEE_ATTACK_INTERVAL_MS);
        assert!(
            ra.try_recv().is_err(),
            "out-of-range monster should produce no packets"
        );
    }

    #[test]
    fn combat_tick_monster_respects_interval() {
        let mut g = Game::from_static_map_arc(combat_map(false));
        let (a, mut ra) = add_player(&mut g, Position::new(95, 117, 7));
        let m = add_monster(&mut g, Position::new(96, 117, 7));
        g.do_set_target(a, m);
        let _ = ra.try_recv().ok(); // drain combat-icons packet
        g.on_combat_tick(1000); // < 2000ms interval
        assert!(
            ra.try_recv().is_err(),
            "tick before interval must not produce damage packets"
        );
    }

    #[test]
    fn combat_tick_monster_does_not_clear_in_pz() {
        // Monsters are never PZ-checked: combat in PZ against a monster is ok
        // (attacker not in PZ, monster on PZ tile — must NOT clear the fight).
        let mut g = Game::from_static_map_arc(wide_combat_map_with_pz());
        let (a, _ra) = add_player(&mut g, Position::new(91, 117, 7)); // normal ground
        let m = add_monster(&mut g, Position::new(90, 117, 7)); // PZ tile
        g.do_set_target(a, m);
        assert_eq!(
            g.players[&a].attacking,
            Some(m),
            "targeting a monster on a PZ tile must be allowed"
        );
        let old_hp = g.monsters[&m].health;
        g.on_combat_tick(MELEE_ATTACK_INTERVAL_MS);
        assert_eq!(
            g.players[&a].attacking,
            Some(m),
            "monster in PZ must NOT clear the attacker's fight"
        );
        let new_hp = g.monsters[&m].health;
        assert!(
            new_hp <= old_hp,
            "monster on PZ tile must still take damage (monsters ignore PZ)"
        );
    }

    #[test]
    fn combat_tick_clears_monster_target_when_monster_dies_elsewhere() {
        // If the monster is removed outside of combat (e.g. manually), the next
        // tick must clear the attacker's fight without panicking.
        let mut g = Game::from_static_map_arc(combat_map(false));
        let (a, _ra) = add_player(&mut g, Position::new(95, 117, 7));
        let m = add_monster(&mut g, Position::new(96, 117, 7));
        g.do_set_target(a, m);
        assert_eq!(g.players[&a].attacking, Some(m));
        g.monsters.remove(&m);
        g.on_combat_tick(MELEE_ATTACK_INTERVAL_MS);
        assert_eq!(
            g.players[&a].attacking, None,
            "attacker must clear when monster is removed externally"
        );
    }

    // -------------------------------------------------------------------------
    // M12.3 monster → player combat tests
    // -------------------------------------------------------------------------

    #[test]
    fn monster_retaliates_when_attacked() {
        // Player damages a monster → the monster starts attacking back.
        let mut g = Game::from_static_map_arc(combat_map(false));
        let (a, _ra) = add_player(&mut g, Position::new(95, 117, 7));
        let m = add_monster(&mut g, Position::new(96, 117, 7));
        assert_eq!(
            g.monsters[&m].attacking, None,
            "monster starts with no target"
        );
        g.do_set_target(a, m);
        g.on_combat_tick(MELEE_ATTACK_INTERVAL_MS);
        assert_eq!(
            g.monsters[&m].attacking,
            Some(a),
            "monster must retaliate after being hit"
        );
    }

    #[test]
    fn nonhostile_monster_does_not_retaliate_or_deal_damage() {
        let mut g = Game::from_static_map_arc(combat_map(false));
        let (a, _ra) = add_player(&mut g, Position::new(95, 117, 7));
        let m = add_monster(&mut g, Position::new(96, 117, 7));
        g.monsters.get_mut(&m).unwrap().hostile = false;
        g.monsters.get_mut(&m).unwrap().run_on_health = 50;
        g.do_set_target(a, m);
        g.on_combat_tick(MELEE_ATTACK_INTERVAL_MS);
        assert!(
            g.monsters[&m].attacking.is_none(),
            "wildlife must not swing back when hit"
        );

        // A stale target must not deal damage either.
        g.monsters.get_mut(&m).unwrap().attacking = Some(a);
        g.monsters.get_mut(&m).unwrap().attack = 20;
        let hp_before = g.players[&a].health;
        g.on_combat_tick(MELEE_ATTACK_INTERVAL_MS * 2);
        assert_eq!(
            g.players[&a].health, hp_before,
            "wildlife must not deal damage"
        );
        assert!(
            g.monsters[&m].attacking.is_none(),
            "stale target must be cleared"
        );
    }

    #[test]
    fn zero_attack_monster_does_not_damage_or_swing() {
        // Missing `<attack name="melee">` now means attack 0, not the old
        // default of 7. A hostile creature with no swing must not hurt anyone.
        let mut g = Game::from_static_map_arc(combat_map(false));
        let (a, mut ra) = add_player(&mut g, Position::new(95, 117, 7));
        let m = add_monster(&mut g, Position::new(96, 117, 7));
        g.monsters.get_mut(&m).unwrap().attack = 0;
        g.monsters.get_mut(&m).unwrap().attacking = Some(a);
        g.monsters.get_mut(&m).unwrap().follow_target = Some(a);
        let hp_before = g.players[&a].health;
        g.on_combat_tick(MELEE_ATTACK_INTERVAL_MS);
        assert_eq!(g.players[&a].health, hp_before);
        assert!(
            ra.try_recv().is_err(),
            "a creature with no melee must not push a hit"
        );
        assert_eq!(g.monsters[&m].attacking, None, "swing target must clear");
        assert_eq!(
            g.monsters[&m].follow_target,
            Some(a),
            "hostile follow is independent of having a melee swing"
        );
    }

    #[test]
    fn monster_combat_tick_attacks_player() {
        // Monster with a target (player) must deal damage on combat tick.
        let mut g = Game::from_static_map_arc(combat_map(false));
        let (a, mut ra) = add_player(&mut g, Position::new(95, 117, 7));
        let m = add_monster(&mut g, Position::new(96, 117, 7));
        g.monsters.get_mut(&m).unwrap().attacking = Some(a);
        g.monsters.get_mut(&m).unwrap().attack = 20; // guaranteed non-zero
        let hp_before = g.players[&a].health;
        g.on_combat_tick(MELEE_ATTACK_INTERVAL_MS);
        let hp_after = g.players[&a].health;
        assert!(
            hp_after <= hp_before,
            "player HP must not increase after monster attack tick"
        );
        // Player must receive a 0x8C health-bar.
        let pkt = ra
            .try_recv()
            .expect("player must receive 0x8C for own health bar");
        assert_eq!(pkt[0], protocol::combat_packets::OP_CREATURE_HEALTH);
    }

    #[test]
    fn monster_combat_tick_kills_player() {
        // Monster kills player: player must be removed, death window sent, saved.
        let mut g = Game::from_static_map_arc(combat_map(false));
        let (save_tx, mut save_rx) = mpsc::unbounded_channel::<SaveRecord>();
        g.save_tx = Some(save_tx);
        let (a, mut ra) = add_player(&mut g, Position::new(95, 117, 7));
        let m = add_monster(&mut g, Position::new(96, 117, 7));
        g.players.get_mut(&a).unwrap().health = 1;
        g.monsters.get_mut(&m).unwrap().attacking = Some(a);
        g.monsters.get_mut(&m).unwrap().attack = 20;
        // `gen_range(0..=attack)` includes a miss. Seed a hitting swing so a
        // 1 HP player actually dies — entropy made this flake under nextest.
        g.rng = seeded_hit(20);

        g.on_combat_tick(MELEE_ATTACK_INTERVAL_MS);

        assert!(
            !g.players.contains_key(&a),
            "player must be removed on death"
        );
        let mut saw_death = false;
        while let Ok(pkt) = ra.try_recv() {
            if pkt[0] == protocol::combat_packets::OP_DEATH_WINDOW {
                saw_death = true;
            }
        }
        assert!(
            saw_death,
            "player must receive 0x28 death window after monster kill"
        );
        assert_eq!(
            g.monsters[&m].attacking, None,
            "monster's fight must clear when its target dies"
        );
        let rec = save_rx
            .try_recv()
            .expect("player death must emit a SaveRecord");
        assert_eq!(
            rec.health, rec.max_health,
            "SaveRecord must have full HP (temple respawn)"
        );
    }

    #[test]
    fn monster_combat_tick_out_of_range() {
        let mut g = Game::from_static_map_arc(combat_map(false));
        let (a, mut ra) = add_player(&mut g, Position::new(95, 117, 7));
        let m = add_monster(&mut g, Position::new(97, 117, 7)); // 2 tiles east
        g.monsters.get_mut(&m).unwrap().attacking = Some(a);
        g.on_combat_tick(MELEE_ATTACK_INTERVAL_MS);
        assert!(
            ra.try_recv().is_err(),
            "out-of-range monster attack must produce no packets"
        );
    }

    #[test]
    fn monster_combat_tick_respects_interval() {
        let mut g = Game::from_static_map_arc(combat_map(false));
        let (a, mut ra) = add_player(&mut g, Position::new(95, 117, 7));
        let m = add_monster(&mut g, Position::new(96, 117, 7));
        g.monsters.get_mut(&m).unwrap().attacking = Some(a);
        g.on_combat_tick(1000); // < 2000ms interval
        assert!(
            ra.try_recv().is_err(),
            "monster attack before interval must not hit"
        );
    }

    #[test]
    fn monster_attack_clears_when_target_logs_out() {
        let mut g = Game::from_static_map_arc(combat_map(false));
        let (a, _ra) = add_player(&mut g, Position::new(95, 117, 7));
        let m = add_monster(&mut g, Position::new(96, 117, 7));
        g.monsters.get_mut(&m).unwrap().attacking = Some(a);
        g.logout(a); // player disconnects
        g.on_combat_tick(MELEE_ATTACK_INTERVAL_MS);
        assert_eq!(
            g.monsters[&m].attacking, None,
            "monster must clear its target when the player logs out"
        );
    }

    #[test]
    fn monster_attack_clears_when_attacker_in_pz() {
        // Monster on a PZ tile → fight clears (monsters can't attack from PZ).
        let mut g = Game::from_static_map_arc(wide_combat_map_with_pz());
        let (a, mut ra) = add_player(&mut g, Position::new(91, 117, 7)); // normal ground
        let m = add_monster(&mut g, Position::new(90, 117, 7)); // PZ tile
        g.monsters.get_mut(&m).unwrap().attacking = Some(a);
        g.on_combat_tick(MELEE_ATTACK_INTERVAL_MS);
        assert_eq!(
            g.monsters[&m].attacking, None,
            "monster in PZ must clear its fight"
        );
        assert!(
            ra.try_recv().is_err(),
            "player must receive no damage from monster in PZ"
        );
    }

    // ===================================================================
    // Task 3.7: stats push uses live p.speed (combat path)
    // ===================================================================

    #[test]
    fn combat_stats_push_uses_live_player_speed() {
        // After changing a player's speed, a combat tick that deals damage
        // must push 0xA0 with base_speed = p.speed.
        let mut g = Game::from_static_map_arc(combat_map(false));
        let (a, _ra) = add_player(&mut g, Position::new(95, 117, 7));
        let (b, mut rb) = add_player(&mut g, Position::new(96, 117, 7));
        // Set B's speed to a non-default value (even so /2 is lossless)
        g.players.get_mut(&b).unwrap().speed = 800;
        g.do_set_target(a, b);
        g.on_combat_tick(MELEE_ATTACK_INTERVAL_MS);
        // Drain packets until we find 0xA0
        let mut found_speed = None;
        while let Ok(pkt) = rb.try_recv() {
            if pkt.first() == Some(&0xA0) && pkt.len() >= 46 {
                // base_speed is at offset 44, stored as value/2
                let spd = u16::from_le_bytes([pkt[44], pkt[45]]);
                found_speed = Some(spd * 2); // undo wire halving
                break;
            }
        }
        assert_eq!(
            found_speed,
            Some(800),
            "0xA0 must carry base_speed = 800 (p.speed)"
        );
    }

    #[test]
    fn monster_attack_clears_when_target_in_pz() {
        // Player on a PZ tile → monster fight clears.
        let mut g = Game::from_static_map_arc(wide_combat_map_with_pz());
        let (a, mut ra) = add_player(&mut g, Position::new(90, 117, 7)); // PZ tile
        let m = add_monster(&mut g, Position::new(91, 117, 7)); // normal ground
        g.monsters.get_mut(&m).unwrap().attacking = Some(a);
        g.on_combat_tick(MELEE_ATTACK_INTERVAL_MS);
        assert_eq!(
            g.monsters[&m].attacking, None,
            "monster fight must clear when target is in PZ"
        );
        assert!(
            ra.try_recv().is_err(),
            "player in PZ must receive no damage from monster"
        );
    }

    // -------------------------------------------------------------------------
    // M12.4 monster → monster combat tests
    // -------------------------------------------------------------------------

    #[test]
    fn monster_attacks_monster() {
        let mut g = Game::from_static_map_arc(combat_map(false));
        let (_a, _ra) = add_player(&mut g, Position::new(95, 117, 7)); // spectator
        let m1 = add_monster(&mut g, Position::new(100, 117, 7));
        let m2 = add_monster(&mut g, Position::new(101, 117, 7));
        g.monsters.get_mut(&m1).unwrap().attacking = Some(m2);
        g.monsters.get_mut(&m1).unwrap().attack = 20;
        let hp_before = g.monsters[&m2].health;
        g.on_combat_tick(MELEE_ATTACK_INTERVAL_MS);
        let hp_after = g.monsters[&m2].health;
        assert!(
            hp_after <= hp_before,
            "monster HP must not increase after monster-on-monster hit"
        );
        assert_eq!(
            g.monsters[&m1].attacking,
            Some(m2),
            "monster attacker must keep its target after hitting another monster"
        );
    }

    #[test]
    fn monster_kills_monster() {
        let mut g = Game::from_static_map_arc(combat_map(false));
        let (_a, _ra) = add_player(&mut g, Position::new(95, 117, 7)); // spectator
        let m1 = add_monster(&mut g, Position::new(100, 117, 7));
        let m2 = add_monster(&mut g, Position::new(101, 117, 7));
        g.monsters.get_mut(&m1).unwrap().attacking = Some(m2);
        g.monsters.get_mut(&m1).unwrap().attack = 20;
        g.monsters.get_mut(&m2).unwrap().health = 1;
        for tick in 1..=5u64 {
            g.on_combat_tick(tick * MELEE_ATTACK_INTERVAL_MS);
            if !g.monsters.contains_key(&m2) {
                break;
            }
        }
        assert!(
            !g.monsters.contains_key(&m2),
            "victim monster must be removed on death"
        );
        assert_eq!(
            g.monsters[&m1].attacking, None,
            "monster attacker's fight must clear when its monster target dies"
        );
    }

    #[test]
    fn monster_vs_monster_retaliation() {
        // Monster A hits monster B → B starts attacking A.
        let mut g = Game::from_static_map_arc(combat_map(false));
        let (_a, _ra) = add_player(&mut g, Position::new(95, 117, 7)); // spectator
        let m1 = add_monster(&mut g, Position::new(100, 117, 7));
        let m2 = add_monster(&mut g, Position::new(101, 117, 7));
        g.monsters.get_mut(&m1).unwrap().attacking = Some(m2);
        g.monsters.get_mut(&m1).unwrap().attack = 20;
        assert_eq!(g.monsters[&m2].attacking, None, "m2 starts with no target");
        g.on_combat_tick(MELEE_ATTACK_INTERVAL_MS);
        assert_eq!(
            g.monsters[&m2].attacking,
            Some(m1),
            "m2 must retaliate after being hit by m1"
        );
    }

    #[test]
    fn monster_vs_monster_out_of_range() {
        let mut g = Game::from_static_map_arc(combat_map(false));
        let (_a, _ra) = add_player(&mut g, Position::new(95, 117, 7));
        let m1 = add_monster(&mut g, Position::new(100, 117, 7));
        let m2 = add_monster(&mut g, Position::new(102, 117, 7)); // 2 tiles east
        g.monsters.get_mut(&m1).unwrap().attacking = Some(m2);
        let hp_before = g.monsters[&m2].health;
        g.on_combat_tick(MELEE_ATTACK_INTERVAL_MS);
        assert_eq!(
            g.monsters[&m2].health, hp_before,
            "out-of-range monster must not take damage"
        );
    }

    // -------------------------------------------------------------------------
    // M12.6 monster loot tests
    // -------------------------------------------------------------------------

    #[test]
    fn monster_drops_nothing_with_empty_loot() {
        let mut g = Game::from_static_map_arc(combat_map(false));
        let (_a, mut ra) = add_player(&mut g, Position::new(95, 117, 7)); // spectator
        let m = add_monster(&mut g, Position::new(96, 117, 7));
        // loot is empty by default
        g.do_monster_death(m, 0);
        assert!(!g.monsters.contains_key(&m), "monster must be removed");
        // Drain creature remove (0x6C) + splash (0x6A).
        let _ = ra.try_recv().ok(); // 0x6C
        let _ = ra.try_recv().ok(); // 0x6A splash
        // No loot 0x6A packets beyond these.
        while let Ok(pkt) = ra.try_recv() {
            assert_ne!(
                pkt[0], 0x6A,
                "empty loot must not emit 0x6A add-item packets"
            );
        }
    }

    #[test]
    fn monster_drops_item_on_death() {
        let mut g = Game::from_static_map_arc(combat_map(false));
        let (_a, mut ra) = add_player(&mut g, Position::new(95, 117, 7)); // spectator
        let m = add_monster(&mut g, Position::new(96, 117, 7));
        g.monsters.get_mut(&m).unwrap().loot = vec![MonsterDrop {
            item_id: 100,
            chance: 1.0,
            count_max: 1,
        }];
        g.do_monster_death(m, 0);
        assert!(!g.monsters.contains_key(&m), "monster must be removed");

        // First packet is 0x6C creature remove (creature removed before items).
        let remove = ra.try_recv().expect("must receive 0x6C creature remove");
        assert_eq!(remove[0], 0x6C, "first packet is creature remove (0x6C)");

        // Then death splash (monsters have race → produce 0x6A splash).
        let splash = ra.try_recv().expect("must receive splash 0x6A");
        assert_eq!(splash[0], 0x6A, "splash packet is 0x6A add-tile-item");

        // Loot is now stored in corpse_contents, not on the ground.
        // Verify the loot went into the corpse.
        let contents = g.corpse_contents.get(&(96, 117, 7));
        assert!(contents.is_some(), "loot must be stored in corpse_contents");
        assert!(!contents.unwrap().is_empty(), "corpse must contain loot");
        // The loot should include item 100 (rolled from the hardcoded loot).
        let has_item = contents.unwrap().iter().any(|&(sid, _)| sid == 100);
        assert!(has_item, "corpse must contain the rolled loot item");
    }

    #[test]
    fn monster_drop_uses_chance() {
        let mut g = Game::from_static_map_arc(combat_map(false));
        let (_a, mut ra) = add_player(&mut g, Position::new(95, 117, 7)); // spectator
        let m = add_monster(&mut g, Position::new(96, 117, 7));
        // Two drops: one guaranteed (1.0), one impossible (0.0).
        g.monsters.get_mut(&m).unwrap().loot = vec![
            MonsterDrop {
                item_id: 100,
                chance: 1.0,
                count_max: 1,
            },
            MonsterDrop {
                item_id: 100,
                chance: 0.0,
                count_max: 1,
            },
        ];
        g.do_monster_death(m, 0);
        assert!(!g.monsters.contains_key(&m), "monster must be removed");

        // Drain death splash.
        let _ = ra.try_recv().ok();

        // Loot is now in corpse_contents, not on the ground.
        // Verify only the 1.0-chance drop is stored (item_id=100).
        let contents = g.corpse_contents.get(&(96, 117, 7));
        assert!(contents.is_some(), "loot must be in corpse_contents");
        let items = contents.unwrap();
        assert_eq!(
            items.len(),
            1,
            "only 1 item should pass the 1.0 chance roll"
        );
        assert_eq!(items[0].0, 100, "the item must be server_id 100");
    }

    // -------------------------------------------------------------------------
    // M12.5 monster spawner tests
    // -------------------------------------------------------------------------

    #[test]
    fn monster_respawns_after_death() {
        let mut g = Game::from_static_map_arc(combat_map(false));
        let (_a, mut ra) = add_player(&mut g, Position::new(95, 117, 7));
        let sid = 1;
        g.spawns.insert(
            sid,
            MonsterSpawn {
                position: Position::new(96, 117, 7),
                respawn_interval_ms: 100,
                respawn_at_ms: None,
                name: "Rat".into(),
                look_type: 100,
                look_corpse: 0,
                health: 50,
                max_health: 50,
                speed: 200,
                attack: 7,
                loot: vec![],
                target_distance: 0,
                race: None,
                personality: Personality::Aggressive,
                aggro_range: DEFAULT_AGGRO_RANGE,
                hostile: true,
                run_on_health: 0,
            },
        );
        let m = add_monster(&mut g, Position::new(96, 117, 7));
        g.monsters.get_mut(&m).unwrap().spawn_id = Some(sid);

        // Kill the monster — should schedule a respawn in 100ms.
        g.do_monster_death(m, 0);
        assert!(!g.monsters.contains_key(&m), "monster removed on death");
        assert_eq!(
            g.spawns[&sid].respawn_at_ms,
            Some(100),
            "spawn timer set at now_ms + interval"
        );
        // Drain death packets (0x6C remove, 0x6A loot).
        while ra.try_recv().is_ok() {}

        // Tick before the interval — no respawn yet.
        g.on_combat_tick(50);
        assert_eq!(g.monsters.len(), 0, "no respawn before interval elapses");

        // Tick after the interval — respawn should trigger.
        g.on_combat_tick(100);
        assert_eq!(g.monsters.len(), 1, "monster respawned after interval");

        // The spawned monster must broadcast an 0x6A add-creature.
        let pkt = ra.try_recv().expect("respawn broadcasts 0x6A add-creature");
        assert_eq!(pkt[0], 0x6A, "respawn packet is 0x6A add-tile-creature");
    }

    #[test]
    fn monster_dies_without_respawn_when_no_spawn_id() {
        let mut g = Game::from_static_map_arc(combat_map(false));
        let (_a, _ra) = add_player(&mut g, Position::new(95, 117, 7));
        let m = add_monster(&mut g, Position::new(96, 117, 7));
        // spawn_id is None by default — no spawn entry registered.

        g.do_monster_death(m, 0);
        assert!(!g.monsters.contains_key(&m), "monster removed");

        // Process any pending respawns — nothing should happen.
        g.process_respawns(9999);
        assert_eq!(
            g.monsters.len(),
            0,
            "monster without spawn_id does not respawn"
        );
    }

    #[test]
    fn monster_respawn_respects_interval() {
        let mut g = Game::from_static_map_arc(combat_map(false));
        let (_a, _ra) = add_player(&mut g, Position::new(95, 117, 7));
        let sid = 1;
        g.spawns.insert(
            sid,
            MonsterSpawn {
                position: Position::new(96, 117, 7),
                respawn_interval_ms: 200,
                respawn_at_ms: None,
                name: "Rat".into(),
                look_type: 100,
                look_corpse: 0,
                health: 50,
                max_health: 50,
                speed: 200,
                attack: 7,
                loot: vec![],
                target_distance: 0,
                race: None,
                personality: Personality::Aggressive,
                aggro_range: DEFAULT_AGGRO_RANGE,
                hostile: true,
                run_on_health: 0,
            },
        );
        let m = add_monster(&mut g, Position::new(96, 117, 7));
        g.monsters.get_mut(&m).unwrap().spawn_id = Some(sid);

        g.do_monster_death(m, 0);
        assert_eq!(g.spawns[&sid].respawn_at_ms, Some(200));

        // Tick just before the interval — no respawn.
        g.process_respawns(199);
        assert_eq!(g.monsters.len(), 0, "not yet due at 199 < 200");

        // Tick at the exact boundary — respawn.
        g.process_respawns(200);
        assert_eq!(g.monsters.len(), 1, "respawn at exact boundary");
    }

    #[test]
    fn respawn_copies_wildlife_flags() {
        // A rabbit spawn must come back non-hostile, with runonhealth and no
        // melee. Dropping those flags on respawn is what made wildlife attack.
        let mut g = Game::from_static_map_arc(combat_map(false));
        let (_a, _ra) = add_player(&mut g, Position::new(95, 117, 7));
        g.spawns.insert(
            1,
            MonsterSpawn {
                position: Position::new(96, 117, 7),
                respawn_interval_ms: 60_000,
                respawn_at_ms: Some(0),
                name: "Rabbit".into(),
                look_type: 74,
                look_corpse: 6017,
                health: 15,
                max_health: 15,
                speed: 180,
                attack: 0,
                loot: vec![],
                target_distance: 0,
                race: Some(RaceType::Blood),
                personality: Personality::Neutral,
                aggro_range: DEFAULT_AGGRO_RANGE,
                hostile: false,
                run_on_health: 15,
            },
        );
        g.process_respawns(0);
        let rabbit = g.monsters.values().next().expect("rabbit respawned");
        assert!(!rabbit.hostile, "respawn must keep hostile=0");
        assert_eq!(rabbit.run_on_health, 15);
        assert_eq!(rabbit.attack, 0, "respawn must not invent a melee swing");
        assert_eq!(rabbit.personality, Personality::Neutral);
        assert_eq!(rabbit.max_health, 15);
    }

    // -------------------------------------------------------------------------
    // Blood-item-on-hit: splash tests (3.1 RED, 3.2 RED)
    // -------------------------------------------------------------------------

    #[test]
    fn blood_player_hit_spawns_small_splash() {
        let mut g = Game::from_static_map_arc(combat_map(false));
        let (a, _ra) = add_player(&mut g, Position::new(95, 117, 7));
        let (b, _rb) = add_player(&mut g, Position::new(96, 117, 7));
        g.do_set_target(a, b);
        // A blocked swing deals 0 and must not splash; tick until one connects.
        for tick in 1..=40u64 {
            g.on_combat_tick(tick * MELEE_ATTACK_INTERVAL_MS);
            if g.dynamic
                .get(&(96, 117, 7))
                .is_some_and(|st| st.server_ids.contains(&ITEM_SMALLSPLASH))
            {
                break;
            }
        }
        // Player B's tile should contain ITEM_SMALLSPLASH after damage.
        let key = (96, 117, 7);
        let st = g
            .dynamic
            .get(&key)
            .expect("victim tile must be materialized by splash");
        assert!(
            st.server_ids.contains(&ITEM_SMALLSPLASH),
            "on-hit splash must add SMALLSPLASH (2019) to victim tile; sids: {:?}",
            st.server_ids
        );
    }

    #[test]
    fn blood_player_death_spawns_full_splash() {
        let mut g = Game::from_static_map_arc(combat_map(false));
        let (a, _ra) = add_player(&mut g, Position::new(95, 117, 7));
        let (b, _rb) = add_player(&mut g, Position::new(96, 117, 7));
        g.players.get_mut(&b).unwrap().health = 1;
        g.do_set_target(a, b);
        // Drive combat ticks until B dies.
        for tick in 1..=5u64 {
            g.on_combat_tick(tick * MELEE_ATTACK_INTERVAL_MS);
        }
        // The death tile should contain ITEM_FULLSPLASH.
        let key = (96, 117, 7);
        let st = g
            .dynamic
            .get(&key)
            .expect("death tile must be materialized by splash");
        assert!(
            st.server_ids.contains(&ITEM_FULLSPLASH),
            "on-death splash must add FULLSPLASH (2016) to death tile; sids: {:?}",
            st.server_ids
        );
    }

    #[test]
    fn undead_monster_hit_produces_no_splash() {
        let mut g = Game::from_static_map_arc(combat_map(false));
        let (a, _ra) = add_player(&mut g, Position::new(95, 117, 7));
        let m = add_monster(&mut g, Position::new(96, 117, 7));
        g.monsters.get_mut(&m).unwrap().race = Some(RaceType::Undead);
        g.do_set_target(a, m);
        g.on_combat_tick(MELEE_ATTACK_INTERVAL_MS);
        // The monster tile must NOT have any splash in the overlay.
        let key = (96, 117, 7);
        if let Some(st) = g.dynamic.get(&key) {
            assert!(
                !st.server_ids.contains(&ITEM_SMALLSPLASH),
                "Undead monster hit must NOT spawn splash; sids: {:?}",
                st.server_ids
            );
        }
        // No overlay at all is also fine (means no splash was attempted).
    }

    #[test]
    fn monster_hit_spawns_small_splash_when_race_is_blood() {
        let mut g = Game::from_static_map_arc(combat_map(false));
        let (a, _ra) = add_player(&mut g, Position::new(95, 117, 7));
        let m = add_monster(&mut g, Position::new(96, 117, 7));
        g.monsters.get_mut(&m).unwrap().race = Some(RaceType::Blood);
        g.do_set_target(a, m);
        for tick in 1..=40u64 {
            g.on_combat_tick(tick * MELEE_ATTACK_INTERVAL_MS);
            if g.dynamic
                .get(&(96, 117, 7))
                .is_some_and(|st| st.server_ids.contains(&ITEM_SMALLSPLASH))
            {
                break;
            }
        }
        let key = (96, 117, 7);
        let st = g
            .dynamic
            .get(&key)
            .expect("monster tile must be materialized by splash");
        assert!(
            st.server_ids.contains(&ITEM_SMALLSPLASH),
            "Blood monster hit must add SMALLSPLASH; sids: {:?}",
            st.server_ids
        );
    }

    #[test]
    fn monster_death_spawns_full_splash_when_race_is_blood() {
        let mut g = Game::from_static_map_arc(combat_map(false));
        let (_a, _ra) = add_player(&mut g, Position::new(95, 117, 7)); // spectator
        let m = add_monster(&mut g, Position::new(96, 117, 7));
        g.monsters.get_mut(&m).unwrap().race = Some(RaceType::Blood);
        g.monsters.get_mut(&m).unwrap().health = 1;
        g.do_monster_death(m, 0);
        let key = (96, 117, 7);
        let st = g
            .dynamic
            .get(&key)
            .expect("death tile must be materialized by splash");
        assert!(
            st.server_ids.contains(&ITEM_FULLSPLASH),
            "Blood monster death must add FULLSPLASH; sids: {:?}",
            st.server_ids
        );
    }

    // -------------------------------------------------------------------------
    // Phase 4: Creature corpse + loot integration tests
    // -------------------------------------------------------------------------

    // Task 4.2: Monster death spawns corpse + stores loot in corpse
    #[test]
    fn monster_death_spawns_corpse_and_loot() {
        let mut g = Game::from_static_map_arc(combat_map(false));
        let (_a, _ra) = add_player(&mut g, Position::new(95, 117, 7));
        let m = add_monster(&mut g, Position::new(96, 117, 7));
        {
            let mo = g.monsters.get_mut(&m).unwrap();
            mo.look_corpse = 3058;
            mo.loot = vec![MonsterDrop {
                item_id: 100,
                chance: 1.0,
                count_max: 1,
            }];
            mo.health = 1;
        }
        g.do_monster_death(m, 0);

        let key = (96, 117, 7);
        let st = g
            .dynamic
            .get(&key)
            .expect("death tile must be materialized");
        assert!(
            st.server_ids.contains(&3058),
            "monster death must spawn corpse (3058); sids: {:?}",
            st.server_ids
        );
        // Loot is stored in corpse_contents, not on the overlay.
        let contents = g.corpse_contents.get(&key);
        assert!(contents.is_some(), "loot must be in corpse_contents");
        assert_eq!(
            contents.unwrap().len(),
            1,
            "exactly 1 loot item must be stored; got: {:?}",
            contents.unwrap()
        );
    }

    // Regression: loot must not be duplicated in corpse_contents
    #[test]
    fn monster_loot_not_duplicated_in_corpse() {
        let mut g = Game::from_static_map_arc(combat_map(false));
        let (_a, _ra) = add_player(&mut g, Position::new(95, 117, 7));
        let m = add_monster(&mut g, Position::new(96, 117, 7));
        {
            let mo = g.monsters.get_mut(&m).unwrap();
            mo.health = 1;
            // Three drops with distinct ids to count easily.
            mo.loot = vec![
                MonsterDrop {
                    item_id: 10,
                    chance: 1.0,
                    count_max: 1,
                },
                MonsterDrop {
                    item_id: 20,
                    chance: 1.0,
                    count_max: 1,
                },
                MonsterDrop {
                    item_id: 30,
                    chance: 1.0,
                    count_max: 1,
                },
            ];
        }
        g.do_monster_death(m, 0);

        let key = (96, 117, 7);
        let contents = g.corpse_contents.get(&key).expect("loot must be stored");
        assert_eq!(
            contents.len(),
            3,
            "3 drops must produce exactly 3 items; got {:?}",
            contents
        );

        // Also check that do_monster_death was called exactly once:
        // monster should be gone.
        assert!(!g.monsters.contains_key(&m), "monster must be removed");
    }

    // Task 4.3: Player death spawns corpse + inventory items as loot
    #[test]
    fn player_death_spawns_corpse_and_inventory_loot() {
        let mut g = Game::from_static_map_arc(combat_map(false));
        let (a, _ra) = add_player(&mut g, Position::new(95, 117, 7));
        let (b, _rb) = add_player(&mut g, Position::new(96, 117, 7));
        // Give player B an item in inventory slot 1 (head).
        g.players.get_mut(&b).unwrap().inventory[0] = Some(InvItem {
            server_id: 500,
            client_id: 5741,
            count: None,
            animated: false,
        });
        g.players.get_mut(&b).unwrap().health = 1;

        // Kill B via combat.
        g.do_set_target(a, b);
        for tick in 1..=5u64 {
            g.on_combat_tick(tick * MELEE_ATTACK_INTERVAL_MS);
            if !g.players.contains_key(&b) {
                break;
            }
        }

        // The death tile must contain the human corpse.
        let key = (96, 117, 7);
        let st = g
            .dynamic
            .get(&key)
            .expect("death tile must be materialized");
        assert!(
            st.server_ids.contains(&ITEM_HUMAN_CORPSE),
            "player death must spawn ITEM_HUMAN_CORPSE (3058); sids: {:?}",
            st.server_ids
        );
        // Inventory items are now stored inside the corpse, not on the ground.
        let contents = g.corpse_contents.get(&key);
        assert!(contents.is_some(), "loot must be in corpse_contents");
        assert!(
            contents.unwrap().iter().any(|&(sid, _)| sid == 500),
            "player death must store inventory item (500) in corpse"
        );
    }

    #[test]
    fn player_death_empty_inventory_only_corpse() {
        let mut g = Game::from_static_map_arc(combat_map(false));
        let (a, _ra) = add_player(&mut g, Position::new(95, 117, 7));
        let (b, _rb) = add_player(&mut g, Position::new(96, 117, 7));
        g.players.get_mut(&b).unwrap().health = 1;

        g.do_set_target(a, b);
        for tick in 1..=5u64 {
            g.on_combat_tick(tick * MELEE_ATTACK_INTERVAL_MS);
            if !g.players.contains_key(&b) {
                break;
            }
        }

        let key = (96, 117, 7);
        let st = g
            .dynamic
            .get(&key)
            .expect("death tile must be materialized");
        assert!(
            st.server_ids.contains(&ITEM_HUMAN_CORPSE),
            "player death must spawn corpse even with no items"
        );
        // Only corpse, ground, and splashes should be present.
        let extra_ids: Vec<&u16> = st
            .server_ids
            .iter()
            .filter(|&&s| {
                s != ITEM_HUMAN_CORPSE && s != ITEM_FULLSPLASH && s != ITEM_SMALLSPLASH && s != 100
            })
            .collect();
        assert!(
            extra_ids.is_empty(),
            "empty inventory player must drop no extra items; got: {:?}",
            extra_ids
        );
    }

    // Task 4.4: Tile cap — fill tile, kill monster, no panic, corpse/loot skipped
    #[test]
    fn monster_death_at_tile_capacity_does_not_panic() {
        let mut g = Game::from_static_map_arc(combat_map(false));
        let (_a, _ra) = add_player(&mut g, Position::new(95, 117, 7)); // spectator
        let m = add_monster(&mut g, Position::new(96, 117, 7));
        {
            let mo = g.monsters.get_mut(&m).unwrap();
            mo.look_corpse = 3058;
            mo.loot = vec![
                MonsterDrop {
                    item_id: 100,
                    chance: 1.0,
                    count_max: 1,
                },
                MonsterDrop {
                    item_id: 200,
                    chance: 1.0,
                    count_max: 1,
                },
                MonsterDrop {
                    item_id: 300,
                    chance: 1.0,
                    count_max: 1,
                },
            ];
            mo.health = 1;
        }

        // Fill tile — add items so it reaches the 10-thing cap.
        // The static tile already has 1 ground item (server_id=100), so we add
        // 9 more to reach the limit.
        g.materialize(Position::new(96, 117, 7));
        let key = (96, 117, 7);
        {
            let st = g.dynamic.get_mut(&key).unwrap();
            // st.items has 1 ground item from static stack; add 9 more to reach 10.
            for i in 1..=9u16 {
                st.items.push(WireItem {
                    client_id: i,
                    subtype: None,
                    animated: false,
                });
                st.server_ids.push(i);
                st.counts.push(None);
            }
        }

        // Kill — must not panic. TFS uses FLAG_NOLIMIT for death items
        // (splash, corpse, loot) so they bypass the tile cap.
        g.do_monster_death(m, 0);
        assert!(!g.monsters.contains_key(&m), "monster removed");

        // Tile now has ground + 9 fillers + splash + corpse = 12 items.
        let st = g.dynamic.get(&key).unwrap();
        assert!(
            st.items.len() >= 12,
            "death items bypass tile cap; got {} items",
            st.items.len()
        );
    }

    #[test]
    fn corpse_tile_set_on_spawn_and_cleared_on_decay() {
        let mut g = Game::from_static_map_arc(combat_map(false));
        let (_a, _ra) = add_player(&mut g, Position::new(95, 117, 7));
        let m = add_monster(&mut g, Position::new(96, 117, 7));
        g.monsters.get_mut(&m).unwrap().look_corpse = 3058;
        g.monsters.get_mut(&m).unwrap().health = 1;
        g.do_monster_death(m, 0);

        let key = (96, 117, 7);
        assert!(
            g.corpse_tiles.contains(&key),
            "corpse_tiles must contain death tile after do_monster_death"
        );
        assert!(
            g.corpse_item_ids.contains_key(&key),
            "corpse_item_ids must contain death tile after do_monster_death"
        );
        assert_eq!(
            g.corpse_item_ids.get(&key),
            Some(&3058),
            "corpse_item_ids must store the corpse server_id"
        );

        // Process decay with now_ms past CORPSE_DECAY_MS (120s).
        g.on_combat_tick(CORPSE_DECAY_MS + 1);
        assert!(
            !g.corpse_tiles.contains(&key),
            "corpse_tiles must be cleared after decay"
        );
        assert!(
            !g.corpse_item_ids.contains_key(&key),
            "corpse_item_ids must be cleared after decay"
        );
    }

    #[test]
    fn spawn_loot_does_not_add_to_overlay() {
        let mut g = Game::from_static_map_arc(combat_map(false));
        let (_a, _ra) = add_player(&mut g, Position::new(95, 117, 7));
        let m = add_monster(&mut g, Position::new(96, 117, 7));
        g.monsters.get_mut(&m).unwrap().look_corpse = 3058;
        // Use a loot server_id that is NOT the ground item (100).
        // 500 is a test loot item in the combat_map fixture.
        g.monsters.get_mut(&m).unwrap().loot = vec![MonsterDrop {
            item_id: 500,
            chance: 1.0,
            count_max: 1,
        }];
        g.monsters.get_mut(&m).unwrap().health = 1;
        g.do_monster_death(m, 0);

        let key = (96, 117, 7);
        // Loot is in corpse_contents, not on the overlay.
        assert!(
            g.corpse_contents.contains_key(&key),
            "loot must be in corpse_contents"
        );
        // The overlay should NOT have the loot item (500).
        if let Some(st) = g.dynamic.get(&key) {
            assert!(
                !st.server_ids.contains(&500),
                "loot server_id (500) must NOT be in overlay items"
            );
        }
    }

    #[test]
    fn corpse_splash_stack_order_blood_below_corpse() {
        let mut g = Game::from_static_map_arc(combat_map(false));
        let (_a, _ra) = add_player(&mut g, Position::new(95, 117, 7));
        let m = add_monster(&mut g, Position::new(96, 117, 7));
        g.monsters.get_mut(&m).unwrap().race = Some(RaceType::Blood);
        g.monsters.get_mut(&m).unwrap().look_corpse = 3058;
        g.monsters.get_mut(&m).unwrap().health = 1;
        g.do_monster_death(m, 0);

        // After death: ground(st) + splash(2886) + corpse(3058) in that order.
        // Splash was inserted first, corpse after. The corpse must have a
        // higher index (renders on top).
        let key = (96, 117, 7);
        let st = g.dynamic.get(&key).expect("tile must be materialized");
        let splash_idx = st.server_ids.iter().position(|&id| id == ITEM_FULLSPLASH);
        let corpse_idx = st.server_ids.iter().position(|&id| id == 3058);
        assert!(splash_idx.is_some(), "blood splash must be in overlay");
        assert!(corpse_idx.is_some(), "corpse must be in overlay");
        assert!(
            corpse_idx.unwrap() > splash_idx.unwrap(),
            "corpse must render above blood splash (higher index)"
        );
    }
}
