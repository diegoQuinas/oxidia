//! Gamemaster commands for the game actor.

use super::*;

/// Every gamemaster command. This enum is the single source of truth: the
/// exhaustive `usage`/`description` matches force any new variant to declare its
/// help text (it won't compile otherwise), and the dispatch `match` in
/// `do_gm_command` forces it to be wired. `/help` iterates [`GmVerb::ALL`], so it
/// can never drift out of sync — the only manual step when adding a command is to
/// list its variant in `ALL`.
#[derive(Clone, Copy, PartialEq)]
enum GmVerb {
    Help,
    Item,
    Goto,
    Temple,
    Teleport,
    TeleportTo,
    Bring,
    ChangeSex,
    SetLookType,
    ReloadLua,
    Monster,
    Ghost,
    Noclip,
    Speed,
}

impl GmVerb {
    /// All commands, in the order `/help` lists them.
    const ALL: &'static [GmVerb] = &[
        Self::Help,
        Self::Item,
        Self::Goto,
        Self::Temple,
        Self::Teleport,
        Self::TeleportTo,
        Self::Bring,
        Self::ChangeSex,
        Self::SetLookType,
        Self::ReloadLua,
        Self::Monster,
        Self::Ghost,
        Self::Noclip,
        Self::Speed,
    ];

    /// Accepted command words (first is canonical; the rest are aliases).
    fn words(self) -> &'static [&'static str] {
        match self {
            Self::Help => &["help"],
            Self::Item => &["item", "i"],
            Self::Goto => &["goto"],
            Self::Temple => &["temple"],
            Self::Teleport => &["teleport"],
            Self::TeleportTo => &["teleportto"],
            Self::Bring => &["bring"],
            Self::ChangeSex => &["changesex"],
            Self::SetLookType => &["setlooktype"],
            Self::ReloadLua => &["reload"],
            Self::Monster => &["monster", "m"],
            Self::Ghost => &["ghost"],
            Self::Noclip => &["noclip"],
            Self::Speed => &["speed", "spd"],
        }
    }

    /// One-line usage syntax shown by `/help`.
    fn usage(self) -> &'static str {
        match self {
            Self::Help => "/help",
            Self::Item => "/item <id|name> [count]",
            Self::Goto => "/goto <x> <y> <z> | /goto \"player\"",
            Self::Temple => "/temple [\"name\"|id]",
            Self::Teleport => "/teleport \"player\" <x> <y> <z>",
            Self::TeleportTo => "/teleportto \"player\"",
            Self::Bring => "/bring \"player\"",
            Self::ChangeSex => "/changesex \"player\" <male|female>",
            Self::SetLookType => "/setlooktype <id> | /setlooktype \"player\" <id>",
            Self::ReloadLua => "/reload lua",
            Self::Monster => "/m <name>",
            Self::Ghost => "/ghost",
            Self::Noclip => "/noclip",
            Self::Speed => "/speed <value> | /speed \"player\" <value>",
        }
    }

    /// Short description shown by `/help`.
    fn description(self) -> &'static str {
        match self {
            Self::Help => "List all gamemaster commands.",
            Self::Item => "Spawn an item on your tile, by id or name.",
            Self::Goto => "Teleport yourself to coordinates or to a player.",
            Self::Temple => "Teleport to a town temple (default, by name, or by id).",
            Self::Teleport => "Teleport another player to coordinates.",
            Self::TeleportTo => "Teleport yourself next to another player.",
            Self::Bring => "Teleport another player to you.",
            Self::ChangeSex => "Change a player's sex (affects outfit catalog).",
            Self::SetLookType => "Set look type on yourself or another player.",
            Self::ReloadLua => "Reload all Lua scripts from disk without restarting the server.",
            Self::Monster => "Spawn a creature (monster) next to you, by name.",
            Self::Ghost => "Toggle ghost mode: invisible to non-GMs, walk through everything.",
            Self::Noclip => "Toggle noclip mode: walk through everything, remains visible.",
            Self::Speed => "Set movement speed on yourself or another player (10-2500).",
        }
    }

    /// Resolve a command word (verb or alias) to its variant.
    fn from_word(word: &str) -> Option<GmVerb> {
        GmVerb::ALL
            .iter()
            .copied()
            .find(|v| v.words().contains(&word))
    }
}

/// Split a GM command's argument string into tokens, treating `"..."` as a
/// single token (so `"Gold Coin" 100` → `["Gold Coin", "100"]`). Unquoted runs
/// split on whitespace; an unterminated quote consumes to the end of input.
fn tokenize_args(s: &str) -> Vec<String> {
    let mut tokens = Vec::new();
    let mut chars = s.chars().peekable();
    while let Some(&c) = chars.peek() {
        if c.is_whitespace() {
            chars.next();
        } else if c == '"' {
            chars.next(); // consume opening quote
            let mut tok = String::new();
            for c in chars.by_ref() {
                if c == '"' {
                    break;
                }
                tok.push(c);
            }
            tokens.push(tok);
        } else {
            let mut tok = String::new();
            while let Some(&c) = chars.peek() {
                if c.is_whitespace() {
                    break;
                }
                tok.push(c);
                chars.next();
            }
            tokens.push(tok);
        }
    }
    tokens
}

/// Parse `<x> <y> <z>` from the front of a GM command's args. `None` if any
/// coordinate is missing or out of range.
fn parse_pos(args: &[&str]) -> Option<Position> {
    let x = args.first()?.parse::<u16>().ok()?;
    let y = args.get(1)?.parse::<u16>().ok()?;
    let z = args.get(2)?.parse::<u8>().ok()?;
    Some(Position::new(x, y, z))
}

