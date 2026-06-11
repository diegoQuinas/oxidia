//! Walk-related server->client packets for protocol 10.98.
//! Ports `sendMoveCreature` (0x6D + directional slices), `sendCancelWalk` (0xB5),
//! and `sendCreatureTurn` (0x6B) from `reference/tfs/src/protocolgame.cpp`.

use crate::map_description::{self, PlacedCreature, TileSource};
use crate::message::MessageWriter;

pub const OP_CREATURE_MOVE: u8 = 0x6D;
pub const OP_CANCEL_WALK: u8 = 0xB5;
pub const OP_CREATURE_TURN: u8 = 0x6B;
pub const OP_REMOVE_TILE_THING: u8 = 0x6C;
pub const OP_FLOOR_CHANGE_UP: u8 = 0xBE;
pub const OP_FLOOR_CHANGE_DOWN: u8 = 0xBF;

/// Auto-walk / GoTo (`0x64`) direction byte.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AutoWalkStep {
    East,
    NorthEast,
    North,
    NorthWest,
    West,
    SouthWest,
    South,
    SouthEast,
}

/// Parse a `0x64` auto-walk (GoTo) body from the client.
/// Format: [num_dirs u8][dir_byte × num_dirs].
/// Direction bytes (TFS convention): 1=East, 2=NE, 3=North, 4=NW, 5=West, 6=SW, 7=South, 8=SE.
pub fn parse_auto_walk(body: &[u8]) -> Option<Vec<AutoWalkStep>> {
    let num_dirs = *body.first()? as usize;
    if num_dirs == 0 || body.len() < 1 + num_dirs {
        return None;
    }
    let steps: Vec<AutoWalkStep> = body[1..=num_dirs]
        .iter()
        .filter_map(|&b| match b {
            1 => Some(AutoWalkStep::East),
            2 => Some(AutoWalkStep::NorthEast),
            3 => Some(AutoWalkStep::North),
            4 => Some(AutoWalkStep::NorthWest),
            5 => Some(AutoWalkStep::West),
            6 => Some(AutoWalkStep::SouthWest),
            7 => Some(AutoWalkStep::South),
            8 => Some(AutoWalkStep::SouthEast),
            _ => None,
        })
        .collect();
    if steps.is_empty() { None } else { Some(steps) }
}

/// Direction deltas for AutoWalkStep, mapping to the same (dx, dy) as Direction.
fn auto_walk_delta(step: &AutoWalkStep) -> (i32, i32) {
    match step {
        AutoWalkStep::East => (1, 0),
        AutoWalkStep::NorthEast => (1, -1),
        AutoWalkStep::North => (0, -1),
        AutoWalkStep::NorthWest => (-1, -1),
        AutoWalkStep::West => (-1, 0),
        AutoWalkStep::SouthWest => (-1, 1),
        AutoWalkStep::South => (0, 1),
        AutoWalkStep::SouthEast => (1, 1),
    }
}

/// Apply 0x64 direction steps to a starting position and return the final
/// coordinates `(x, y, z)`. The floor `z` remains unchanged — only x/y are
/// adjusted by the step deltas. Returns `None` if any step would overflow u16.
pub fn auto_walk_destination(
    start: (u16, u16, u8),
    steps: &[AutoWalkStep],
) -> Option<(u16, u16, u8)> {
    let (mut x, mut y, z) = start;
    for step in steps {
        let (dx, dy) = auto_walk_delta(step);
        let nx = i32::from(x) + dx;
        let ny = i32::from(y) + dy;
        if !(0..=i32::from(u16::MAX)).contains(&nx) || !(0..=i32::from(u16::MAX)).contains(&ny) {
            return None;
        }
        x = nx as u16;
        y = ny as u16;
    }
    Some((x, y, z))
}

const SLICE_NORTH: u8 = 0x65;
const SLICE_EAST: u8 = 0x66;
const SLICE_SOUTH: u8 = 0x67;
const SLICE_WEST: u8 = 0x68;

const VIEW_X: i32 = 8; // Map::maxClientViewportX
const VIEW_Y: i32 = 6; // Map::maxClientViewportY
const SLICE_W: i32 = (VIEW_X * 2) + 2; // 18
const SLICE_H: i32 = (VIEW_Y * 2) + 2; // 14

