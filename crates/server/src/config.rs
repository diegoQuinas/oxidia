//! Server configuration loaded from a TOML file.

use std::net::{IpAddr, SocketAddr};
use std::path::Path;

use anyhow::{Context, Result};
use serde::Deserialize;

#[derive(Debug, Clone, Deserialize)]
pub struct Config {
    pub server: ServerConfig,
    pub world: WorldConfig,
    pub network: NetworkConfig,
    pub database: DatabaseConfig,
    pub log: LogConfig,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ServerConfig {
    pub world_name: String,
    pub host: String,
    pub motd: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct WorldConfig {
    /// Path to the `.otbm` map file, relative to the working directory or
    /// absolute. The server refuses to start if it is missing or fails to parse.
    pub map_path: String,
    /// Name of the town whose temple players spawn at. When absent or unknown,
    /// the world falls back to the map's first town.
    #[serde(default)]
    pub spawn_town: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct NetworkConfig {
    pub login_port: u16,
    pub game_port: u16,
    pub bind: IpAddr,
}

#[derive(Debug, Clone, Deserialize)]
pub struct DatabaseConfig {
    pub path: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct LogConfig {
    pub filter: String,
}

impl Config {
    /// Parse a [`Config`] from a TOML string.
    pub fn from_toml(s: &str) -> Result<Self> {
        toml::from_str(s).context("parsing server configuration")
    }

    /// Load a [`Config`] from a TOML file on disk.
    pub fn load(path: &Path) -> Result<Self> {
        let raw = std::fs::read_to_string(path)
            .with_context(|| format!("reading config file {}", path.display()))?;
        Self::from_toml(&raw)
    }

    /// Socket the login listener should bind to.
    pub fn login_addr(&self) -> SocketAddr {
        SocketAddr::new(self.network.bind, self.network.login_port)
    }

    /// Socket the game listener should bind to.
    pub fn game_addr(&self) -> SocketAddr {
        SocketAddr::new(self.network.bind, self.network.game_port)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r#"
        [server]
        world_name = "Rusted"
        host = "127.0.0.1"
        motd = "Welcome to Rusted"

        [world]
        map_path = "reference/tfs/data/world/map.otbm"
        spawn_town = "Ab'Dendriel"

        [network]
        login_port = 7171
        game_port = 7172
        bind = "0.0.0.0"

        [database]
        path = "tibia.db"

        [log]
        filter = "info"
    "#;

    #[test]
    fn parses_sample_config() {
        let cfg = Config::from_toml(SAMPLE).unwrap();
        assert_eq!(cfg.server.world_name, "Rusted");
        assert_eq!(cfg.network.login_port, 7171);
        assert_eq!(cfg.network.game_port, 7172);
        assert_eq!(cfg.login_addr().to_string(), "0.0.0.0:7171");
        assert_eq!(cfg.game_addr().to_string(), "0.0.0.0:7172");
        assert_eq!(cfg.server.motd, "Welcome to Rusted");
        assert_eq!(cfg.world.map_path, "reference/tfs/data/world/map.otbm");
        assert_eq!(cfg.world.spawn_town.as_deref(), Some("Ab'Dendriel"));
    }

    #[test]
    fn rejects_world_without_map_path() {
        let cfg = Config::from_toml(
            "[server]\nworld_name=\"x\"\nhost=\"y\"\nmotd=\"z\"\n[world]\n\
             [network]\nlogin_port=1\ngame_port=2\nbind=\"0.0.0.0\"\n\
             [database]\npath=\"d\"\n[log]\nfilter=\"info\"\n",
        );
        assert!(cfg.is_err(), "config without [world].map_path must fail");
    }

    #[test]
    fn rejects_missing_sections() {
        let err = Config::from_toml("[server]\nworld_name = \"x\"\nhost = \"y\"\n");
        assert!(err.is_err(), "config without [network] must fail");
    }

    #[test]
    fn rejects_config_without_world_section() {
        // [world] is required (no serde default on the field); a config that omits
        // it entirely must fail, guarding against a future accidental default.
        let cfg = Config::from_toml(
            "[server]\nworld_name=\"x\"\nhost=\"y\"\nmotd=\"z\"\n\
             [network]\nlogin_port=1\ngame_port=2\nbind=\"0.0.0.0\"\n\
             [database]\npath=\"d\"\n[log]\nfilter=\"info\"\n",
        );
        assert!(cfg.is_err(), "config without [world] must fail");
    }
}
