//! `AddCreature` / `AddOutfit` serialization for protocol 10.98.
//! Byte-faithful port of `reference/tfs/src/protocolgame.cpp` (`AddCreature`
//! 2935-3005, `AddOutfit` 3066-3081). A creature is written as a "thing" inside
//! a tile description, after the tile's items.

use crate::message::MessageWriter;

pub const CREATURETYPE_PLAYER: u8 = 0;
pub const CREATURETYPE_MONSTER: u8 = 1;
pub const MARK_UNMARKED: u8 = 0xFF;

/// A creature look. `look_type == 0` means a non-creature look (an item id).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Outfit {
    pub look_type: u16,
    pub head: u8,
    pub body: u8,
    pub legs: u8,
    pub feet: u8,
    pub addons: u8,
    pub mount: u16,
}

/// The subset of creature fields that vary in M4. Everything else is a constant
/// matching a plain, friendly player creature.
#[derive(Debug, Clone, Copy)]
pub struct CreatureView<'a> {
    pub id: u32,
    pub name: &'a [u8],
    pub health_percent: u8,
    pub direction: u8,
    pub outfit: Outfit,
    pub light_level: u8,
    pub light_color: u8,
    pub speed: u16,
    pub creature_type: u8,
    /// Walkthrough byte: 0 = normal, 1 = ghost (GM ghost mode).
    pub walkthrough: u8,
}

/// `AddOutfit` (protocolgame.cpp:3066). `lookType` then either 5 color/addon
/// bytes (creature look) or a `u16` item id (item look), then the mount `u16`.
pub fn add_outfit(w: &mut MessageWriter, o: &Outfit) {
    w.write_u16(o.look_type);
    if o.look_type != 0 {
        w.write_u8(o.head);
        w.write_u8(o.body);
        w.write_u8(o.legs);
        w.write_u8(o.feet);
        w.write_u8(o.addons);
    } else {
        w.write_u16(0); // lookTypeEx (no item look in M4)
    }
    w.write_u16(o.mount);
}

