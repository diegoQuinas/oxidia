//! Container engine for the game actor.

use super::*;
use super::lua::LuaArgs;

fn matches_source(a: ContainerSource, b: ContainerSource) -> bool {
    match (a, b) {
        (ContainerSource::Slot(x), ContainerSource::Slot(y)) => x == y,
        (ContainerSource::Nested { parent_cid: pa, parent_slot: ps },
         ContainerSource::Nested { parent_cid: qa, parent_slot: qs }) => pa == qa && ps == qs,
        (ContainerSource::Ground(p), ContainerSource::Ground(q)) => p == q,
        _ => false,
    }
}

/// TFS `Position::areInRange<1,1,0>`: within one tile on x and y, same floor.
fn in_close_range(a: Position, b: Position) -> bool {
    a.z == b.z
        && (i32::from(a.x) - i32::from(b.x)).abs() <= 1
        && (i32::from(a.y) - i32::from(b.y)).abs() <= 1
}

impl Game {
    /// Restore container state from persisted items. Builds closed `OpenContainer`
    /// entries (not shown to the client on login — the player must re-open them).
    /// Supports one level of nesting: items directly inside an inventory-slot bag.
    /// Nested-bag contents are not restored (shown empty until moved this session).
    pub(super) fn restore_containers(
        rows: &[(u8, String, u16, u8)],
        inventory: &[Option<InvItem>; 10],
        map: &StaticMap,
    ) -> [Option<OpenContainer>; 16] {
        // Group rows by inv_slot, sorting by the numeric path so items are in order.
        let mut by_slot: std::collections::HashMap<u8, Vec<(usize, u16, u8)>> = Default::default();
        for (inv_slot, path, sid, cnt) in rows {
            let idx = path.parse::<usize>().unwrap_or(0);
            by_slot.entry(*inv_slot).or_default().push((idx, *sid, *cnt));
        }
        for v in by_slot.values_mut() {
            v.sort_by_key(|&(idx, _, _)| idx);
        }

        let mut result: [Option<OpenContainer>; 16] = Default::default();
        let mut cid = 0u8;
        for (slot_0, inv_item) in inventory.iter().enumerate() {
            let inv_slot = (slot_0 + 1) as u8;
            let Some(it) = inv_item else { continue };
            let Some(meta) = map.item_meta(it.server_id) else { continue };
            if !meta.is_container { continue; }
            let items_for_slot = by_slot.remove(&inv_slot).unwrap_or_default();
            let items: Vec<ContainerItem> = items_for_slot.into_iter().filter_map(|(_, sid, cnt)| {
                let m = map.item_meta(sid)?;
                Some(ContainerItem {
                    server_id: sid,
                    client_id: m.client_id,
                    count: if m.stackable { Some(cnt) } else { None },
                    animated: m.animated,
                })
            }).collect();
            if cid < 16 {
                result[cid as usize] = Some(OpenContainer {
                    server_id: it.server_id,
                    client_id: meta.client_id,
                    capacity: meta.container_capacity.max(1),
                    name: meta.name.clone(),
                    items,
                    source: ContainerSource::Slot(inv_slot),
                    is_open: false,
                });
                cid += 1;
            }
        }
        result
    }

    /// Export container contents for persistence. Walks ALL cid slots (both open
    /// and closed windows) and produces `(inv_slot, path, server_id, count)` rows.
    /// Only containers rooted in an inventory slot are exported (ground containers
    /// are transient). Nested containers are exported as items of their parent; their
    /// own contents are exported under the parent slot with a "/child_idx" path suffix.
    pub(super) fn export_container_items(
        _inventory: &[Option<InvItem>; 10],
        open_containers: &[Option<OpenContainer>; 16],
    ) -> Vec<(u8, String, u16, u8)> {
        let mut rows = Vec::new();
        for oc in open_containers.iter().flatten() {
            let inv_slot = match oc.source {
                ContainerSource::Slot(s) if s >= 1 => s,
                _ => continue, // ground or nested — skipped (nested exported via parent)
            };
            for (idx, item) in oc.items.iter().enumerate() {
                rows.push((inv_slot, idx.to_string(), item.server_id, item.count.unwrap_or(1)));
            }
        }
        rows
    }

    /// Find the first unallocated cid slot (None) for a brand-new container.
    /// When all 16 slots are allocated, picks the first slot whose window is
    /// not currently open (so its contents can be silently evicted). Returns
    /// `None` only if all 16 slots are open simultaneously (pathological case).
    fn next_free_cid(p: &PlayerState) -> Option<u8> {
        // Prefer a completely unallocated slot.
        if let Some(cid) = (0u8..16).find(|&c| p.open_containers[c as usize].is_none()) {
            return Some(cid);
        }
        // Fall back to the first closed (but allocated) slot — its contents will
        // be silently replaced by the new container.
        (0u8..16).find(|&c| p.open_containers[c as usize].as_ref().map(|o| !o.is_open).unwrap_or(false))
    }

    /// Push an `0x6E` open-container packet to `id` for the given cid.
    fn push_open_container(&mut self, id: u32, cid: u8) {
        let Some(p) = self.players.get(&id) else { return };
        let Some(oc) = p.open_containers[cid as usize].as_ref() else { return };
        let has_parent = matches!(oc.source, ContainerSource::Nested { .. });
        let server_id = oc.server_id;
        let client_id = oc.client_id;
        let wire_items: Vec<protocol::container::ContainerWireItem> =
            oc.items.iter().map(|i| i.wire()).collect();
        let animated = self.map.item_meta(server_id).map(|m| m.animated).unwrap_or(false);
        let bag = WireItem { client_id, subtype: None, animated };
        let pkt = protocol::container::open_container(
            cid, &bag, &oc.name, oc.capacity, has_parent, &wire_items,
        );
        self.push(id, pkt);
    }

    // -----------------------------------------------------------------------
    // Container commands
    // -----------------------------------------------------------------------

