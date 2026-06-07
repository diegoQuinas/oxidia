//! Encoders for the enter-world login burst (protocol 10.98).
//! Mirrors `reference/tfs/src/protocolgame.cpp` (`sendAddCreature` self path and
//! the AddPlayerStats/AddPlayerSkills/light helpers). Each function returns one
//! packet's payload (opcode + fields); the caller concatenates them.

use crate::message::MessageWriter;

pub const OP_SELF_INFO: u8 = 0x17;
pub const OP_PENDING_STATE: u8 = 0x0A;
pub const OP_ENTER_WORLD: u8 = 0x0F;
pub const OP_STATS: u8 = 0xA0;
pub const OP_SKILLS: u8 = 0xA1;
pub const OP_WORLD_LIGHT: u8 = 0x82;
pub const OP_CREATURE_LIGHT: u8 = 0x8D;
pub const OP_INVENTORY_SET: u8 = 0x78;
pub const OP_INVENTORY_EMPTY: u8 = 0x79;
pub const OP_BASIC_DATA: u8 = 0x9F;
pub const OP_ICONS: u8 = 0xA2;
pub const OP_MAGIC_EFFECT: u8 = 0x83;
pub const OP_EXTENDED: u8 = 0x32;

pub const INVENTORY_SLOTS: u8 = 11;
/// `ICON_PIGEON = 1 << 14` (TFS const.h:343): the protection-zone "dove" badge.
pub const ICON_PIGEON: u16 = 1 << 14;
/// TFS `CONST_ME_TELEPORT = 11` (const.h); wire value = TFS enum − 1.
pub const EFFECT_TELEPORT: u8 = 10;
/// TFS `CONST_ME_DRAWBLOOD = 1` (const.h:12). TFS `sendMagicEffect` sends the
/// effect byte directly (protocolgame.cpp:2326), so the wire value IS the enum
/// value. Wire `0` is treated as "no effect" by the client and renders nothing.
/// (Note: `EFFECT_TELEPORT = 10` works live despite TFS `CONST_ME_TELEPORT = 11`;
/// that anomaly is tracked separately and left as-is.)
pub const EFFECT_DRAWBLOOD: u8 = 1;

/// Variable stats the client renders; the rest are M3 constants baked in.
#[derive(Debug, Clone, Copy)]
pub struct Stats {
    pub health: u16,
    pub max_health: u16,
    pub free_capacity: u32, // oz * 100
    pub total_capacity: u32,
    pub experience: u32,
    pub level: u16,
    pub level_percent: u8,
    pub mana: u16,
    pub max_mana: u16,
    pub magic_level: u8,
    pub soul: u8,
    pub stamina_minutes: u16,
    pub base_speed: u16,
}

/// TFS `addDouble(value, precision)`: `[u8 precision][u32 (value*10^precision)+i32::MAX]`.
#[allow(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    clippy::cast_precision_loss
)]
// Intentional: mirrors TFS addDouble which encodes a float as (val*10^p + i32::MAX) as u32.
fn add_double(w: &mut MessageWriter, value: f64, precision: u8) {
    w.write_u8(precision);
    let encoded = (value * 10f64.powi(precision as i32)) + i32::MAX as f64;
    w.write_u32(encoded as u32);
}

/// `0x17` self-info: player id, beat duration, speed formula, store config.
pub fn self_info(player_id: u32) -> Vec<u8> {
    let mut w = MessageWriter::new();
    w.write_u8(OP_SELF_INFO);
    w.write_u32(player_id);
    w.write_u16(0x0032); // beat duration = 50ms
    add_double(&mut w, 857.36, 3); // speed A
    add_double(&mut w, 261.29, 3); // speed B
    add_double(&mut w, -4795.01, 3); // speed C
    w.write_u8(0); // can report bugs
    w.write_u8(0); // can change pvp framing
    w.write_u8(0); // expert mode
    w.write_u16(0); // store images url (empty string length)
    w.write_u16(25); // premium coin package size
    w.into_bytes()
}

/// `0x0A` pending-state-entered (opcode only).
pub fn pending_state() -> Vec<u8> {
    vec![OP_PENDING_STATE]
}

/// `0x0F` enter-world (opcode only).
pub fn enter_world() -> Vec<u8> {
    vec![OP_ENTER_WORLD]
}