impl Game {
    /// Gate + parse + dispatch for `/`-prefixed GM commands. Non-gamemasters are
    /// silently ignored (their `/` line is simply dropped). Every parse/lookup
    /// failure replies to the sender via `push_status_message` and leaves the
    /// world untouched — no panics, no partial state.
    pub(super) fn do_gm_command(&mut self, id: u32, text: String) {
        if !self.players.get(&id).map(|p| p.gamemaster).unwrap_or(false) {
            return; // not a GM: drop silently
        }
        let line = text.trim_start_matches('/').trim();
        // Quote-aware tokenization so multi-word arguments survive: item names
        // (`/item "Gold Coin" 100`) and player names with spaces (`/bring "God Diego"`).
        let tokens = tokenize_args(line);
        let Some((verb, rest)) = tokens.split_first() else {
            return;
        };
        let args: Vec<&str> = rest.iter().map(|s| s.as_str()).collect();
        let Some(cmd) = GmVerb::from_word(verb) else {
            self.push_status_message(
                id,
                format!("Unknown command: /{verb}. Type /help.").as_bytes(),
            );
            return;
        };
        match cmd {
            GmVerb::Help => self.gm_help(id),
            GmVerb::Item => self.gm_item(id, &args),
            GmVerb::Goto => self.gm_goto(id, &args),
            GmVerb::Temple => self.gm_temple(id, &args),
            GmVerb::Teleport => self.gm_teleport(id, &args),
            GmVerb::TeleportTo => self.gm_teleportto(id, &args),
            GmVerb::Bring => self.gm_bring(id, &args),
            GmVerb::ChangeSex => self.gm_changesex(id, &args),
            GmVerb::SetLookType => self.gm_setlooktype(id, &args),
            GmVerb::ReloadLua => self.gm_reload_lua(id),
            GmVerb::Monster => self.gm_monster(id, &args),
            GmVerb::Ghost => self.gm_ghost(id),
            GmVerb::Noclip => self.gm_noclip(id),
            GmVerb::Speed => self.gm_speed(id, &args),
        }
    }

    /// `/help` — list every gamemaster command with a short description. Sent as
    /// `0xB4` console messages to the requester only: this server has no chat
    /// channels yet, but console text is private to the session and scrollable,
    /// which is what `/help` needs. Iterates [`GmVerb::ALL`], so newly added
    /// commands appear automatically.
    ///
    /// TODO(future): real chat channels (0xB2/0xAB/0xAC handshake) for richer GM
    /// output — see docs/superpowers/backlog.md.
    fn gm_help(&mut self, id: u32) {
        self.push_console_blue(id, "Gamemaster commands:");
        for &cmd in GmVerb::ALL {
            self.push_console_blue(id, &format!("{} - {}", cmd.usage(), cmd.description()));
        }
    }

    /// Find an online player's creature id by name (case-insensitive).
    fn find_player_by_name(&self, name: &str) -> Option<u32> {
        self.players
            .iter()
            .find(|(_, p)| p.name.eq_ignore_ascii_case(name))
            .map(|(&id, _)| id)
    }

    /// `/goto <x> <y> <z>` — teleport the GM to coordinates.
    /// `/goto <player>` — teleport the GM to that online player's tile.
    fn gm_goto(&mut self, id: u32, args: &[&str]) {
        // Coordinate form: three numeric args.
        if let Some(pos) = parse_pos(args) {
            if !self.chunks.has_ground(pos) {
                self.push_status_message(id, b"There is no tile there.");
                return;
            }
            self.do_teleport(id, pos);
            self.push_status_message(
                id,
                format!("Teleported to {}, {}, {}.", pos.x, pos.y, pos.z).as_bytes(),
            );
            return;
        }
        // Player form: a single name argument (quote multi-word names).
        if let [name] = args {
            let Some(target) = self.find_player_by_name(name) else {
                self.push_status_message(id, format!("Player '{name}' not found.").as_bytes());
                return;
            };
            let Some(pos) = self.players.get(&target).map(|p| p.position) else {
                return;
            };
            self.do_teleport(id, pos);
            self.push_status_message(id, format!("Teleported to {name}.").as_bytes());
            return;
        }
        self.push_status_message(id, b"Usage: /goto <x> <y> <z> | /goto \"player\"");
    }

    /// `/temple` — teleport to the default (spawn) town temple.
    /// `/temple <name>` — teleport to the named town's temple.
    /// `/temple <id>` — teleport to the temple of the town with that id.
    /// (No per-character home town exists yet, so the no-arg form uses the
    /// server's spawn temple — see `StaticMap::temple_for`.)
    ///
    /// TODO(future): per-character home town so no-arg /temple targets the char's
    /// own town — see docs/superpowers/backlog.md.
    fn gm_temple(&mut self, id: u32, args: &[&str]) {
        let temple = match args.first() {
            None => self.meta.spawn(),
            Some(arg) => match arg.parse::<u32>() {
                Ok(town_id) => match self.meta.town_temple_by_id(town_id) {
                    Some(p) => p,
                    None => {
                        self.push_status_message(
                            id,
                            format!("No town with id {town_id}.").as_bytes(),
                        );
                        return;
                    }
                },
                Err(_) => match self.meta.town_temple_by_name(arg) {
                    Some(p) => p,
                    None => {
                        self.push_status_message(id, format!("No town named '{arg}'.").as_bytes());
                        return;
                    }
                },
            },
        };
        self.do_teleport(id, temple);
        self.push_status_message(
            id,
            format!(
                "Teleported to temple ({}, {}, {}).",
                temple.x, temple.y, temple.z
            )
            .as_bytes(),
        );
    }

    /// `/item <id|name> [count]` — spawn an item on the GM's own tile. A leading
    /// number is a server id; otherwise the name is taken from the arguments
    /// (case-insensitive, singular or plural, no quotes needed for multi-word
    /// names) and an optional trailing number is the count. Quotes still group.
    fn gm_item(&mut self, id: u32, args: &[&str]) {
        if args.is_empty() {
            self.push_status_message(id, b"Usage: /item <id|name> [count]");
            return;
        }
        // ID form: a leading number is unambiguously a server id, since no item
        // name in Tibia contains digits. `/item 2400 100`.
        let (server_id, count) = if let Ok(server_id) = args[0].parse::<u16>() {
            let count = args.get(1).and_then(|s| s.parse::<u16>().ok()).unwrap_or(1);
            (server_id, count)
        } else {
            // Name form: an optional trailing number is the count; everything
            // before it joins into the (possibly multi-word, unquoted) item name.
            // `/item crystal coin 100` → name "crystal coin", count 100.
            let (name_tokens, count) = match args.split_last() {
                Some((last, head)) if !head.is_empty() => match last.parse::<u16>() {
                    Ok(n) => (head, n),
                    Err(_) => (args, 1),
                },
                _ => (args, 1),
            };
            let name = name_tokens.join(" ");
            match self.meta.find_item_id_by_name(&name) {
                Some(sid) => (sid, count),
                None => {
                    self.push_status_message(id, format!("No item named '{name}'.").as_bytes());
                    return;
                }
            }
        };
        let Some(pos) = self.players.get(&id).map(|p| p.position) else {
            return;
        };
        self.do_spawn_item(id, pos, server_id, count);
    }

