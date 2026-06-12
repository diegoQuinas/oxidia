//! Session lifecycle (login, logout, save, outfit) for the game actor.

use super::*;

impl Game {
    pub(super) fn login(
        &mut self,
        name: String,
        initial: InitialState,
        push_tx: mpsc::Sender<Vec<u8>>,
    ) -> LoginAck {
        let id = self.next_id;
        self.next_id += 1;
        // Resolve position. Either way the tile must be free: you never log in on
        // top of another creature (unlike stair/height co-occupancy during
        // movement). A returning player lands on their saved tile when it's free,
        // otherwise the nearest free tile around it; a new player spawns at/near
        // the map spawn.
        let position = match initial.position {
            Some(saved) => self.free_spawn_near(saved, id),
            None => self.free_spawn(),
        };
        let login_chunks: Vec<crate::map::ChunkId> =
            crate::map::chunks_around(position).into_iter().collect();
        self.chunks.ensure_loaded(&login_chunks);
        let direction = initial.direction;
        let outfit = initial.outfit;

        // Existing in-range players, before inserting self.
        let others_ids = self.spectators(position, id);

        let mut inventory: [Option<InvItem>; 10] = [None; 10];
        for &(slot, server_id, count) in &initial.inventory {
            if !(1..=10).contains(&slot) {
                continue;
            }
            if let Some(meta) = self.meta.item_meta(server_id) {
                let cnt = if meta.stackable {
                    Some(count.max(1))
                } else {
                    None
                };
                inventory[(slot - 1) as usize] = Some(InvItem {
                    server_id,
                    client_id: meta.client_id,
                    count: cnt,
                    animated: meta.animated,
                });
            }
        }

        // Restore container contents from InitialState.
        let open_containers =
            Self::restore_containers(&initial.container_items, &inventory, &self.meta);

        self.players.insert(
            id,
            PlayerState {
                name,
                position,
                direction,
                outfit,
                push_tx,
                known: HashSet::new(),
                health: initial.health,
                max_health: initial.max_health,
                fist_skill: 10,
                race: RaceType::Blood,
                attacking: None,
                last_attack_ms: 0,
                sex: initial.sex,
                gamemaster: initial.gamemaster,
                ghost: false,
                prev_outfit: None,
                noclip: false,
                speed: 220,
                inventory,
                open_containers,
                follow_target: None,
                go_to_position: None,
                failed_repaths: None,
                list_walk_dir: VecDeque::new(),
                last_walk_ms: 0,
                conditions: Vec::new(),
            },
        );

        // Render each existing player into the new client's enter-world map, and
        // tell each existing player that the new one appeared.
        let mut others = Vec::new();
        for other in others_ids {
            if let Some(bytes) = self.introduce(id, other) {
                let p = self.players.get(&other).expect("listed spectator exists");
                others.push(PlacedCreature {
                    x: p.position.x,
                    y: p.position.y,
                    z: p.position.z,
                    bytes,
                });
            }
            if let Some(bytes) = self.introduce(other, id) {
                let stackpos = self.creature_stackpos_on(position, id);
                self.push(
                    other,
                    tile_creature::add_tile_creature(
                        (position.x, position.y, position.z),
                        stackpos,
                        &bytes,
                    ),
                );
                // Spectators also see the teleport puff on login (TFS
                // sendAddCreature isLogin -> sendMagicEffect CONST_ME_TELEPORT).
                // The spawning client gets it from its own enter-world burst;
                // without this, other players see the creature appear with no effect.
                self.push(
                    other,
                    enter_world::magic_effect(
                        position.x,
                        position.y,
                        position.z,
                        enter_world::EFFECT_TELEPORT,
                    ),
                );
            }
        }
        // M12.1: introduce monsters visible from the login position.
        for mid in self.monsters_visible_from(position) {
            if let Some(bytes) = self.introduce(id, mid) {
                let mpos = self
                    .monsters
                    .get(&mid)
                    .expect("monster listed as visible")
                    .position;
                others.push(PlacedCreature {
                    x: mpos.x,
                    y: mpos.y,
                    z: mpos.z,
                    bytes,
                });
            }
        }

        // Build the enter-world map slice from the MERGED view (static + dynamic
        // overlay) so a returning player sees items dropped on the ground while
        // they were away — not the pristine OTBM tile. Online spectators already
        // get the overlay via `merged()`; previously the login burst was encoded
        // from the raw `StaticMap` in the server layer, leaving the relogging
        // player blind to dynamic ground items.
        //
        // Self is rendered in full (unknown) form WITHOUT touching its known-set,
        // identical to the legacy server-layer path. Routing self through
        // `introduce(id, id)` here would mark self as known and desync the next
        // teleport's full-form rebuild, which relies on self being unknown.
        let self_name = self
            .players
            .get(&id)
            .map(|p| p.name.clone())
            .unwrap_or_default();
        let mut placed = others.clone();
        let self_view = CreatureView {
            id,
            name: self_name.as_bytes(),
            health_percent: 100,
            direction: direction.to_byte(),
            outfit,
            light_level: 0,
            light_color: 0,
            speed: 220,
            creature_type: protocol::creature::CREATURETYPE_PLAYER,
            walkthrough: 0, // ghost is always false at login
        };
        placed.push(PlacedCreature {
            x: position.x,
            y: position.y,
            z: position.z,
            bytes: creature::add_creature(&self_view, false, 0),
        });
        let map_description = {
            let merged = self.merged();
            protocol::map_description::encode(
                protocol::map_description::Center {
                    x: position.x,
                    y: position.y,
                    z: position.z,
                },
                &merged,
                &placed,
            )
        };

        LoginAck {
            snapshot: PlayerSnapshot {
                id,
                position,
                direction,
                outfit,
                health: initial.health,
                max_health: initial.max_health,
            },
            others,
            map_description,
        }
    }