    /// Handle `0x82` use-item: if the item is a container, open it as a window.
    /// Other use cases (levers, runes, potions) are M11/Lua — silently ignored here.
    pub(super) fn do_use_item(&mut self, id: u32, pos_x: u16, pos_y: u16, pos_z: u8, stackpos: u8, index: u8) {
        let Some(p) = self.players.get(&id) else { return };

        // Resolve where the item is and its server_id.
        let (server_id, source) = if pos_x == 0xFFFF {
            // Inventory or container endpoint.
            if pos_y & 0x40 != 0 {
                // Container endpoint: (cid, slot_index).
                let cid = (pos_y & 0x0F) as u8;
                let slot_idx = pos_z as usize;
                let Some(oc) = p.open_containers[cid as usize].as_ref() else { return };
                let Some(item) = oc.items.get(slot_idx) else { return };
                let sid = item.server_id;
                (sid, ContainerSource::Nested { parent_cid: cid, parent_slot: slot_idx as u8 })
            } else {
                // Inventory slot.
                let slot = pos_y as u8;
                if !(1..=10).contains(&slot) { return; }
                let Some(it) = p.inventory[(slot - 1) as usize] else { return };
                (it.server_id, ContainerSource::Slot(slot))
            }
        } else {
            // Ground item.
            let pos = Position::new(pos_x, pos_y, pos_z);
            let player_pos = p.position;
            let near = (i32::from(player_pos.x) - i32::from(pos.x)).abs() <= 1
                && (i32::from(player_pos.y) - i32::from(pos.y)).abs() <= 1
                && player_pos.z == pos.z;
            if !near { return; }

            let pre = self.dynamic.get(&(pos_x, pos_y, pos_z))
                .map(|st| st.pre_creature_len)
                .unwrap_or_else(|| self.map.tile_pre_creature_len(pos));
            let creatures_len = self.creatures_on(pos).len();
            let sp = stackpos as usize;
            let src_idx = if sp < pre { sp }
                else if sp < pre + creatures_len { return; }
                else { sp - creatures_len };
            let Some(sid) = self.merged_server_id(pos, src_idx) else { return };
            (sid, ContainerSource::Ground(pos)) // ground container, tracked by tile; not persisted
        };

        let Some(meta) = self.map.item_meta(server_id) else { return };
        if !meta.is_container {
            // Non-container item: check the XML registry for a Lua script
            // binding and dispatch the onUse hook if one exists. This is the
            // extension point for script-driven item behavior (ladders,
            // levers, runes, etc.). Container items proceed to the existing
            // open-container path below.
            if let Some(ref lua) = self.lua {
                if self.registry.lookup(server_id).is_some() {
                    let args = LuaArgs {
                        player_id: id,
                        item_id: server_id,
                        pos_x, pos_y, pos_z, stackpos,
                    };
                    if let Err(e) = lua.dispatch("onUse", &args) {
                        tracing::error!(%server_id, error = %e, "Lua onUse dispatch failed");
                    }
                    // Execute any actions the Lua script requested (teleport, etc.).
                    for action in lua.drain_actions() {
                        match action {
                            super::lua::GameAction::Teleport { player_id, landing } => {
                                self.do_teleport(player_id, landing);
                            }
                        }
                    }
                }
            }
            return;
        }

        let capacity = meta.container_capacity.max(1);
        let name = meta.name.clone();
        let client_id = meta.client_id;

        // Look for an existing slot (open or closed) that already holds a container
        // from the same source — reuse it so items are never lost on close+reopen.
        let p = self.players.get(&id).unwrap();
        let existing_cid = (0u8..16).find(|&c| {
            p.open_containers[c as usize].as_ref().map(|oc| {
                matches_source(oc.source, source)
            }).unwrap_or(false)
        });

        let cid = if let Some(c) = existing_cid {
            // TFS toggle (actions.cpp useItem): using an already-open container
            // closes it. TFS keys "open" off the openContainers map (erased on
            // close); we retain the slot with `is_open=false` so contents survive
            // a reopen, so "currently open" means `is_open == true`.
            let already_open = self.players.get(&id)
                .and_then(|p| p.open_containers[c as usize].as_ref())
                .map(|oc| oc.is_open)
                .unwrap_or(false);
            if already_open {
                self.do_close_container(id, c);
                return;
            }
            // Reopen the existing (closed) slot: update metadata (in case the
            // container item changed type somehow) and mark it visible again.
            if let Some(p) = self.players.get_mut(&id) {
                if let Some(oc) = p.open_containers[c as usize].as_mut() {
                    oc.is_open = true;
                    oc.name = name;
                    oc.capacity = capacity;
                    oc.client_id = client_id;
                }
            }
            c
        } else {
            // No prior slot for this source — allocate a fresh one.
            // The client hints with `index`; use it if the slot is completely free.
            let p = self.players.get(&id).unwrap();
            let new_cid = if (index as usize) < 16 && p.open_containers[index as usize].is_none() {
                index
            } else {
                match Self::next_free_cid(p) {
                    Some(c) => c,
                    None => return, // all 16 windows occupied
                }
            };
            let oc = OpenContainer {
                server_id, client_id, capacity, name,
                items: Vec::new(), source, is_open: true,
            };
            if let Some(p) = self.players.get_mut(&id) {
                p.open_containers[new_cid as usize] = Some(oc);
            }
            new_cid
        };

        self.push_open_container(id, cid);
    }

    /// Close `cid` and every container nested inside it (depth-first).
    /// Sets `is_open = false` and sends `close_container` for each.
    fn close_container_tree(&mut self, id: u32, cid: u8) {
        let children: Vec<u8> = if let Some(p) = self.players.get(&id) {
            (0u8..16).filter(|&c| {
                p.open_containers[c as usize].as_ref().is_some_and(|oc| {
                    matches!(oc.source, ContainerSource::Nested { parent_cid: pc, .. } if pc == cid)
                })
            }).collect()
        } else {
            return;
        };
        for child in children {
            self.close_container_tree(id, child);
        }
        if let Some(p) = self.players.get_mut(&id) {
            if let Some(oc) = p.open_containers[cid as usize].as_mut() {
                oc.is_open = false;
            }
        }
        self.push(id, protocol::container::close_container(cid));
    }