/// `0xA0` player stats. Constant fields use sane M3 defaults.
pub fn stats(s: &Stats) -> Vec<u8> {
    let mut w = MessageWriter::new();
    w.write_u8(OP_STATS);
    w.write_u16(s.health);
    w.write_u16(s.max_health);
    w.write_u32(s.free_capacity);
    w.write_u32(s.total_capacity);
    write_u64(&mut w, u64::from(s.experience));
    w.write_u16(s.level);
    w.write_u8(s.level_percent);
    w.write_u16(100); // base xp gain rate
    w.write_u16(0); // xp voucher
    w.write_u16(0); // low level bonus
    w.write_u16(0); // xp boost
    w.write_u16(100); // stamina multiplier
    w.write_u16(s.mana);
    w.write_u16(s.max_mana);
    w.write_u8(s.magic_level);
    w.write_u8(s.magic_level); // base magic level
    w.write_u8(0); // magic level percent
    w.write_u8(s.soul);
    w.write_u16(s.stamina_minutes);
    w.write_u16(s.base_speed / 2);
    w.write_u16(0); // regeneration ticks
    w.write_u16(0); // offline training time
    w.write_u16(0); // xp boost time
    w.write_u8(0); // xp boost buyable
    w.into_bytes()
}

/// `0xA1` skills. M3 placeholder: all skills level 10, specials zero.
pub fn skills() -> Vec<u8> {
    let mut w = MessageWriter::new();
    w.write_u8(OP_SKILLS);
    for _ in 0..7 {
        // FIST..FISHING
        w.write_u16(10); // level
        w.write_u16(10); // base level
        w.write_u8(0); // percent
    }
    for _ in 0..6 {
        // critical/leech specials
        w.write_u16(0); // value
        w.write_u16(0); // base value
    }
    w.into_bytes()
}

/// `0x82` world light.
pub fn world_light(level: u8, color: u8) -> Vec<u8> {
    vec![OP_WORLD_LIGHT, level, color]
}

/// `0x8D` creature light.
pub fn creature_light(creature_id: u32, level: u8, color: u8) -> Vec<u8> {
    let mut w = MessageWriter::new();
    w.write_u8(OP_CREATURE_LIGHT);
    w.write_u32(creature_id);
    w.write_u8(level);
    w.write_u8(color);
    w.into_bytes()
}

/// `0x79` for every inventory slot 1..=11 (M3 sends all slots empty).
pub fn empty_inventory() -> Vec<u8> {
    let mut w = MessageWriter::new();
    for slot in 1..=INVENTORY_SLOTS {
        w.write_u8(OP_INVENTORY_EMPTY);
        w.write_u8(slot);
    }
    w.into_bytes()
}

/// `0x78` set-inventory-slot: place `item` into equipment `slot` (1..=10).
/// Reuses the tile item wire-form, so equipped stackables (ammo) carry a count
/// byte and animated items a `0xFE` phase byte — exactly like a tile item.
pub fn set_inventory_slot(slot: u8, item: &crate::map_description::WireItem) -> Vec<u8> {
    let mut w = MessageWriter::new();
    w.write_u8(OP_INVENTORY_SET);
    w.write_u8(slot);
    crate::tile_item::write_item(&mut w, item);
    w.into_bytes()
}

/// `0x9F` basic data: not premium, knight-ish vocation, 255 placeholder spell ids.
pub fn basic_data() -> Vec<u8> {
    let mut w = MessageWriter::new();
    w.write_u8(OP_BASIC_DATA);
    w.write_u8(0); // is premium
    w.write_u32(0); // premium ends at
    w.write_u8(1); // vocation client id
    w.write_u16(0x00FF); // known spell count = 255
    for id in 0u8..=0xFE {
        w.write_u8(id);
    }
    w.into_bytes()
}

/// `0xA2` status-icons bitmask. `mask` is the OR of active `ICON_*` bits.
pub fn icons(mask: u16) -> Vec<u8> {
    let mut w = MessageWriter::new();
    w.write_u8(OP_ICONS);
    w.write_u16(mask);
    w.into_bytes()
}

/// `0x83` magic effect at a position (login teleport poof).
pub fn magic_effect(x: u16, y: u16, z: u8, effect: u8) -> Vec<u8> {
    let mut w = MessageWriter::new();
    w.write_u8(OP_MAGIC_EFFECT);
    w.write_u16(x);
    w.write_u16(y);
    w.write_u8(z);
    w.write_u8(effect);
    w.into_bytes()
}

/// `0x32` OTClient extended-opcode init (sent only for OTClient OSes).
pub fn extended_opcode_init() -> Vec<u8> {
    vec![OP_EXTENDED, 0x00, 0x00, 0x00]
}