    pub(super) fn logout(&mut self, id: u32) {
        let Some(p) = self.players.remove(&id) else {
            return;
        };
        // Emit save record BEFORE broadcasting the removal, while `p` is owned.
        if let Some(tx) = &self.save_tx {
            let inventory: Vec<(u8, u16, u8)> = p
                .inventory
                .iter()
                .enumerate()
                .filter_map(|(i, slot)| {
                    slot.map(|it| ((i + 1) as u8, it.server_id, it.count.unwrap_or(1)))
                })
                .collect();
            let container_items = Self::export_container_items(&p.inventory, &p.open_containers);
            let rec = SaveRecord {
                name: p.name.clone(),
                position: p.position,
                direction: p.direction,
                outfit: p.outfit,
                health: p.health,
                max_health: p.max_health,
                sex: p.sex,
                inventory,
                container_items,
            };
            // Unbounded send never blocks; error only if the worker is gone
            // (server shutting down) — silently drop in that case.
            let _ = tx.send(rec);
        }
        let pos = p.position;
        for spec in self.spectators(pos, id) {
            // A teleport puff on the departing creature's tile, then the remove.
            // (A deliberate polish over TFS, whose removeCreature disappears
            // silently; symmetric with the login appear effect.)
            self.push(
                spec,
                enter_world::magic_effect(pos.x, pos.y, pos.z, enter_world::EFFECT_TELEPORT),
            );
            // id-form remove: unambiguous even if the logging-out creature shared
            // its tile with another (stair/height co-occupancy).
            self.push(spec, walk::remove_creature_by_id(id));
            // The departed creature must be re-introduced (full form) if it ever
            // returns: drop it from each spectator's known-set.
            if let Some(s) = self.players.get_mut(&spec) {
                s.known.remove(&id);
            }
        }
    }