/// `0x6D` creature move, **creature-id form**: `[0x6D][0xFFFF][creatureId u32][newPos]`.
///
/// The client locates the creature via `getCreatureById` (OTClient
/// `getMappedThing`, `x == 0xFFFF` branch) instead of by `(oldPos, stackPos)`.
/// This is deliberate: the server derives a tile's stackpos from `items.otb`
/// (`FLAG_ALWAYSONTOP`), but OTClient re-inserts a moved creature by its `.dat`
/// `getStackPriority`. When those two data sources disagree about whether a tile
/// item sits above or below the creature, the `(oldPos, stackPos)` form points the
/// client at the wrong thing and the move is silently dropped (observed live as
/// "no creature found to move" / "no thing at pos:…,stackpos:2"). The id form is
/// immune to that divergence. TFS itself uses it whenever stackpos >= 10
/// (`protocolgame.cpp:2603`), so it is protocol-legal for every move.
pub fn creature_move(id: u32, new: (u16, u16, u8)) -> Vec<u8> {
    let mut w = MessageWriter::new();
    w.write_u8(OP_CREATURE_MOVE);
    w.write_u16(0xFFFF);
    w.write_u32(id);
    add_position(&mut w, new);
    w.into_bytes()
}

/// `0xB5` cancel walk: snaps the client back and sets its facing.
pub fn cancel_walk(direction: u8) -> Vec<u8> {
    vec![OP_CANCEL_WALK, direction]
}

/// `0x6B` creature turn (`GameServerChangeOnMap` -> `parseTileTransformThing`),
/// **creature-id form**: `[0x6B][0xFFFF][id u32][0x0063][id u32][direction][walkthrough]`.
///
/// Like [`creature_move`], the leading `0xFFFF` makes the client locate the
/// existing creature via `getCreatureById` instead of `(pos, stackpos)`, so a
/// turn on a decorated tile is immune to the same items.otb-vs-`.dat` stackpos
/// divergence. The trailing `0x0063` block is the replacement creature thing the
/// client adds back, carrying the new facing.
pub fn creature_turn(id: u32, direction: u8, walkthrough: u8) -> Vec<u8> {
    let mut w = MessageWriter::new();
    w.write_u8(OP_CREATURE_TURN);
    w.write_u16(0xFFFF);
    w.write_u32(id);
    w.write_u16(0x0063);
    w.write_u32(id);
    w.write_u8(direction);
    w.write_u8(walkthrough); // 0 = normal, 1 = ghost
    w.into_bytes()
}

/// `0x6C` remove, **creature-id form**: `[0x6C][0xFFFF][id u32]`. Used when a
/// creature leaves the client's view (viewport scroll, logout, or the
/// overground->underground boundary). The `0xFFFF` makes the client locate the
/// creature via `getCreatureById` rather than `(pos, stackpos)`, so it removes
/// the right thing even when several creatures co-occupy the tile (stair/height
/// landings) — a position+stackpos remove would be ambiguous there.
pub fn remove_creature_by_id(id: u32) -> Vec<u8> {
    let mut w = MessageWriter::new();
    w.write_u8(OP_REMOVE_TILE_THING);
    w.write_u16(0xFFFF);
    w.write_u32(id);
    w.into_bytes()
}

/// `0xBE` floor-change-up block for the moving player: the newly revealed upper
/// floors plus the west+north out-of-sync correction slices. Port of TFS
/// `MoveUpCreature` (`protocolgame.cpp:3124-3165`).
fn move_up_block<S: TileSource>(
    old: (u16, u16, u8),
    new: (u16, u16, u8),
    src: &S,
    creatures: &[PlacedCreature],
) -> Vec<u8> {
    let mut w = MessageWriter::new();
    w.write_u8(OP_FLOOR_CHANGE_UP);
    let (ox, oy) = (i32::from(old.0), i32::from(old.1));
    let oz = i32::from(old.2);
    let nz = i32::from(new.2);

    if nz == 7 {
        // going to surface: reveal floors 5..0
        let mut skip = -1;
        for i in (0..=5i32).rev() {
            map_description::floor_description(
                &mut w,
                ox - VIEW_X,
                oy - VIEW_Y,
                i,
                8 - i,
                SLICE_W,
                SLICE_H,
                &mut skip,
                src,
                creatures,
            );
        }
        if skip >= 0 {
            w.write_u8(skip as u8);
            w.write_u8(0xFF);
        }
    } else if nz > 7 {
        // still underground, one floor up: reveal floor oz-3
        let mut skip = -1;
        map_description::floor_description(
            &mut w,
            ox - VIEW_X,
            oy - VIEW_Y,
            oz - 3,
            3,
            SLICE_W,
            SLICE_H,
            &mut skip,
            src,
            creatures,
        );
        if skip >= 0 {
            w.write_u8(skip as u8);
            w.write_u8(0xFF);
        }
    }

    // west then north correction slices (anchored per TFS 3159-3164).
    w.write_bytes(&map_description::encode_slice(
        SLICE_WEST,
        ox - VIEW_X,
        oy - (VIEW_Y - 1),
        nz,
        1,
        SLICE_H,
        src,
        creatures,
    ));
    w.write_bytes(&map_description::encode_slice(
        SLICE_NORTH,
        ox - VIEW_X,
        oy - VIEW_Y,
        nz,
        SLICE_W,
        1,
        src,
        creatures,
    ));
    w.into_bytes()
}

