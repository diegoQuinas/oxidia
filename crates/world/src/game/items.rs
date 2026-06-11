//! Ground/inventory item movement for the game actor.

use super::*;

impl Game {
    /// Push the private `0x78`/`0x79` for one equipment slot to its owner.
    pub(super) fn push_inventory_slot(&mut self, id: u32, slot: u8) {
        let Some(p) = self.players.get(&id) else {
            return;
        };
        let pkt = match p.inventory[(slot - 1) as usize] {
            Some(it) => {
                let wi = WireItem {
                    client_id: it.client_id,
                    subtype: it.count,
                    animated: it.animated,
                };
                enter_world::set_inventory_slot(slot, &wi)
            }
            None => vec![enter_world::OP_INVENTORY_EMPTY, slot],
        };
        self.push(id, pkt);
    }

    /// The wire stackpos for the item at overlay/static index `idx` on `pos`,
    /// accounting for creatures inserted between the pre-creature items and the
    /// down items. Capped at 9 like the client stack.
    fn item_wire_stackpos(&self, pos: Position, idx: usize) -> u8 {
        let pre = self
            .dynamic
            .get(&(pos.x, pos.y, pos.z))
            .map(|st| st.pre_creature_len)
            .unwrap_or_else(|| self.map.tile_pre_creature_len(pos));
        let creatures = self.creatures_on(pos).len();
        let sp = if idx < pre { idx } else { idx + creatures };
        sp.min(9) as u8
    }

    /// COW the source tile and remove (or split) `want` units of the item at stack
    /// index `src_idx`. Returns the amount actually taken and whether the slot was
    /// fully removed. `src_idx` must already be validated by the caller.
    pub(super) fn take_from_ground(
        &mut self,
        from: Position,
        src_idx: usize,
        want: u8,
        stackable: bool,
    ) -> Option<(u8, bool)> {
        if !self.materialize(from) {
            return None;
        }
        let st = self.dynamic.get_mut(&(from.x, from.y, from.z)).unwrap();
        let cur = st.counts[src_idx].unwrap_or(1).max(1);
        let moved = if stackable { want.max(1).min(cur) } else { 1 };
        let removed_fully;
        if stackable && cur > moved {
            let left = cur - moved;
            st.counts[src_idx] = Some(left);
            st.items[src_idx].subtype = Some(left);
            removed_fully = false;
        } else {
            st.items.remove(src_idx);
            st.server_ids.remove(src_idx);
            st.counts.remove(src_idx);
            if src_idx < st.pre_creature_len {
                st.pre_creature_len -= 1;
            }
            removed_fully = true;
        }
        Some((moved, removed_fully))
    }

    /// COW the dest tile and insert `moved` units of item `src_sid` at the FRONT of
    /// the down-items, merging into a same-type front stack (cap 100, spill on
    /// overflow). Returns `(merged_update, added)` wire items to broadcast at slot S.
    pub(super) fn add_to_ground_front(
        &mut self,
        to: Position,
        src_sid: u16,
        client_id: u16,
        moved: u8,
        animated: bool,
        stackable: bool,
    ) -> Option<(Option<WireItem>, Option<WireItem>)> {
        if !self.materialize(to) {
            return None;
        }
        let st = self.dynamic.get_mut(&(to.x, to.y, to.z)).unwrap();
        let front = st.pre_creature_len;
        let merge_at_front =
            stackable && st.server_ids.len() > front && st.server_ids[front] == src_sid;
        if merge_at_front {
            let total = u32::from(st.counts[front].unwrap_or(1).max(1)) + u32::from(moved);
            let capped = total.min(100) as u8;
            st.counts[front] = Some(capped);
            st.items[front].subtype = Some(capped);
            let merged_item = st.items[front];
            if total > 100 {
                let spill_count = u8::try_from(total - 100).unwrap_or(u8::MAX);
                let spill_wi = WireItem {
                    client_id,
                    subtype: Some(spill_count),
                    animated,
                };
                st.items.insert(front, spill_wi);
                st.server_ids.insert(front, src_sid);
                st.counts.insert(front, Some(spill_count));
                Some((Some(merged_item), Some(spill_wi)))
            } else {
                Some((Some(merged_item), None))
            }
        } else {
            let subtype = if stackable { Some(moved) } else { None };
            let wi = WireItem {
                client_id,
                subtype,
                animated,
            };
            st.items.insert(front, wi);
            st.server_ids.insert(front, src_sid);
            st.counts.insert(front, subtype);
            Some((None, Some(wi)))
        }
    }

