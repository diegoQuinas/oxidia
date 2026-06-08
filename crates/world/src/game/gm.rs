//! Gamemaster commands for the game actor.

use super::*;

/// Every gamemaster command. This enum is the single source of truth: the
/// exhaustive `usage`/`description` matches force any new variant to declare its
/// help text (it won't compile otherwise), and the dispatch `match` in
/// `do_gm_command` forces it to be wired. `/help` iterates [`GmVerb::ALL`], so it
/// can never drift out of sync — the only manual step when adding a command is to
/// list its variant in `ALL`.
#[derive(Clone, Copy)]
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
        }
    }

    /// Resolve a command word (verb or alias) to its variant.
    fn from_word(word: &str) -> Option<GmVerb> {
        GmVerb::ALL.iter().copied().find(|v| v.words().contains(&word))
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
        let Some((verb, rest)) = tokens.split_first() else { return };
        let args: Vec<&str> = rest.iter().map(|s| s.as_str()).collect();
        let Some(cmd) = GmVerb::from_word(verb) else {
            self.push_status_message(id, format!("Unknown command: /{verb}. Type /help.").as_bytes());
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
        self.players.iter()
            .find(|(_, p)| p.name.eq_ignore_ascii_case(name))
            .map(|(&id, _)| id)
    }

    /// `/goto <x> <y> <z>` — teleport the GM to coordinates.
    /// `/goto <player>` — teleport the GM to that online player's tile.
    fn gm_goto(&mut self, id: u32, args: &[&str]) {
        // Coordinate form: three numeric args.
        if let Some(pos) = parse_pos(args) {
            if !self.map.has_ground(pos) {
                self.push_status_message(id, b"There is no tile there.");
                return;
            }
            self.do_teleport(id, pos);
            self.push_status_message(id, format!("Teleported to {}, {}, {}.", pos.x, pos.y, pos.z).as_bytes());
            return;
        }
        // Player form: a single name argument (quote multi-word names).
        if let [name] = args {
            let Some(target) = self.find_player_by_name(name) else {
                self.push_status_message(id, format!("Player '{name}' not found.").as_bytes());
                return;
            };
            let Some(pos) = self.players.get(&target).map(|p| p.position) else { return };
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
            None => self.map.spawn(),
            Some(arg) => match arg.parse::<u32>() {
                Ok(town_id) => match self.map.town_temple_by_id(town_id) {
                    Some(p) => p,
                    None => {
                        self.push_status_message(id, format!("No town with id {town_id}.").as_bytes());
                        return;
                    }
                },
                Err(_) => match self.map.town_temple_by_name(arg) {
                    Some(p) => p,
                    None => {
                        self.push_status_message(id, format!("No town named '{arg}'.").as_bytes());
                        return;
                    }
                },
            },
        };
        self.do_teleport(id, temple);
        self.push_status_message(id, format!("Teleported to temple ({}, {}, {}).", temple.x, temple.y, temple.z).as_bytes());
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
            match self.map.find_item_id_by_name(&name) {
                Some(sid) => (sid, count),
                None => {
                    self.push_status_message(id, format!("No item named '{name}'.").as_bytes());
                    return;
                }
            }
        };
        let Some(pos) = self.players.get(&id).map(|p| p.position) else { return };
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
        if !self.map.has_ground(pos) {
            self.push_status_message(id, b"There is no tile there.");
            return;
        }
        self.do_teleport(target, pos);
        self.push_status_message(id, format!("Teleported {} to {}, {}, {}.", name, pos.x, pos.y, pos.z).as_bytes());
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
        let Some(pos) = self.players.get(&target).map(|p| p.position) else { return };
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
        let Some(pos) = self.players.get(&id).map(|p| p.position) else { return };
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
                    self.push_status_message(id, b"Usage: /setlooktype <id> | /setlooktype \"player\" <id>");
                    return;
                };
                (id, lt)
            }
            [name, raw_id] => {
                let Ok(lt) = raw_id.parse::<u16>() else {
                    self.push_status_message(id, b"Usage: /setlooktype <id> | /setlooktype \"player\" <id>");
                    return;
                };
                let Some(t) = self.find_player_by_name(name) else {
                    self.push_status_message(id, format!("Player '{name}' not found.").as_bytes());
                    return;
                };
                (t, lt)
            }
            _ => {
                self.push_status_message(id, b"Usage: /setlooktype <id> | /setlooktype \"player\" <id>");
                return;
            }
        };
        let new_outfit = match self.players.get_mut(&target) {
            Some(p) => { p.outfit.look_type = look_type; p.outfit }
            None => return,
        };
        self.do_change_outfit(target, new_outfit);
        self.push_status_message(id, format!("Look type set to {look_type}.").as_bytes());
    }

    /// Place a fresh item on `pos` and broadcast a `0x6A` add to spectators.
    /// Mirrors the destination half of `do_move_thing`: materialize the tile,
    /// insert at the front of the down-items (newest on top), broadcast at the
    /// top down-item stackpos. Replies to `gm_id` on success or failure.
    fn do_spawn_item(&mut self, gm_id: u32, pos: Position, server_id: u16, count: u16) {
        let Some(meta) = self.map.item_meta(server_id) else {
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
        let len = self.dynamic.get(&(pos.x, pos.y, pos.z)).map(|st| st.items.len()).unwrap_or(0);
        if len >= 10 {
            self.push_status_message(gm_id, b"This tile is full.");
            return;
        }

        let subtype = if stackable { Some(count.clamp(1, 100) as u8) } else { None };
        let wi = WireItem { client_id, subtype, animated };

        // creatures_on borrows &self immutably; compute before the &mut get_mut.
        let dest_creatures = self.creatures_on(pos).len();
        {
            let st = self.dynamic.get_mut(&(pos.x, pos.y, pos.z)).unwrap();
            let front = st.pre_creature_len; // first down-item slot
            st.items.insert(front, wi);
            st.server_ids.insert(front, server_id);
            st.counts.insert(front, subtype);
        }
        let front = self.dynamic.get(&(pos.x, pos.y, pos.z)).map(|st| st.pre_creature_len).unwrap_or(0);
        let dest_s = (front + dest_creatures).min(9) as u8;
        self.broadcast_dest(pos, dest_s, wi, false);

        self.push_status_message(gm_id, format!("Created item {server_id}.").as_bytes());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use super::super::test_support::*;

    #[test]
    fn tokenize_args_groups_quoted_segments() {
        assert_eq!(tokenize_args("crystal coin 100"), ["crystal", "coin", "100"]);
        assert_eq!(tokenize_args("\"Gold Coin\" 100"), ["Gold Coin", "100"]);
        assert_eq!(tokenize_args("   spaced   out   "), ["spaced", "out"]);
        assert_eq!(tokenize_args("\"unterminated"), ["unterminated"]); // quote to end of input
        assert!(tokenize_args("").is_empty());
    }

    #[test]
    fn parse_pos_reads_three_coords() {
        assert_eq!(parse_pos(&["100", "200", "7"]), Some(Position::new(100, 200, 7)));
        assert_eq!(parse_pos(&["100", "200"]), None); // too few
        assert_eq!(parse_pos(&["x", "200", "7"]), None); // non-numeric
        assert_eq!(parse_pos(&[]), None);
    }

    #[test]
    fn find_player_by_name_is_case_insensitive() {
        let mut g = Game::new(stair_map());
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
            assert!(!cmd.description().is_empty(), "a GmVerb is missing its description");
            assert!(!cmd.words().is_empty(), "a GmVerb is missing its words");
            for w in cmd.words() {
                assert!(GmVerb::from_word(w).is_some(), "from_word does not resolve '{w}'");
            }
        }
        assert!(matches!(GmVerb::from_word("i"), Some(GmVerb::Item))); // alias
        assert!(matches!(GmVerb::from_word("item"), Some(GmVerb::Item)));
        assert!(GmVerb::from_word("nonsense").is_none());
    }
}