/// `0xBF` floor-change-down block. Port of TFS `MoveDownCreature`
/// (`protocolgame.cpp:3167-3207`).
fn move_down_block<S: TileSource>(
    old: (u16, u16, u8),
    new: (u16, u16, u8),
    src: &S,
    creatures: &[PlacedCreature],
) -> Vec<u8> {
    let mut w = MessageWriter::new();
    w.write_u8(OP_FLOOR_CHANGE_DOWN);
    let (ox, oy) = (i32::from(old.0), i32::from(old.1));
    let oz = i32::from(old.2);
    let nz = i32::from(new.2);

    if nz == 8 {
        // surface -> underground: reveal floors 8,9,10
        let mut skip = -1;
        for i in 0..3i32 {
            map_description::floor_description(
                &mut w,
                ox - VIEW_X,
                oy - VIEW_Y,
                nz + i,
                -i - 1,
                SLICE_W,
                SLICE_H,
                &mut skip,
                src,
                creatures,
            );
        }
        if skip >= 0 {
            w.write_u8(skip as u8);
            w.write_u8(0xFF);
        }
    } else if nz > oz && nz > 8 && nz < 14 {
        // deeper underground: reveal floor nz+2
        let mut skip = -1;
        map_description::floor_description(
            &mut w,
            ox - VIEW_X,
            oy - VIEW_Y,
            nz + 2,
            -3,
            SLICE_W,
            SLICE_H,
            &mut skip,
            src,
            creatures,
        );
        if skip >= 0 {
            w.write_u8(skip as u8);
            w.write_u8(0xFF);
        }
    }

    // east then south correction slices (anchored per TFS 3201-3206).
    w.write_bytes(&map_description::encode_slice(
        SLICE_EAST,
        ox + (VIEW_X + 1),
        oy - (VIEW_Y + 1),
        nz,
        1,
        SLICE_H,
        src,
        creatures,
    ));
    w.write_bytes(&map_description::encode_slice(
        SLICE_SOUTH,
        ox - VIEW_X,
        oy + (VIEW_Y + 1),
        nz,
        SLICE_W,
        1,
        src,
        creatures,
    ));
    w.into_bytes()
}