    /// Move a thing from one map tile to another (M10.1: ground items only).
    /// Validates moveability, reach, and throw line-of-sight; removes `count`
    /// from the source (split or whole), then merges same-type stackables on the
    /// destination (cap 100, overflow spills) or appends a new down item. Both
    /// tiles are copied-on-write into `dynamic` before mutation, then the change
    /// is broadcast to spectators.
    pub(super) fn do_move_thing(
        &mut self,
        id: u32,
        from: Position,
        from_stackpos: u8,
        to: Position,
        count: u8,
    ) {
        // Route any endpoint with x==0xFFFF (inventory or container).
        if from.x == 0xFFFF || to.x == 0xFFFF {
            // Check for container endpoints: y & 0x40 != 0.
            let from_is_container = from.x == 0xFFFF && (from.y & 0x40) != 0;
            let to_is_container = to.x == 0xFFFF && (to.y & 0x40) != 0;
            if from_is_container || to_is_container {
                self.do_move_container(id, from, from_stackpos, to, count);
            } else {
                self.do_move_inventory(id, from, from_stackpos, to, count);
            }
            return;
        }
        if from == to {
            return;
        }
        let Some(p) = self.players.get(&id) else {
            return;
        };
        let player_pos = p.position;

        let near = (i32::from(player_pos.x) - i32::from(from.x)).abs() <= 1
            && (i32::from(player_pos.y) - i32::from(from.y)).abs() <= 1
            && player_pos.z == from.z;
        if !near {
            self.push_cannot_move(id, "You are too far away.");
            return;
        }

        if !self.map.can_throw_object_to(from, to) {
            self.push_cannot_move(id, "You cannot throw there.");
            return;
        }

        let creatures = self.creatures_on(from);
        let pre = self
            .dynamic
            .get(&(from.x, from.y, from.z))
            .map(|st| st.pre_creature_len)
            .unwrap_or_else(|| self.map.tile_pre_creature_len(from));
        let sp = from_stackpos as usize;
        let src_idx = if sp < pre {
            sp
        } else if sp < pre + creatures.len() {
            return;
        }
        // a creature, not an item
        else {
            sp - creatures.len()
        };

        let Some(src_sid) = self.merged_server_id(from, src_idx) else {
            return;
        };
        let Some(meta) = self.map.item_meta(src_sid) else {
            return;
        };
        let stackable = meta.stackable;
        if !meta.moveable {
            self.push_cannot_move(id, "You cannot move this object.");
            return;
        }
        let client_id = meta.client_id;
        let animated = meta.animated;
        let is_container = meta.is_container;

        if self.map.tile_pre_creature_len(to) == 0 && self.map.tile_stack_clone(to).is_none() {
            self.push_cannot_move(id, "You cannot put that there.");
            return;
        }
        // Reject block-solid destinations (walls): TFS refuses to place items on a
        // tile whose stack holds an unpassable item.
        if self.map.is_blocked(to) {
            self.push_cannot_move(id, "You cannot put that there.");
            return;
        }

        let moved_req = if stackable { count.max(1) } else { 1 };

        let Some((moved, removed_fully)) =
            self.take_from_ground(from, src_idx, moved_req, stackable)
        else {
            return;
        };

        let dest_creatures = self.creatures_on(to).len();
        let Some((dest_merged_update, dest_added)) =
            self.add_to_ground_front(to, src_sid, client_id, moved, animated, stackable)
        else {
            return;
        };
        // Broadcast: all dest changes target the top down-item slot S = front + creatures.
        // For the overflow case: UPDATE the existing stack at S first (it gets capped to 100),
        // then ADD the spill at S (which pushes the existing one down to S+1 on the client).
        let dest_front = self
            .dynamic
            .get(&(to.x, to.y, to.z))
            .map(|st| st.pre_creature_len)
            .unwrap_or(0);
        let dest_s = (dest_front + dest_creatures).min(9) as u8;
        if let Some(item) = dest_merged_update {
            self.broadcast_dest(to, dest_s, item, true); // 0x6B update existing stack
        }
        if let Some(item) = dest_added {
            self.broadcast_dest(to, dest_s, item, false); // 0x6A add new/spill on top
        }
        self.broadcast_source(from, from_stackpos, removed_fully, src_idx);

        // A container dragged tile-to-tile carries its open window (and contents)
        // with it: re-key to the new tile, then close it if it landed out of range.
        if is_container {
            self.rekey_container_source(
                id,
                ContainerSource::Ground(from),
                ContainerSource::Ground(to),
            );
            self.auto_close_ground_containers(id);
        }
    }

