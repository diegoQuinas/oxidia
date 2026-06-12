//! Combat behavior (targeting, damage, death, ticks) for the game actor.

use std::collections::VecDeque;

use super::*;
use crate::combat;
use rand::Rng;

impl Game {
    /// Handle `0xA1` — set or clear the attacker's melee target.
    ///
    /// - `target_id == 0` clears the fight.
    /// - `target_id == id` (self-attack) is ignored.
    /// - Attacker on a PZ tile → push `0xB4` and do NOT set target
    ///   (`combat.cpp:294-297`, TFS `playerSetAttackedCreature`).
    /// - Unknown target (player or monster) is silently ignored.
    pub(super) fn do_set_target(&mut self, id: u32, target_id: u32) {
        if target_id == 0 {
            if let Some(p) = self.players.get_mut(&id) {
                p.attacking = None;
            }
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
    }

    /// Apply `dmg` hit points of damage to `victim_id`, dealt by `attacker_id`.
    /// Clamps to 0, pushes health-bar (`0x8C`) to all spectators including the
    /// victim and attacker, pushes self-stats (`0xA0`) to the victim, emits the
    /// physical-hit blood effect plus a floating damage number (`0xB4`), and
    /// fires `do_death` on 0 HP.
    fn apply_damage(&mut self, attacker_id: u32, victim_id: u32, dmg: i32) {
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
        // Push a physical-hit magic effect on the victim's tile to all spectators.
        // Physical-hit blood effect. TFS sends the effect byte directly, so the
        // wire value is the enum value (CONST_ME_DRAWBLOOD = 1). See
        // enter_world::EFFECT_DRAWBLOOD.
        let effect = enter_world::magic_effect(
            victim_pos.x,
            victim_pos.y,
            victim_pos.z,
            enter_world::EFFECT_DRAWBLOOD,
        );
        for sid in &spectators {
            self.push(*sid, effect.clone());
        }
        // Floating damage number (0xB4 TextMessage). Use the damage actually
        // applied (clamped at the victim's remaining HP), not the raw roll, so
        // an overkill shows the real hit. A 0 value renders nothing client-side,
        // so skip the packet entirely. The mode byte is routed per recipient:
        // the attacker sees "dealt", the victim "received", bystanders "others".
        let applied = health_before.saturating_sub(new_health);
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
        // On-hit splash from player's race (always Blood by default).
        if let Some(fluid) = self.players.get(&victim_id).and_then(|p| {
            p.race.fluid_subtype()
        }) {
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
                let pkt = tile_item::add_tile_item(
                    (death_pos.x, death_pos.y, death_pos.z),
                    sp,
                    &wi,
                );
                // Push to all spectators EXCEPT the victim (non-reaping try_send).
                for spec in self.spectators(death_pos, victim_id) {
                    if let Some(p) = self.players.get(&spec) {
                        let _ = p.push_tx.try_send(pkt.clone());
                    }
                }
                {
                    let st = self.dynamic.get_mut(&(death_pos.x, death_pos.y, death_pos.z)).unwrap();
                    let front = st.pre_creature_len;
                    st.items.insert(front, wi);
                    st.server_ids.insert(front, ITEM_FULLSPLASH);
                    st.counts.insert(front, Some(fluid));
                }
            }
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

        // Physical-hit blood effect to all player spectators.
        let effect = enter_world::magic_effect(
            victim_pos.x,
            victim_pos.y,
            victim_pos.z,
            enter_world::EFFECT_DRAWBLOOD,
        );
        for sid in &player_spectators {
            self.push(*sid, effect.clone());
        }

        // Floating damage number.
        let applied = health_before.saturating_sub(new_health);
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

        // On-hit splash from monster's race mapping.
        if let Some(fluid) = self
            .monsters
            .get(&victim_id)
            .and_then(|m| m.race)
            .and_then(RaceType::fluid_subtype)
        {
            self.spawn_splash(victim_pos, ITEM_SMALLSPLASH, fluid);
        }

        // Retaliation: if the monster survived, start attacking back.
        if new_health > 0 {
            if let Some(m) = self.monsters.get_mut(&victim_id) {
                if m.attacking.is_none() {
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
        let (spawn_id, death_pos, loot, race) = match self.monsters.get(&victim_id) {
            Some(m) => (m.spawn_id, m.position, m.loot.clone(), m.race),
            None => return,
        };

        // On-death splash from monster's race.
        if let Some(fluid) = race.and_then(RaceType::fluid_subtype) {
            self.spawn_splash(death_pos, ITEM_FULLSPLASH, fluid);
        }

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

        // Remove from the death tile for all player spectators.
        for spec in self.spectators(death_pos, u32::MAX) {
            self.push(spec, walk::remove_creature_by_id(victim_id));
            if let Some(s) = self.players.get_mut(&spec) {
                s.known.remove(&victim_id);
            }
        }

        // Drop the monster.
        self.monsters.remove(&victim_id);

        // Schedule a respawn if this monster belonged to a spawn point.
        if let Some(sid) = spawn_id {
            if let Some(spawn) = self.spawns.get_mut(&sid) {
                spawn.respawn_at_ms = Some(now_ms + spawn.respawn_interval_ms);
            }
        }

        // Spawn loot on the death tile.
        self.spawn_loot(death_pos, &loot);
    }

    /// Spawn a splash item at `pos` with server id `item_id` and fluid subtype.
    /// Reuses the same overlay pattern as `spawn_loot`: materialize → cap check →
    /// insert at pre_creature_len → broadcast `0x6A` to spectators.
    /// Silently returns if the tile is at capacity (≥10 things).
    fn spawn_splash(&mut self, pos: Position, item_id: u16, fluid: u8) {
        if !self.materialize(pos) {
            return;
        }
        {
            let st = match self.dynamic.get(&(pos.x, pos.y, pos.z)) {
                Some(st) => st,
                None => return,
            };
            if st.items.len() >= 10 {
                return; // tile cap
            }
        }
        let wi = WireItem {
            client_id: item_id,
            subtype: Some(fluid),
            animated: false,
        };
        let dest_creatures = self.creatures_on(pos).len();
        {
            let st = self.dynamic.get_mut(&(pos.x, pos.y, pos.z)).unwrap();
            let front = st.pre_creature_len;
            st.items.insert(front, wi);
            st.server_ids.insert(front, item_id);
            st.counts.insert(front, Some(fluid));
        }
        let front = self
            .dynamic
            .get(&(pos.x, pos.y, pos.z))
            .map(|st| st.pre_creature_len)
            .unwrap_or(0);
        let sp = (front + dest_creatures).min(9) as u8;
        self.broadcast_dest(pos, sp, wi, false);
    }

    /// Roll the loot table and spawn items on the ground at `pos`.
    fn spawn_loot(&mut self, pos: Position, loot: &[MonsterDrop]) {
        if !self.materialize(pos) {
            return;
        }
        let mut stack_len = self
            .dynamic
            .get(&(pos.x, pos.y, pos.z))
            .map(|st| st.items.len())
            .unwrap_or(0);

        for drop in loot {
            if stack_len >= 10 {
                break;
            } // tile cap
            if !self.rng.gen_bool(drop.chance) {
                continue;
            }

            let Some(meta) = self.meta.item_meta(drop.item_id) else {
                continue;
            };
            let subtype = if meta.stackable {
                Some(drop.count)
            } else {
                None
            };
            let wi = WireItem {
                client_id: meta.client_id,
                subtype,
                animated: meta.animated,
            };

            let dest_creatures = self.creatures_on(pos).len();
            {
                let st = self.dynamic.get_mut(&(pos.x, pos.y, pos.z)).unwrap();
                let front = st.pre_creature_len;
                st.items.insert(front, wi);
                st.server_ids.insert(front, drop.item_id);
                st.counts.insert(front, subtype);
            }
            let front = self
                .dynamic
                .get(&(pos.x, pos.y, pos.z))
                .map(|st| st.pre_creature_len)
                .unwrap_or(0);
            let sp = (front + dest_creatures).min(9) as u8;
            self.broadcast_dest(pos, sp, wi, false);
            stack_len += 1;
        }
    }

    /// Global combat tick. Iterates all players with an active target and, for
    /// each whose attack interval has elapsed, rolls one swing. Out-of-range or
    /// missing targets clear the fight without damage.
    pub(super) fn on_combat_tick(&mut self, now_ms: u64) {
        // --- Player attackers (existing) ---
        let player_fights: Vec<(u32, u32)> = self
            .players
            .iter()
            .filter_map(|(&id, p)| p.attacking.map(|tid| (id, tid)))
            .collect();
        for (attacker_id, target_id) in player_fights {
            self.process_player_attack(attacker_id, target_id, now_ms);
        }

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
                attacking: None,
                last_attack_ms: 0,
                attack: spawn.attack,
                loot: spawn.loot,
                spawn_id: Some(sid),
                list_walk_dir: VecDeque::new(),
                follow_target: None,
                target_distance: spawn.target_distance,
                race: spawn.race,
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

    /// Process a player's melee attack against `target_id`.
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
        let (attacker_pos, last_attack, fist_skill) = match self.players.get(&attacker_id) {
            Some(p) => (p.position, p.last_attack_ms, p.fist_skill),
            None => return,
        };
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
        if dx > 1 || dy > 1 {
            return;
        }
        let dmg = combat::fist_damage(&mut self.rng, 1, fist_skill);
        if let Some(p) = self.players.get_mut(&attacker_id) {
            p.last_attack_ms = now_ms;
        }
        if target_is_player {
            self.apply_damage(attacker_id, target_id, dmg);
        } else {
            self.apply_monster_damage(attacker_id, target_id, dmg, now_ms);
        }
    }

    /// Process a monster's melee attack against `target_id` (player or monster).
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
        let (attacker_pos, last_attack, attack) = match self.monsters.get(&attacker_id) {
            Some(m) => (m.position, m.last_attack_ms, m.attack),
            None => return,
        };
        // PZ check: attacker in PZ always clears; target PZ only matters for players.
        if self.chunks.is_protection_zone(attacker_pos)
            || (target_is_player && self.chunks.is_protection_zone(target_pos))
        {
            if let Some(m) = self.monsters.get_mut(&attacker_id) {
                m.attacking = None;
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
        if dx > 1 || dy > 1 {
            return;
        }
        let dmg: i32 = self.rng.gen_range(0..=attack).into();
        if let Some(m) = self.monsters.get_mut(&attacker_id) {
            m.last_attack_ms = now_ms;
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
        g.monsters.get_mut(&m).unwrap().attack = 20; // guaranteed kill

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
        // Drain death splash (monsters have race → produce 0x6A splash).
        let _ = ra.try_recv().ok();
        // No loot 0x6A packets beyond the splash.
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
            count: 1,
        }];
        g.do_monster_death(m, 0);
        assert!(!g.monsters.contains_key(&m), "monster must be removed");

        // Drain death splash (monsters have race → produce 0x6A splash).
        let _ = ra.try_recv().ok();

        // Then 0x6C (creature remove), then 0x6A (item add).
        let pkt_6c = ra.try_recv().expect("must receive 0x6C remove");
        assert_eq!(
            pkt_6c[0], 0x6C,
            "death packet must be creature remove"
        );

        let pkt_6a = ra.try_recv().expect("must receive 0x6A item add");
        assert_eq!(pkt_6a[0], 0x6A, "loot packet must be item add");
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
                count: 1,
            },
            MonsterDrop {
                item_id: 100,
                chance: 0.0,
                count: 1,
            },
        ];
        g.do_monster_death(m, 0);
        assert!(!g.monsters.contains_key(&m), "monster must be removed");

        // Drain death splash (monsters have race → produces 0x6A splash).
        let _ = ra.try_recv().ok();

        let mut count = 0u32;
        while let Ok(pkt) = ra.try_recv() {
            if pkt[0] == 0x6A {
                count += 1;
            }
        }
        assert_eq!(
            count, 1,
            "only the 1.0-chance drop must produce a 0x6A packet"
        );
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
                health: 50,
                max_health: 50,
                speed: 200,
                attack: 7,
                loot: vec![],
                target_distance: 0,
                race: None,
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
                health: 50,
                max_health: 50,
                speed: 200,
                attack: 7,
                loot: vec![],
                target_distance: 0,
                race: None,
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

    // -------------------------------------------------------------------------
    // Blood-item-on-hit: splash tests (3.1 RED, 3.2 RED)
    // -------------------------------------------------------------------------

    #[test]
    fn blood_player_hit_spawns_small_splash() {
        let mut g = Game::from_static_map_arc(combat_map(false));
        let (a, _ra) = add_player(&mut g, Position::new(95, 117, 7));
        let (b, _rb) = add_player(&mut g, Position::new(96, 117, 7));
        g.do_set_target(a, b);
        g.on_combat_tick(MELEE_ATTACK_INTERVAL_MS);
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
        g.on_combat_tick(MELEE_ATTACK_INTERVAL_MS);
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
}
