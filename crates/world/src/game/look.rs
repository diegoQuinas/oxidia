//! Look/describe for the game actor.

use super::*;

impl Game {
    /// Handle `0x8C` look-at. Resolve the thing at `(x,y,z)` stackpos, build the
    /// TFS "You see …" text, and push `0xB4`. Mirrors `Game::playerLookAt`
    /// (game.cpp:3100): resolve thing, canSee check, distance, describe.
    pub(super) fn do_look(&mut self, id: u32, x: u16, y: u16, z: u8, stackpos: u8) {
        let Some(looker) = self.players.get(&id) else { return };
        let looker_pos = looker.position;
        let gm = looker.gamemaster;
        let pos = Position::new(x, y, z);

        if !Self::can_see(looker_pos, pos) {
            return;
        }

        let pre = self.merged_pre_creature_len(pos);
        let creatures = self.creatures_on(pos);

        let sp = stackpos as usize;
        let text = if sp < pre {
            self.describe_tile_item(pos, sp, looker_pos, gm)
        } else if !creatures.is_empty() && sp < pre + creatures.len() {
            let target = creatures[sp - pre];
            self.describe_creature(id, target, gm)
        } else {
            let idx = sp.saturating_sub(creatures.len());
            self.describe_tile_item(pos, idx, looker_pos, gm)
        };

        if let Some(text) = text {
            self.push_info_descr(id, &text);
        }
    }

    /// Handle `0x8D` look-in-battle-list: describe a creature by id.
    pub(super) fn do_look_battle(&mut self, id: u32, target_id: u32) {
        let Some(looker) = self.players.get(&id) else { return };
        let Some(target) = self.players.get(&target_id) else { return };
        if !Self::can_see(looker.position, target.position) {
            return;
        }
        let gm = looker.gamemaster;
        if let Some(text) = self.describe_creature(id, target_id, gm) {
            self.push_info_descr(id, &text);
        }
    }

    /// Build the "You see …" text for the tile item at stack index `idx`.
    /// `None` if the tile / index has no catalogued item. Ports
    /// `item.cpp::getDescription` (plain-item subset) + `getNameDescription`.
    fn describe_tile_item(
        &self,
        pos: Position,
        idx: usize,
        looker_pos: Position,
        gm: bool,
    ) -> Option<String> {
        let sid = self.merged_server_id(pos, idx)?;
        let meta = self.map.item_meta(sid)?;
        let count = u32::from(self.merged_count(pos, idx).unwrap_or(1).max(1));

        let mut dist = (i32::from(looker_pos.x) - i32::from(pos.x))
            .abs()
            .max((i32::from(looker_pos.y) - i32::from(pos.y)).abs());
        if looker_pos.z != pos.z {
            dist += 15;
        }

        let mut s = String::from("You see ");
        if meta.stackable && count > 1 && meta.show_count {
            s.push_str(&format!("{} {}", count, meta.plural_name()));
        } else if !meta.name.is_empty() {
            if !meta.article.is_empty() {
                s.push_str(&meta.article);
                s.push(' ');
            }
            s.push_str(&meta.name);
        } else {
            s.push_str(&format!("an item of type {}", sid));
        }
        s.push('.');

        if dist <= 1 {
            if meta.pickupable && meta.weight != 0 {
                let total = meta.weight * count;
                let plural = meta.stackable && count > 1;
                s.push('\n');
                s.push_str(if plural { "They weigh " } else { "It weighs " });
                s.push_str(&format!("{}.{:02} oz.", total / 100, total % 100));
            }
            if !meta.description.is_empty() {
                s.push('\n');
                s.push_str(&meta.description);
            }
        }

        if gm {
            s.push_str(&format!("\nItem ID: {}", sid));
            s.push_str(&format!("\nPosition: {}, {}, {}", pos.x, pos.y, pos.z));
        }
        Some(s)
    }