    /// Handle a move where at least one endpoint is an inventory slot (`x==0xFFFF`,
    /// slot = `y`). Three cases: ground→slot (equip), slot→ground (unequip),
    /// slot→slot (move/swap). Equipment packets are private to the player.
    pub(super) fn do_move_inventory(
        &mut self,
        id: u32,
        from: Position,
        from_stackpos: u8,
        to: Position,
        count: u8,
    ) {
        // An inventory endpoint must be a real slot 1..=10, validated on the raw
        // u16 BEFORE truncating to u8 — else a hacked client sending e.g.
        // y == 0x4001 (clears the upstream `& 0x40` container guard) would
        // truncate to a valid-looking slot. A 0xFFFF endpoint with an out-of-range
        // slot is rejected outright rather than mis-read as a ground tile.
        let from_slot = if from.x == 0xFFFF {
            if !(1..=10).contains(&from.y) {
                return;
            }
            Some(from.y as u8)
        } else {
            None
        };
        let to_slot = if to.x == 0xFFFF {
            if !(1..=10).contains(&to.y) {
                return;
            }
            Some(to.y as u8)
        } else {
            None
        };

        match (from_slot, to_slot) {
            // ---- equip: ground → slot ----
            (None, Some(slot)) => {
                if !(1..=10).contains(&slot) {
                    return;
                }
                let Some(p) = self.players.get(&id) else {
                    return;
                };
                let player_pos = p.position;
                if p.inventory[(slot - 1) as usize].is_some() {
                    self.push_cannot_move(id, "You cannot equip this object.");
                    return;
                }
                let near = (i32::from(player_pos.x) - i32::from(from.x)).abs() <= 1
                    && (i32::from(player_pos.y) - i32::from(from.y)).abs() <= 1
                    && player_pos.z == from.z;
                if !near {
                    self.push_cannot_move(id, "You are too far away.");
                    return;
                }

                let creatures = self.creatures_on(from).len();
                let pre = self
                    .dynamic
                    .get(&(from.x, from.y, from.z))
                    .map(|st| st.pre_creature_len)
                    .unwrap_or_else(|| self.map.tile_pre_creature_len(from));
                let sp = from_stackpos as usize;
                let src_idx = if sp < pre {
                    sp
                } else if sp < pre + creatures {
                    return;
                } else {
                    sp - creatures
                };

                let Some(src_sid) = self.merged_server_id(from, src_idx) else {
                    return;
                };
                let Some(meta) = self.map.item_meta(src_sid) else {
                    return;
                };
                if !meta.moveable {
                    self.push_cannot_move(id, "You cannot move this object.");
                    return;
                }
                let Some(eq) = meta.equip_slot else {
                    self.push_cannot_move(id, "You cannot equip this object.");
                    return;
                };
                if !eq.admits(slot) {
                    self.push_cannot_move(id, "You cannot equip this object.");
                    return;
                }
                let (stackable, client_id, animated) =
                    (meta.stackable, meta.client_id, meta.animated);
                let want = if stackable { count.max(1) } else { 1 };

                let Some((moved, removed_fully)) =
                    self.take_from_ground(from, src_idx, want, stackable)
                else {
                    return;
                };
                self.broadcast_source(from, from_stackpos, removed_fully, src_idx);

                let cnt = if stackable { Some(moved) } else { None };
                if let Some(p) = self.players.get_mut(&id) {
                    p.inventory[(slot - 1) as usize] = Some(InvItem {
                        server_id: src_sid,
                        client_id,
                        count: cnt,
                        animated,
                    });
                }
                self.push_inventory_slot(id, slot);
                // A container picked up from the ground keeps its contents: follow
                // its open window from the ground tile to the inventory slot.
                self.rekey_container_source(
                    id,
                    ContainerSource::Ground(from),
                    ContainerSource::Slot(slot),
                );
            }
            // ---- unequip: slot → ground ----
            (Some(slot), None) => {
                if !(1..=10).contains(&slot) {
                    return;
                }
                let Some(p) = self.players.get(&id) else {
                    return;
                };
                let player_pos = p.position;
                let Some(it) = p.inventory[(slot - 1) as usize] else {
                    return;
                };
                // Unequip THROWS the item from the body to the ground. The source
                // is on the player, so (matching TFS playerMoveItem: mapFromPos is
                // the player's own tile when fromCylinder is the inventory) the only
                // distance constraint is throw range + line of sight to the dest —
                // NOT adjacency. You can toss an unequipped item across the screen.
                if player_pos.z != to.z || !self.map.can_throw_object_to(player_pos, to) {
                    self.push_cannot_move(id, "You cannot throw there.");
                    return;
                }
                if self.map.tile_pre_creature_len(to) == 0
                    && self.map.tile_stack_clone(to).is_none()
                {
                    self.push_cannot_move(id, "You cannot put that there.");
                    return;
                }
                // Reject block-solid destinations (walls), mirroring do_move_thing.
                if self.map.is_blocked(to) {
                    self.push_cannot_move(id, "You cannot put that there.");
                    return;
                }
                let meta_stackable = self
                    .map
                    .item_meta(it.server_id)
                    .map(|m| m.stackable)
                    .unwrap_or(false);
                let moved = it.count.unwrap_or(1).max(1);

                if let Some(p) = self.players.get_mut(&id) {
                    p.inventory[(slot - 1) as usize] = None;
                }
                self.push_inventory_slot(id, slot);

                let dest_creatures = self.creatures_on(to).len();
                let Some((merged, added)) = self.add_to_ground_front(
                    to,
                    it.server_id,
                    it.client_id,
                    moved,
                    it.animated,
                    meta_stackable,
                ) else {
                    return;
                };
                let dest_front = self
                    .dynamic
                    .get(&(to.x, to.y, to.z))
                    .map(|st| st.pre_creature_len)
                    .unwrap_or(0);
                let dest_s = (dest_front + dest_creatures).min(9) as u8;
                if let Some(item) = merged {
                    self.broadcast_dest(to, dest_s, item, true);
                }
                if let Some(item) = added {
                    self.broadcast_dest(to, dest_s, item, false);
                }
                // A thrown container keeps its contents: follow its open window to
                // the ground tile, then close it if the throw landed out of range.
                self.rekey_container_source(
                    id,
                    ContainerSource::Slot(slot),
                    ContainerSource::Ground(to),
                );
                self.auto_close_ground_containers(id);
            }
            // ---- slot → slot: move or swap ----
            (Some(src), Some(dst)) => {
                if !(1..=10).contains(&src) || !(1..=10).contains(&dst) || src == dst {
                    return;
                }
                let Some(p) = self.players.get(&id) else {
                    return;
                };
                let Some(moving) = p.inventory[(src - 1) as usize] else {
                    return;
                };
                if let Some(eq) = self
                    .map
                    .item_meta(moving.server_id)
                    .and_then(|m| m.equip_slot)
                {
                    if !eq.admits(dst) {
                        self.push_cannot_move(id, "You cannot equip this object.");
                        return;
                    }
                } else {
                    self.push_cannot_move(id, "You cannot equip this object.");
                    return;
                }
                let displaced = p.inventory[(dst - 1) as usize];
                if let Some(d) = displaced {
                    let ok = self
                        .map
                        .item_meta(d.server_id)
                        .and_then(|m| m.equip_slot)
                        .map(|eq| eq.admits(src))
                        .unwrap_or(false);
                    if !ok {
                        self.push_cannot_move(id, "You cannot equip this object.");
                        return;
                    }
                }
                if let Some(p) = self.players.get_mut(&id) {
                    p.inventory[(dst - 1) as usize] = Some(moving);
                    p.inventory[(src - 1) as usize] = displaced;
                }
                self.push_inventory_slot(id, src);
                self.push_inventory_slot(id, dst);
            }
            (None, None) => {}
        }
    }