    /// After removing the item at `removed_slot` from `parent_cid`:
    /// close any open window that tracked that item (and its children),
    /// and fix slot indices for siblings that shifted down.
    fn close_orphaned_nested_container(&mut self, id: u32, parent_cid: u8, removed_slot: usize) {
        let mut orphaned: Option<u8> = None;
        if let Some(p) = self.players.get_mut(&id) {
            for (c, oc_opt) in p.open_containers.iter_mut().enumerate() {
                let Some(oc) = oc_opt.as_mut() else { continue };
                let ContainerSource::Nested { parent_cid: pc, ref mut parent_slot } = oc.source
                    else { continue };
                if pc != parent_cid { continue; }
                let ps = *parent_slot as usize;
                if ps == removed_slot {
                    orphaned = Some(c as u8);
                } else if ps > removed_slot {
                    *parent_slot -= 1;
                }
            }
        }
        if let Some(cid) = orphaned {
            self.close_container_tree(id, cid);
        }
    }

    /// Handle `0x87` close-container: mark the window as closed (keep the items
    /// in memory so they survive a re-open within the same session) and send
    /// `0x6F` to the client.
    pub(super) fn do_close_container(&mut self, id: u32, cid: u8) {
        if cid >= 16 { return; }
        let Some(p) = self.players.get_mut(&id) else { return };
        match p.open_containers[cid as usize].as_mut() {
            Some(oc) => { oc.is_open = false; }
            None => return,
        }
        self.push(id, protocol::container::close_container(cid));
    }

    /// Handle `0x88` up-arrow: navigate from a nested container to its parent.
    pub(super) fn do_up_arrow(&mut self, id: u32, cid: u8) {
        if cid >= 16 { return; }
        let Some(p) = self.players.get(&id) else { return };
        let Some(oc) = p.open_containers[cid as usize].as_ref() else { return };
        let source = oc.source;

        match source {
            ContainerSource::Nested { parent_cid, .. } => {
                // The parent is already open in another cid — just send its packet.
                if parent_cid < 16 && p.open_containers[parent_cid as usize].is_some() {
                    self.push_open_container(id, parent_cid);
                }
                // Close the child window.
                if let Some(p) = self.players.get_mut(&id) {
                    p.open_containers[cid as usize] = None;
                }
                self.push(id, protocol::container::close_container(cid));
            }
            ContainerSource::Slot(_) | ContainerSource::Ground(_) => {
                // Already at the top level; up-arrow does nothing.
            }
        }
    }

    /// Add `item` to the front (slot 0) of a container. Notifies the player only
    /// when the window is currently open — a retained-but-closed container (e.g. a
    /// nested bag the item was dropped into without opening it) tracks the item in
    /// memory but sends no `0x70`, since the client has no widget for that cid.
    fn push_item_to_container(&mut self, id: u32, cid: u8, item: ContainerItem) {
        let Some(p) = self.players.get_mut(&id) else { return };
        let notify = {
            let Some(oc) = p.open_containers[cid as usize].as_mut() else { return };
            if oc.items.len() >= oc.capacity as usize { return; } // container full
            let notify = oc.is_open;
            oc.items.insert(0, item);
            notify
        };
        // Inserting at the front shifts every existing item down one slot, so any
        // nested container addressed by (parent_cid == cid, parent_slot) must have
        // its cached slot incremented to stay addressable. Without this the slot
        // goes stale, a duplicate cid is allocated on the next drop-into, and the
        // item is stranded. Symmetric to close_orphaned_nested_container's removal
        // adjustment.
        for o in p.open_containers.iter_mut().flatten() {
            if let ContainerSource::Nested { parent_cid: pc, ref mut parent_slot } = o.source {
                if pc == cid { *parent_slot = parent_slot.saturating_add(1); }
            }
        }
        if notify {
            let wire = item.wire();
            self.push(id, protocol::container::add_container_item(cid, 0, &wire));
        }
    }

    /// Remove the item at `slot` from an open container and notify the player.
    fn pop_item_from_container(&mut self, id: u32, cid: u8, slot: usize) -> Option<ContainerItem> {
        let p = self.players.get_mut(&id)?;
        let oc = p.open_containers[cid as usize].as_mut()?;
        if slot >= oc.items.len() { return None; }
        let removed = oc.items.remove(slot);
        // OTClient's onRemoveItem erases the slot and shifts items up automatically.
        // The `lastItem` (replacement) field is only for scrolled containers — it brings
        // in a previously hidden item at the bottom of the visible window. Our containers
        // never exceed capacity, so there is never a hidden item to reveal: always None.
        let pkt = protocol::container::remove_container_item(cid, slot as u16, None);
        self.push(id, pkt);
        Some(removed)
    }

    /// If the item at `slot_idx` inside container `parent_cid` is itself a
    /// container, return the cid that tracks that nested container's contents —
    /// reusing its already-allocated cid, or allocating a fresh closed one if the
    /// bag has never been opened. Returns `None` when the destination slot holds
    /// no container (the caller then inserts into `parent_cid` directly).
    ///
    /// Faithful to TFS `Container::queryDestination`, which descends into a
    /// destination slot that holds a container instead of placing beside it.
    fn nested_dest_cid(&mut self, id: u32, parent_cid: u8, slot_idx: usize) -> Option<u8> {
        let sid = {
            let p = self.players.get(&id)?;
            let oc = p.open_containers[parent_cid as usize].as_ref()?;
            oc.items.get(slot_idx)?.server_id
        };
        let meta = self.map.item_meta(sid)?;
        if !meta.is_container { return None; }

        // Reuse the cid already tracking this exact nested slot, if any.
        let target = ContainerSource::Nested { parent_cid, parent_slot: slot_idx as u8 };
        if let Some(p) = self.players.get(&id) {
            if let Some(c) = (0u8..16).find(|&c| {
                p.open_containers[c as usize].as_ref().is_some_and(|o| matches_source(o.source, target))
            }) {
                return Some(c);
            }
        }

        // Allocate a fresh, closed cid to hold the nested bag's contents.
        let p = self.players.get(&id)?;
        let cid = Self::next_free_cid(p)?;
        let oc = OpenContainer {
            server_id: sid,
            client_id: meta.client_id,
            capacity: meta.container_capacity.max(1),
            name: meta.name.clone(),
            items: Vec::new(),
            source: target,
            is_open: false,
        };
        self.players.get_mut(&id)?.open_containers[cid as usize] = Some(oc);
        Some(cid)
    }