    /// `/teleport <name> <x> <y> <z>` — teleport another player to a position.
    fn gm_teleport(&mut self, id: u32, args: &[&str]) {
        let Some(name) = args.first() else {
            self.push_status_message(id, b"Usage: /teleport <name> <x> <y> <z>");
            return;
        };
        let Some(pos) = parse_pos(&args[1..]) else {
            self.push_status_message(id, b"Usage: /teleport <name> <x> <y> <z>");
            return;
        };
        let Some(target) = self.find_player_by_name(name) else {
            self.push_status_message(id, format!("Player '{name}' not found.").as_bytes());
            return;
        };
        if !self.chunks.has_ground(pos) {
            self.push_status_message(id, b"There is no tile there.");
            return;
        }
        self.do_teleport(target, pos);
        self.push_status_message(
            id,
            format!("Teleported {} to {}, {}, {}.", name, pos.x, pos.y, pos.z).as_bytes(),
        );
    }

    /// `/teleportto <name>` — teleport the GM to another player's tile.
    fn gm_teleportto(&mut self, id: u32, args: &[&str]) {
        let Some(name) = args.first() else {
            self.push_status_message(id, b"Usage: /teleportto <name>");
            return;
        };
        let Some(target) = self.find_player_by_name(name) else {
            self.push_status_message(id, format!("Player '{name}' not found.").as_bytes());
            return;
        };
        let Some(pos) = self.players.get(&target).map(|p| p.position) else {
            return;
        };
        self.do_teleport(id, pos);
        self.push_status_message(id, format!("Teleported to {name}.").as_bytes());
    }

    /// `/bring <name>` — teleport another player to the GM's tile.
    fn gm_bring(&mut self, id: u32, args: &[&str]) {
        let Some(name) = args.first() else {
            self.push_status_message(id, b"Usage: /bring <name>");
            return;
        };
        let Some(target) = self.find_player_by_name(name) else {
            self.push_status_message(id, format!("Player '{name}' not found.").as_bytes());
            return;
        };
        let Some(pos) = self.players.get(&id).map(|p| p.position) else {
            return;
        };
        self.do_teleport(target, pos);
        self.push_status_message(id, format!("Brought {name} to you.").as_bytes());
    }

    fn gm_changesex(&mut self, id: u32, args: &[&str]) {
        let (Some(name), Some(sex_str)) = (args.first(), args.get(1)) else {
            self.push_status_message(id, b"Usage: /changesex <name> <male|female>");
            return;
        };
        let new_sex: u8 = match sex_str.to_lowercase().as_str() {
            "male" => 1,
            "female" => 0,
            _ => {
                self.push_status_message(id, b"Sex must be 'male' or 'female'.");
                return;
            }
        };
        let Some(target) = self.find_player_by_name(name) else {
            self.push_status_message(id, format!("Player '{name}' not found.").as_bytes());
            return;
        };
        match self.players.get_mut(&target) {
            Some(p) => p.sex = new_sex,
            None => return,
        };
        let sex_label = if new_sex == 1 { "male" } else { "female" };
        self.push_status_message(id, format!("{name} is now {sex_label}.").as_bytes());
    }

    fn gm_setlooktype(&mut self, id: u32, args: &[&str]) {
        // /setlooktype <id>  OR  /setlooktype "player" <id>
        let (target, look_type) = match args {
            [raw_id] => {
                let Ok(lt) = raw_id.parse::<u16>() else {
                    self.push_status_message(
                        id,
                        b"Usage: /setlooktype <id> | /setlooktype \"player\" <id>",
                    );
                    return;
                };
                (id, lt)
            }
            [name, raw_id] => {
                let Ok(lt) = raw_id.parse::<u16>() else {
                    self.push_status_message(
                        id,
                        b"Usage: /setlooktype <id> | /setlooktype \"player\" <id>",
                    );
                    return;
                };
                let Some(t) = self.find_player_by_name(name) else {
                    self.push_status_message(id, format!("Player '{name}' not found.").as_bytes());
                    return;
                };
                (t, lt)
            }
            _ => {
                self.push_status_message(
                    id,
                    b"Usage: /setlooktype <id> | /setlooktype \"player\" <id>",
                );
                return;
            }
        };
        let new_outfit = match self.players.get_mut(&target) {
            Some(p) => {
                p.outfit.look_type = look_type;
                p.outfit
            }
            None => return,
        };
        self.do_change_outfit(target, new_outfit);
        self.push_status_message(id, format!("Look type set to {look_type}.").as_bytes());
    }

    /// `/reload lua` — drop and re-initialise the Lua runtime so script changes
    /// on disk take effect without restarting the server. Reports success/failure
    /// as a `0xB4` status message to the requesting GM.
    fn gm_reload_lua(&mut self, id: u32) {
        self.do_reload_lua();
        self.push_status_message(id, b"Lua scripts reloaded from disk.");
    }

    /// `/m <name>` — spawn a monster next to the GM. The name is used as-is
    /// (case-insensitive lookup against monster data is deferred). Creates the
    /// creature, inserts it, and broadcasts its appearance to nearby spectators.
    fn gm_monster(&mut self, id: u32, args: &[&str]) {
        if args.is_empty() {
            self.push_status_message(id, b"Usage: /m <name>");
            return;
        }
        let name = args.join(" ");
        let Some(pos) = self.players.get(&id).map(|p| p.position) else {
            return;
        };
        if !self.materialize(pos) {
            self.push_status_message(id, b"You cannot spawn a creature there.");
            return;
        }
        let mid = self.next_monster_id;
        self.next_monster_id += 1;
        let template = self.monster_types.get(&name.to_ascii_lowercase());
        let monster = MonsterState {
            name: name.clone(),
            position: pos,
            direction: Direction::South,
            health: template.map(|t| t.health).unwrap_or(100),
            max_health: template.map(|t| t.max_health).unwrap_or(100),
            speed: template.map(|t| t.speed).unwrap_or(200),
            look_type: template.map(|t| t.look_type).unwrap_or(100),
            attacking: None,
            last_attack_ms: 0,
            attack: template.map(|t| t.attack).unwrap_or(7),
            loot: template.map(|t| t.loot.clone()).unwrap_or_default(),
            spawn_id: None,
            list_walk_dir: VecDeque::new(),
            follow_target: None,
            target_distance: template.map(|t| t.target_distance).unwrap_or(0),
            race: template.and_then(|t| t.race),
        };
        self.monsters.insert(mid, monster);
        // Broadcast the new monster to every spectator (including the GM).
        for spec in self.spectators(pos, u32::MAX) {
            if let Some(bytes) = self.introduce(spec, mid) {
                let stackpos = self.creature_stackpos_on(pos, mid);
                self.push(
                    spec,
                    tile_creature::add_tile_creature((pos.x, pos.y, pos.z), stackpos, &bytes),
                );
                self.push(
                    spec,
                    enter_world::magic_effect(pos.x, pos.y, pos.z, enter_world::EFFECT_TELEPORT),
                );
            }
        }
        self.push_status_message(id, format!("Spawned {name}.").as_bytes());
    }