/// Assemble the moving player's own floor/step update. Same-floor steps emit the
/// `0x6D` move + directional slices (M4 behavior). Floor changes additionally
/// emit the `0xBE`/`0xBF` revealed-floor block, and the overground->underground
/// boundary swaps the `0x6D` header for the id-form remove. Port of the
/// `creature == player` branch of TFS `sendMoveCreature` (`protocolgame.cpp:2590-2631`).
pub fn walk_update<S: TileSource>(
    id: u32,
    old: (u16, u16, u8),
    new: (u16, u16, u8),
    src: &S,
    creatures: &[PlacedCreature],
) -> Vec<u8> {
    let (ox, oy) = (i32::from(old.0), i32::from(old.1));
    let (nx, ny) = (i32::from(new.0), i32::from(new.1));
    let (oz, nz) = (i32::from(old.2), i32::from(new.2));

    // Stair/ladder jumps that change floor AND leap more than one tile
    // horizontally cannot be represented by OTClient's incremental floor-change
    // handling: `parseFloorChangeDown`/`Up` derive the new view-center from a
    // fixed projection shift (`-1,-1` / `+1,+1`) plus the single ±1 correction
    // slice — never from the packet. A sloped stair that lands +2 tiles away (the
    // lower tile's NORTH/SOUTH_ALT redirect in `resolve_floor_change`) leaves the
    // client's central position offset from the player, so every later step
    // anchors its slices on the wrong center and the map tears (and eventually
    // cleanTile detaches the player -> "unable to remove creature"). Send a
    // teleport instead: a clean id-form remove + a full map description, which
    // carries the landing position explicitly (`parseMapDescription` sets the
    // central position from the 0x64 header). Mirrors TFS sendMoveCreature's
    // `teleport` branch (protocolgame.cpp:2591-2593). `creatures` already carries
    // the mover at `new`, so the full map re-adds it on its landing tile.
    if nz != oz && ((nx - ox).abs() > 1 || (ny - oy).abs() > 1) {
        let mut out = remove_creature_by_id(id);
        out.extend(map_description::encode(
            map_description::Center {
                x: new.0,
                y: new.1,
                z: new.2,
            },
            src,
            creatures,
        ));
        return out;
    }

    // Header: id-form remove at the surface->underground boundary, else 0x6D move.
    let mut out = if oz == 7 && nz >= 8 {
        remove_creature_by_id(id)
    } else {
        creature_move(id, new)
    };

    // Floor-change revealed-floor block.
    if nz > oz {
        out.extend(move_down_block(old, new, src, creatures));
    } else if nz < oz {
        out.extend(move_up_block(old, new, src, creatures));
    }

    // Directional slices for the x/y component (TFS 2616-2630), at floor nz.
    if oy > ny {
        out.extend(map_description::encode_slice(
            SLICE_NORTH,
            ox - VIEW_X,
            ny - VIEW_Y,
            nz,
            SLICE_W,
            1,
            src,
            creatures,
        ));
    } else if oy < ny {
        out.extend(map_description::encode_slice(
            SLICE_SOUTH,
            ox - VIEW_X,
            ny + (VIEW_Y + 1),
            nz,
            SLICE_W,
            1,
            src,
            creatures,
        ));
    }
    if ox < nx {
        out.extend(map_description::encode_slice(
            SLICE_EAST,
            nx + (VIEW_X + 1),
            ny - VIEW_Y,
            nz,
            1,
            SLICE_H,
            src,
            creatures,
        ));
    } else if ox > nx {
        out.extend(map_description::encode_slice(
            SLICE_WEST,
            nx - VIEW_X,
            ny - VIEW_Y,
            nz,
            1,
            SLICE_H,
            src,
            creatures,
        ));
    }

    // Slice geometry trace: the floor band (z-2..=z+2 underground) and which
    // directional stripes were emitted. Correlate with the world-side "move ok"
    // log to see if the wire slices match the intended step.
    if tracing::enabled!(tracing::Level::DEBUG) {
        let (band_lo, band_hi) = if nz > 7 {
            (nz - 2, (nz + 2).min(15))
        } else {
            (0, 7)
        };
        tracing::debug!(
            id, old = ?old, new = ?new,
            floor_change = nz != oz, underground = nz > 7,
            band_lo, band_hi,
            slice_n = oy > ny, slice_s = oy < ny,
            slice_e = ox < nx, slice_w = ox > nx,
            pkt_len = out.len(),
            "walk_update slices"
        );
    }
    out
}