    /// Re-key the open-container window whose source matches `old` to `new`, so a
    /// container's tracked contents follow the item when it moves between an
    /// inventory slot and the ground. A source location uniquely identifies one
    /// window, so the first match is the only one. No-op if nothing matches (the
    /// moved item is not a container, or was never opened this session).
    pub(super) fn rekey_container_source(&mut self, id: u32, old: ContainerSource, new: ContainerSource) {
        let Some(p) = self.players.get_mut(&id) else { return };
        for oc in p.open_containers.iter_mut().flatten() {
            if matches_source(oc.source, old) {
                oc.source = new;
                break;
            }
        }
    }

    /// Close every open ground container the player has walked out of range of
    /// (more than one tile on x/y, or a different floor). Inventory and nested
    /// containers travel with the player and are never closed by walking. Mirrors
    /// TFS `Player::onCreatureMove` + `autoCloseContainers`, which key off the
    /// container's tile position. Call after the player's position is committed.
    pub(super) fn auto_close_ground_containers(&mut self, id: u32) {
        let Some(player_pos) = self.players.get(&id).map(|p| p.position) else { return };
        let to_close: Vec<u8> = {
            let Some(p) = self.players.get(&id) else { return };
            (0u8..16).filter(|&c| {
                p.open_containers[c as usize].as_ref().is_some_and(|oc| {
                    oc.is_open
                        && matches!(oc.source, ContainerSource::Ground(gp) if !in_close_range(gp, player_pos))
                })
            }).collect()
        };
        // close_container_tree also closes any sub-containers opened from inside
        // the ground bag (depth-first) and sends each `0x6F`.
        for cid in to_close {
            self.close_container_tree(id, cid);
        }
    }