/// MessageWriter has no write_u64; emit 8 LE bytes manually.
fn write_u64(w: &mut MessageWriter, v: u64) {
    w.write_bytes(&v.to_le_bytes());
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn self_info_layout() {
        let p = self_info(0x0000_2A01);
        assert_eq!(p[0], OP_SELF_INFO);
        assert_eq!(u32::from_le_bytes([p[1], p[2], p[3], p[4]]), 0x0000_2A01);
        assert_eq!(u16::from_le_bytes([p[5], p[6]]), 50);
        assert_eq!(p[7], 3); // precision byte for first speed double
        // total length: 1+4+2 + 3*(1+4) + 1+1+1 + 2 + 2 = 29 (verified vs OTClient 1098 parse)
        assert_eq!(p.len(), 29);
    }

    #[test]
    fn opcode_only_packets() {
        assert_eq!(pending_state(), [OP_PENDING_STATE]);
        assert_eq!(enter_world(), [OP_ENTER_WORLD]);
    }

    #[test]
    fn stats_layout_length_and_fields() {
        let s = Stats {
            health: 150,
            max_health: 150,
            free_capacity: 40000,
            total_capacity: 40000,
            experience: 0,
            level: 1,
            level_percent: 0,
            mana: 0,
            max_mana: 0,
            magic_level: 0,
            soul: 100,
            stamina_minutes: 2520,
            base_speed: 220,
        };
        let p = stats(&s);
        assert_eq!(p[0], OP_STATS);
        assert_eq!(u16::from_le_bytes([p[1], p[2]]), 150); // health
        assert_eq!(u16::from_le_bytes([p[3], p[4]]), 150); // max health
        // opcode + (hp,maxhp u16) + (cap u32 x2) + exp u64 + level u16 + lvl% u8
        // + 5 xp-rate u16 + (mana,maxmana u16) + 3 magic u8 + soul u8 + stamina u16
        // + speed u16 + regen u16 + offline u16 + xpboost u16 + buyable u8
        // = 1 + 4 + 8 + 8 + 3 + 10 + 4 + 3 + 1 + 2 + 2 + 2 + 2 + 2 + 1 = 53
        // (health/mana are u16 at 1098; u32 only from GameDoubleHealth >= 1300)
        assert_eq!(p.len(), 53);
    }

    #[test]
    fn skills_layout_length() {
        let p = skills();
        assert_eq!(p[0], OP_SKILLS);
        // 1 + 7*(2+2+1) + 6*(2+2) = 1 + 35 + 24 = 60
        assert_eq!(p.len(), 60);
    }

    #[test]
    fn lights_and_inventory_and_basic() {
        assert_eq!(world_light(0xFF, 215), [OP_WORLD_LIGHT, 0xFF, 215]);
        assert_eq!(creature_light(7, 0, 0).len(), 1 + 4 + 1 + 1);
        let inv = empty_inventory();
        assert_eq!(inv.len(), 11 * 2);
        assert_eq!(inv[0], OP_INVENTORY_EMPTY);
        assert_eq!(inv[1], 1); // first slot id
        let basic = basic_data();
        assert_eq!(basic[0], OP_BASIC_DATA);
        // 1 + 1 + 4 + 1 + 2 + 255 = 264
        assert_eq!(basic.len(), 264);
        assert_eq!(icons(0), [OP_ICONS, 0, 0]);
        assert_eq!(extended_opcode_init(), [OP_EXTENDED, 0, 0, 0]);
        assert_eq!(magic_effect(1000, 1000, 7, EFFECT_TELEPORT).len(), 1 + 2 + 2 + 1 + 1);
    }

    #[test]
    fn icons_encodes_pigeon_bit_little_endian() {
        // ICON_PIGEON = 1<<14 = 0x4000 -> LE bytes 0x00 0x40.
        assert_eq!(icons(ICON_PIGEON), [OP_ICONS, 0x00, 0x40]);
        assert_eq!(icons(0), [OP_ICONS, 0x00, 0x00]);
    }

    #[test]
    fn drawblood_effect_is_nonzero_and_matches_tfs() {
        // CONST_ME_DRAWBLOOD = 1 (const.h); TFS sends the effect byte directly.
        // Wire value 0 is dropped by the client (no effect), which is the bug.
        assert_eq!(EFFECT_DRAWBLOOD, 1);
        let pkt = magic_effect(100, 100, 7, EFFECT_DRAWBLOOD);
        assert_eq!(*pkt.last().unwrap(), 1, "drawblood effect byte must be 1");
    }

    // -------------------------------------------------------------------------
    // M10.2 set_inventory_slot tests
    // -------------------------------------------------------------------------

    #[test]
    fn set_inventory_slot_layout() {
        use crate::map_description::WireItem;
        // Non-stackable helmet in slot 1: [0x78][slot][client_id LE][0xFF mark]
        let pkt = set_inventory_slot(1, &WireItem { client_id: 0x0C5A, subtype: None, animated: false });
        assert_eq!(pkt[0], OP_INVENTORY_SET);
        assert_eq!(pkt[1], 1);
        assert_eq!(u16::from_le_bytes([pkt[2], pkt[3]]), 0x0C5A);
        assert_eq!(pkt[4], 0xFF);
        assert_eq!(pkt.len(), 5);
        // Stackable ammo in slot 10 carries the count byte after the 0xFF mark.
        let pkt = set_inventory_slot(10, &WireItem { client_id: 0x0BB3, subtype: Some(50), animated: false });
        assert_eq!(pkt[1], 10);
        assert_eq!(*pkt.last().unwrap(), 50);
    }
}
