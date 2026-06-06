#![forbid(unsafe_code)]

//! Authoritative game state: tile grid, positions, creatures, players.
//!
//! The game runs on a **single** authoritative task that owns all state and
//! processes a command queue. Networking tasks never share game state behind
//! locks — they talk to the loop over channels.
//!
//! M3/M4 fill in the grid, creatures, and the loop.

pub mod game;
pub mod map;

/// A position in the game world. `z` is the floor (7 = ground level).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Position {
    pub x: u16,
    pub y: u16,
    pub z: u8,
}

impl Position {
    pub const fn new(x: u16, y: u16, z: u8) -> Self {
        Self { x, y, z }
    }
}
