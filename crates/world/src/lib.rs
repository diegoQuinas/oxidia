#![forbid(unsafe_code)]

//! Authoritative game state: tile grid, positions, creatures, players.
//!
//! The game runs on a **single** authoritative task that owns all state and
//! processes a command queue. Networking tasks never share game state behind
//! locks — they talk to the loop over channels.
//!
//! M3/M4 fill in the grid, creatures, and the loop.

pub mod combat;
pub mod game;
pub mod map;
pub mod outfit_catalog;
pub mod pathfinding;

// Chunked map loading types (PR 1 — coexist with StaticMap)
pub use map::{CHUNK_DIM, Chunk, ChunkId, ChunkManager, ChunkedMap, WorldMeta};

/// A position in the game world. `z` is the floor (7 = ground level).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub struct Position {
    pub x: u16,
    pub y: u16,
    pub z: u8,
}

impl Position {
    pub const fn new(x: u16, y: u16, z: u8) -> Self {
        Self { x, y, z }
    }

    /// Translate by `(dx, dy)` on the same floor. Returns `None` if it would
    /// leave the `u16` coordinate range.
    pub fn offset(self, dx: i32, dy: i32) -> Option<Position> {
        let x = i32::from(self.x) + dx;
        let y = i32::from(self.y) + dy;
        if (0..=i32::from(u16::MAX)).contains(&x) && (0..=i32::from(u16::MAX)).contains(&y) {
            Some(Position::new(x as u16, y as u16, self.z))
        } else {
            None
        }
    }

    /// Shift floor by `dz` (negative = up toward the surface). `None` if it
    /// leaves the `u8` floor range.
    pub fn offset_z(self, dz: i32) -> Option<Position> {
        let z = i32::from(self.z) + dz;
        if (0..=i32::from(u8::MAX)).contains(&z) {
            Some(Position::new(self.x, self.y, z as u8))
        } else {
            None
        }
    }
}

/// A facing/movement direction. Wire bytes match TFS `Direction` (`position.h`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Direction {
    North,
    East,
    South,
    West,
    NorthEast,
    SouthEast,
    SouthWest,
    NorthWest,
}

impl Direction {
    /// The protocol byte the client expects (N=0, E=1, S=2, W=3, SW=4, SE=5, NW=6, NE=7).
    pub const fn to_byte(self) -> u8 {
        match self {
            Direction::North => 0,
            Direction::East => 1,
            Direction::South => 2,
            Direction::West => 3,
            Direction::SouthWest => 4,
            Direction::SouthEast => 5,
            Direction::NorthWest => 6,
            Direction::NorthEast => 7,
        }
    }

    /// `(dx, dy)` step on the tile grid for this direction.
    pub const fn delta(self) -> (i32, i32) {
        match self {
            Direction::North => (0, -1),
            Direction::East => (1, 0),
            Direction::South => (0, 1),
            Direction::West => (-1, 0),
            Direction::NorthEast => (1, -1),
            Direction::SouthEast => (1, 1),
            Direction::SouthWest => (-1, 1),
            Direction::NorthWest => (-1, -1),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn direction_wire_bytes_match_tfs() {
        assert_eq!(Direction::North.to_byte(), 0);
        assert_eq!(Direction::East.to_byte(), 1);
        assert_eq!(Direction::South.to_byte(), 2);
        assert_eq!(Direction::West.to_byte(), 3);
        assert_eq!(Direction::SouthWest.to_byte(), 4);
        assert_eq!(Direction::SouthEast.to_byte(), 5);
        assert_eq!(Direction::NorthWest.to_byte(), 6);
        assert_eq!(Direction::NorthEast.to_byte(), 7);
    }

    #[test]
    fn direction_deltas() {
        assert_eq!(Direction::North.delta(), (0, -1));
        assert_eq!(Direction::East.delta(), (1, 0));
        assert_eq!(Direction::South.delta(), (0, 1));
        assert_eq!(Direction::West.delta(), (-1, 0));
        assert_eq!(Direction::NorthEast.delta(), (1, -1));
        assert_eq!(Direction::SouthEast.delta(), (1, 1));
        assert_eq!(Direction::SouthWest.delta(), (-1, 1));
        assert_eq!(Direction::NorthWest.delta(), (-1, -1));
    }

    #[test]
    fn position_offset_stays_in_bounds() {
        let p = Position::new(100, 100, 7);
        assert_eq!(p.offset(1, -1), Some(Position::new(101, 99, 7)));
        assert_eq!(Position::new(0, 0, 7).offset(-1, 0), None);
        assert_eq!(Position::new(u16::MAX, 0, 7).offset(1, 0), None);
    }

    #[test]
    fn position_serde_round_trip() {
        let pos = Position::new(123, 456, 7);
        let bytes = bincode::serialize(&pos).expect("serialize");
        let back: Position = bincode::deserialize(&bytes).expect("deserialize");
        assert_eq!(pos, back);
    }
}
