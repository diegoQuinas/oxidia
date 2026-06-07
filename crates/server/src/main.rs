#![forbid(unsafe_code)]

//! Tibia server binary: load config, set up tracing, run the listeners.

mod config;
mod game_service;
mod login_service;

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Context, Result};
use persistence::Store;
use protocol::rsa::RsaPrivateKey;
use tracing::{info, warn};
use tracing_subscriber::EnvFilter;

use config::Config;
use game_service::handle_game;
use login_service::{LoginConfig, handle_login};

#[tokio::main]
async fn main() -> Result<()> {
    let config_path = std::env::args()
        .nth(1)
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("config/server.toml"));

    let cfg = Config::load(&config_path)
        .with_context(|| format!("loading config from {}", config_path.display()))?;

    init_tracing(&cfg.log.filter);

    info!(
        world = %cfg.server.world_name,
        host = %cfg.server.host,
        db = %cfg.database.path,
        "starting tibia server"
    );

    let login_addr = cfg.login_addr();
    let game_addr = cfg.game_addr();

    // Open the database and make sure the M1 test account exists.
    let store = Store::connect(&cfg.database.path)
        .await
        .with_context(|| format!("opening database {}", cfg.database.path))?;
    store
        .seed_test_account_if_empty()
        .await
        .context("seeding test account")?;

    let rsa = Arc::new(RsaPrivateKey::open_tibia());
    let store = Arc::new(store);
    let login_cfg = Arc::new(LoginConfig {
        world_name: cfg.server.world_name.clone(),
        host: cfg.server.host.clone(),
        game_port: cfg.network.game_port,
        motd: Some(cfg.server.motd),
        motd_num: 1,
    });

    // Load the world for the game port.
    let items_bytes =
        std::fs::read("reference/tfs/data/items/items.otb").context("reading items.otb")?;
    let map_bytes =
        std::fs::read("reference/tfs/data/world/forgotten.otbm").context("reading forgotten.otbm")?;
    let items = formats::otb::parse(&items_bytes).context("parsing items.otb")?;
    let map = formats::otbm::parse(&map_bytes).context("parsing forgotten.otbm")?;
    let static_map = std::sync::Arc::new(world::map::StaticMap::from_formats(&map, &items));
    let world_handle = world::game::spawn(static_map);
    info!(spawn = ?world_handle.map.spawn(), "world loaded");

    let rsa_for_login = Arc::clone(&rsa);
    let login_handler = move |stream, peer| {
        let store = Arc::clone(&store);
        let rsa = Arc::clone(&rsa_for_login);
        let login_cfg = Arc::clone(&login_cfg);
        async move {
            let mut stream = stream;
            if let Err(error) = handle_login(&mut stream, &store, &rsa, &login_cfg).await {
                warn!(%peer, %error, "login handler failed");
            }
        }
    };

    let game_handler = {
        let rsa = Arc::clone(&rsa);
        let world = world_handle.clone();
        move |stream, peer| {
            let rsa = Arc::clone(&rsa);
            let world = world.clone();
            async move {
                let stream = stream;
                let ts: u32 = 0x5EED_0000;
                let rnd: u8 = 0x2A;
                if let Err(error) = handle_game(stream, &rsa, &world, ts, rnd).await {
                    warn!(%peer, %error, "game handler failed");
                }
            }
        }
    };

    let login = tokio::spawn(net::serve_with(
        net::Protocol::Login,
        login_addr,
        login_handler,
    ));
    let game = tokio::spawn(net::serve_with(net::Protocol::Game, game_addr, game_handler));

    // If either listener exits (bind error), bring the whole process down.
    tokio::select! {
        res = login => res.context("login listener task panicked")?.context("login listener failed")?,
        res = game => res.context("game listener task panicked")?.context("game listener failed")?,
    }

    Ok(())
}

/// Resolve a [`EnvFilter`] from a config string, falling back to a safe
/// default when the string is not a valid filter directive.
fn resolve_filter(default_filter: &str) -> EnvFilter {
    const FALLBACK: &str = "info";

    EnvFilter::try_new(default_filter).unwrap_or_else(|err| {
        eprintln!("invalid log filter {default_filter:?} ({err}); falling back to {FALLBACK:?}");
        EnvFilter::new(FALLBACK)
    })
}

/// Initialise `tracing`. `RUST_LOG` takes precedence over the config filter.
fn init_tracing(default_filter: &str) {
    let filter =
        EnvFilter::try_from_default_env().unwrap_or_else(|_| resolve_filter(default_filter));
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(false)
        .init();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn valid_filter_is_used_as_is() {
        assert_eq!(resolve_filter("debug").to_string(), "debug");
    }

    #[test]
    fn valid_per_target_filter_survives() {
        // The whole point of EnvFilter: per-target directives must pass through.
        let resolved = resolve_filter("server=debug,info").to_string();
        assert!(resolved.contains("server=debug"), "got: {resolved}");
    }

    #[test]
    fn invalid_filter_falls_back_to_info() {
        // "bogus" is not a valid level, so this directive must fail to parse.
        assert_eq!(resolve_filter("server=bogus").to_string(), "info");
    }
}
