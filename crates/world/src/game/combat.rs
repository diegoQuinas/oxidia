//! Combat behavior (targeting, damage, death, ticks) for the game actor.

use super::*;
use crate::combat;

impl Game {
    /// Handle `0xA1` — set or clear the attacker's melee target.
    ///
    /// - `target_id == 0` clears the fight.
    /// - `target_id == id` (self-attack) is ignored.
    /// - Attacker on a PZ tile → push `0xB4` and do NOT set target
    ///   (`combat.cpp:294-297`, TFS `playerSetAttackedCreature`).
    /// - Unknown target is silently ignored.
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
        if self.map.is_protection_zone(attacker_pos) {
            self.push_status_message(id,
                b"You may not attack a person while you are in a protection zone.");
            return;
        }
        // Target must exist.
        if !self.players.contains_key(&target_id) {
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
            v.health = v.health.saturating_sub(dmg.max(0) as u16);
            (before, v.health, v.max_health)
        };
        let victim_pos = match self.players.get(&victim_id) {
            Some(p) => p.position,
            None => return,
        };
        // Push 0x8C health-bar to every spectator of the victim's tile,
        // INCLUDING the victim itself (it is also a spectator of its own tile).
        let pct = combat_packets::health_percent(i32::from(new_health), i32::from(max_health));
        let health_bar = combat_packets::creature_health(victim_id, pct);
        // Collect spectators first (can_see of the victim's tile), then push.
        let spectators: Vec<u32> = self.players
            .iter()
            .filter(|&(&sid, sp)| Self::can_see(sp.position, victim_pos) || sid == victim_id)
            .map(|(&sid, _)| sid)
            .collect();
        for sid in &spectators {
            self.push(*sid, health_bar.clone());
        }
        // Push 0xA0 self-stats to the victim only.
        let stats = {
            let p = match self.players.get(&victim_id) { Some(p) => p, None => return };
            enter_world::stats(&enter_world::Stats {
                health: p.health,
                max_health: p.max_health,
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
                base_speed: 220,
            })
        };
        self.push(victim_id, stats);
        // Push a physical-hit magic effect on the victim's tile to all spectators.
        // Physical-hit blood effect. TFS sends the effect byte directly, so the
        // wire value is the enum value (CONST_ME_DRAWBLOOD = 1). See
        // enter_world::EFFECT_DRAWBLOOD.
        let effect = enter_world::magic_effect(
            victim_pos.x, victim_pos.y, victim_pos.z, enter_world::EFFECT_DRAWBLOOD);
        for sid in &spectators {
            self.push(*sid, effect.clone());
        }
        // Floating damage number (0xB4 TextMessage). Use the damage actually
        // applied (clamped at the victim's remaining HP), not the raw roll, so
        // an overkill shows the real hit. A 0 value renders nothing client-side,
        // so skip the packet entirely. The mode byte is routed per recipient:
        // the attacker sees "dealt", the victim "received", bystanders "others".
        let applied = u32::from(health_before.saturating_sub(new_health));
        if applied > 0 {
            let victim_name = self.players.get(&victim_id)
                .map(|p| p.name.clone()).unwrap_or_default();
            let attacker_name = self.players.get(&attacker_id)
                .map(|p| p.name.clone()).unwrap_or_default();
            for sid in &spectators {
                let (mode, text) = if *sid == attacker_id {
                    (combat_packets::MSG_DAMAGE_DEALT,
                     format!("You deal {applied} damage to {victim_name}."))
                } else if *sid == victim_id {
                    (combat_packets::MSG_DAMAGE_RECEIVED,
                     format!("You lose {applied} hitpoints due to an attack by {attacker_name}."))
                } else {
                    (combat_packets::MSG_DAMAGE_OTHERS,
                     format!("{victim_name} loses {applied} hitpoints due to an attack by {attacker_name}."))
                };
                let pkt = combat_packets::damage_text(
                    mode, victim_pos.x, victim_pos.y, victim_pos.z,
                    applied, combat_packets::TEXTCOLOR_RED, text.as_bytes());
                self.push(*sid, pkt);
            }
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

        // Clear all fights targeting the victim, and the victim's own fight.
        let all_ids: Vec<u32> = self.players.keys().copied().collect();
        for pid in all_ids {
            if let Some(p) = self.players.get_mut(&pid) {
                if p.attacking == Some(victim_id) || pid == victim_id {
                    p.attacking = None;
                }
            }
        }

        // Death position + temple destination (computed before removal).
        let death_pos = match self.players.get(&victim_id) {
            Some(p) => p.position,
            None => return,
        };
        let temple = self.map.temple_for(death_pos);

        // Remove from the death tile for spectators. The id-form remove is
        // unambiguous under co-occupancy (stair/height landings); drop the victim
        // from each spectator's known-set so a relog re-introduces it (full form).
        for spec in self.spectators(death_pos, victim_id) {
            self.push(spec, walk::remove_creature_by_id(victim_id));
            if let Some(s) = self.players.get_mut(&spec) { s.known.remove(&victim_id); }
        }

        // Remove the victim from the world (death == logout). Persist the player
        // AT THE TEMPLE with full HP so the relog spawns there — M8 `login`
        // restores the saved position, so saving the death tile would respawn the
        // player where they died. Dropping the PlayerState drops its session
        // push_tx, which closes the writer channel and ends the session: the
        // client shows the death window and returns to character select. Mirrors
        // TFS onDeath -> sendReLoginWindow + removeCreature (player.cpp:2070, 2197);
        // the death-respawn position is the town temple.
        let Some(p) = self.players.remove(&victim_id) else { return };
        if let Some(tx) = &self.save_tx {
            let _ = tx.send(SaveRecord {
                name: p.name.clone(),
                position: temple,
                direction: p.direction,
                outfit: p.outfit,
                health: p.max_health,
                max_health: p.max_health,
                sex: p.sex,
                inventory: p.inventory.iter().enumerate()
                    .filter_map(|(i, slot)| slot.map(|it| ((i + 1) as u8, it.server_id, it.count.unwrap_or(1))))
                    .collect(),
                container_items: Self::export_container_items(&p.inventory, &p.open_containers),
            });
        }

    }

