//! The authoritative game loop. M3 owns only the player registry (assigning ids
//! and spawn positions); the immutable map is shared as an `Arc<StaticMap>`.
//! M4 will move tile mutations behind this actor too.

use std::collections::HashMap;
use std::sync::Arc;

use tokio::sync::{mpsc, oneshot};

use crate::map::StaticMap;
use crate::{Direction, Position};

/// What the game service needs to build the enter-world burst for a player.
#[derive(Debug, Clone, Copy)]
pub struct PlayerSnapshot {
    pub id: u32,
    pub position: Position,
    pub direction: Direction,
}

struct PlayerState {
    #[allow(dead_code)]
    name: String,
    position: Position,
    direction: Direction,
}

/// Result of a move attempt: whether the player moved, plus the resulting facing.
#[derive(Debug, Clone, Copy)]
pub struct MoveResult {
    pub outcome: MoveOutcome,
    pub facing: Direction,
}

#[derive(Debug, Clone, Copy)]
pub enum MoveOutcome {
    Moved { from: Position, to: Position },
    Blocked,
}

enum Command {
    Login { name: String, reply: oneshot::Sender<PlayerSnapshot> },
    Move { id: u32, direction: Direction, reply: oneshot::Sender<Option<MoveResult>> },
    Turn { id: u32, direction: Direction, reply: oneshot::Sender<Option<Direction>> },
}

/// Cloneable handle to the running world.
#[derive(Clone)]
pub struct WorldHandle {
    tx: mpsc::Sender<Command>,
    pub map: Arc<StaticMap>,
}

impl WorldHandle {
    /// Register a player by character name; returns its id + spawn position.
    pub async fn login(&self, name: String) -> Option<PlayerSnapshot> {
        let (reply, rx) = oneshot::channel();
        self.tx.send(Command::Login { name, reply }).await.ok()?;
        rx.await.ok()
    }

    /// Attempt a one-tile step. `None` if the player id is unknown.
    pub async fn move_player(&self, id: u32, direction: Direction) -> Option<MoveResult> {
        let (reply, rx) = oneshot::channel();
        self.tx.send(Command::Move { id, direction, reply }).await.ok()?;
        rx.await.ok().flatten()
    }

    /// Turn in place. Returns the new facing, or `None` if the id is unknown.
    pub async fn turn_player(&self, id: u32, direction: Direction) -> Option<Direction> {
        let (reply, rx) = oneshot::channel();
        self.tx.send(Command::Turn { id, direction, reply }).await.ok()?;
        rx.await.ok().flatten()
    }
}

/// Spawn the world actor task and return a handle.
pub fn spawn(map: Arc<StaticMap>) -> WorldHandle {
    let (tx, mut rx) = mpsc::channel::<Command>(64);
    let handle = WorldHandle { tx, map: Arc::clone(&map) };
    tokio::spawn(async move {
        let mut players: HashMap<u32, PlayerState> = HashMap::new();
        let mut next_id: u32 = 0x1000_0000; // creature id range for players
        while let Some(cmd) = rx.recv().await {
            match cmd {
                Command::Login { name, reply } => {
                    let id = next_id;
                    next_id += 1;
                    let position = map.spawn();
                    let direction = Direction::South;
                    players.insert(id, PlayerState { name, position, direction });
                    let _ = reply.send(PlayerSnapshot { id, position, direction });
                }
                Command::Move { id, direction, reply } => {
                    let result = players.get_mut(&id).map(|p| {
                        p.direction = direction;
                        let (dx, dy) = direction.delta();
                        let outcome = match p.position.offset(dx, dy) {
                            Some(dest) if map.is_walkable(dest) => {
                                let from = p.position;
                                p.position = dest;
                                MoveOutcome::Moved { from, to: dest }
                            }
                            _ => MoveOutcome::Blocked,
                        };
                        MoveResult { outcome, facing: direction }
                    });
                    let _ = reply.send(result);
                }
                Command::Turn { id, direction, reply } => {
                    let facing = players.get_mut(&id).map(|p| {
                        p.direction = direction;
                        direction
                    });
                    let _ = reply.send(facing);
                }
            }
        }
    });
    handle
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::map::StaticMap;
    use crate::Direction;
    use formats::otb::{ItemType, ItemsOtb};
    use formats::otbm::{MapItem, MapTile, OtbmMap, Town};

    fn walk_map() -> Arc<StaticMap> {
        let items = ItemsOtb {
            major_version: 3, minor_version: 57, build_number: 0,
            items: vec![
                ItemType { group: 0, flags: 0, server_id: 100, client_id: 4526 },
                ItemType { group: 0, flags: 0x0000_0001, server_id: 200, client_id: 1059 },
            ],
        };
        let ground = |x, y| MapTile { x, y, z: 7, flags: 0, house_id: None,
            items: vec![MapItem { id: 100, contents: vec![] }] };
        let map = OtbmMap {
            width: 200, height: 200, major_items: 3, minor_items: 57,
            description: String::new(), spawn_file: None, house_file: None,
            tiles: vec![
                ground(95, 117), ground(96, 117), ground(95, 116),
                // wall to the west of spawn
                MapTile { x: 94, y: 117, z: 7, flags: 0, house_id: None,
                    items: vec![MapItem { id: 100, contents: vec![] }, MapItem { id: 200, contents: vec![] }] },
            ],
            towns: vec![Town { id: 1, name: "Thais".into(), x: 95, y: 117, z: 7 }],
            waypoints: vec![],
        };
        Arc::new(StaticMap::from_formats(&map, &items))
    }

    #[tokio::test]
    async fn login_assigns_id_and_temple_position() {
        let world = spawn(walk_map());
        let snap = world.login("Test Knight".into()).await.unwrap();
        assert_eq!(snap.position, Position::new(95, 117, 7));
        assert_eq!(snap.direction, Direction::South);
        let snap2 = world.login("Test Sorcerer".into()).await.unwrap();
        assert_ne!(snap.id, snap2.id);
    }

    #[tokio::test]
    async fn move_onto_walkable_tile_updates_position_and_facing() {
        let world = spawn(walk_map());
        let snap = world.login("Test Knight".into()).await.unwrap();
        let res = world.move_player(snap.id, Direction::East).await.unwrap();
        assert_eq!(res.facing, Direction::East);
        match res.outcome {
            MoveOutcome::Moved { from, to } => {
                assert_eq!(from, Position::new(95, 117, 7));
                assert_eq!(to, Position::new(96, 117, 7));
            }
            MoveOutcome::Blocked => panic!("expected Moved"),
        }
    }

    #[tokio::test]
    async fn move_into_wall_is_blocked_but_turns() {
        let world = spawn(walk_map());
        let snap = world.login("Test Knight".into()).await.unwrap();
        let res = world.move_player(snap.id, Direction::West).await.unwrap();
        assert_eq!(res.facing, Direction::West, "still faces the wall");
        assert!(matches!(res.outcome, MoveOutcome::Blocked));
        let res2 = world.move_player(snap.id, Direction::East).await.unwrap();
        assert!(matches!(res2.outcome, MoveOutcome::Moved { .. }));
    }

    #[tokio::test]
    async fn turn_updates_facing_only() {
        let world = spawn(walk_map());
        let snap = world.login("Test Knight".into()).await.unwrap();
        assert_eq!(snap.direction, Direction::South);
        let facing = world.turn_player(snap.id, Direction::North).await.unwrap();
        assert_eq!(facing, Direction::North);
    }
}