    /// Broadcast a 0x6A add (`is_update=false`) or 0x6B update (`is_update=true`) of `item`
    /// at explicit wire stackpos `sp` on `pos`, to every player who can see the tile.
    pub(super) fn broadcast_dest(
        &mut self,
        pos: Position,
        sp: u8,
        item: WireItem,
        is_update: bool,
    ) {
        let mut targets = self.spectators(pos, u32::MAX);
        targets.extend(
            self.players
                .iter()
                .filter(|(_, p)| p.position == pos)
                .map(|(&i, _)| i),
        );
        targets.sort_unstable();
        targets.dedup();
        for t in targets {
            let pkt = if is_update {
                tile_item::update_tile_item((pos.x, pos.y, pos.z), sp, &item)
            } else {
                tile_item::add_tile_item((pos.x, pos.y, pos.z), sp, &item)
            };
            self.push(t, pkt);
        }
    }

    /// Broadcast the source-tile change: a full removal (`0x6C`) or an in-place
    /// count update (`0x6B`) when only part of a stack was taken.
    pub(super) fn broadcast_source(
        &mut self,
        pos: Position,
        from_stackpos: u8,
        removed_fully: bool,
        src_idx: usize,
    ) {
        let mut targets = self.spectators(pos, u32::MAX);
        targets.extend(
            self.players
                .iter()
                .filter(|(_, p)| p.position == pos)
                .map(|(&i, _)| i),
        );
        targets.sort_unstable();
        targets.dedup();
        let pkt = if removed_fully {
            tile_creature::remove_tile_thing((pos.x, pos.y, pos.z), from_stackpos)
        } else {
            let item = self
                .dynamic
                .get(&(pos.x, pos.y, pos.z))
                .and_then(|st| st.items.get(src_idx).copied());
            let Some(item) = item else { return };
            let sp = self.item_wire_stackpos(pos, src_idx);
            tile_item::update_tile_item((pos.x, pos.y, pos.z), sp, &item)
        };
        for t in targets {
            self.push(t, pkt.clone());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::MSG_STATUS_SMALL;
    use super::super::test_support::*;
    use super::*;

    #[test]
    fn do_move_thing_multi_hop_never_duplicates_including_on_tile() {
        // Reproduction for the reported "moving an item duplicates it at every
        // tile" bug. Drag the stone (sid 200, non-stackable, moveable) across a
        // chain of tiles — including a hop where the destination is the player's
        // OWN tile (creature present), which is the on-tile case the existing
        // tests deliberately avoid. After every hop exactly ONE stone must exist.
        let mut g = Game::new(move_map());
        // Player stands on (102,100,7) the whole time; it can reach 101/102/103.
        let (player, mut rx) = add_player(&mut g, Position::new(102, 100, 7));
        drain(&mut rx);

        // Hop 1: 101 -> 102 (onto the player's tile). Stone on 101 is at item
        // index 1 (ground at 0), no creature on 101 -> wire stackpos 1.
        g.do_move_thing(
            player,
            Position::new(101, 100, 7),
            1,
            Position::new(102, 100, 7),
            1,
        );
        assert_eq!(
            count_sid_in_overlays(&g, 200),
            1,
            "after hop 1 exactly one stone must exist; overlays: {:?}",
            g.dynamic
                .iter()
                .map(|(k, v)| (*k, v.server_ids.clone()))
                .collect::<Vec<_>>()
        );

        // Hop 2: 102 -> 103. The stone is now a DOWN item on the player's tile:
        // pre_creature_len=1 (ground), 1 creature, stone at down-index 0
        // -> wire stackpos = 1 + 1 = 2.
        g.do_move_thing(
            player,
            Position::new(102, 100, 7),
            2,
            Position::new(103, 100, 7),
            1,
        );
        assert_eq!(
            count_sid_in_overlays(&g, 200),
            1,
            "after hop 2 exactly one stone must exist; overlays: {:?}",
            g.dynamic
                .iter()
                .map(|(k, v)| (*k, v.server_ids.clone()))
                .collect::<Vec<_>>()
        );

        // Hop 3: 103 -> 101. Stone on 103 sits above the deco (down-index 1):
        // pre_creature_len=1, no creature on 103 -> wire stackpos 1.
        g.do_move_thing(
            player,
            Position::new(103, 100, 7),
            1,
            Position::new(101, 100, 7),
            1,
        );
        assert_eq!(
            count_sid_in_overlays(&g, 200),
            1,
            "after hop 3 exactly one stone must exist; overlays: {:?}",
            g.dynamic
                .iter()
                .map(|(k, v)| (*k, v.server_ids.clone()))
                .collect::<Vec<_>>()
        );
    }

    #[test]
    fn do_move_thing_from_eq_to_is_noop() {
        // from == to must be an early return with no overlay change and no packet.
        let mut g = Game::new(move_map());
        // Player at (100,100,7) (spawn), source at (101,100,7) — adjacent.
        let (player, mut rx) = add_player(&mut g, Position::new(100, 100, 7));
        let pos = Position::new(101, 100, 7);
        drain(&mut rx); // discard any login-related packets
        g.do_move_thing(player, pos, 1, pos, 1);
        let pkts = drain(&mut rx);
        assert!(pkts.is_empty(), "from==to must produce no packet");
    }

    #[test]
    fn do_move_thing_non_moveable_is_rejected_with_status_push() {
        // sid 400 has no FLAG_MOVEABLE → must be rejected.
        // Player at (102,100,7), source deco at (103,100,7) — adjacent (dx=1).
        // stackpos for the item: pre_creature_len for (103,100,7) = 1 (ground),
        // no creatures on that tile → item is at stackpos 1.
        let mut g = Game::new(move_map());
        let (player, mut rx) = add_player(&mut g, Position::new(102, 100, 7));
        drain(&mut rx);

        let from = Position::new(103, 100, 7);
        let to = Position::new(105, 100, 7); // valid dest, 2 tiles away (within range)
        g.do_move_thing(player, from, 1, to, 1);

        let pkts = drain(&mut rx);
        // Must receive a status push (0xB4 MSG_STATUS_SMALL = 21).
        assert!(
            has_op(&pkts, 0xB4),
            "non-moveable rejection must push 0xB4; got {:?}",
            pkts
        );
        let status_pkt = pkts.iter().find(|p| p.first() == Some(&0xB4)).unwrap();
        assert_eq!(
            status_pkt[1], MSG_STATUS_SMALL,
            "must be MSG_STATUS_SMALL (21)"
        );

        // The overlay for the from tile must be absent or unmodified (item still there).
        let overlay_count = g
            .dynamic
            .get(&(103, 100, 7))
            .map(|st| st.server_ids.iter().filter(|&&sid| sid == 400).count())
            .unwrap_or(1); // static map still has the item
        assert!(
            overlay_count >= 1,
            "non-moveable item must remain on source tile"
        );
    }

    #[test]
    fn do_move_thing_out_of_reach_is_rejected() {
        // Player at (100,100,7), source at (103,100,7) — dx=3 > 1 → too far.
        let mut g = Game::new(move_map());
        let (player, mut rx) = add_player(&mut g, Position::new(100, 100, 7));
        drain(&mut rx);

        let from = Position::new(103, 100, 7);
        let to = Position::new(105, 100, 7);
        g.do_move_thing(player, from, 1, to, 1);

        let pkts = drain(&mut rx);
        // Must get a status push (too far away).
        assert!(
            has_op(&pkts, 0xB4),
            "out-of-reach rejection must push 0xB4; got {:?}",
            pkts
        );
        // Source overlay for 103,100,7 must not exist (no mutation attempted).
        assert!(
            !g.dynamic.contains_key(&(103, 100, 7)),
            "overlay must not be materialized for an out-of-reach source"
        );
    }

    #[test]
    fn do_move_thing_full_move_removes_item_from_source() {
        // Move the stone (non-stackable) from (101,100,7) to (105,100,7).
        // Player at (100,100,7) — adjacent to source (dx=1), not ON the source tile
        // so creature stackpos does not interfere with the item's stackpos.
        // pre_creature_len for (101,100,7) = 1 (ground), no creatures on that tile
        // → stone is at item index 1 → wire stackpos 1.
        let mut g = Game::new(move_map());
        let (player, mut rx) = add_player(&mut g, Position::new(100, 100, 7));
        drain(&mut rx);

        let from = Position::new(101, 100, 7);
        let to = Position::new(102, 100, 7); // adjacent valid tile
        g.do_move_thing(player, from, 1, to, 1);

        // Source overlay: stone (sid 200) must no longer be present.
        let src_st = g
            .dynamic
            .get(&(101, 100, 7))
            .expect("source must have been materialized");
        assert!(
            !src_st.server_ids.contains(&200),
            "stone must be gone from source overlay; sids: {:?}",
            src_st.server_ids
        );

        // Destination overlay: stone (sid 200) must be present.
        let dst_st = g
            .dynamic
            .get(&(102, 100, 7))
            .expect("destination must have been materialized");
        assert!(
            dst_st.server_ids.contains(&200),
            "stone must appear on destination overlay; sids: {:?}",
            dst_st.server_ids
        );
        // Stone must be at index pre_creature_len (front of down-items, newest on top).
        // (102,100,7) had [ground(100), coins(300)] → pre_creature_len=1; stone inserts at 1.
        assert_eq!(
            dst_st.server_ids[dst_st.pre_creature_len], 200,
            "stone must be at front of down-items (index pre_creature_len); sids: {:?}",
            dst_st.server_ids
        );

        // Spectator / mover receives a 0x6A add-tile-item for the destination.
        let pkts = drain(&mut rx);
        assert!(
            has_op(&pkts, 0x6A),
            "player must receive 0x6A (add tile item) for destination; pkts: {:?}",
            pkts.iter().map(|p| p.first().copied()).collect::<Vec<_>>()
        );
        // And a removal (0x6C) or update (0x6B) for the source.
        assert!(
            has_op(&pkts, 0x6C) || has_op(&pkts, 0x6B),
            "player must receive 0x6C or 0x6B for source; pkts: {:?}",
            pkts.iter().map(|p| p.first().copied()).collect::<Vec<_>>()
        );
    }

    #[test]
    fn do_move_thing_stackable_split_source_keeps_remainder() {
        // Move 3 of 10 gold coins from (102,100,7).
        // Player at (101,100,7) — adjacent to source (dx=1), not ON it.
        // pre_creature_len for (102,100,7) = 1 (ground), no creatures on it
        // → coins at item index 1 → wire stackpos 1.
        let mut g = Game::new(move_map());
        let (player, mut rx) = add_player(&mut g, Position::new(101, 100, 7));
        drain(&mut rx);

        let from = Position::new(102, 100, 7);
        let to = Position::new(103, 100, 7); // adjacent
        g.do_move_thing(player, from, 1, to, 3);

        // Source must have 7 coins left.
        let src_st = g.dynamic.get(&(102, 100, 7)).expect("source materialized");
        let coin_idx = src_st
            .server_ids
            .iter()
            .position(|&s| s == 300)
            .expect("sid 300 still on source");
        let remaining = src_st.counts[coin_idx].unwrap_or(0);
        assert_eq!(
            remaining, 7,
            "source must retain 7 coins after moving 3; got {remaining}"
        );

        // Destination must have 3 coins.
        let dst_st = g
            .dynamic
            .get(&(103, 100, 7))
            .expect("destination materialized");
        let dst_idx = dst_st
            .server_ids
            .iter()
            .position(|&s| s == 300)
            .expect("sid 300 on destination");
        let moved = dst_st.counts[dst_idx].unwrap_or(0);
        assert_eq!(moved, 3, "destination must have 3 coins; got {moved}");

        // Partial split → source gets 0x6B (update, not 0x6C remove).
        let pkts = drain(&mut rx);
        assert!(
            has_op(&pkts, 0x6B),
            "partial split must produce 0x6B for source; pkts: {:?}",
            pkts.iter().map(|p| p.first().copied()).collect::<Vec<_>>()
        );
    }

    #[test]
    fn do_move_thing_stackable_clamps_to_available() {
        // Attempt to move 20 of 10 coins — must clamp to 10 (no duplication).
        // Player at (101,100,7), coins at (102,100,7).
        let mut g = Game::new(move_map());
        let (player, mut rx) = add_player(&mut g, Position::new(101, 100, 7));
        drain(&mut rx);

        let from = Position::new(102, 100, 7);
        let to = Position::new(103, 100, 7);
        g.do_move_thing(player, from, 1, to, 20); // request 20, only 10 available

        // Source must be fully removed (clamped to 10 = all of them).
        let src_st = g.dynamic.get(&(102, 100, 7)).expect("source materialized");
        assert!(
            !src_st.server_ids.contains(&300),
            "all coins moved → source must no longer have sid 300; sids: {:?}",
            src_st.server_ids
        );

        // Destination must have exactly 10.
        let dst_st = g
            .dynamic
            .get(&(103, 100, 7))
            .expect("destination materialized");
        let dst_idx = dst_st
            .server_ids
            .iter()
            .position(|&s| s == 300)
            .expect("sid 300 on destination");
        let moved = dst_st.counts[dst_idx].unwrap_or(0);
        assert_eq!(
            moved, 10,
            "destination must have exactly 10 coins (clamped); got {moved}"
        );

        drain(&mut rx);
    }

    #[test]
    fn do_move_thing_spectator_receives_tile_update() {
        // A spectator near both tiles must receive the add-tile-item packet.
        // Player at (100,100,7), stone at (101,100,7), spectator also at (100,100,7).
        let mut g = Game::new(move_map());
        let (player, mut rx_player) = add_player(&mut g, Position::new(100, 100, 7));
        let (_spec, mut rx_spec) = add_player(&mut g, Position::new(100, 100, 7));
        drain(&mut rx_player);
        drain(&mut rx_spec);

        let from = Position::new(101, 100, 7);
        let to = Position::new(102, 100, 7);
        g.do_move_thing(player, from, 1, to, 1);

        let spec_pkts = drain(&mut rx_spec);
        assert!(
            has_op(&spec_pkts, 0x6A) || has_op(&spec_pkts, 0x6B) || has_op(&spec_pkts, 0x6C),
            "spectator must receive at least one tile-update packet; got {:?}",
            spec_pkts
                .iter()
                .map(|p| p.first().copied())
                .collect::<Vec<_>>()
        );
    }

    /// Regression: a non-stackable moved onto a tile that already has a down-item must land
    /// at index `pre_creature_len` (front / newest-on-top), and the broadcast 0x6A must use
    /// stackpos `pre_creature_len + creatures` (no creatures → `pre_creature_len`).
    #[test]
    fn do_move_thing_dest_insert_front_of_down_items() {
        // (102,100,7) starts with [ground(100), coins(300)] → pre_creature_len=1.
        // Move stone (sid 200, non-stackable) from (101,100,7) onto (102,100,7).
        // Expected after move: sids = [ground(100), stone(200), coins(300)]
        //   i.e. stone at index 1 = pre_creature_len, coins shift to index 2.
        // Expected broadcast: 0x6A at stackpos 1 (pre_creature_len=1, no creatures).
        let mut g = Game::new(move_map());
        let (player, mut rx) = add_player(&mut g, Position::new(100, 100, 7));
        drain(&mut rx);

        let from = Position::new(101, 100, 7);
        let to = Position::new(102, 100, 7);
        // stone is at stackpos 1 on source (pre_creature_len=1, no creatures, stone at idx 1)
        g.do_move_thing(player, from, 1, to, 1);

        let dst_st = g
            .dynamic
            .get(&(102, 100, 7))
            .expect("destination must have been materialized");
        let pre = dst_st.pre_creature_len;

        // Stone (sid 200) must be at index pre_creature_len (front of down-items).
        assert_eq!(
            dst_st.server_ids[pre], 200,
            "moved stone must be at index pre_creature_len (front/top); sids: {:?}",
            dst_st.server_ids
        );
        // Coins (sid 300) must have shifted to index pre_creature_len + 1.
        assert_eq!(
            dst_st.server_ids[pre + 1],
            300,
            "pre-existing coins must shift to pre_creature_len+1; sids: {:?}",
            dst_st.server_ids
        );

        // Broadcast: the 0x6A add must carry stackpos = pre_creature_len (no creatures on tile).
        // Packet layout: [0x6A, x_lo, x_hi, y_lo, y_hi, z, stackpos, ...]
        let pkts = drain(&mut rx);
        let add_pkt = pkts
            .iter()
            .find(|p| p.first() == Some(&0x6A))
            .expect("must have a 0x6A add-tile-item packet for destination");
        let broadcast_sp = add_pkt[6];
        assert_eq!(
            broadcast_sp, pre as u8,
            "broadcast stackpos must equal pre_creature_len ({pre}); got {broadcast_sp}"
        );
    }

    // -------------------------------------------------------------------------
    // M10.2 equip / unequip routing tests
    // -------------------------------------------------------------------------

    #[test]
    fn equip_ground_item_into_matching_slot() {
        let mut g = Game::new(move_map());
        // Player adjacent to the helmet tile (106,100,7); stand on (105,100,7).
        let (player, mut rx) = add_player(&mut g, Position::new(105, 100, 7));
        drain(&mut rx);
        // Equip helmet (sid 500) from ground stackpos 1 into the head slot (1).
        // pre_creature_len for (106,100,7) = 1 (ground only), no creature on 106
        // → helmet at item index 1 → wire stackpos 1.
        g.do_move_thing(player, Position::new(106, 100, 7), 1, inv_pos(1), 1);
        // Slot 1 now holds the helmet.
        let slot = g.players.get(&player).unwrap().inventory[0];
        assert_eq!(
            slot.map(|it| it.server_id),
            Some(500),
            "helmet must be in head slot"
        );
        // A 0x78 set-inventory packet was pushed to the player.
        assert!(has_op(&drain(&mut rx), 0x78), "equip must push 0x78");
        // The helmet left the ground overlay.
        assert_eq!(
            count_sid_in_overlays(&g, 500),
            0,
            "helmet must leave the ground"
        );
    }

    #[test]
    fn equip_into_wrong_slot_is_rejected() {
        let mut g = Game::new(move_map());
        let (player, mut rx) = add_player(&mut g, Position::new(105, 100, 7));
        drain(&mut rx);
        // Try to put the helmet (head item) into the feet slot (8) → rejected.
        g.do_move_thing(player, Position::new(106, 100, 7), 1, inv_pos(8), 1);
        assert!(
            g.players.get(&player).unwrap().inventory[7].is_none(),
            "feet slot stays empty"
        );
        assert!(
            g.players
                .get(&player)
                .unwrap()
                .inventory
                .iter()
                .all(|s| s.is_none()),
            "nothing equipped"
        );
        let pkts = drain(&mut rx);
        assert!(
            has_op(&pkts, 0xB4),
            "wrong-slot equip must push a 0xB4 status message"
        );
        assert!(!has_op(&pkts, 0x78), "no inventory set on a rejected equip");
    }

    #[test]
    fn unequip_returns_item_to_the_ground() {
        let mut g = Game::new(move_map());
        let (player, mut rx) = add_player(&mut g, Position::new(105, 100, 7));
        drain(&mut rx);
        // Equip first.
        g.do_move_thing(player, Position::new(106, 100, 7), 1, inv_pos(1), 1);
        drain(&mut rx);
        // Unequip back onto the player's own tile (105,100,7) — within throw range.
        g.do_move_thing(player, inv_pos(1), 0, Position::new(105, 100, 7), 1);
        assert!(
            g.players.get(&player).unwrap().inventory[0].is_none(),
            "head slot cleared"
        );
        let pkts = drain(&mut rx);
        assert!(has_op(&pkts, 0x79), "unequip must push 0x79 clear");
        assert_eq!(
            count_sid_in_overlays(&g, 500),
            1,
            "helmet back on the ground"
        );
    }
}