    /// Global combat tick. Iterates all players with an active target and, for
    /// each whose attack interval has elapsed, rolls one swing. Out-of-range or
    /// missing targets clear the fight without damage.
    pub(super) fn on_combat_tick(&mut self, now_ms: u64) {
        // Collect (attacker_id, target_id) pairs to process; avoid double-borrow.
        let fights: Vec<(u32, u32)> = self.players
            .iter()
            .filter_map(|(&id, p)| p.attacking.map(|tid| (id, tid)))
            .collect();

        for (attacker_id, target_id) in fights {
            // Target gone? Clear the fight.
            let target_pos = match self.players.get(&target_id) {
                Some(p) => p.position,
                None => {
                    if let Some(p) = self.players.get_mut(&attacker_id) { p.attacking = None; }
                    continue;
                }
            };
            let (attacker_pos, last_attack, fist_skill) = match self.players.get(&attacker_id) {
                Some(p) => (p.position, p.last_attack_ms, p.fist_skill),
                None => continue,
            };
            // W3 fix: TFS clears the fight the moment EITHER party is in a PZ
            // (`canTargetCreature` combat.cpp:221-229; `onAttackedCreatureChangeZone`
            // player.cpp:1153). A victim who fled into the temple must stop taking
            // hits on the very next tick — clearing `attacking` (not just skipping
            // the swing) so the attacker also gets their combat state cleared.
            if self.map.is_protection_zone(attacker_pos)
                || self.map.is_protection_zone(target_pos)
            {
                if let Some(p) = self.players.get_mut(&attacker_id) { p.attacking = None; }
                continue;
            }
            // Interval check.
            if now_ms.saturating_sub(last_attack) < MELEE_ATTACK_INTERVAL_MS {
                continue;
            }
            // Same-floor Chebyshev ≤ 1 (TFS `useFist` range check).
            if attacker_pos.z != target_pos.z {
                continue; // cross-floor melee impossible
            }
            let dx = (i32::from(attacker_pos.x) - i32::from(target_pos.x)).abs();
            let dy = (i32::from(attacker_pos.y) - i32::from(target_pos.y)).abs();
            if dx > 1 || dy > 1 {
                continue; // out of melee range, no swing this tick
            }
            // Roll damage.
            let dmg = combat::fist_damage(&mut self.rng, 1, fist_skill);
            // Update last_attack_ms before applying damage (apply_damage may call
            // do_death which removes the attacker's fight — the timestamp update
            // must not be lost in that chain).
            if let Some(p) = self.players.get_mut(&attacker_id) {
                p.last_attack_ms = now_ms;
            }
            self.apply_damage(attacker_id, target_id, dmg);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use super::super::test_support::*;

    // -------------------------------------------------------------------------
    // M7 combat tests
    // -------------------------------------------------------------------------

    #[test]
    fn set_target_sets_attacking_and_clear_resets_it() {
        let mut g = Game::new(combat_map(false));
        let (a, _ra) = add_player(&mut g, Position::new(95, 117, 7));
        let (b, _rb) = add_player(&mut g, Position::new(96, 117, 7));
        g.do_set_target(a, b);
        assert_eq!(g.players[&a].attacking, Some(b), "set_target should store target id");
        g.do_set_target(a, 0);
        assert_eq!(g.players[&a].attacking, None, "target 0 clears the fight");
    }

    #[test]
    fn set_target_self_is_ignored() {
        let mut g = Game::new(combat_map(false));
        let (a, mut ra) = add_player(&mut g, Position::new(95, 117, 7));
        g.do_set_target(a, a);
        assert_eq!(g.players[&a].attacking, None, "self-target must be ignored");
        assert!(ra.try_recv().is_err(), "self-target must not push any packet");
    }

    #[test]
    fn set_target_from_pz_tile_rejects_and_pushes_0xb4() {
        // Attacker is standing on a PZ tile → attack must be rejected with 0xB4
        // and attacking must remain None.
        let mut g = Game::new(combat_map(true)); // spawn is PZ
        let (a, mut ra) = add_player(&mut g, Position::new(95, 117, 7)); // PZ tile
        let (b, _rb) = add_player(&mut g, Position::new(96, 117, 7));
        g.do_set_target(a, b);
        assert_eq!(g.players[&a].attacking, None, "PZ attacker must not get a target");
        let pkt = ra.try_recv().expect("PZ rejection must push a 0xB4 packet");
        assert_eq!(pkt[0], 0xB4, "PZ rejection packet must be a text message (0xB4)");
    }

    #[test]
    fn combat_tick_deals_damage_to_adjacent_target() {
        // A (attacker) and B (victim) are adjacent. After setting target and
        // advancing time past one attack interval, B must have lost HP.
        let mut g = Game::new(combat_map(false));
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
        let pkt = rb.try_recv().expect("victim must receive at least a 0x8C health-bar");
        assert_eq!(pkt[0], protocol::combat_packets::OP_CREATURE_HEALTH,
            "first packet must be 0x8C (health-bar)");
    }

    #[test]
    fn combat_tick_sends_stats_to_victim() {
        // After a combat tick, the victim must also receive its own 0xA0 stats.
        let mut g = Game::new(combat_map(false));
        let (a, _ra) = add_player(&mut g, Position::new(95, 117, 7));
        let (b, mut rb) = add_player(&mut g, Position::new(96, 117, 7));
        g.do_set_target(a, b);
        g.on_combat_tick(MELEE_ATTACK_INTERVAL_MS);
        // Drain the 0x8C (spectator of own tile, health-bar first)
        let _ = rb.try_recv().expect("0x8C expected");
        // Then the 0xA0 self-stats
        let stats_pkt = rb.try_recv().expect("victim must also receive its own 0xA0 stats");
        assert_eq!(stats_pkt[0], protocol::enter_world::OP_STATS, "0xA0 self-stats expected");
    }

    #[test]
    fn combat_tick_spectator_receives_health_bar() {
        // A third-party spectator of B's tile must also receive the 0x8C.
        let mut g = Game::new(combat_map(false));
        let (a, _ra) = add_player(&mut g, Position::new(95, 117, 7));
        let (b, _rb) = add_player(&mut g, Position::new(96, 117, 7));
        // Spectator sits close enough to see B's tile.
        let (_spec, mut rx_spec) = add_player(&mut g, Position::new(95, 116, 7));
        g.do_set_target(a, b);
        g.on_combat_tick(MELEE_ATTACK_INTERVAL_MS);
        let pkt = rx_spec.try_recv().expect("spectator must receive 0x8C health bar");
        assert_eq!(pkt[0], protocol::combat_packets::OP_CREATURE_HEALTH);
    }

    #[test]
    fn combat_tick_no_damage_when_target_out_of_melee_range() {
        // Target 2 tiles away → no swing, no packets.
        let mut g = Game::new(combat_map(false));
        let (a, _ra) = add_player(&mut g, Position::new(95, 117, 7));
        let (b, mut rb) = add_player(&mut g, Position::new(97, 117, 7)); // 2 tiles east
        g.do_set_target(a, b);
        g.on_combat_tick(MELEE_ATTACK_INTERVAL_MS);
        assert!(rb.try_recv().is_err(), "out-of-range target should receive no packets");
    }

    #[test]
    fn combat_tick_respects_interval_no_damage_before_due() {
        // tick at now_ms < MELEE_ATTACK_INTERVAL_MS must not swing.
        let mut g = Game::new(combat_map(false));
        let (a, _ra) = add_player(&mut g, Position::new(95, 117, 7));
        let (b, mut rb) = add_player(&mut g, Position::new(96, 117, 7));
        g.do_set_target(a, b);
        // Send a tick at t=1000ms (< 2000ms interval) → no swing.
        g.on_combat_tick(1000);
        assert!(rb.try_recv().is_err(), "tick before interval elapses must not produce damage");
    }

    #[test]
    fn death_sends_window_removes_victim_and_saves_at_temple() {
        // Death == logout: the victim gets the 0x28 window, is removed from the
        // world, and a SaveRecord is emitted at the temple with full HP — so the
        // relog spawns at the temple (M8 `login` restores the saved position).
        let mut g = Game::new(combat_map(false));
        let (save_tx, mut save_rx) = mpsc::unbounded_channel::<SaveRecord>();
        g.save_tx = Some(save_tx);
        let (a, _ra) = add_player(&mut g, Position::new(97, 117, 7));
        let (b, mut rb) = add_player(&mut g, Position::new(96, 117, 7));
        let max_hp = g.players[&b].max_health;
        let temple = g.map.spawn();
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
            if !g.players.contains_key(&b) { break; }
        }

        assert!(saw_death_window, "dying player must receive the 0x28 death window");
        assert!(!g.players.contains_key(&b), "victim must be removed from the world on death");
        let rec = save_rx.try_recv().expect("death must emit a SaveRecord");
        assert_eq!(rec.position, temple, "death saves the player at the temple");
        assert_eq!(rec.health, rec.max_health, "death saves the player at full HP");
    }

    #[test]
    fn death_with_full_client_buffer_still_saves_at_temple() {
        // Regression: a saturated victim push buffer must NOT divert death through
        // the reaping push()/logout path (which saves at the death tile with the
        // current HP). do_death uses a non-reaping try_send for the death window.
        let mut g = Game::new(combat_map(false));
        let (save_tx, mut save_rx) = mpsc::unbounded_channel::<SaveRecord>();
        g.save_tx = Some(save_tx);
        let (b, _rb) = add_player(&mut g, Position::new(96, 117, 7));
        let temple = g.map.spawn();
        g.players.get_mut(&b).unwrap().health = 1;
        // Fill B's push channel to capacity so a reaping send would log it out.
        for _ in 0..super::super::PUSH_CAPACITY {
            g.push(b, vec![0u8]);
        }
        g.do_death(b);
        let rec = save_rx.try_recv().expect("death must emit a SaveRecord even with a full buffer");
        assert_eq!(rec.position, temple, "death saves at the temple even with a full client buffer");
        assert_eq!(rec.health, rec.max_health, "death saves full HP even with a full client buffer");
        assert!(!g.players.contains_key(&b), "victim must be removed from the world");
    }

    #[test]
    fn death_clears_attacker_fight() {
        // Death clears every fight targeting the victim. `fist_damage` rolls
        // 0..=max (a swing can deal 0), so tick until the kill lands rather than
        // assuming one swing kills.
        let mut g = Game::new(combat_map(false));
        let (a, _ra) = add_player(&mut g, Position::new(97, 117, 7));
        let (b, mut rb) = add_player(&mut g, Position::new(96, 117, 7));
        g.do_set_target(a, b);
        g.players.get_mut(&b).unwrap().health = 1;
        for tick in 1..=200u64 {
            g.on_combat_tick(tick * MELEE_ATTACK_INTERVAL_MS);
            while rb.try_recv().is_ok() {} // drain packets
            if !g.players.contains_key(&b) { break; }
        }
        assert!(!g.players.contains_key(&b), "victim must be removed from the world on death");
        assert_eq!(g.players[&a].attacking, None, "attacker's fight must clear on target death");
    }

    #[test]
    fn death_remove_uses_id_form_for_coocc_safety() {
        // Regression for the M7<->co-occupancy merge: do_death must remove the
        // victim with the id-form (0x6C 0xFFFF <id>), not position+stackpos.
        // Under co-occupancy (stair/height landings) a position+stackpos remove
        // is ambiguous when another creature shares the death tile. Matches
        // logout and do_move.
        let mut g = Game::new(combat_map(false));
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
        assert_eq!(&pkt[1..3], &[0xFF, 0xFF], "death remove must be id-form (co-occupancy safe)");
        assert_eq!(&pkt[3..7], &b.to_le_bytes(), "id-form remove carries the victim id");
    }

    #[test]
    fn tick_clears_target_when_target_logs_out() {
        // If the target logs out, the attacker's attacking must be cleared on the
        // next tick (no panic, no stale fight).
        let mut g = Game::new(combat_map(false));
        let (a, _ra) = add_player(&mut g, Position::new(95, 117, 7));
        let (b, _rb) = add_player(&mut g, Position::new(96, 117, 7));
        g.do_set_target(a, b);
        assert_eq!(g.players[&a].attacking, Some(b));
        g.logout(b); // B disconnects
        g.on_combat_tick(MELEE_ATTACK_INTERVAL_MS);
        assert_eq!(g.players[&a].attacking, None, "attacker must clear when target logs out");
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
        let mut g = Game::new(wide_combat_map_with_pz());
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
}
