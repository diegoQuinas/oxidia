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
    ReloadLua,
    Ghost,
    Noclip,
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
        Self::Ghost,
        Self::Noclip,
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
            Self::Ghost => &["ghost"],
            Self::Noclip => &["noclip"],
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
            Self::Ghost => "/ghost",
            Self::Noclip => "/noclip",
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
            Self::Ghost => "Toggle ghost mode: invisible to non-GMs, bypasses collision, ghost looktype.",
            Self::Noclip => "Toggle noclip mode: bypasses collision, visible to everyone.",
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
            GmVerb::ReloadLua => self.gm_reload_lua(id),
            GmVerb::Ghost => self.gm_ghost(id),
            GmVerb::Noclip => self.gm_noclip(id),
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

    /// `/reload lua` — drop and re-initialise the Lua runtime so script changes
    /// on disk take effect without restarting the server. Reports success/failure
    /// as a `0xB4` status message to the requesting GM.
    fn gm_reload_lua(&mut self, id: u32) {
        self.do_reload_lua();
        self.push_status_message(id, b"Lua scripts reloaded from disk.");
    }

    /// Place a fresh item on `pos` and broadcast a `0x6A` add to spectators.
    /// Mirrors the destination half of `do_move_thing`: materialize the tile,
    /// insert at the front of the down-items (newest on top), broadcast at the
    /// top down-item stackpos. Replies to `gm_id` on success or failure.
    pub(crate) fn do_spawn_item(&mut self, gm_id: u32, pos: Position, server_id: u16, count: u16) {
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

    /// `/ghost` — toggle ghost mode.
    ///
    /// When ghost mode is ON:
    /// - GM is invisible to non-GM players (filtered from spectators)
    /// - GM bypasses all collision (same as noclip)
    /// - GM's looktype changes to ghost sprite
    /// - GM's creature packets carry walkthrough=1
    ///
    /// When toggling OFF: restores looktype, re-introduces to non-GM spectators.
    fn gm_ghost(&mut self, id: u32) {
        let was_ghost = self.players.get(&id).map(|p| p.ghost).unwrap_or(false);
        if was_ghost {
            // Restore previous outfit before re-introducing.
            let restored = match self.players.get_mut(&id) {
                Some(p) => {
                    p.ghost = false;
                    p.prev_outfit.take().unwrap_or(p.outfit)
                }
                None => return,
            };
            self.do_change_outfit(id, restored);
        } else {
            // Save current outfit and apply ghost looktype.
            let ghost_outfit = match self.players.get_mut(&id) {
                Some(p) => {
                    p.ghost = true;
                    p.prev_outfit = Some(p.outfit);
                    Outfit { look_type: GHOST_LOOKTYPE, ..p.outfit }
                }
                None => return,
            };
            self.do_change_outfit(id, ghost_outfit);
        }
        // Broadcast the state change to ALL spectators (they will be filtered
        // naturally by the ghost-aware spectators()/visible_from()).
        // TODO: when toggling ghost ON, remove GM from non-GM known-sets.
        // TODO: when toggling OFF, re-introduce GM to non-GM spectators.
        let msg = if was_ghost { "Ghost mode OFF." } else { "Ghost mode ON." };
        self.push_status_message(id, msg.as_bytes());
    }

    /// `/noclip` — toggle noclip mode.
    /// When ON: bypasses collision, visible to everyone, no looktype change.
    fn gm_noclip(&mut self, id: u32) {
        let noclip = match self.players.get_mut(&id) {
            Some(p) => { p.noclip = !p.noclip; p.noclip }
            None => return,
        };
        let msg = if noclip { "Noclip mode ON." } else { "Noclip mode OFF." };
        self.push_status_message(id, msg.as_bytes());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use super::super::test_support::*;
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
        let mut g = Game::new(walk_map());
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

    #[test]
    fn ghost_command_toggles_ghost_flag_and_looktype() {
        let mut g = Game::new(walk_map());
        let (id, _rx) = add_player(&mut g, Position::new(100, 100, 7));
        g.players.get_mut(&id).unwrap().gamemaster = true;

        // Initial state: ghost = false, normal outfit.
        assert!(!g.players.get(&id).unwrap().ghost);
        let orig_outfit = g.players.get(&id).unwrap().outfit;

        // Toggle ON
        g.do_gm_command(id, "/ghost".into());
        assert!(g.players.get(&id).unwrap().ghost);
        assert_eq!(g.players.get(&id).unwrap().outfit.look_type, GHOST_LOOKTYPE);
        assert_eq!(g.players.get(&id).unwrap().prev_outfit, Some(orig_outfit));

        // Toggle OFF
        g.do_gm_command(id, "/ghost".into());
        assert!(!g.players.get(&id).unwrap().ghost);
        assert_eq!(g.players.get(&id).unwrap().outfit, orig_outfit);
        assert_eq!(g.players.get(&id).unwrap().prev_outfit, None);
    }

    #[test]
    fn ghost_command_rejected_for_non_gm() {
        let mut g = Game::new(walk_map());
        let (id, mut rx) = add_player(&mut g, Position::new(100, 100, 7));
        drain(&mut rx);

        g.do_gm_command(id, "/ghost".into());
        // Non-GM commands are silently dropped.
        assert!(!g.players.get(&id).unwrap().ghost);
    }

    #[test]
    fn noclip_command_toggles_noclip_flag() {
        let mut g = Game::new(walk_map());
        let (id, _rx) = add_player(&mut g, Position::new(100, 100, 7));
        g.players.get_mut(&id).unwrap().gamemaster = true;

        assert!(!g.players.get(&id).unwrap().noclip);
        g.do_gm_command(id, "/noclip".into());
        assert!(g.players.get(&id).unwrap().noclip);
        g.do_gm_command(id, "/noclip".into());
        assert!(!g.players.get(&id).unwrap().noclip);
    }

    #[test]
    fn noclip_does_not_change_looktype_or_visibility() {
        let mut g = Game::new(walk_map());
        let (id, _rx) = add_player(&mut g, Position::new(100, 100, 7));
        g.players.get_mut(&id).unwrap().gamemaster = true;
        let orig_outfit = g.players.get(&id).unwrap().outfit;

        g.do_gm_command(id, "/noclip".into());
        assert_eq!(g.players.get(&id).unwrap().outfit, orig_outfit);
        assert!(!g.players.get(&id).unwrap().ghost); // noclip does not affect ghost
    }

    #[test]
    fn noclip_via_command_bypasses_blocked_tile() {
        let mut g = Game::new(walk_map());
        let spawn = Position::new(95, 117, 7);
        let wall = Position::new(94, 117, 7); // blocked tile
        let (id, mut rx) = add_player(&mut g, spawn);
        g.players.get_mut(&id).unwrap().gamemaster = true;
        drain(&mut rx);

        // Send /noclip command → toggles noclip = true
        g.do_gm_command(id, "/noclip".into());
        assert!(g.players.get(&id).unwrap().noclip);

        // Now walk through the blocked tile
        g.do_move(id, Direction::West);
        assert_eq!(g.players.get(&id).unwrap().position, wall,
            "/noclip mode must bypass blocked tiles");
    }
}
