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
    let map_path = &cfg.world.map_path;
    let mut items = formats::otb::parse(&items_bytes).context("parsing items.otb")?;
    // Merge items.xml (floorChange, etc.) onto the otb dictionary so the live
    // world has stair/floor-change data on its tiles.
    let items_xml_bytes = std::fs::read_to_string("reference/tfs/data/items/items.xml")
        .context("reading items.xml")?;
    let items_xml = formats::items_xml::parse_items_xml(&items_xml_bytes)
        .context("parsing items.xml")?;
    formats::items_xml::merge_items_xml(&mut items, &items_xml);
    // A missing or unparseable map is fatal: the server must not start without a
    // world (mirrors TFS `startupErrorMessage` on `loadMainMap` failure). The
    // full real map is ~119 MB and takes tens of seconds to parse, so announce
    // it — otherwise startup looks hung.
    info!(map_path, "loading world map (large maps take a while)…");
    let map_bytes = std::fs::read(map_path).with_context(|| {
        format!("map file '{map_path}' could not be read — set [world].map_path to a valid .otbm")
    })?;
    let map = formats::otbm::parse(&map_bytes)
        .with_context(|| format!("parsing map file '{map_path}'"))?;
    let mut static_map = world::map::StaticMap::from_formats_with_spawn(
        &map,
        &items,
        cfg.world.spawn_town.as_deref(),
    );
    static_map.load_item_metadata(&items, &items_xml);
    let static_map = std::sync::Arc::new(static_map);
    let (world_handle, mut save_rx) = world::game::spawn(static_map);
    info!(spawn = ?world_handle.map.spawn(), "world loaded");

    // Background save worker: drains save records emitted by the world actor on
    // logout and persists them. Fields the world doesn't track (mana, level)
    // default to 0/0/1 — real progression lands in a later milestone.
    let save_store = Arc::clone(&store);
    let save_task = tokio::spawn(async move {
        while let Some(rec) = save_rx.recv().await {
            let save = game_service::save_record_to_player_save(&rec);
            let _ = save_store.save_player(&save).await;
        }
    });

    let store_for_login = Arc::clone(&store);
    let rsa_for_login = Arc::clone(&rsa);
    let login_handler = move |stream, peer| {
        let store = Arc::clone(&store_for_login);
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
        let store_for_game = Arc::clone(&store);
        move |stream, peer| {
            let rsa = Arc::clone(&rsa);
            let world = world.clone();
            let store = Arc::clone(&store_for_game);
            async move {
                let stream = stream;
                let ts: u32 = 0x5EED_0000;
                let rnd: u8 = 0x2A;
                if let Err(error) = handle_game(stream, &rsa, &world, &store, ts, rnd).await {
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

    // Run until a listener exits (bind error) or a shutdown signal arrives. On
    // Ctrl+C / SIGTERM we persist every online player BEFORE exiting — otherwise
    // sessions that never logged out cleanly revert to their last clean save
    // (default outfit + temple spawn) on the next login.
    tokio::select! {
        res = login => { res.context("login listener task panicked")?.context("login listener failed")?; }
        res = game => { res.context("game listener task panicked")?.context("game listener failed")?; }
        _ = shutdown_signal() => {
            info!("shutdown signal received — saving online players");
            world_handle.shutdown_and_save().await;
            // The actor has dropped its save_tx; drain the remaining records to
            // the DB before the process exits.
            let _ = save_task.await;
            info!("graceful shutdown complete — all online players saved");
        }
    }

    Ok(())
}

/// Resolve when the process receives an interrupt (Ctrl+C) or, on Unix, a
/// SIGTERM (e.g. `systemctl stop` / container stop). Either triggers the
/// graceful save-and-exit path.
async fn shutdown_signal() {
    let ctrl_c = async {
        let _ = tokio::signal::ctrl_c().await;
    };

    #[cfg(unix)]
    let terminate = async {
        match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            Ok(mut sig) => {
                sig.recv().await;
            }
            Err(_) => std::future::pending::<()>().await,
        }
    };

    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        _ = ctrl_c => {}
        _ = terminate => {}
    }
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