/// `AddCreature` (protocolgame.cpp:2935). `known` selects the `0x0062` short
/// form; otherwise the `0x0061` form carries `remove_id`, type, and name, plus a
/// guild-emblem byte. Returns the creature "thing" bytes to splice into a tile.
pub fn add_creature(view: &CreatureView, known: bool, remove_id: u32) -> Vec<u8> {
    let mut w = MessageWriter::new();
    if known {
        w.write_u16(0x0062);
        w.write_u32(view.id);
    } else {
        w.write_u16(0x0061);
        w.write_u32(remove_id);
        w.write_u32(view.id);
        w.write_u8(view.creature_type);
        w.write_string(view.name);
    }
    w.write_u8(view.health_percent);
    w.write_u8(view.direction);
    add_outfit(&mut w, &view.outfit);
    w.write_u8(view.light_level);
    w.write_u8(view.light_color);
    w.write_u16(view.speed / 2);
    w.write_u8(0); // skull
    w.write_u8(0); // party shield
    if !known {
        w.write_u8(0); // guild emblem (unknown path only)
    }
    w.write_u8(view.creature_type); // creatureType (re-emitted)
    w.write_u8(0); // speech bubble
    w.write_u8(MARK_UNMARKED);
    w.write_u16(0); // helpers
    w.write_u8(view.walkthrough); // walkthrough
    w.into_bytes()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn knight() -> Outfit {
        Outfit {
            look_type: 128,
            head: 78,
            body: 69,
            legs: 58,
            feet: 76,
            addons: 0,
            mount: 0,
        }
    }

    fn view() -> CreatureView<'static> {
        CreatureView {
            id: 0x1000_0000,
            name: b"Test Knight",
            health_percent: 100,
            direction: 2, // South
            outfit: knight(),
            light_level: 0,
            light_color: 0,
            speed: 220,
            creature_type: CREATURETYPE_PLAYER,
            walkthrough: 0,
        }
    }

    #[test]
    fn outfit_with_looktype_is_nine_bytes() {
        let mut w = MessageWriter::new();
        add_outfit(&mut w, &knight());
        assert_eq!(w.as_bytes().len(), 9);
        assert_eq!(u16::from_le_bytes([w.as_bytes()[0], w.as_bytes()[1]]), 128);
    }

    #[test]
    fn outfit_with_item_looktype_is_six_bytes() {
        let mut w = MessageWriter::new();
        add_outfit(
            &mut w,
            &Outfit {
                look_type: 0,
                head: 0,
                body: 0,
                legs: 0,
                feet: 0,
                addons: 0,
                mount: 0,
            },
        );
        assert_eq!(w.as_bytes().len(), 6);
    }

    #[test]
    fn unknown_creature_field_order() {
        let bytes = add_creature(&view(), false, 0);
        let mut p = 0usize;
        assert_eq!(u16::from_le_bytes([bytes[p], bytes[p + 1]]), 0x0061);
        p += 2;
        assert_eq!(
            u32::from_le_bytes([bytes[p], bytes[p + 1], bytes[p + 2], bytes[p + 3]]),
            0
        );
        p += 4; // removeId
        assert_eq!(
            u32::from_le_bytes([bytes[p], bytes[p + 1], bytes[p + 2], bytes[p + 3]]),
            0x1000_0000
        );
        p += 4; // id
        assert_eq!(bytes[p], 0);
        p += 1; // creatureType = player
        let name_len = u16::from_le_bytes([bytes[p], bytes[p + 1]]) as usize;
        p += 2;
        assert_eq!(&bytes[p..p + name_len], b"Test Knight");
        p += name_len;
        assert_eq!(bytes[p], 100);
        p += 1; // health%
        assert_eq!(bytes[p], 2);
        p += 1; // direction South
        assert_eq!(u16::from_le_bytes([bytes[p], bytes[p + 1]]), 128);
        p += 9; // outfit (9 bytes)
        assert_eq!(bytes[p], 0);
        p += 1; // light level
        assert_eq!(bytes[p], 0);
        p += 1; // light color
        assert_eq!(u16::from_le_bytes([bytes[p], bytes[p + 1]]), 110);
        p += 2; // speed / 2
        assert_eq!(bytes[p], 0);
        p += 1; // skull
        assert_eq!(bytes[p], 0);
        p += 1; // party shield
        assert_eq!(bytes[p], 0);
        p += 1; // guild emblem (unknown only)
        assert_eq!(bytes[p], 0);
        p += 1; // creatureType2
        assert_eq!(bytes[p], 0);
        p += 1; // speech bubble
        assert_eq!(bytes[p], 0xFF);
        p += 1; // mark
        assert_eq!(u16::from_le_bytes([bytes[p], bytes[p + 1]]), 0);
        p += 2; // helpers
        assert_eq!(bytes[p], 0);
        p += 1; // walkthrough
        assert_eq!(p, bytes.len(), "no trailing bytes");
    }

    fn monster_view() -> CreatureView<'static> {
        CreatureView {
            id: 0x4000_0001,
            name: b"Rat",
            health_percent: 50,
            direction: 0,
            outfit: Outfit {
                look_type: 100,
                head: 0,
                body: 0,
                legs: 0,
                feet: 0,
                addons: 0,
                mount: 0,
            },
            light_level: 0,
            light_color: 0,
            speed: 200,
            creature_type: CREATURETYPE_MONSTER,
            walkthrough: 0,
        }
    }

    #[test]
    fn unknown_monster_uses_creaturetype_monster_in_both_positions() {
        let bytes = add_creature(&monster_view(), false, 0);
        // First creatureType is right after id (+10 = 0x61 | 2, removeId 4, id 4).
        assert_eq!(
            bytes[10], CREATURETYPE_MONSTER,
            "first creatureType byte must be MONSTER"
        );
        // Compute the second creatureType position: after guild emblem.
        // First pass: find guild emblem (it's the 0 before the second creatureType).
        // Actually, compute: first creatuteType(1) + name_len(2) + "Rat"(3) + health(dir,4) + outfit(9) + light(2) + speed(2) + skull(1) + party(1) + guild(1)
        // = position 10 + 1 + 2 + 3 + ... wait, let me just search for the second occurrence
        // Second creatureType is at 34 for "Rat" (10 + 1 + 2+3 + 1+1 + 9 + 1+1 + 2 + 1+1 + 1 = 34)
        assert_eq!(
            bytes[34], CREATURETYPE_MONSTER,
            "second creatureType byte must be MONSTER for 'Rat'"
        );
    }

    #[test]
    fn known_creature_is_marker_and_id_only_prefix() {
        let bytes = add_creature(&view(), true, 0);
        assert_eq!(u16::from_le_bytes([bytes[0], bytes[1]]), 0x0062);
        assert_eq!(
            u32::from_le_bytes([bytes[2], bytes[3], bytes[4], bytes[5]]),
            0x1000_0000
        );
        assert_eq!(bytes[6], 100); // health% immediately follows
    }
}