    /// Place a fresh item on `pos` and broadcast a `0x6A` add to spectators.
    /// Mirrors the destination half of `do_move_thing`: materialize the tile,
    /// insert at the front of the down-items (newest on top), broadcast at the
    /// top down-item stackpos. Replies to `gm_id` on success or failure.
    pub(crate) fn do_spawn_item(&mut self, gm_id: u32, pos: Position, server_id: u16, count: u16) {
        let Some(meta) = self.meta.item_meta(server_id) else {
            self.push_status_message(gm_id, format!("Unknown item id {server_id}.").as_bytes());
            return;
        };
        let client_id = meta.client_id;
        let animated = meta.animated;
        let stackable = meta.stackable;

        if !self.materialize(pos) {
            self.push_status_message(gm_id, b"You cannot create an item there.");
            return;
        }
        // TFS 10-thing-per-tile cap.
        let len = self
            .dynamic
            .get(&(pos.x, pos.y, pos.z))
            .map(|st| st.items.len())
            .unwrap_or(0);
        if len >= 10 {
            self.push_status_message(gm_id, b"This tile is full.");
            return;
        }

        let subtype = if stackable {
            Some(count.clamp(1, 100) as u8)
        } else {
            None
        };
        let wi = WireItem {
            client_id,
            subtype,
            animated,
        };

        // creatures_on borrows &self immutably; compute before the &mut get_mut.
        let dest_creatures = self.creatures_on(pos).len();
        {
            let st = self.dynamic.get_mut(&(pos.x, pos.y, pos.z)).unwrap();
            let front = st.pre_creature_len; // first down-item slot
            st.items.insert(front, wi);
            st.server_ids.insert(front, server_id);
            st.counts.insert(front, subtype);
        }
        let front = self
            .dynamic
            .get(&(pos.x, pos.y, pos.z))
            .map(|st| st.pre_creature_len)
            .unwrap_or(0);
        let dest_s = (front + dest_creatures).min(9) as u8;
        self.broadcast_dest(pos, dest_s, wi, false);

        self.push_status_message(gm_id, format!("Created item {server_id}.").as_bytes());
    }

    /// `/ghost` — toggle ghost mode ON/OFF.
    ///
    /// ON: saves current outfit, switches looktype to [`GHOST_LOOKTYPE`],
    /// removes player from non-GM spectators. OFF: restores original outfit,
    /// re-introduces player to non-GM spectators.
    fn gm_ghost(&mut self, id: u32) {
        // Gate: gamemaster only.
        let is_gm = self.players.get(&id).map(|p| p.gamemaster).unwrap_or(false);
        if !is_gm {
            self.push_status_message(id, b"Only gamemasters can use /ghost.");
            return;
        }

        let is_ghost = self.players.get(&id).map(|p| p.ghost).unwrap_or(false);
        let pos = self.players.get(&id).map(|p| p.position).unwrap();

        if is_ghost {
            // --- Toggle OFF ---
            let restored = {
                let p = self.players.get_mut(&id).unwrap();
                p.ghost = false;
                let outfit = p.prev_outfit.take().unwrap_or(p.outfit);
                p.outfit = outfit;
                outfit
            };
            // Broadcast outfit change. Now that ghost=OFF, spectators() no longer
            // filters non-GMs, so all spectators see it.
            self.do_change_outfit(id, restored);
            // Collect non-GM player ids that can see `pos`.
            let targets: Vec<u32> = self
                .players
                .iter()
                .filter(|&(&sid, s)| sid != id && !s.gamemaster && Self::can_see(s.position, pos))
                .map(|(&sid, _)| sid)
                .collect();
            for spec in targets {
                if let Some(bytes) = self.introduce(spec, id) {
                    let sp = self.creature_stackpos_on(pos, id);
                    self.push(
                        spec,
                        tile_creature::add_tile_creature((pos.x, pos.y, pos.z), sp, &bytes),
                    );
                }
            }
            self.push_status_message(id, b"Ghost mode OFF.");
        } else {
            // --- Toggle ON ---
            let ghost_outfit = {
                let p = self.players.get_mut(&id).unwrap();
                let prev_outfit = p.outfit;
                let mut ghost_outfit = prev_outfit;
                ghost_outfit.look_type = GHOST_LOOKTYPE;
                p.ghost = true;
                p.prev_outfit = Some(prev_outfit);
                p.outfit = ghost_outfit;
                ghost_outfit
            };
            // Broadcast outfit change (still visible, including to non-GMs briefly).
            self.do_change_outfit(id, ghost_outfit);
            // Collect non-GM spectator ids (these are about to be removed).
            let targets: Vec<u32> = self
                .spectators(pos, id)
                .into_iter()
                .filter(|&sid| {
                    self.players
                        .get(&sid)
                        .map(|s| !s.gamemaster)
                        .unwrap_or(false)
                })
                .collect();
            for spec in targets {
                self.push(spec, walk::remove_creature_by_id(id));
                if let Some(s) = self.players.get_mut(&spec) {
                    s.known.remove(&id);
                }
            }
            self.push_status_message(id, b"Ghost mode ON.");
        }
    }

    /// `/noclip` — toggle noclip mode ON/OFF.
    ///
    /// Noclip only affects collision bypass (walls, creatures, items).
    /// No visibility, looktype, or walkthrough changes.
    fn gm_noclip(&mut self, id: u32) {
        let Some(p) = self.players.get(&id) else {
            return;
        };
        if !p.gamemaster {
            self.push_status_message(id, b"Only gamemasters can use /noclip.");
            return;
        }
        let on = {
            let p = self.players.get_mut(&id).unwrap();
            p.noclip = !p.noclip;
            p.noclip
        };
        if on {
            self.push_status_message(id, b"Noclip ON.");
        } else {
            self.push_status_message(id, b"Noclip OFF.");
        }
    }