    /// Emit a `SaveRecord` for every online player **without** removing them or
    /// broadcasting. Called on graceful shutdown so in-memory outfit/position
    /// changes are persisted even when the sessions never logged out cleanly —
    /// otherwise killing the server reverts everyone to their last clean save.
    pub(super) fn save_all(&mut self) {
        let Some(tx) = &self.save_tx else { return };
        for p in self.players.values() {
            let _ = tx.send(SaveRecord {
                name: p.name.clone(),
                position: p.position,
                direction: p.direction,
                outfit: p.outfit,
                health: p.health,
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

    /// Apply a new outfit to `id`, then broadcast a `0x8E` creature-outfit packet
    /// to the player and all current spectators.
    ///
    /// If `id` is not in the game, this is a no-op.
    ///
    /// NOTE(pre-alpha): the requested outfit is trusted unconditionally.
    /// TFS checks `getOutfitAddons` to verify the player owns the addons;
    /// validation is deferred to a later milestone.
    pub(super) fn do_change_outfit(&mut self, id: u32, outfit: Outfit) {
        let pos = match self.players.get_mut(&id) {
            Some(p) => {
                p.outfit = outfit;
                p.position
            }
            None => return,
        };
        let pkt = outfit_packets::creature_outfit(id, &outfit);
        self.push(id, pkt.clone());
        for spec in self.spectators(pos, id) {
            self.push(spec, pkt.clone());
        }
    }

    /// Push a `0xC8` outfit-window packet to `id` only (no broadcast).
    ///
    /// The catalog is gender-correct: a female player (sex 0) is offered the
    /// female look_types, a male player (sex 1) the male ones — see
    /// [`crate::outfit_catalog`], sourced from `reference/tfs/data/XML/outfits.xml`.
    /// All outfits (free and premium) are offered with both addons (addons = 3):
    /// the 10.98 wire format carries no premium flag, so "available" simply means
    /// "present in the list".
    ///
    /// If `id` is not in the game, this is a no-op.
    pub(super) fn do_request_outfit(&mut self, id: u32) {
        let (outfit, sex) = match self.players.get(&id) {
            Some(p) => (p.outfit, p.sex),
            None => return,
        };
        let available: Vec<outfit_packets::AvailableOutfit> =
            crate::outfit_catalog::catalog_for_sex(sex)
                .iter()
                .map(|&(look_type, name)| outfit_packets::AvailableOutfit {
                    look_type,
                    name,
                    addons: 3,
                })
                .collect();
        let pkt = outfit_packets::outfit_window(&outfit, &available, &[]);
        self.push(id, pkt);
    }
}

#[cfg(test)]
mod tests {
    use super::super::test_support::*;
    use super::*;

    #[tokio::test]
    async fn login_pushes_appear_to_existing_spectator() {
        let (world, _save_rx) = spawn_from_static_map(walk_map(), GameConfig::default());
        let (tx_a, mut rx_a) = push_channel();
        let ack_a = world
            .login("A".into(), default_initial(knight()), tx_a)
            .await
            .unwrap();
        // Second player logs in next to A; A must receive a 0x6A appear.
        let (tx_b, _rx_b) = push_channel();
        let _ack_b = world
            .login("B".into(), default_initial(knight()), tx_b)
            .await
            .unwrap();
        let pkt = rx_a.recv().await.unwrap();
        assert_eq!(pkt[0], protocol::tile_creature::OP_ADD_TILE_CREATURE);
        // ...followed by the teleport puff, so spectators see the spawn effect too.
        let effect = rx_a.recv().await.unwrap();
        assert_eq!(effect[0], protocol::enter_world::OP_MAGIC_EFFECT);
        assert_ne!(ack_a.snapshot.id, 0);
    }

    #[tokio::test]
    async fn second_login_sees_first_in_ack_others() {
        let (world, _save_rx) = spawn_from_static_map(walk_map(), GameConfig::default());
        let (tx_a, _rx_a) = push_channel();
        world
            .login("A".into(), default_initial(knight()), tx_a)
            .await
            .unwrap();
        let (tx_b, _rx_b) = push_channel();
        let ack_b = world
            .login("B".into(), default_initial(knight()), tx_b)
            .await
            .unwrap();
        assert_eq!(ack_b.others.len(), 1, "B's enter-world includes A");
    }

    #[test]
    fn login_includes_visible_monster_in_others() {
        let mut g = Game::from_static_map_arc(walk_map());
        // Add a monster next to spawn BEFORE the player logs in
        let _mid = add_monster(&mut g, Position::new(96, 117, 7));
        let (tx, _rx) = mpsc::channel(super::super::PUSH_CAPACITY);
        let ack = g.login("Hero".into(), default_initial(knight()), tx);
        // others should contain the monster (appears in the login enter-world)
        let monster_ct = protocol::creature::CREATURETYPE_MONSTER;
        let has_monster = ack
            .others
            .iter()
            .any(|pc| pc.bytes.len() > 10 && pc.bytes[10] == monster_ct);
        assert!(has_monster, "login ack.others must include monster");
    }

    #[test]
    fn login_visible_monster_uses_creaturetype_monster() {
        let mut g = Game::from_static_map_arc(walk_map());
        let _mid = add_monster(&mut g, Position::new(96, 117, 7));
        let (tx, _rx) = mpsc::channel(super::super::PUSH_CAPACITY);
        let ack = g.login("Hero".into(), default_initial(knight()), tx);
        // Introduce monster in ack.others must have CREATURETYPE_MONSTER at
        // both creatureType positions (before name and after guild emblem).
        // For "Rat" (3-char name), creatureType1 is at byte 10, creatureType2 at byte 34.
        for pc in &ack.others {
            if pc.bytes.len() <= 34 {
                continue;
            }
            let ct1 = pc.bytes[10];
            let ct2 = pc.bytes[34];
            if ct1 == protocol::creature::CREATURETYPE_MONSTER {
                assert_eq!(
                    ct2,
                    protocol::creature::CREATURETYPE_MONSTER,
                    "both creatureType positions must be MONSTER for monsters"
                );
                return; // found the monster
            }
        }
        panic!("no monster with CREATURETYPE_MONSTER found in ack.others");
    }

    #[test]
    fn login_out_of_range_monster_not_included() {
        let mut g = Game::from_static_map_arc(walk_map());
        // Monster far from spawn (outside viewport).
        let _mid = add_monster(&mut g, Position::new(200, 200, 7));
        let (tx, _rx) = mpsc::channel(super::super::PUSH_CAPACITY);
        let ack = g.login("Hero".into(), default_initial(knight()), tx);
        let monster_ct = protocol::creature::CREATURETYPE_MONSTER;
        let has_monster = ack
            .others
            .iter()
            .any(|pc| pc.bytes.len() > 10 && pc.bytes[10] == monster_ct);
        assert!(
            !has_monster,
            "out-of-range monster must NOT be in ack.others"
        );
    }

    #[test]
    fn relogin_map_description_includes_dynamic_ground_items() {
        // Regression: a player logging in must see items dropped on the ground
        // (the dynamic overlay), not the pristine OTBM tile. The enter-world map
        // slice is encoded from the MERGED view inside `Game::login`; before the
        // fix it was encoded from the raw `StaticMap` in the server layer, so the
        // relogging client was blind to dynamic ground items while online
        // spectators (fed by `merged()`) still saw them.
        let mut g = Game::from_static_map_arc(walk_map());
        let drop_pos = Position::new(96, 117, 7); // adjacent to spawn, in viewport
        // A client id absent from the static encoding (ground 4526, wall 1059) so
        // its presence proves the dynamic item — not a coincidence — leaked in.
        const DROP_CID: u16 = 0x7777;

        // Baseline: a login BEFORE any drop must NOT carry the item.
        let (tx0, _rx0) = mpsc::channel(super::super::PUSH_CAPACITY);
        let initial0 = InitialState {
            position: Some(Position::new(95, 117, 7)),
            ..default_initial(knight())
        };
        let ack0 = g.login("Before".into(), initial0, tx0);
        assert!(
            !ack0
                .map_description
                .windows(2)
                .any(|w| w == DROP_CID.to_le_bytes()),
            "baseline login must not contain an item that was never dropped"
        );

        // Drop a non-stackable item on the ground next to spawn.
        let _ = g.add_to_ground_front(drop_pos, 999, DROP_CID, 1, false, false);

        // A fresh login near the same spot must now carry the dropped item.
        let (tx1, _rx1) = mpsc::channel(super::super::PUSH_CAPACITY);
        let initial1 = InitialState {
            position: Some(Position::new(95, 117, 7)),
            ..default_initial(knight())
        };
        let ack1 = g.login("After".into(), initial1, tx1);
        assert!(
            ack1.map_description
                .windows(2)
                .any(|w| w == DROP_CID.to_le_bytes()),
            "relogin map slice must include the dynamic ground item"
        );
    }

    #[tokio::test]
    async fn logout_pushes_remove_to_spectator() {
        let (world, _save_rx) = spawn_from_static_map(walk_map(), GameConfig::default());
        let (tx_a, mut rx_a) = push_channel();
        world
            .login("A".into(), default_initial(knight()), tx_a)
            .await
            .unwrap();
        let (tx_b, _rx_b) = push_channel();
        let ack_b = world
            .login("B".into(), default_initial(knight()), tx_b)
            .await
            .unwrap();
        let _ = rx_a.recv().await.unwrap(); // appear (0x6A)
        let effect = rx_a.recv().await.unwrap(); // login teleport puff (0x83)
        assert_eq!(effect[0], protocol::enter_world::OP_MAGIC_EFFECT);
        world.logout(ack_b.snapshot.id).await;
        // Logout pushes a teleport puff, then the remove.
        let poof = rx_a.recv().await.unwrap();
        assert_eq!(poof[0], protocol::enter_world::OP_MAGIC_EFFECT);
        let pkt = rx_a.recv().await.unwrap();
        assert_eq!(pkt[0], protocol::tile_creature::OP_REMOVE_TILE_THING);
    }

    #[tokio::test]
    async fn shutdown_and_save_persists_online_players_then_stops_actor() {
        // Graceful shutdown through the live actor: shutdown_and_save resolves
        // once the save record is queued, and the actor stops afterwards.
        let (world, mut save_rx) = spawn_from_static_map(walk_map(), GameConfig::default());
        let (tx_a, _rx_a) = push_channel();
        let ack = world
            .login("Diego".into(), default_initial(wizard_outfit()), tx_a)
            .await
            .unwrap();

        world.shutdown_and_save().await;

        // The logged-in player's state was persisted.
        let rec = save_rx
            .recv()
            .await
            .expect("shutdown must emit a SaveRecord");
        assert_eq!(rec.name, "Diego");
        assert_eq!(rec.position, ack.snapshot.position);
        assert_eq!(rec.outfit, wizard_outfit());

        // The actor has stopped: the save channel is closed (no more records)
        // and further logins fail because the command channel is gone.
        assert!(
            save_rx.recv().await.is_none(),
            "save channel must close after shutdown"
        );
        let (tx_b, _rx_b) = push_channel();
        assert!(
            world
                .login("B".into(), default_initial(knight()), tx_b)
                .await
                .is_none(),
            "logins must fail once the actor has shut down"
        );
    }

    #[tokio::test]
    async fn second_login_on_occupied_spawn_gets_free_tile() {
        let (world, _save_rx) = spawn_from_static_map(walk_map(), GameConfig::default());
        let (tx_a, _ra) = push_channel();
        let ack_a = world
            .login("A".into(), default_initial(knight()), tx_a)
            .await
            .unwrap();
        let (tx_b, _rb) = push_channel();
        let ack_b = world
            .login("B".into(), default_initial(knight()), tx_b)
            .await
            .unwrap();
        assert_ne!(
            ack_a.snapshot.position, ack_b.snapshot.position,
            "co-logins must not share a tile"
        );
    }

    #[test]
    fn login_on_occupied_saved_position_gets_free_adjacent_tile() {
        // A returning player carries a saved position. If someone is already
        // standing on that tile, login must bump them to a free adjacent tile —
        // you never log in on top of another creature. (Stair/height co-occupancy
        // is allowed during movement, but NOT on login.)
        let mut g = Game::from_static_map_arc(walk_map());
        let saved = Position::new(95, 117, 7);
        let (_occupant, _ro) = add_player(&mut g, saved); // someone is already there
        let (tx, _rx) = mpsc::channel(super::super::PUSH_CAPACITY);
        let initial = InitialState {
            position: Some(saved),
            direction: Direction::South,
            outfit: knight(),
            health: 150,
            max_health: 150,
            sex: 1,
            gamemaster: false,
            inventory: Vec::new(),
            container_items: Vec::new(),
        };
        let ack = g.login("Returning".into(), initial, tx);
        let ps = g.players.get(&ack.snapshot.id).expect("player must exist");
        assert_ne!(ps.position, saved, "must not log in on top of the occupant");
        assert!(
            g.chunks.is_walkable(ps.position),
            "bumped tile must be walkable"
        );
        let sharing = g
            .players
            .values()
            .filter(|p| p.position == ps.position)
            .count();
        assert_eq!(
            sharing, 1,
            "bumped tile must hold only the returning player"
        );
    }

    #[test]
    fn login_with_initial_position_places_player_at_that_position() {
        // RED: Game::login accepts InitialState { position: Some(p) } and places
        // the player at p with the given outfit and health.
        let mut g = Game::from_static_map_arc(walk_map());
        let (tx, _rx) = mpsc::channel(super::super::PUSH_CAPACITY);
        let pos = Position::new(96, 117, 7);
        let outfit = wizard_outfit();
        let initial = InitialState {
            position: Some(pos),
            direction: Direction::North,
            outfit,
            health: 80,
            max_health: 120,
            sex: 1,
            gamemaster: false,
            inventory: Vec::new(),
            container_items: Vec::new(),
        };
        let ack = g.login("Restored".into(), initial, tx);
        let ps = g.players.get(&ack.snapshot.id).expect("player must exist");
        assert_eq!(
            ps.position, pos,
            "restored player must be at saved position"
        );
        assert_eq!(ps.outfit, outfit, "restored player must have saved outfit");
        assert_eq!(ps.health, 80, "restored player must have saved health");
        assert_eq!(
            ps.max_health, 120,
            "restored player must have saved max_health"
        );
        assert_eq!(
            ps.direction,
            Direction::North,
            "restored player must face saved direction"
        );
        assert_eq!(ack.snapshot.outfit, outfit, "snapshot outfit must match");
    }

    #[test]
    fn login_with_no_position_falls_back_to_free_spawn() {
        // RED: Game::login with InitialState { position: None } resolves position
        // from free_spawn(), using default outfit/health for a new player.
        let mut g = Game::from_static_map_arc(walk_map());
        let (tx, _rx) = mpsc::channel(super::super::PUSH_CAPACITY);
        let spawn = g.meta.spawn();
        let initial = InitialState {
            position: None,
            direction: Direction::South,
            outfit: knight(),
            health: 150,
            max_health: 150,
            sex: 1,
            gamemaster: false,
            inventory: Vec::new(),
            container_items: Vec::new(),
        };
        let ack = g.login("NewPlayer".into(), initial, tx);
        assert_eq!(
            g.players.get(&ack.snapshot.id).unwrap().position,
            spawn,
            "new player with no saved position must spawn at free_spawn()"
        );
    }

    #[test]
    fn logout_with_save_tx_emits_save_record() {
        // RED: Game::logout emits a SaveRecord on save_tx when one is set.
        // The record must carry the player's current name/position/direction/outfit/health.
        let mut g = Game::from_static_map_arc(walk_map());
        let (save_tx, mut save_rx) = mpsc::unbounded_channel::<SaveRecord>();
        g.save_tx = Some(save_tx);

        let pos = Position::new(96, 117, 7);
        let outfit = wizard_outfit();
        let (tx, _rx) = mpsc::channel(super::super::PUSH_CAPACITY);
        let id = g.next_id;
        g.next_id += 1;
        g.players.insert(
            id,
            PlayerState {
                name: "Hero".into(),
                position: pos,
                direction: Direction::East,
                outfit,
                push_tx: tx,
                known: HashSet::new(),
                health: 77,
                max_health: 150,
                fist_skill: 10,
                race: RaceType::Blood,
                attacking: None,
                last_attack_ms: 0,
                sex: 1,
                gamemaster: false,
                ghost: false,
                prev_outfit: None,
                noclip: false,
                speed: 220,
                inventory: [None; 10],
                open_containers: std::array::from_fn(|_| None),
                follow_target: None,
                go_to_position: None,
                failed_repaths: None,
                list_walk_dir: VecDeque::new(),
                last_walk_ms: 0,
                conditions: Vec::new(),
            },
        );

        g.logout(id);

        let rec = save_rx.try_recv().expect("logout must emit a SaveRecord");
        assert_eq!(rec.name, "Hero");
        assert_eq!(rec.position, pos);
        assert_eq!(rec.direction, Direction::East);
        assert_eq!(rec.outfit, outfit);
        assert_eq!(rec.health, 77);
        assert_eq!(rec.max_health, 150);
    }

    #[test]
    fn save_all_emits_one_record_per_player_without_removing_them() {
        // Graceful shutdown: save_all must emit a SaveRecord for EVERY online
        // player (carrying their live outfit/position) and leave them in the map
        // — it persists without logging anyone out.
        let mut g = Game::from_static_map_arc(walk_map());
        let (save_tx, mut save_rx) = mpsc::unbounded_channel::<SaveRecord>();
        g.save_tx = Some(save_tx);

        let (tx_a, _rx_a) = mpsc::channel(super::super::PUSH_CAPACITY);
        let id_a = g.next_id;
        g.next_id += 1;
        let pos_a = Position::new(96, 117, 7);
        g.players.insert(
            id_a,
            PlayerState {
                name: "Diego".into(),
                position: pos_a,
                direction: Direction::North,
                outfit: wizard_outfit(),
                push_tx: tx_a,
                known: HashSet::new(),
                health: 90,
                max_health: 150,
                fist_skill: 10,
                race: RaceType::Blood,
                attacking: None,
                last_attack_ms: 0,
                sex: 1,
                gamemaster: true,
                ghost: false,
                prev_outfit: None,
                noclip: false,
                speed: 220,
                inventory: [None; 10],
                open_containers: std::array::from_fn(|_| None),
                follow_target: None,
                go_to_position: None,
                failed_repaths: None,
                list_walk_dir: VecDeque::new(),
                last_walk_ms: 0,
                conditions: Vec::new(),
            },
        );

        let (tx_b, _rx_b) = mpsc::channel(super::super::PUSH_CAPACITY);
        let id_b = g.next_id;
        g.next_id += 1;
        let pos_b = Position::new(100, 120, 7);
        g.players.insert(
            id_b,
            PlayerState {
                name: "Grissda".into(),
                position: pos_b,
                direction: Direction::South,
                outfit: knight(),
                push_tx: tx_b,
                known: HashSet::new(),
                health: 145,
                max_health: 145,
                fist_skill: 10,
                race: RaceType::Blood,
                attacking: None,
                last_attack_ms: 0,
                sex: 0,
                gamemaster: false,
                ghost: false,
                prev_outfit: None,
                noclip: false,
                speed: 220,
                inventory: [None; 10],
                open_containers: std::array::from_fn(|_| None),
                follow_target: None,
                go_to_position: None,
                failed_repaths: None,
                list_walk_dir: VecDeque::new(),
                last_walk_ms: 0,
                conditions: Vec::new(),
            },
        );

        g.save_all();

        // Both players must still be in the world (save_all does not log out).
        assert_eq!(g.players.len(), 2, "save_all must not remove players");
        assert!(g.players.contains_key(&id_a) && g.players.contains_key(&id_b));

        // Exactly two records, one per player, carrying live state.
        let mut recs = Vec::new();
        while let Ok(rec) = save_rx.try_recv() {
            recs.push(rec);
        }
        assert_eq!(
            recs.len(),
            2,
            "save_all must emit one record per online player"
        );

        let diego = recs
            .iter()
            .find(|r| r.name == "Diego")
            .expect("Diego record");
        assert_eq!(diego.position, pos_a);
        assert_eq!(diego.direction, Direction::North);
        assert_eq!(diego.outfit, wizard_outfit());
        assert_eq!(diego.health, 90);

        let grissda = recs
            .iter()
            .find(|r| r.name == "Grissda")
            .expect("Grissda record");
        assert_eq!(grissda.position, pos_b);
        assert_eq!(grissda.outfit, knight());
        assert_eq!(grissda.health, 145);
    }

    #[test]
    fn save_all_with_no_save_tx_is_a_noop() {
        // With no save channel wired, save_all must not panic (and there is
        // nowhere for records to go).
        let mut g = Game::from_static_map_arc(walk_map());
        let (tx, _rx) = mpsc::channel(super::super::PUSH_CAPACITY);
        let id = g.next_id;
        g.next_id += 1;
        g.players.insert(
            id,
            PlayerState {
                name: "Lonely".into(),
                position: Position::new(96, 117, 7),
                direction: Direction::West,
                outfit: knight(),
                push_tx: tx,
                known: HashSet::new(),
                health: 100,
                max_health: 100,
                fist_skill: 10,
                race: RaceType::Blood,
                attacking: None,
                last_attack_ms: 0,
                sex: 1,
                gamemaster: false,
                ghost: false,
                prev_outfit: None,
                noclip: false,
                speed: 220,
                inventory: [None; 10],
                open_containers: std::array::from_fn(|_| None),
                follow_target: None,
                go_to_position: None,
                failed_repaths: None,
                list_walk_dir: VecDeque::new(),
                last_walk_ms: 0,
                conditions: Vec::new(),
            },
        );

        g.save_all(); // must not panic
        assert_eq!(
            g.players.len(),
            1,
            "players are untouched when there is no save_tx"
        );
    }

    #[test]
    fn push_to_dead_channel_reap_also_emits_save_record() {
        // RED: The internal dead-session reap path (push() -> logout()) also emits
        // a SaveRecord when save_tx is set.
        let mut g = Game::from_static_map_arc(walk_map());
        let (save_tx, mut save_rx) = mpsc::unbounded_channel::<SaveRecord>();
        g.save_tx = Some(save_tx);

        // Create a player whose push channel has a DROPPED receiver — any push will fail.
        let (tx, rx) = mpsc::channel::<Vec<u8>>(1);
        drop(rx); // receiver gone: try_send will immediately fail
        let id = g.next_id;
        g.next_id += 1;
        g.players.insert(
            id,
            PlayerState {
                name: "Ghost".into(),
                position: g.meta.spawn(),
                direction: Direction::South,
                outfit: knight(),
                push_tx: tx,
                known: HashSet::new(),
                health: 50,
                max_health: 150,
                fist_skill: 10,
                race: RaceType::Blood,
                attacking: None,
                last_attack_ms: 0,
                sex: 1,
                gamemaster: false,
                ghost: false,
                prev_outfit: None,
                noclip: false,
                speed: 220,
                inventory: [None; 10],
                open_containers: std::array::from_fn(|_| None),
                follow_target: None,
                go_to_position: None,
                failed_repaths: None,
                list_walk_dir: VecDeque::new(),
                last_walk_ms: 0,
                conditions: Vec::new(),
            },
        );

        // Pushing any payload triggers the dead-session reap → logout → save.
        g.push(id, vec![0xFF]);
        let rec = save_rx
            .try_recv()
            .expect("dead-session reap must also emit a SaveRecord");
        assert_eq!(rec.name, "Ghost");
        assert_eq!(rec.health, 50);
    }

    #[test]
    fn change_outfit_updates_player_state() {
        let mut g = Game::from_static_map_arc(walk_map());
        let (id, _rx) = add_player(&mut g, Position::new(95, 117, 7));
        let new_outfit = Outfit {
            look_type: 130,
            head: 10,
            body: 20,
            legs: 30,
            feet: 40,
            addons: 3,
            mount: 0,
        };
        g.do_change_outfit(id, new_outfit);
        assert_eq!(g.players[&id].outfit, new_outfit);
    }

    #[test]
    fn change_outfit_broadcasts_0x8e_to_player_and_spectator() {
        let mut g = Game::from_static_map_arc(walk_map());
        // Both players at the same tile so they are each other's spectators.
        let (id, mut rx_self) = add_player(&mut g, Position::new(95, 117, 7));
        let (_spec, mut rx_spec) = add_player(&mut g, Position::new(95, 117, 7));
        let new_outfit = Outfit {
            look_type: 130,
            head: 0,
            body: 0,
            legs: 0,
            feet: 0,
            addons: 0,
            mount: 0,
        };
        g.do_change_outfit(id, new_outfit);

        // Drain initial login messages; the LAST packet in the channel is the outfit broadcast.
        let pkt_self = {
            let mut last = None;
            while let Ok(p) = rx_self.try_recv() {
                last = Some(p);
            }
            last.expect("player must receive at least one packet (the 0x8E)")
        };
        let pkt_spec = {
            let mut last = None;
            while let Ok(p) = rx_spec.try_recv() {
                last = Some(p);
            }
            last.expect("spectator must receive at least one packet (the 0x8E)")
        };
        assert_eq!(
            pkt_self[0],
            protocol::outfit::OP_CREATURE_OUTFIT,
            "player must receive 0x8E"
        );
        assert_eq!(
            pkt_spec[0],
            protocol::outfit::OP_CREATURE_OUTFIT,
            "spectator must receive 0x8E"
        );
        // Both packets must carry the changer's id.
        let id_bytes = id.to_le_bytes();
        assert_eq!(&pkt_self[1..5], &id_bytes);
        assert_eq!(&pkt_spec[1..5], &id_bytes);
    }

    #[test]
    fn change_outfit_unknown_id_is_noop() {
        let mut g = Game::from_static_map_arc(walk_map());
        // Should not panic; game has no players.
        g.do_change_outfit(
            0xDEAD_BEEF,
            Outfit {
                look_type: 130,
                head: 0,
                body: 0,
                legs: 0,
                feet: 0,
                addons: 0,
                mount: 0,
            },
        );
    }

    #[test]
    fn request_outfit_sends_0xc8_to_requester_only() {
        let mut g = Game::from_static_map_arc(walk_map());
        let (id, mut rx_self) = add_player(&mut g, Position::new(95, 117, 7));
        let (_spec, mut rx_spec) = add_player(&mut g, Position::new(95, 117, 7));
        // Drain any login-side packets first.
        while rx_self.try_recv().is_ok() {}
        while rx_spec.try_recv().is_ok() {}
        g.do_request_outfit(id);
        let pkt = rx_self.try_recv().expect("requester must receive 0xC8");
        assert_eq!(
            pkt[0],
            protocol::outfit::OP_OUTFIT_WINDOW,
            "packet must be 0xC8"
        );
        assert!(
            rx_spec.try_recv().is_err(),
            "spectator must NOT receive anything"
        );
    }

    #[test]
    fn request_outfit_male_gets_male_catalog() {
        let mut g = Game::from_static_map_arc(walk_map());
        let (id, mut rx) = add_player(&mut g, Position::new(95, 117, 7)); // sex = 1 (male)
        while rx.try_recv().is_ok() {}
        g.do_request_outfit(id);
        let pkt = rx.try_recv().expect("requester must receive 0xC8");
        assert_eq!(pkt[0], protocol::outfit::OP_OUTFIT_WINDOW);
        let types = outfit_window_looktypes(&pkt);
        assert_eq!(types.len(), 55, "male catalog has all 55 outfits");
        assert!(
            types.contains(&128),
            "male catalog contains male Citizen (128)"
        );
        assert!(
            !types.contains(&136),
            "male catalog must NOT contain female Citizen (136)"
        );
    }

    #[test]
    fn request_outfit_female_gets_female_catalog() {
        let mut g = Game::from_static_map_arc(walk_map());
        let (id, mut rx) = add_player(&mut g, Position::new(95, 117, 7));
        g.players.get_mut(&id).unwrap().sex = 0; // female
        while rx.try_recv().is_ok() {}
        g.do_request_outfit(id);
        let pkt = rx.try_recv().expect("requester must receive 0xC8");
        let types = outfit_window_looktypes(&pkt);
        assert_eq!(types.len(), 55, "female catalog has all 55 outfits");
        assert!(
            types.contains(&136),
            "female catalog contains female Citizen (136)"
        );
        assert!(
            !types.contains(&128),
            "female catalog must NOT contain male Citizen (128)"
        );
    }

    #[test]
    fn sex_is_set_from_initial_state_on_login() {
        // RED: InitialState must carry a `sex` field that is stored in the live
        // PlayerState and exposed via do_request_outfit catalog selection later.
        let mut g = Game::from_static_map_arc(walk_map());
        let (tx, _rx) = mpsc::channel(super::super::PUSH_CAPACITY);
        let initial = InitialState {
            position: None,
            direction: Direction::South,
            outfit: knight(),
            health: 150,
            max_health: 150,
            sex: 0, // female
            gamemaster: false,
            inventory: Vec::new(),
            container_items: Vec::new(),
        };
        let ack = g.login("Tester".into(), initial, tx);
        assert_eq!(
            g.players[&ack.snapshot.id].sex, 0,
            "sex from InitialState must be stored in PlayerState"
        );
    }

    #[test]
    fn sex_is_emitted_in_save_record_on_logout() {
        // RED: logout must carry sex into SaveRecord so the server can persist it.
        let mut g = Game::from_static_map_arc(walk_map());
        let (save_tx, mut save_rx) = mpsc::unbounded_channel::<SaveRecord>();
        g.save_tx = Some(save_tx);
        let (tx, _rx) = mpsc::channel(super::super::PUSH_CAPACITY);
        let initial = InitialState {
            position: None,
            direction: Direction::South,
            outfit: knight(),
            health: 150,
            max_health: 150,
            sex: 0, // female
            gamemaster: false,
            inventory: Vec::new(),
            container_items: Vec::new(),
        };
        let ack = g.login("Tester".into(), initial, tx);
        let id = ack.snapshot.id;
        g.logout(id);
        let rec = save_rx.try_recv().expect("logout must emit a SaveRecord");
        assert_eq!(
            rec.sex, 0,
            "sex must round-trip login→logout through SaveRecord"
        );
    }

    // ===================================================================
    // Task 3.5: walkthrough = 1 in creature packets for ghost GM;
    // walkthrough = 0 for normal.
    // ===================================================================

    #[test]
    fn introduce_ghost_gm_has_walkthrough_byte_set() {
        let mut g = Game::from_static_map_arc(walk_map());
        let (gm, _rg) = add_player(&mut g, Position::new(95, 117, 7));
        g.players.get_mut(&gm).unwrap().gamemaster = true;
        let (viewer, _rv) = add_player(&mut g, Position::new(96, 117, 7));

        // Normal (non-ghost): introduce should have walkthrough = 0.
        let normal_bytes = g.introduce(viewer, gm).unwrap();
        // Walkthrough byte is the last byte of the creature thing.
        let wt_normal = normal_bytes[normal_bytes.len() - 1];
        assert_eq!(wt_normal, 0, "non-ghost introduce must have walkthrough=0");

        // Enable ghost.
        g.do_gm_command(gm, "/ghost".into());

        // Ghost: introduce should have walkthrough = 1.
        let ghost_bytes = g.introduce(viewer, gm).unwrap();
        let wt_ghost = ghost_bytes[ghost_bytes.len() - 1];
        assert_eq!(wt_ghost, 1, "ghost GM introduce must have walkthrough=1");
    }

    // ===================================================================
    // Task 3.6: ghost NOT in SaveRecord; login resets ghost to false.
    // ===================================================================

    #[test]
    fn ghost_not_persisted_in_save_record() {
        // When saving, the ghost flag must NOT appear in the SaveRecord.
        // SaveRecord only has explicit fields — ghost is runtime-only.
        let mut g = Game::from_static_map_arc(walk_map());
        let (save_tx, mut save_rx) = mpsc::unbounded_channel::<SaveRecord>();
        g.save_tx = Some(save_tx);
        let (tx, _rx) = mpsc::channel(super::super::PUSH_CAPACITY);
        let id = g.next_id;
        g.next_id += 1;
        g.players.insert(
            id,
            PlayerState {
                name: "GhostGuy".into(),
                position: g.meta.spawn(),
                direction: Direction::South,
                outfit: knight(),
                push_tx: tx,
                known: HashSet::new(),
                health: 150,
                max_health: 150,
                fist_skill: 10,
                race: RaceType::Blood,
                attacking: None,
                last_attack_ms: 0,
                sex: 1,
                gamemaster: true,
                ghost: true,
                prev_outfit: Some(knight()),
                noclip: false,
                speed: 220,
                inventory: [None; 10],
                open_containers: std::array::from_fn(|_| None),
                follow_target: None,
                go_to_position: None,
                failed_repaths: None,
                list_walk_dir: VecDeque::new(),
                last_walk_ms: 0,
                conditions: Vec::new(),
            },
        );

        g.logout(id);
        let rec = save_rx.try_recv().expect("logout must emit SaveRecord");
        // SaveRecord has no ghost field — the struct doesn't have one.
        // Just verify the record was emitted and contains expected fields.
        assert_eq!(rec.name, "GhostGuy");
        assert_eq!(
            rec.outfit,
            knight(),
            "outfit is the knight outfit, not ghost"
        );
    }

    #[test]
    fn ghost_resets_on_login() {
        // Fresh login must have ghost = false.
        let mut g = Game::from_static_map_arc(walk_map());
        let (tx, _rx) = mpsc::channel(super::super::PUSH_CAPACITY);
        let initial = InitialState {
            position: Some(Position::new(95, 117, 7)),
            direction: Direction::South,
            outfit: knight(),
            health: 150,
            max_health: 150,
            sex: 1,
            gamemaster: true,
            inventory: Vec::new(),
            container_items: Vec::new(),
        };
        let ack = g.login("TestGM".into(), initial, tx);
        let p = g.players.get(&ack.snapshot.id).unwrap();
        assert!(!p.ghost, "ghost must be false after login");
        assert_eq!(p.prev_outfit, None, "prev_outfit must be None after login");
    }

    // ===================================================================
    // Task 3.9: noclip NOT in SaveRecord; login resets noclip to false.
    // ===================================================================

    // ===================================================================
    // Task 3.6: creature_speed() reads from PlayerState.speed
    // ===================================================================

    #[test]
    fn creature_speed_returns_player_speed_from_state() {
        let mut g = Game::from_static_map_arc(walk_map());
        let (id, _rx) = add_player(&mut g, Position::new(95, 117, 7));
        // Default speed from add_player() -> 220
        assert_eq!(g.creature_speed(id), 220, "default speed must be 220");
        // Change speed directly
        g.players.get_mut(&id).unwrap().speed = 500;
        assert_eq!(
            g.creature_speed(id),
            500,
            "creature_speed must read live p.speed"
        );
        // Change to another value
        g.players.get_mut(&id).unwrap().speed = 150;
        assert_eq!(
            g.creature_speed(id),
            150,
            "creature_speed must reflect updated p.speed"
        );
    }

    // ===================================================================
    // Task 3.9: noclip NOT in SaveRecord; login resets noclip to false.
    // ===================================================================

    #[test]
    fn noclip_resets_on_login() {
        // Fresh login must have noclip = false.
        let mut g = Game::from_static_map_arc(walk_map());
        let (tx, _rx) = mpsc::channel(super::super::PUSH_CAPACITY);
        let initial = InitialState {
            position: Some(Position::new(95, 117, 7)),
            direction: Direction::South,
            outfit: knight(),
            health: 150,
            max_health: 150,
            sex: 1,
            gamemaster: true,
            inventory: Vec::new(),
            container_items: Vec::new(),
        };
        let ack = g.login("TestGM".into(), initial, tx);
        assert!(
            !g.players[&ack.snapshot.id].noclip,
            "noclip must be false after login"
        );
    }
}