    /// Handle container-endpoint moves:
    ///   - container → container
    ///   - ground → container
    ///   - container → ground
    ///   - inventory slot → container (or vice versa)
    pub(super) fn do_move_container(&mut self, id: u32, from: Position, from_stackpos: u8, to: Position, count: u8) {
        // Decode endpoints.
        let from_is_container = from.x == 0xFFFF && (from.y & 0x40) != 0;
        let to_is_container   = to.x   == 0xFFFF && (to.y   & 0x40) != 0;
        let from_is_inv_slot  = from.x == 0xFFFF && (from.y & 0x40) == 0;
        let to_is_inv_slot    = to.x   == 0xFFFF && (to.y   & 0x40) == 0;

        // Decode container endpoints.
        let from_cid      = if from_is_container { Some((from.y & 0x0F) as u8) } else { None };
        let from_slot_idx = if from_is_container { Some(from.z as usize) } else { None };
        let to_cid        = if to_is_container   { Some((to.y   & 0x0F) as u8) } else { None };
        let to_slot_idx   = if to_is_container   { Some(to.z   as usize) } else { None };

        // --- CASE 1: container → container ---
        if from_is_container && to_is_container {
            let fc = from_cid.unwrap();
            let fs = from_slot_idx.unwrap();
            let tc = to_cid.unwrap();
            let ts = to_slot_idx.unwrap();

            // Dropping an item onto its own source slot is a no-op.
            if fc == tc && fs == ts { return; }

            // Resolve the real destination BEFORE removing the source item, so the
            // descent index math isn't disturbed by the removal. If the destination
            // slot holds a container, the item goes INTO it (TFS queryDestination).
            // We don't descend when the moved item is itself a container: our flat
            // cid model can't safely deep-nest bag-in-bag-in-bag.
            let moving_is_container = self.players.get(&id)
                .and_then(|p| p.open_containers[fc as usize].as_ref())
                .and_then(|oc| oc.items.get(fs))
                .and_then(|it| self.map.item_meta(it.server_id))
                .is_some_and(|m| m.is_container);
            let dest_cid = if moving_is_container { tc }
                else { self.nested_dest_cid(id, tc, ts).unwrap_or(tc) };

            // Pull the item out of `from`. close_orphaned fixes nested-cid slot
            // bookkeeping for the removal — including the dest cid if fc == tc.
            let item = match self.pop_item_from_container(id, fc, fs) {
                Some(i) => i,
                None => return,
            };
            self.close_orphaned_nested_container(id, fc, fs);

            // Check capacity on the resolved destination.
            let dest_full = {
                let p = match self.players.get(&id) { Some(p) => p, None => return };
                match p.open_containers[dest_cid as usize].as_ref() {
                    Some(oc) => oc.items.len() >= oc.capacity as usize,
                    None => return,
                }
            };
            if dest_full {
                // Put back into source at front (since we already removed it).
                self.push_item_to_container(id, fc, item);
                return;
            }

            // Insert into destination at front (TFS: addThing → push_front).
            self.push_item_to_container(id, dest_cid, item);
            return;
        }

        // --- CASE 2: ground → container ---
        if !from_is_container && !from_is_inv_slot && to_is_container {
            let tc = to_cid.unwrap();
            let from_pos = Position::new(from.x, from.y, from.z);
            let Some(p) = self.players.get(&id) else { return };
            let player_pos = p.position;

            // Adjacency check.
            let near = (i32::from(player_pos.x) - i32::from(from_pos.x)).abs() <= 1
                && (i32::from(player_pos.y) - i32::from(from_pos.y)).abs() <= 1
                && player_pos.z == from_pos.z;
            if !near { self.push_cannot_move(id, "You are too far away."); return; }

            // Resolve item from ground stack.
            let creatures = self.creatures_on(from_pos);
            let pre = self.dynamic.get(&(from_pos.x, from_pos.y, from_pos.z))
                .map(|st| st.pre_creature_len)
                .unwrap_or_else(|| self.map.tile_pre_creature_len(from_pos));
            let sp = from_stackpos as usize;
            let src_idx = if sp < pre { sp }
                else if sp < pre + creatures.len() { return; }
                else { sp - creatures.len() };

            let Some(src_sid) = self.merged_server_id(from_pos, src_idx) else { return };
            let Some(meta) = self.map.item_meta(src_sid) else { return };
            if !meta.moveable { self.push_cannot_move(id, "You cannot move this object."); return; }

            // Check dest capacity.
            let dest_full = {
                let p = match self.players.get(&id) { Some(p) => p, None => return };
                match p.open_containers[tc as usize].as_ref() {
                    Some(oc) => oc.items.len() >= oc.capacity as usize,
                    None => return,
                }
            };
            if dest_full { return; }

            let stackable  = meta.stackable;
            let client_id  = meta.client_id;
            let animated   = meta.animated;
            let want = if stackable { count.max(1) } else { 1 };

            let Some((moved, removed_fully)) = self.take_from_ground(from_pos, src_idx, want, stackable) else { return };
            self.broadcast_source(from_pos, from_stackpos, removed_fully, src_idx);

            let cnt = if stackable { Some(moved) } else { None };
            let item = ContainerItem { server_id: src_sid, client_id, count: cnt, animated };
            self.push_item_to_container(id, tc, item);
            return;
        }

        // --- CASE 3: container → ground ---
        if from_is_container && !to_is_container && !to_is_inv_slot {
            let fc = from_cid.unwrap();
            let fs = from_slot_idx.unwrap();
            let to_pos = Position::new(to.x, to.y, to.z);

            // Validate destination.
            let Some(p) = self.players.get(&id) else { return };
            let player_pos = p.position;
            if player_pos.z != to_pos.z || !self.map.can_throw_object_to(player_pos, to_pos) {
                self.push_cannot_move(id, "You cannot throw there."); return;
            }
            if self.map.tile_pre_creature_len(to_pos) == 0 && self.map.tile_stack_clone(to_pos).is_none() {
                self.push_cannot_move(id, "You cannot put that there."); return;
            }
            if self.map.is_blocked(to_pos) {
                self.push_cannot_move(id, "You cannot put that there."); return;
            }

            let item = match self.pop_item_from_container(id, fc, fs) {
                Some(i) => i,
                None => return,
            };
            self.close_orphaned_nested_container(id, fc, fs);
            let meta_stackable = self.map.item_meta(item.server_id).map(|m| m.stackable).unwrap_or(false);
            let moved = item.count.unwrap_or(1).max(1);
            let dest_creatures = self.creatures_on(to_pos).len();
            let Some((dest_merged, dest_added)) =
                self.add_to_ground_front(to_pos, item.server_id, item.client_id, moved, item.animated, meta_stackable)
            else { return };
            let dest_front = self.dynamic.get(&(to_pos.x, to_pos.y, to_pos.z))
                .map(|st| st.pre_creature_len).unwrap_or(0);
            let dest_s = (dest_front + dest_creatures).min(9) as u8;
            if let Some(wi) = dest_merged { self.broadcast_dest(to_pos, dest_s, wi, true); }
            if let Some(wi) = dest_added  { self.broadcast_dest(to_pos, dest_s, wi, false); }
            return;
        }

        // --- CASE 4: inventory slot → container ---
        if from_is_inv_slot && to_is_container {
            let inv_slot = from.y as u8;
            let tc = to_cid.unwrap();
            if !(1..=10).contains(&inv_slot) { return; }

            let item = {
                let p = match self.players.get(&id) { Some(p) => p, None => return };
                match p.inventory[(inv_slot - 1) as usize] {
                    Some(it) => it,
                    None => return,
                }
            };
            // Check capacity on dest.
            let dest_full = {
                let p = match self.players.get(&id) { Some(p) => p, None => return };
                match p.open_containers[tc as usize].as_ref() {
                    Some(oc) => oc.items.len() >= oc.capacity as usize,
                    None => return,
                }
            };
            if dest_full { return; }

            if let Some(p) = self.players.get_mut(&id) {
                p.inventory[(inv_slot - 1) as usize] = None;
            }
            self.push_inventory_slot(id, inv_slot);

            let cnt = item.count;
            let ci = ContainerItem { server_id: item.server_id, client_id: item.client_id, count: cnt, animated: item.animated };
            self.push_item_to_container(id, tc, ci);
            return;
        }

        // --- CASE 5: container → inventory slot ---
        if from_is_container && to_is_inv_slot {
            let fc = from_cid.unwrap();
            let fs = from_slot_idx.unwrap();
            let inv_slot = to.y as u8;
            if !(1..=10).contains(&inv_slot) { return; }

            // Dest slot must be empty.
            let slot_empty = {
                let p = match self.players.get(&id) { Some(p) => p, None => return };
                p.inventory[(inv_slot - 1) as usize].is_none()
            };
            if !slot_empty { return; }

            let item = match self.pop_item_from_container(id, fc, fs) {
                Some(i) => i,
                None => return,
            };
            self.close_orphaned_nested_container(id, fc, fs);

            // Check equip slot compatibility.
            let admits = self.map.item_meta(item.server_id)
                .and_then(|m| m.equip_slot)
                .map(|eq| eq.admits(inv_slot))
                .unwrap_or(false);
            if !admits { return; }

            if let Some(p) = self.players.get_mut(&id) {
                p.inventory[(inv_slot - 1) as usize] = Some(InvItem {
                    server_id: item.server_id,
                    client_id: item.client_id,
                    count: item.count,
                    animated: item.animated,
                });
            }
            self.push_inventory_slot(id, inv_slot);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use super::super::test_support::*;

    #[test]
    fn throwing_open_inventory_container_follows_to_ground_with_contents() {
        // New issue: throwing an open inventory backpack must not strand its
        // contents on the old slot window. The window follows the item to the
        // ground tile (contents intact) and closes if the throw lands out of
        // range — exactly one window, no duplicate, no empty ground bag.
        let mut g = Game::new(move_map());
        let (player, mut rx) = add_player(&mut g, Position::new(100, 100, 7));
        {
            let p = g.players.get_mut(&player).unwrap();
            p.inventory[2] = Some(InvItem { server_id: 600, client_id: 1988, count: None, animated: false });
            p.open_containers[0] = Some(OpenContainer {
                server_id: 600, client_id: 1988, capacity: 20, name: "backpack".into(),
                items: vec![ContainerItem { server_id: 200, client_id: 1987, count: None, animated: false }],
                source: ContainerSource::Slot(3), is_open: true,
            });
        }
        drain(&mut rx);

        // Throw the backpack from slot 3 to a far ground tile (105,100,7).
        g.do_move_inventory(player, Position::new(0xFFFF, 3, 0), 0, Position::new(105, 100, 7), 1);

        let cids: Vec<&OpenContainer> = (0u8..16)
            .filter_map(|c| g.players[&player].open_containers[c as usize].as_ref())
            .collect();
        assert_eq!(cids.len(), 1, "still exactly one container window (no duplicate)");
        assert!(matches!(cids[0].source, ContainerSource::Ground(p) if p == Position::new(105, 100, 7)),
            "window re-keyed to the ground tile; got {:?}", cids[0].source);
        assert!(!cids[0].is_open, "thrown out of range -> window closed");
        assert!(cids[0].items.iter().any(|i| i.server_id == 200),
            "contents preserved on the ground container");
        assert!(g.players[&player].inventory[2].is_none(), "slot 3 emptied by the throw");
    }

    #[test]
    fn walking_away_closes_ground_container_keeps_inventory_open() {
        // Issue 3: a ground container auto-closes when the player walks more than
        // one tile away; an inventory container travels with the player and stays
        // open. Player starts on (102,100,7) with both open.
        let mut g = Game::new(move_map());
        let (player, mut rx) = add_player(&mut g, Position::new(102, 100, 7));
        {
            let p = g.players.get_mut(&player).unwrap();
            p.open_containers[0] = Some(OpenContainer {
                server_id: 600, client_id: 1988, capacity: 20, name: "backpack".into(),
                items: vec![], source: ContainerSource::Ground(Position::new(102, 100, 7)), is_open: true,
            });
            p.open_containers[1] = Some(OpenContainer {
                server_id: 600, client_id: 1988, capacity: 20, name: "backpack".into(),
                items: vec![], source: ContainerSource::Slot(3), is_open: true,
            });
        }
        drain(&mut rx);

        // Step to 101: ground container at 102 is 1 tile away -> stays open.
        g.do_move(player, Direction::West);
        assert!(g.players[&player].open_containers[0].as_ref().unwrap().is_open,
            "ground container one tile away must stay open");

        // Step to 100: ground container at 102 is now 2 tiles away -> closes.
        drain(&mut rx);
        g.do_move(player, Direction::West);
        let pkts = drain(&mut rx);
        assert!(has_op(&pkts, protocol::container::OP_CLOSE_CONTAINER),
            "a 0x6F close must be sent for the out-of-range ground container; got {:?}", pkts);
        assert!(!g.players[&player].open_containers[0].as_ref().unwrap().is_open,
            "ground container more than one tile away must close");
        assert!(g.players[&player].open_containers[1].as_ref().unwrap().is_open,
            "inventory container must stay open while walking");
    }

    #[test]
    fn drop_onto_nested_bag_opened_before_parent_shift_is_not_lost() {
        // Real loss repro: open the inner bag FIRST (its cid is pinned to the slot
        // it occupied then), THEN insert an item at the front of the parent — which
        // shifts the inner bag down a slot — THEN drag an item onto the inner bag.
        // Without slot maintenance on insertion the inner bag's cached parent_slot
        // goes stale: a duplicate cid is allocated, close_orphaned collapses both to
        // the same source, and the empty stale cid shadows the one holding the item
        // on reopen -> the item is stranded. The fix keeps exactly one nested cid,
        // open, holding the item.
        let mut g = Game::new(move_map());
        let (player, mut rx) = add_player(&mut g, Position::new(100, 100, 7));
        g.players.get_mut(&player).unwrap().inventory[2] =
            Some(InvItem { server_id: 600, client_id: 1988, count: None, animated: false });
        // Parent backpack (cid 0) initially holds only the inner bag at slot 0.
        g.players.get_mut(&player).unwrap().open_containers[0] = Some(OpenContainer {
            server_id: 600, client_id: 1988, capacity: 20, name: "backpack".into(),
            items: vec![ContainerItem { server_id: 600, client_id: 1988, count: None, animated: false }],
            source: ContainerSource::Slot(3), is_open: true,
        });
        drain(&mut rx);

        // Open the inner bag (parent cid 0, slot 0) -> nested cid pinned to slot 0.
        g.do_use_item(player, 0xFFFF, 0x40, 0, 0, 0);
        // Insert a stone at the FRONT of the parent, shifting the inner bag to slot 1.
        g.push_item_to_container(player, 0, ContainerItem { server_id: 200, client_id: 1987, count: None, animated: false });
        drain(&mut rx);

        // Drag the stone (cid 0, slot 0) onto the inner bag icon (cid 0, slot 1).
        g.do_move_container(player, Position::new(0xFFFF, 0x40, 0), 0, Position::new(0xFFFF, 0x40, 1), 1);

        // Exactly one nested cid under the parent, open, holding the stone.
        let nested: Vec<&OpenContainer> = (0u8..16).filter_map(|c| {
            g.players[&player].open_containers[c as usize].as_ref()
                .filter(|oc| matches!(oc.source, ContainerSource::Nested { parent_cid: 0, .. }))
        }).collect();
        assert_eq!(nested.len(), 1, "exactly one nested cid (no shadow duplicate)");
        assert!(nested[0].items.iter().any(|i| i.server_id == 200),
            "stone must be inside the inner bag, not stranded; contents: {:?}",
            nested[0].items.iter().map(|i| i.server_id).collect::<Vec<_>>());
        assert!(nested[0].is_open, "inner bag stays open and shows the stone");
    }

    #[test]
    fn drop_item_onto_nested_bag_routes_inside_and_is_retrievable() {
        // Issue 2 repro: drag a non-container item onto a CLOSED nested bag icon
        // inside an open parent backpack. The item must route INTO the nested bag
        // (vanishing from the parent window, no false re-add) and be retrievable
        // by opening that nested bag. Parent = cid 0 (inventory slot 3) holding
        // [stone@slot0, inner-backpack@slot1].
        let mut g = Game::new(move_map());
        let (player, mut rx) = add_player(&mut g, Position::new(100, 100, 7));
        {
            let p = g.players.get_mut(&player).unwrap();
            p.inventory[2] = Some(InvItem { server_id: 600, client_id: 1988, count: None, animated: false });
            p.open_containers[0] = Some(OpenContainer {
                server_id: 600, client_id: 1988, capacity: 20, name: "backpack".into(),
                items: vec![
                    ContainerItem { server_id: 200, client_id: 1987, count: None, animated: false }, // stone slot 0
                    ContainerItem { server_id: 600, client_id: 1988, count: None, animated: false }, // inner bag slot 1
                ],
                source: ContainerSource::Slot(3), is_open: true,
            });
        }
        drain(&mut rx);

        // Drag stone (cid 0, slot 0) onto the inner bag (cid 0, slot 1).
        let from = Position::new(0xFFFF, 0x40, 0); // cid 0, slot 0
        let to = Position::new(0xFFFF, 0x40, 1);   // cid 0, slot 1 (inner bag)
        g.do_move_container(player, from, 0, to, 1);

        // Parent must keep only the inner bag; the stone left it.
        let parent = g.players[&player].open_containers[0].as_ref().unwrap();
        assert_eq!(parent.items.len(), 1, "parent should keep only the inner bag");
        assert_eq!(parent.items[0].server_id, 600, "remaining parent item is the inner bag");

        // A nested cid must now track the inner bag and hold the stone.
        let nested = (0u8..16).find(|&c| {
            c != 0 && g.players[&player].open_containers[c as usize].as_ref()
                .is_some_and(|o| matches!(o.source, ContainerSource::Nested { parent_cid: 0, .. }))
        }).expect("a nested cid must be allocated for the inner bag");
        let noc = g.players[&player].open_containers[nested as usize].as_ref().unwrap();
        assert_eq!(noc.items.len(), 1, "inner bag must hold the routed stone (not lost)");
        assert_eq!(noc.items[0].server_id, 200, "routed item is the stone");

        // Retrievable: opening the inner bag (now at parent slot 0) shows the stone.
        drain(&mut rx);
        g.do_use_item(player, 0xFFFF, 0x40, 0, 0, 0); // container endpoint: cid 0, slot 0
        let pkts = drain(&mut rx);
        assert!(has_op(&pkts, protocol::container::OP_OPEN_CONTAINER),
            "opening the inner bag must push 0x6E; got {:?}", pkts);
        let noc = g.players[&player].open_containers[nested as usize].as_ref().unwrap();
        assert!(noc.is_open, "inner bag must be open after use");
        assert_eq!(noc.items[0].server_id, 200, "stone must be visible inside the opened inner bag");
    }

    #[test]
    fn do_use_item_on_open_container_toggles_closed() {
        // Issue 4: using (0x82) a container that is already open must CLOSE it
        // (TFS actions.cpp toggle), not re-send another 0x6E open. A third use
        // re-opens it. Source is inventory slot 3 (pos_x=0xFFFF, pos_y=3, no 0x40).
        let mut g = Game::new(move_map());
        let (player, mut rx) = add_player(&mut g, Position::new(100, 100, 7));
        // Put the backpack (sid 600) in inventory slot 3.
        g.players.get_mut(&player).unwrap().inventory[2] = Some(InvItem {
            server_id: 600, client_id: 1988, count: None, animated: false,
        });
        drain(&mut rx);

        // 1st use → open. Expect a 0x6E and is_open == true.
        g.do_use_item(player, 0xFFFF, 3, 0, 0, 0);
        let pkts = drain(&mut rx);
        assert!(has_op(&pkts, protocol::container::OP_OPEN_CONTAINER),
            "first use must push 0x6E open; got {:?}", pkts);
        let cid = (0u8..16).find(|&c| g.players[&player].open_containers[c as usize].is_some())
            .expect("a container slot must be allocated");
        assert!(g.players[&player].open_containers[cid as usize].as_ref().unwrap().is_open,
            "container must be open after first use");

        // 2nd use → close. Expect a 0x6F and is_open == false (slot retained).
        g.do_use_item(player, 0xFFFF, 3, 0, 0, 0);
        let pkts = drain(&mut rx);
        assert!(has_op(&pkts, protocol::container::OP_CLOSE_CONTAINER),
            "second use must push 0x6F close; got {:?}", pkts);
        assert!(!has_op(&pkts, protocol::container::OP_OPEN_CONTAINER),
            "second use must NOT re-send 0x6E open; got {:?}", pkts);
        assert!(!g.players[&player].open_containers[cid as usize].as_ref().unwrap().is_open,
            "container must be closed (but retained) after second use");

        // 3rd use → re-open the retained slot.
        g.do_use_item(player, 0xFFFF, 3, 0, 0, 0);
        let pkts = drain(&mut rx);
        assert!(has_op(&pkts, protocol::container::OP_OPEN_CONTAINER),
            "third use must re-open with 0x6E; got {:?}", pkts);
        assert!(g.players[&player].open_containers[cid as usize].as_ref().unwrap().is_open,
            "container must be open again after third use");
    }

    // -------------------------------------------------------------------------
    // M10.1 do_move_thing tests
    // -------------------------------------------------------------------------

    // -------------------------------------------------------------------------
    // M11.3 — onUse Lua dispatch integration
    // -------------------------------------------------------------------------

    /// Create a unique temp directory for a Lua integration test.
    fn lua_test_dir(label: &str) -> std::path::PathBuf {
        static COUNTER: std::sync::atomic::AtomicU16 = std::sync::atomic::AtomicU16::new(0);
        let seq = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("oxidia-containers-{label}-{seq}"));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// Count how many times `call_count` was incremented by the Lua script.
    fn lua_call_count(g: &Game) -> i64 {
        g.lua.as_ref()
            .and_then(|rt| rt.get_global_i64("call_count"))
            .unwrap_or(0)
    }

    #[test]
    fn registered_item_triggers_lua_onuse_dispatch() {
        // RED: Current do_use_item returns early for non-container items, so
        // Lua dispatch is never called. This test fails with call_count == 0.
        let script_dir = lua_test_dir("registered");
        std::fs::write(
            script_dir.join("test.lua"),
            b"call_count = 0\nfunction onUse(args) call_count = call_count + 1 return true end",
        )
        .unwrap();
        let mut g = Game::new(move_map());
        g.lua = Some(LuaRuntime::new(&script_dir));
        g.registry = XmlRegistry::from_actions_xml(
            r#"<actions><action itemid="200" script="test.onUse"/></actions>"#,
        )
        .unwrap();
        let (player, mut rx) = add_player(&mut g, Position::new(100, 100, 7));
        drain(&mut rx);

        // Use the stone (sid 200) at (101,100,7), stackpos 1 (ground + stone).
        g.do_use_item(player, 101, 100, 7, 1, 0);

        assert_eq!(
            lua_call_count(&g),
            1,
            "onUse must be dispatched exactly once for registered item 200"
        );
        let _ = std::fs::remove_dir_all(&script_dir);
    }

    #[test]
    fn unregistered_item_silently_ignored_by_lua_dispatch() {
        // Triangulation: a non-container item NOT in the XmlRegistry must NOT
        // trigger Lua dispatch. call_count stays 0.
        let script_dir = lua_test_dir("unregistered");
        std::fs::write(
            script_dir.join("test.lua"),
            b"call_count = 0\nfunction onUse(args) call_count = call_count + 1 return true end",
        )
        .unwrap();
        let mut g = Game::new(move_map());
        g.lua = Some(LuaRuntime::new(&script_dir));
        // Empty registry — no items registered.
        g.registry = XmlRegistry::default();
        let (player, mut rx) = add_player(&mut g, Position::new(100, 100, 7));
        drain(&mut rx);

        // Use gold coin (sid 300) at (102,100,7) — NOT registered.
        g.do_use_item(player, 102, 100, 7, 1, 0);

        assert_eq!(
            lua_call_count(&g),
            0,
            "onUse must NOT be dispatched for unregistered item"
        );
        let _ = std::fs::remove_dir_all(&script_dir);
    }

    #[test]
    fn throwing_open_ground_container_follows_and_closes_out_of_range() {
        // New detail: a container opened from the ground and thrown far must close.
        // The tile-to-tile move re-keys the window to the new tile and auto-closes
        // it when it lands out of range. Backpack on (100,110,7); player adjacent
        // on (100,111,7); throw to (100,113,7) (2 tiles from the player).
        let mut g = Game::new(move_map());
        let (player, mut rx) = add_player(&mut g, Position::new(100, 111, 7));
        // Open the ground backpack window (cid keyed to its tile).
        g.players.get_mut(&player).unwrap().open_containers[0] = Some(OpenContainer {
            server_id: 600, client_id: 1988, capacity: 20, name: "backpack".into(),
            items: vec![], source: ContainerSource::Ground(Position::new(100, 110, 7)), is_open: true,
        });
        drain(&mut rx);

        // Throw the backpack from (100,110,7) to (100,113,7). Stackpos 1 = the
        // backpack (ground at 0, no creatures on that tile).
        g.do_move_thing(player, Position::new(100, 110, 7), 1, Position::new(100, 113, 7), 1);

        let oc = g.players[&player].open_containers[0].as_ref().expect("window retained");
        assert!(matches!(oc.source, ContainerSource::Ground(p) if p == Position::new(100, 113, 7)),
            "window re-keyed to the destination tile; got {:?}", oc.source);
        assert!(!oc.is_open, "container thrown out of range must close");
    }

    // -------------------------------------------------------------------------
    // M11.4 — Teleport integration
    // -------------------------------------------------------------------------

    #[test]
    fn lua_onuse_teleports_player_upstairs() {
        // RED: After dispatching to a Lua script that calls do_teleport, the
        // player must appear on the floor above. Prior to the action-drain
        // mechanism, the teleport request from Lua is ignored.
        let script_dir = lua_test_dir("teleport_up");
        std::fs::write(
            script_dir.join("test.lua"),
            b"function onUse(args) do_teleport(args.player_id, args.pos_x, args.pos_y, args.pos_z - 1) return true end",
        )
        .unwrap();
        let mut g = Game::new(move_map());
        g.lua = Some(LuaRuntime::new(&script_dir));
        g.registry = XmlRegistry::from_actions_xml(
            r#"<actions><action itemid="200" script="test.onUse"/></actions>"#,
        )
        .unwrap();
        let (player, mut rx) = add_player(&mut g, Position::new(100, 100, 7));
        drain(&mut rx);

        // Use the stone (sid 200) at (101,100,7), stackpos 1.
        g.do_use_item(player, 101, 100, 7, 1, 0);

        assert_eq!(
            g.players[&player].position,
            Position::new(101, 100, 6),
            "player must be teleported upstairs (z-1) after using a registered item"
        );
        let _ = std::fs::remove_dir_all(&script_dir);
    }

    #[test]
    fn lua_error_during_onuse_does_not_crash_server() {
        // RED: a Lua script that calls error() must be caught by the dispatch
        // pcall and logged, not propagated as a Rust panic.
        let script_dir = lua_test_dir("lua_error");
        std::fs::write(
            script_dir.join("test.lua"),
            b"function onUse(args) error('boom') return true end",
        )
        .unwrap();
        let mut g = Game::new(move_map());
        g.lua = Some(LuaRuntime::new(&script_dir));
        g.registry = XmlRegistry::from_actions_xml(
            r#"<actions><action itemid="200" script="test.onUse"/></actions>"#,
        )
        .unwrap();
        let (player, mut rx) = add_player(&mut g, Position::new(100, 100, 7));
        drain(&mut rx);

        // Must not panic.
        g.do_use_item(player, 101, 100, 7, 1, 0);

        // Player position unchanged (no teleport happened).
        assert_eq!(
            g.players[&player].position,
            Position::new(100, 100, 7),
            "player must stay at original position after a Lua error"
        );
        let _ = std::fs::remove_dir_all(&script_dir);
    }
}