    /// Build the "You see …" text for a creature. Ports `player.cpp:85`
    /// (faithful subset: name, level, vocation; no party/mana/IP).
    fn describe_creature(&self, looker_id: u32, target_id: u32, gm: bool) -> Option<String> {
        let target = self.players.get(&target_id)?;
        let is_self = looker_id == target_id;
        let mut s = String::from("You see ");
        if is_self {
            s.push_str("yourself. You have no vocation.");
        } else {
            s.push_str(&target.name);
            s.push_str(" (Level 1).");
            s.push_str(if target.sex == 0 { " She" } else { " He" });
            s.push_str(" has no vocation.");
        }
        if gm {
            let p = target.position;
            s.push_str(&format!("\nPosition: {}, {}, {}", p.x, p.y, p.z));
        }
        Some(s)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use super::super::test_support::*;

    // -------------------------------------------------------------------------
    // M9 do_look tests
    // -------------------------------------------------------------------------

    #[test]
    fn do_look_ground_item_adjacent_shows_article_name_and_weight() {
        let mut g = Game::new(look_map());
        // Looker at (100,100,7), stone is at (101,100,7) — distance 1 (adjacent).
        let (looker, mut rx) = add_player(&mut g, Position::new(100, 100, 7));
        // stackpos 1 = stone on tile (101,100,7)
        g.do_look(looker, 101, 100, 7, 1);
        let text = recv_look_text(&mut rx);
        // Must contain "You see a stone."
        assert!(text.contains("You see a stone."), "text: {text:?}");
        // Adjacent → must show weight line: "It weighs 1.10 oz."
        assert!(text.contains("It weighs 1.10 oz."), "adjacent item must show weight; text: {text:?}");
    }

    #[test]
    fn do_look_ground_item_far_away_omits_weight() {
        let mut g = Game::new(look_map());
        // Looker at (100,100,7). Put another player at (103,100,7) to create a
        // position 3 tiles away. Actually just move the looker far from the stone.
        // Stone is at (101,100,7). Place looker at (103,100,7) → dist = 2.
        let (looker, mut rx) = add_player(&mut g, Position::new(103, 100, 7));
        g.do_look(looker, 101, 100, 7, 1);
        let text = recv_look_text(&mut rx);
        assert!(text.contains("You see a stone."), "text: {text:?}");
        // Distance ≥ 2 → no weight line
        assert!(!text.contains("weighs"), "far look must NOT show weight; text: {text:?}");
    }

    #[test]
    fn do_look_non_pickupable_item_no_weight_line() {
        // Ground item (sid 100) is not pickupable → no weight line even when adjacent.
        let mut g = Game::new(look_map());
        let (looker, mut rx) = add_player(&mut g, Position::new(100, 100, 7));
        // stackpos 0 = ground at (100,100,7) itself
        g.do_look(looker, 100, 100, 7, 0);
        let text = recv_look_text(&mut rx);
        assert!(!text.contains("weighs"), "non-pickupable item must not show weight; text: {text:?}");
    }

    #[test]
    fn do_look_stackable_item_with_count_shows_count_and_plural() {
        // gold coins (sid 300) at (102,100,7), count 50. show_count true.
        let mut g = Game::new(look_map());
        let (looker, mut rx) = add_player(&mut g, Position::new(100, 100, 7));
        // stackpos 1 = gold coins at (102,100,7)
        g.do_look(looker, 102, 100, 7, 1);
        let text = recv_look_text(&mut rx);
        // "You see 50 gold coins."
        assert!(text.contains("You see 50 gold coins."), "text: {text:?}");
    }

    #[test]
    fn do_look_other_player_shows_name_level_and_pronoun() {
        let mut g = Game::new(look_map());
        let (looker, mut rx) = add_player(&mut g, Position::new(100, 100, 7));
        // Add a target player with a distinctive name, placed adjacent.
        let (tx2, _rx2) = mpsc::channel(PUSH_CAPACITY);
        let target_id = g.next_id;
        g.next_id += 1;
        g.players.insert(target_id, PlayerState {
            name: "Alice".into(), position: Position::new(100, 100, 7),
            direction: Direction::South, outfit: knight(), push_tx: tx2,
            known: HashSet::new(), health: 150, max_health: 150, fist_skill: 10,
            attacking: None, last_attack_ms: 0,
            sex: 0, // female
            gamemaster: false,
            inventory: [None; 10],
            open_containers: std::array::from_fn(|_| None),
        });
        // Looker is at the same tile; tile pre_creature_len is 1 (just the ground),
        // creatures = [looker_id, target_id] (sorted). stackpos 1 = first creature
        // (the lower id), stackpos 2 = second. Since both players are at (100,100,7)
        // and ids are assigned sequentially with looker first, target_id > looker_id.
        // pre = 1, creatures = [looker, target] sorted by id.
        // looker_id < target_id so stackpos 1 = looker, stackpos 2 = target.
        g.do_look(looker, 100, 100, 7, 2);
        let text = recv_look_text(&mut rx);
        assert!(text.contains("Alice (Level 1)."), "text: {text:?}");
        assert!(text.contains("She has no vocation."), "female pronoun; text: {text:?}");
        // Now change to male and re-verify.
        g.players.get_mut(&target_id).unwrap().sex = 1;
        g.do_look(looker, 100, 100, 7, 2);
        let text2 = recv_look_text(&mut rx);
        assert!(text2.contains("He has no vocation."), "male pronoun; text2: {text2:?}");
    }

    #[test]
    fn do_look_self_shows_yourself() {
        let mut g = Game::new(look_map());
        let (looker, mut rx) = add_player(&mut g, Position::new(100, 100, 7));
        // pre_creature_len = 1 (ground), stackpos 1 = looker itself.
        g.do_look(looker, 100, 100, 7, 1);
        let text = recv_look_text(&mut rx);
        assert!(text.contains("You see yourself."), "text: {text:?}");
        assert!(text.contains("You have no vocation."), "text: {text:?}");
    }

    #[test]
    fn do_look_gamemaster_item_appends_item_id_and_position() {
        let mut g = Game::new(look_map());
        let (looker, mut rx) = add_player(&mut g, Position::new(100, 100, 7));
        // Elevate to GM.
        g.players.get_mut(&looker).unwrap().gamemaster = true;
        // Look at stone (sid 200) at (101,100,7), stackpos 1.
        g.do_look(looker, 101, 100, 7, 1);
        let text = recv_look_text(&mut rx);
        assert!(text.ends_with("\nItem ID: 200\nPosition: 101, 100, 7"),
            "GM look must end with Item ID and Position; text: {text:?}");
    }

    #[test]
    fn do_look_non_gamemaster_no_debug_suffix() {
        let mut g = Game::new(look_map());
        let (looker, mut rx) = add_player(&mut g, Position::new(100, 100, 7));
        // gamemaster = false (default)
        g.do_look(looker, 101, 100, 7, 1);
        let text = recv_look_text(&mut rx);
        assert!(!text.contains("Item ID:"), "non-GM must not see Item ID; text: {text:?}");
        assert!(!text.contains("Position:"), "non-GM must not see Position; text: {text:?}");
    }

    #[test]
    fn do_look_out_of_viewport_pushes_nothing() {
        let mut g = Game::new(look_map());
        // Looker at (100,100,7). A tile that is far out of the viewport
        // (dx = 50 > 9) must produce no packet.
        let (looker, mut rx) = add_player(&mut g, Position::new(100, 100, 7));
        g.do_look(looker, 150, 100, 7, 0);
        assert!(rx.try_recv().is_err(), "look outside viewport must push nothing");
    }
}