    /// `/speed <value>` — set your own movement speed.
    /// `/speed "player" <value>` — set another player's speed.
    ///
    /// Valid range: 10–2500. The target receives a 0xA0 stats packet; spectators
    /// see a remove+re-introduce so the new speed takes effect client-side.
    fn gm_speed(&mut self, id: u32, args: &[&str]) {
        // Parse (optional name, value).
        let (target, value_str) = match args {
            [raw_value] => (id, raw_value),
            [name, raw_value] => {
                let Some(t) = self.find_player_by_name(name) else {
                    self.push_status_message(id, format!("Player '{name}' not found.").as_bytes());
                    return;
                };
                (t, raw_value)
            }
            _ => {
                self.push_status_message(id, b"Usage: /speed <value> | /speed \"player\" <value>");
                return;
            }
        };

        // Validate range.
        let Ok(value) = value_str.parse::<u16>() else {
            self.push_status_message(
                id,
                b"Invalid speed value. Must be a number between 10 and 2500.",
            );
            return;
        };
        if !(10..=2500).contains(&value) {
            self.push_status_message(id, b"Speed out of range. Valid range: 10-2500.");
            return;
        }

        // Set the speed.
        let pos = match self.players.get_mut(&target) {
            Some(p) => {
                p.speed = value;
                p.position
            }
            None => return,
        };

        // Push 0xA0 stats to the target.
        if let Some(p) = self.players.get(&target) {
            self.push(
                target,
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
                    base_speed: value,
                }),
            );
        }

        // Remove + re-introduce for spectators (so they see the new speed).
        for spec in self.spectators(pos, target) {
            self.push(spec, walk::remove_creature_by_id(target));
            if let Some(s) = self.players.get_mut(&spec) {
                s.known.remove(&target);
            }
            if let Some(bytes) = self.introduce(spec, target) {
                let sp = self.creature_stackpos_on(pos, target);
                self.push(
                    spec,
                    tile_creature::add_tile_creature((pos.x, pos.y, pos.z), sp, &bytes),
                );
            }
        }
        // Also remove + re-introduce for the target themselves. The client uses
        // the creature appearance speed even for the player's own movement, so
        // without this the walking speed never updates client-side.
        {
            self.push(target, walk::remove_creature_by_id(target));
            if let Some(s) = self.players.get_mut(&target) {
                s.known.remove(&target);
            }
            if let Some(bytes) = self.introduce(target, target) {
                let sp = self.creature_stackpos_on(pos, target);
                self.push(
                    target,
                    tile_creature::add_tile_creature((pos.x, pos.y, pos.z), sp, &bytes),
                );
            }
        }

        let who = if target == id {
            "Your".to_owned()
        } else {
            self.players
                .get(&target)
                .map(|p| format!("{}'s", p.name))
                .unwrap_or_default()
        };
        self.push_status_message(id, format!("{who} speed set to {value}.").as_bytes());
    }
}

#[cfg(test)]
mod tests {
    use super::super::test_support::*;
    use super::*;
    use std::sync::atomic::{AtomicU16, Ordering};

