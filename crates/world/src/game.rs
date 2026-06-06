//! The authoritative game loop. M3 owns only the player registry (assigning ids
//! and spawn positions); the immutable map is shared as an `Arc<StaticMap>`.
//! M4 will move tile mutations behind this actor too.

use std::collections::HashMap;
use std::sync::Arc;

use tokio::sync::{mpsc, oneshot};

use crate::map::StaticMap;
use crate::Position;

/// What the game service needs to build the enter-world burst for a player.
#[derive(Debug, Clone, Copy)]
pub struct PlayerSnapshot {
    pub id: u32,
    pub position: Position,
}

struct PlayerState {
    #[allow(dead_code)]
    name: String,
    #[allow(dead_code)]
    position: Position,
}

enum Command {
    Login { name: String, reply: oneshot::Sender<PlayerSnapshot> },
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
                    players.insert(id, PlayerState { name, position });
                    let _ = reply.send(PlayerSnapshot { id, position });
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
    use formats::otb::{ItemType, ItemsOtb};
    use formats::otbm::{OtbmMap, Town};

    fn empty_map_with_town() -> Arc<StaticMap> {
        let items = ItemsOtb { major_version: 3, minor_version: 57, build_number: 0, items: vec![ItemType { group: 0, flags: 0, server_id: 100, client_id: 4526 }] };
        let map = OtbmMap {
            width: 100, height: 100, major_items: 3, minor_items: 57,
            description: String::new(), spawn_file: None, house_file: None,
            tiles: vec![], towns: vec![Town { id: 1, name: "Thais".into(), x: 95, y: 117, z: 7 }],
            waypoints: vec![],
        };
        Arc::new(StaticMap::from_formats(&map, &items))
    }

    #[tokio::test]
    async fn login_assigns_id_and_temple_position() {
        let world = spawn(empty_map_with_town());
        let snap = world.login("Test Knight".into()).await.unwrap();
        assert_eq!(snap.position, Position::new(95, 117, 7));
        let snap2 = world.login("Test Sorcerer".into()).await.unwrap();
        assert_ne!(snap.id, snap2.id);
    }
}