fn add_position(w: &mut MessageWriter, p: (u16, u16, u8)) {
    w.write_u16(p.0);
    w.write_u16(p.1);
    w.write_u8(p.2);
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    use crate::map_description::{TileSlices, WireItem};

    // -------------------------------------------------------------------------
    // auto_walk_destination tests (TDD: RED first)
    // -------------------------------------------------------------------------

    #[test]
    fn auto_walk_destination_returns_correct_target_for_known_steps() {
        // GIVEN start at (100, 100, 7) and steps [East, East, South, South]
        let steps = vec![
            AutoWalkStep::East,
            AutoWalkStep::East,
            AutoWalkStep::South,
            AutoWalkStep::South,
        ];
        // WHEN computing destination
        let dest = auto_walk_destination((100, 100, 7), &steps);
        // THEN position should be (102, 102, 7)
        assert_eq!(dest, Some((102, 102, 7)));
    }

    #[test]
    fn auto_walk_destination_overflow_returns_none() {
        // GIVEN start at (0, 0, 7) and a step West (dx=-1)
        // WHEN computing destination
        let dest = auto_walk_destination((0, 0, 7), &[AutoWalkStep::West]);
        // THEN overflow yields None
        assert_eq!(dest, None);
    }

    #[test]
    fn auto_walk_destination_overflow_south_returns_none() {
        // GIVEN start at (100, u16::MAX, 7) and a step South (dy=+1)
        let dest = auto_walk_destination((100, u16::MAX, 7), &[AutoWalkStep::South]);
        // THEN overflow yields None
        assert_eq!(dest, None);
    }

    #[test]
    fn auto_walk_destination_single_step_works() {
        // GIVEN a single North step
        let dest = auto_walk_destination((100, 100, 7), &[AutoWalkStep::North]);
        // THEN y decreases by one
        assert_eq!(dest, Some((100, 99, 7)));
    }

    #[test]
    fn auto_walk_destination_northwest_moves_both_axes() {
        // GIVEN a single NorthWest step
        let dest = auto_walk_destination((100, 100, 7), &[AutoWalkStep::NorthWest]);
        // THEN both x and y change
        assert_eq!(dest, Some((99, 99, 7)));
    }

    #[test]
    fn auto_walk_destination_empty_steps_returns_start() {
        // GIVEN zero steps
        let dest = auto_walk_destination((100, 100, 7), &[]);
        // THEN position unchanged
        assert_eq!(dest, Some((100, 100, 7)));
    }

    #[test]
    fn auto_walk_destination_diagonal_chain() {
        // GIVEN steps [NorthEast, SouthEast, SouthWest, NorthWest] — a full loop
        let steps = vec![
            AutoWalkStep::NorthEast,
            AutoWalkStep::SouthEast,
            AutoWalkStep::SouthWest,
            AutoWalkStep::NorthWest,
        ];
        let dest = auto_walk_destination((50, 50, 7), &steps);
        // THEN the loop should return to start
        assert_eq!(dest, Some((50, 50, 7)));
    }

    struct MapStub(HashMap<(i32, i32, i32), WireItem>);
    impl TileSource for MapStub {
        fn tile(&self, x: i32, y: i32, z: i32) -> Option<TileSlices<'_>> {
            self.0.get(&(x, y, z)).map(|item| TileSlices {
                pre_creature: std::slice::from_ref(item),
                post_creature: &[],
            })
        }
        fn creature_stackpos(&self, _x: i32, _y: i32, _z: i32) -> u8 {
            1
        }
    }

    #[test]
    fn creature_move_uses_id_form() {
        let p = creature_move(0x1000_0000, (101, 100, 7));
        assert_eq!(p[0], OP_CREATURE_MOVE);
        assert_eq!(u16::from_le_bytes([p[1], p[2]]), 0xFFFF); // id-form marker
        assert_eq!(u32::from_le_bytes([p[3], p[4], p[5], p[6]]), 0x1000_0000); // creature id
        assert_eq!(u16::from_le_bytes([p[7], p[8]]), 101); // new x
        assert_eq!(u16::from_le_bytes([p[9], p[10]]), 100); // new y
        assert_eq!(p[11], 7); // new z
        assert_eq!(p.len(), 12);
    }

    #[test]
    fn cancel_walk_layout() {
        assert_eq!(cancel_walk(3), [OP_CANCEL_WALK, 3]);
    }

    #[test]
    fn creature_turn_uses_id_form() {
        let p = creature_turn(0x1000_0000, 1, 0);
        assert_eq!(p[0], OP_CREATURE_TURN);
        assert_eq!(u16::from_le_bytes([p[1], p[2]]), 0xFFFF); // id-form marker
        assert_eq!(u32::from_le_bytes([p[3], p[4], p[5], p[6]]), 0x1000_0000); // lookup id
        assert_eq!(u16::from_le_bytes([p[7], p[8]]), 0x0063); // replacement thing marker
        assert_eq!(u32::from_le_bytes([p[9], p[10], p[11], p[12]]), 0x1000_0000); // creature id
        assert_eq!(p[13], 1); // direction
        assert_eq!(p[14], 0); // walkthrough
        assert_eq!(p.len(), 15);
    }

    #[test]
    fn creature_turn_ghost_walkthrough_byte() {
        let p = creature_turn(0x1000_0000, 2, 1);
        assert_eq!(p[14], 1, "walkthrough byte must be 1 for ghost");
    }

    #[test]
    fn east_step_emits_move_then_east_slice() {
        let stub = MapStub(HashMap::new());
        let out = walk_update(0x1000_0000, (100, 100, 7), (101, 100, 7), &stub, &[]);
        assert_eq!(out[0], OP_CREATURE_MOVE);
        assert_eq!(out[12], SLICE_EAST);
    }

    #[test]
    fn northeast_step_emits_both_slices() {
        let stub = MapStub(HashMap::new());
        let out = walk_update(0x1000_0000, (100, 100, 7), (101, 99, 7), &stub, &[]);
        assert_eq!(out[0], OP_CREATURE_MOVE);
        assert!(out.contains(&SLICE_NORTH));
        assert!(out.contains(&SLICE_EAST));
    }

    #[test]
    fn down_step_emits_remove_then_bf_then_slices() {
        let stub = MapStub(HashMap::new());
        // overground -> underground boundary: 7 -> 8, stepping south+down.
        let out = walk_update(0x1000_0000, (100, 100, 7), (100, 101, 8), &stub, &[]);
        // 7->8 boundary uses the id-form remove (0x6C, 0xFFFF, id), not 0x6D.
        assert_eq!(out[0], OP_REMOVE_TILE_THING);
        assert_eq!(u16::from_le_bytes([out[1], out[2]]), 0xFFFF);
        assert_eq!(
            u32::from_le_bytes([out[3], out[4], out[5], out[6]]),
            0x1000_0000
        );
        // floor-change-down opcode present.
        assert!(out.contains(&OP_FLOOR_CHANGE_DOWN));
        // the down correction slices (east 0x66 + south 0x67) are appended.
        assert!(out.contains(&SLICE_EAST));
        assert!(out.contains(&SLICE_SOUTH));
    }

    #[test]
    fn teleport_like_floor_jump_emits_full_map_with_mover() {
        // A sloped stair/ladder that changes floor AND leaps >1 tile (here dy=2)
        // can't be followed by the client's incremental floor-change handling, so
        // it goes out as a teleport: id-form remove + a full 0x64 map centered on
        // the landing, carrying the mover so it re-attaches to its tile.
        let mut m = HashMap::new();
        m.insert((100, 102, 8), WireItem::plain(4526)); // landing tile must exist
        let stub = MapStub(m);
        let mover = PlacedCreature {
            x: 100,
            y: 102,
            z: 8,
            bytes: vec![0x62, 0x00, 0x01, 0x00, 0x00, 0x10],
        };
        let out = walk_update(
            0x1000_0000,
            (100, 100, 7),
            (100, 102, 8),
            &stub,
            std::slice::from_ref(&mover),
        );
        assert_eq!(out[0], OP_REMOVE_TILE_THING); // id-form remove header
        assert_eq!(u16::from_le_bytes([out[1], out[2]]), 0xFFFF);
        assert_eq!(
            u32::from_le_bytes([out[3], out[4], out[5], out[6]]),
            0x1000_0000
        );
        assert_eq!(out[7], map_description::OPCODE_MAP_DESCRIPTION); // full 0x64 map, not 0xBF
        assert!(
            !out.contains(&OP_FLOOR_CHANGE_DOWN),
            "teleport replaces the incremental floor block"
        );
        assert!(
            out.windows(mover.bytes.len()).any(|w| w == mover.bytes),
            "mover re-added in teleport map"
        );
    }

    #[test]
    fn up_step_underground_emits_move_then_be() {
        let stub = MapStub(HashMap::new());
        // underground up: 9 -> 8, stepping north+up (stays underground).
        let out = walk_update(0x1000_0000, (100, 100, 9), (100, 99, 8), &stub, &[]);
        assert_eq!(out[0], OP_CREATURE_MOVE); // not a boundary, normal 0x6D header
        assert!(out.contains(&OP_FLOOR_CHANGE_UP));
        // up correction slices (west 0x68 + north 0x65).
        assert!(out.contains(&SLICE_WEST));
        assert!(out.contains(&SLICE_NORTH));
    }
}