    fn gm_lua_test_dir(label: &str) -> std::path::PathBuf {
        static COUNTER: AtomicU16 = AtomicU16::new(0);
        let seq = COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("oxidia-gm-{label}-{seq}"));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn reload_lua_command_refreshes_lua_state() {
        // RED: /reload lua does not exist yet. The test expects version=2
        // after reload, but the unknown command leaves version at 1 → fails.
        let dir = gm_lua_test_dir("reload");
        std::fs::write(
            dir.join("test.lua"),
            b"version = 1\nfunction onUse(args) return true end",
        )
        .unwrap();
        let mut g = Game::from_static_map_arc(walk_map());
        g.lua = Some(LuaRuntime::new(&dir));
        let (player, mut rx) = add_player(&mut g, Position::new(100, 100, 7));
        g.players.get_mut(&player).unwrap().gamemaster = true;
        drain(&mut rx);

        // Confirm initial version.
        assert_eq!(
            g.lua.as_ref().unwrap().get_global_i64("version"),
            Some(1),
            "Lua state must have version=1 after initial load"
        );

        // Replace the script file on disk.
        std::fs::write(
            dir.join("test.lua"),
            b"version = 2\nfunction onUse(args) return true end",
        )
        .unwrap();

        // Execute /reload lua — the handler does not exist yet (RED).
        g.do_gm_command(player, "/reload lua".into());

        assert_eq!(
            g.lua.as_ref().unwrap().get_global_i64("version"),
            Some(2),
            "after /reload lua the Lua state must reflect the new script"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn tokenize_args_groups_quoted_segments() {
        assert_eq!(
            tokenize_args("crystal coin 100"),
            ["crystal", "coin", "100"]
        );
        assert_eq!(tokenize_args("\"Gold Coin\" 100"), ["Gold Coin", "100"]);
        assert_eq!(tokenize_args("   spaced   out   "), ["spaced", "out"]);
        assert_eq!(tokenize_args("\"unterminated"), ["unterminated"]); // quote to end of input
        assert!(tokenize_args("").is_empty());
    }

    #[test]
    fn parse_pos_reads_three_coords() {
        assert_eq!(
            parse_pos(&["100", "200", "7"]),
            Some(Position::new(100, 200, 7))
        );
        assert_eq!(parse_pos(&["100", "200"]), None); // too few
        assert_eq!(parse_pos(&["x", "200", "7"]), None); // non-numeric
        assert_eq!(parse_pos(&[]), None);
    }

    #[test]
    fn find_player_by_name_is_case_insensitive() {
        let mut g = Game::from_static_map_arc(stair_map());
        let (id, _rx) = add_player(&mut g, Position::new(100, 100, 7));
        g.players.get_mut(&id).unwrap().name = "God Diego".into();
        assert_eq!(g.find_player_by_name("god diego"), Some(id));
        assert_eq!(g.find_player_by_name("GOD DIEGO"), Some(id));
        assert_eq!(g.find_player_by_name("nobody"), None);
    }

    #[test]
    fn gmverb_registry_is_complete_and_resolvable() {
        // The enum is the single source of truth: every command must declare help
        // text and every word must resolve back to its variant.
        for &cmd in GmVerb::ALL {
            assert!(!cmd.usage().is_empty(), "a GmVerb is missing its usage");
            assert!(
                !cmd.description().is_empty(),
                "a GmVerb is missing its description"
            );
            assert!(!cmd.words().is_empty(), "a GmVerb is missing its words");
            for w in cmd.words() {
                assert!(
                    GmVerb::from_word(w).is_some(),
                    "from_word does not resolve '{w}'"
                );
            }
        }
        assert!(matches!(GmVerb::from_word("i"), Some(GmVerb::Item))); // alias
        assert!(matches!(GmVerb::from_word("item"), Some(GmVerb::Item)));
        assert!(GmVerb::from_word("nonsense").is_none());
        // ghost and noclip resolve from their words
        assert!(matches!(GmVerb::from_word("ghost"), Some(GmVerb::Ghost)));
        assert!(matches!(GmVerb::from_word("noclip"), Some(GmVerb::Noclip)));
    }

    // ===================================================================
    // Task 3.2: /ghost toggle ON/OFF sets/unsets ghost, swaps looktype to
    // 40, restores original outfit.
    // ===================================================================

    #[test]
    fn ghost_toggle_on_sets_flag_and_changes_looktype_to_40() {
        let mut g = Game::from_static_map_arc(walk_map());
        let (gm, _rx) = add_player(&mut g, Position::new(95, 117, 7));
        g.players.get_mut(&gm).unwrap().gamemaster = true;

        let orig_outfit = g.players[&gm].outfit;
        assert_ne!(
            orig_outfit.look_type, GHOST_LOOKTYPE,
            "precondition: orig looktype != 40"
        );

        g.do_gm_command(gm, "/ghost".into());

        let p = &g.players[&gm];
        assert!(p.ghost, "ghost flag must be true after /ghost");
        assert_eq!(
            p.outfit.look_type, GHOST_LOOKTYPE,
            "looktype must be GHOST_LOOKTYPE (40)"
        );
        assert_eq!(
            p.prev_outfit,
            Some(orig_outfit),
            "original outfit must be saved"
        );
    }

    #[test]
    fn ghost_toggle_off_restores_looktype_and_clears_flag() {
        let mut g = Game::from_static_map_arc(walk_map());
        let (gm, _rx) = add_player(&mut g, Position::new(95, 117, 7));
        g.players.get_mut(&gm).unwrap().gamemaster = true;
        let orig_outfit = g.players[&gm].outfit;

        // Toggle ON first
        g.do_gm_command(gm, "/ghost".into());
        assert!(g.players[&gm].ghost);
        assert_eq!(g.players[&gm].outfit.look_type, GHOST_LOOKTYPE);

        // Toggle OFF
        g.do_gm_command(gm, "/ghost".into());

        let p = &g.players[&gm];
        assert!(!p.ghost, "ghost flag must be false after second /ghost");
        assert_eq!(p.outfit, orig_outfit, "original outfit must be restored");
        assert_eq!(
            p.prev_outfit, None,
            "prev_outfit must be cleared after restore"
        );
    }

    #[test]
    fn ghost_toggle_non_gm_is_silently_dropped() {
        let mut g = Game::from_static_map_arc(walk_map());
        let (player, mut rx) = add_player(&mut g, Position::new(95, 117, 7));
        drain(&mut rx);

        g.do_gm_command(player, "/ghost".into());

        assert!(
            !g.players[&player].ghost,
            "non-GM ghost state must not change"
        );
        // Existing codebase behavior: non-GM commands are silently dropped
        // (matching all other GM commands, not just ghost/noclip).
        assert!(
            drain(&mut rx).is_empty(),
            "non-GM must NOT receive any message (silent drop)"
        );
    }

    // ===================================================================
    // Task 3.1: GmVerb::Speed variant is registered and resolvable
    // ===================================================================

    #[test]
    fn gmverb_registry_includes_speed_variant() {
        // Speed must be in ALL, have help text, and resolve from its words.
        let speed = GmVerb::Speed;
        assert!(GmVerb::ALL.contains(&speed), "Speed must be in GmVerb::ALL");
        assert!(!speed.usage().is_empty(), "Speed must have usage text");
        assert!(
            !speed.description().is_empty(),
            "Speed must have description"
        );
        assert!(!speed.words().is_empty(), "Speed must have words");
        for w in speed.words() {
            assert!(
                GmVerb::from_word(w).is_some(),
                "from_word must resolve '{w}' to Speed"
            );
        }
        // Canonical and alias
        assert!(matches!(GmVerb::from_word("speed"), Some(GmVerb::Speed)));
        assert!(matches!(GmVerb::from_word("spd"), Some(GmVerb::Speed)));
    }

    // ===================================================================
    // Task 3.2: /speed 500 self-target sets speed, pushes 0xA0
    // ===================================================================

    #[test]
    fn speed_self_target_sets_speed_and_pushes_stats() {
        let mut g = Game::from_static_map_arc(walk_map());
        let (gm, mut rx) = add_player(&mut g, Position::new(95, 117, 7));
        g.players.get_mut(&gm).unwrap().gamemaster = true;
        drain(&mut rx);

        g.do_gm_command(gm, "/speed 500".into());

        assert_eq!(g.players[&gm].speed, 500, "self-target speed must be 500");
        // Must receive: remove (0x6C), tile-creature add (0x6A), and stats (0xA0)
        let stats = drain(&mut rx);
        assert!(
            has_op(&stats, 0xA0),
            "must push 0xA0 stats packet after speed change"
        );
        assert!(
            has_op(&stats, 0x6C),
            "must push remove (0x6C) for self re-introduce"
        );
        assert!(
            has_op(&stats, protocol::tile_creature::OP_ADD_TILE_CREATURE),
            "must push add tile creature (0x6A) for self re-introduce"
        );
        let a0: Vec<&Vec<u8>> = stats.iter().filter(|p| p.first() == Some(&0xA0)).collect();
        assert!(!a0.is_empty(), "at least one 0xA0 packet");
        // Protocol encoding: op(1)+health(2)+max_health(2)+free_cap(4)+total_cap(4)+xp(8)
        // +level(2)+level_pct(1)+base_xp_rate(2)+xp_voucher(2)+low_lvl_bonus(2)+xp_boost(2)
        // +stamina_mult(2)+mana(2)+max_mana(2)+magic_lvl(1)+base_magic(1)+magic_pct(1)
        // +soul(1)+stamina(2)+base_speed(2) → base_speed at offset 44, stored as value/2
        let speed_wire = u16::from_le_bytes([a0[0][44], a0[0][45]]);
        assert_eq!(
            speed_wire, 250,
            "0xA0 base_speed must be 500/2 = 250 (value halved on wire)"
        );
    }

    // ===================================================================
    // Task 3.3: named-target sets target speed + invalid name error
    // ===================================================================

    #[test]
    fn speed_named_target_sets_target_speed() {
        let mut g = Game::from_static_map_arc(walk_map());
        let (gm, mut rx) = add_player(&mut g, Position::new(95, 117, 7));
        g.players.get_mut(&gm).unwrap().gamemaster = true;
        let (target, _rt) = add_player(&mut g, Position::new(96, 117, 7));
        g.players.get_mut(&target).unwrap().name = "Alice".into();
        drain(&mut rx);

        g.do_gm_command(gm, r#"/speed "Alice" 750"#.into());

        assert_eq!(
            g.players[&target].speed, 750,
            "named target speed must be 750"
        );
        // GM must get a status message
        let msgs = drain(&mut rx);
        assert!(
            has_op(&msgs, 0xB4),
            "GM must receive status message after named speed change"
        );
    }

    #[test]
    fn speed_invalid_name_returns_error() {
        let mut g = Game::from_static_map_arc(walk_map());
        let (gm, mut rx) = add_player(&mut g, Position::new(95, 117, 7));
        g.players.get_mut(&gm).unwrap().gamemaster = true;
        drain(&mut rx);

        g.do_gm_command(gm, r#"/speed "Nobody" 500"#.into());

        // Must receive a 0xB4 error message and NO player speed was modified
        let msgs = drain(&mut rx);
        assert!(
            has_op(&msgs, 0xB4),
            "must receive error for invalid player name"
        );
        let b4s: Vec<&Vec<u8>> = msgs.iter().filter(|p| p.first() == Some(&0xB4)).collect();
        let first_msg = String::from_utf8_lossy(&b4s[0][3..]); // skip op(1) + mode(1) + len(2)
        assert!(
            first_msg.to_lowercase().contains("not found"),
            "error must mention player not found; got: {first_msg}"
        );
    }

    // ===================================================================
    // Task 3.4: range validation
    // ===================================================================

    #[test]
    fn speed_range_below_minimum_is_rejected() {
        let mut g = Game::from_static_map_arc(walk_map());
        let (gm, mut rx) = add_player(&mut g, Position::new(95, 117, 7));
        g.players.get_mut(&gm).unwrap().gamemaster = true;
        drain(&mut rx);

        g.do_gm_command(gm, "/speed 9".into());

        assert_eq!(
            g.players[&gm].speed, 220,
            "speed must remain at default 220"
        );
        let msgs = drain(&mut rx);
        assert!(
            has_op(&msgs, 0xB4),
            "must receive error for value below minimum"
        );
    }

    #[test]
    fn speed_range_above_maximum_is_rejected() {
        let mut g = Game::from_static_map_arc(walk_map());
        let (gm, mut rx) = add_player(&mut g, Position::new(95, 117, 7));
        g.players.get_mut(&gm).unwrap().gamemaster = true;
        drain(&mut rx);

        g.do_gm_command(gm, "/speed 2501".into());

        assert_eq!(
            g.players[&gm].speed, 220,
            "speed must remain at default 220"
        );
        let msgs = drain(&mut rx);
        assert!(
            has_op(&msgs, 0xB4),
            "must receive error for value above maximum"
        );
    }

    #[test]
    fn speed_range_minimum_accepted() {
        let mut g = Game::from_static_map_arc(walk_map());
        let (gm, _rx) = add_player(&mut g, Position::new(95, 117, 7));
        g.players.get_mut(&gm).unwrap().gamemaster = true;

        g.do_gm_command(gm, "/speed 10".into());

        assert_eq!(
            g.players[&gm].speed, 10,
            "minimum speed 10 must be accepted"
        );
    }

    #[test]
    fn speed_range_maximum_accepted() {
        let mut g = Game::from_static_map_arc(walk_map());
        let (gm, _rx) = add_player(&mut g, Position::new(95, 117, 7));
        g.players.get_mut(&gm).unwrap().gamemaster = true;

        g.do_gm_command(gm, "/speed 2500".into());

        assert_eq!(
            g.players[&gm].speed, 2500,
            "maximum speed 2500 must be accepted"
        );
    }

    #[test]
    fn speed_non_numeric_value_is_rejected() {
        let mut g = Game::from_static_map_arc(walk_map());
        let (gm, mut rx) = add_player(&mut g, Position::new(95, 117, 7));
        g.players.get_mut(&gm).unwrap().gamemaster = true;
        drain(&mut rx);

        g.do_gm_command(gm, "/speed fast".into());

        assert_eq!(
            g.players[&gm].speed, 220,
            "speed must remain at default 220 after non-numeric input"
        );
        let msgs = drain(&mut rx);
        assert!(
            has_op(&msgs, 0xB4),
            "must receive error for non-numeric value"
        );
    }

    // ===================================================================
    // Task 3.5: spectator receives remove+re-introduce after speed change
    // ===================================================================

    #[test]
    fn speed_change_broadcasts_remove_and_reintroduce_to_spectators() {
        let mut g = Game::from_static_map_arc(walk_map());
        let (gm, mut rx) = add_player(&mut g, Position::new(95, 117, 7));
        g.players.get_mut(&gm).unwrap().gamemaster = true;
        let (target, _rt) = add_player(&mut g, Position::new(96, 117, 7));
        g.players.get_mut(&target).unwrap().name = "Alice".into();
        let (_spec, mut rs) = add_player(&mut g, Position::new(95, 116, 7)); // spectator

        // Drain initial login packets.
        drain(&mut rx);

        // Change target's speed via named-target command.
        g.do_gm_command(gm, r#"/speed "Alice" 999"#.into());

        // Spectator must receive: remove (0x6C) then add-tile-creature (0x6A)
        let packets = drain(&mut rs);
        let has_remove = packets.iter().any(|p| p.first() == Some(&0x6C));
        let has_add = packets
            .iter()
            .any(|p| p.first() == Some(&protocol::tile_creature::OP_ADD_TILE_CREATURE));
        assert!(
            has_remove,
            "spectator must see remove (0x6C) after speed change"
        );
        assert!(has_add, "spectator must see add (0x6A) after speed change");
        // Target speed updated in state
        assert_eq!(g.players[&target].speed, 999, "target speed must be 999");
    }

    #[test]
    fn speed_no_packets_to_out_of_range_spectators() {
        let mut g = Game::from_static_map_arc(walk_map());
        let (gm, mut rx) = add_player(&mut g, Position::new(95, 117, 7));
        g.players.get_mut(&gm).unwrap().gamemaster = true;
        // Target is far away from spectator (out of view range).
        let (target, _rt) = add_player(&mut g, Position::new(96, 117, 7));
        g.players.get_mut(&target).unwrap().name = "Bob".into();
        let (_far, mut far_rx) = add_player(&mut g, Position::new(150, 150, 7));

        drain(&mut rx);
        drain(&mut far_rx);

        g.do_gm_command(gm, r#"/speed "Bob" 500"#.into());

        // Out-of-range spectator must receive NO packets (no remove, no add).
        let packets = drain(&mut far_rx);
        assert!(
            packets.is_empty(),
            "out-of-range spectator must NOT receive any packets; got {}: {:?}",
            packets.len(),
            packets
                .iter()
                .map(|p| p.first().unwrap_or(&0))
                .collect::<Vec<_>>()
        );
        // Target speed is still updated
        assert_eq!(g.players[&target].speed, 500, "target speed must be 500");
    }

    // ===================================================================
    // Task 3.7: /noclip toggle sets/unsets noclip; outfit unchanged;
    // noclip GM visible to all.
    // ===================================================================

    #[test]
    fn noclip_toggle_on_sets_flag_outfit_unchanged() {
        let mut g = Game::from_static_map_arc(walk_map());
        let (gm, _rx) = add_player(&mut g, Position::new(95, 117, 7));
        g.players.get_mut(&gm).unwrap().gamemaster = true;
        let orig_outfit = g.players[&gm].outfit;

        g.do_gm_command(gm, "/noclip".into());

        let p = &g.players[&gm];
        assert!(p.noclip, "noclip flag must be true after /noclip");
        assert_eq!(p.outfit, orig_outfit, "noclip must NOT change outfit");
        assert!(!p.ghost, "noclip must NOT set ghost flag");
    }

    #[test]
    fn noclip_toggle_off_clears_flag() {
        let mut g = Game::from_static_map_arc(walk_map());
        let (gm, _rx) = add_player(&mut g, Position::new(95, 117, 7));
        g.players.get_mut(&gm).unwrap().gamemaster = true;

        // Toggle ON
        g.do_gm_command(gm, "/noclip".into());
        assert!(g.players[&gm].noclip);

        // Toggle OFF
        g.do_gm_command(gm, "/noclip".into());
        assert!(
            !g.players[&gm].noclip,
            "noclip must be false after second /noclip"
        );
    }

    #[test]
    fn noclip_toggle_non_gm_is_silently_dropped() {
        let mut g = Game::from_static_map_arc(walk_map());
        let (player, mut rx) = add_player(&mut g, Position::new(95, 117, 7));
        drain(&mut rx);

        g.do_gm_command(player, "/noclip".into());

        assert!(
            !g.players[&player].noclip,
            "non-GM noclip state must not change"
        );
        assert!(
            drain(&mut rx).is_empty(),
            "non-GM must NOT receive any message (silent drop)"
        );
    }

    // ===================================================================
    // Task 3.3: ghost GM excluded from non-GM spectators() + visible_from()
    // but visible to GM viewers.
    // ===================================================================

    #[test]
    fn non_gm_spectator_does_not_see_ghost_gm() {
        let mut g = Game::from_static_map_arc(walk_map());
        let (gm, _rg) = add_player(&mut g, Position::new(95, 117, 7));
        g.players.get_mut(&gm).unwrap().gamemaster = true;
        // Non-GM player one tile east — both can see pos (95,117).
        let (non_gm, _rn) = add_player(&mut g, Position::new(96, 117, 7));

        // Activate ghost.
        g.do_gm_command(gm, "/ghost".into());

        // spectators(95,117, gm) must NOT include non_gm.
        let specs = g.spectators(Position::new(95, 117, 7), gm);
        assert!(
            !specs.contains(&non_gm),
            "non-GM must NOT be in spectators of ghost GM's pos"
        );
    }

    #[test]
    fn gm_spectator_sees_ghost_gm() {
        let mut g = Game::from_static_map_arc(walk_map());
        let (gm, _rg) = add_player(&mut g, Position::new(95, 117, 7));
        g.players.get_mut(&gm).unwrap().gamemaster = true;
        // GM viewer one tile east — both GMs, should see ghost.
        let (gm2, _r2) = add_player(&mut g, Position::new(96, 117, 7));
        g.players.get_mut(&gm2).unwrap().gamemaster = true;

        g.do_gm_command(gm, "/ghost".into());

        let specs = g.spectators(Position::new(95, 117, 7), gm);
        assert!(
            specs.contains(&gm2),
            "GM viewer must see ghost GM in spectators"
        );
    }

    #[test]
    fn non_gm_visible_from_excludes_ghost_gm() {
        let mut g = Game::from_static_map_arc(walk_map());
        let (gm, _rg) = add_player(&mut g, Position::new(96, 117, 7));
        g.players.get_mut(&gm).unwrap().gamemaster = true;
        // Non-GM at 95,117 looks north? Actually they share same floor.
        // Both at z=7 in range: 96,117 is visible from 95,117.
        let (non_gm, _rn) = add_player(&mut g, Position::new(95, 117, 7));

        g.do_gm_command(gm, "/ghost".into());

        let visible = g.visible_from(Position::new(95, 117, 7), non_gm);
        assert!(
            !visible.contains(&gm),
            "non-GM's visible_from must exclude ghost GM"
        );
    }

    #[test]
    fn gm_visible_from_includes_ghost_gm() {
        let mut g = Game::from_static_map_arc(walk_map());
        let (gm, _rg) = add_player(&mut g, Position::new(96, 117, 7));
        g.players.get_mut(&gm).unwrap().gamemaster = true;
        // GM viewer at 95,117.
        let (gm2, _r2) = add_player(&mut g, Position::new(95, 117, 7));
        g.players.get_mut(&gm2).unwrap().gamemaster = true;

        g.do_gm_command(gm, "/ghost".into());

        let visible = g.visible_from(Position::new(95, 117, 7), gm2);
        assert!(
            visible.contains(&gm),
            "GM's visible_from must include ghost GM"
        );
    }
}
