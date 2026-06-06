#![forbid(unsafe_code)]

//! Tibia server binary: load config, set up tracing, run the listeners.

mod config;
mod login_service;

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Context, Result};
use persistence::Store;
use protocol::rsa::RsaPrivateKey;
use tracing::{info, warn};
use tracing_subscriber::EnvFilter;

use config::Config;
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

    let login_handler = move |stream, peer| {
        let store = Arc::clone(&store);
        let rsa = Arc::clone(&rsa);
        let login_cfg = Arc::clone(&login_cfg);
        async move {
            let mut stream = stream;
            if let Err(error) = handle_login(&mut stream, &store, &rsa, &login_cfg).await {
                warn!(%peer, %error, "login handler failed");
            }
        }
    };

    let login = tokio::spawn(net::serve_with(
        net::Protocol::Login,
        login_addr,
        login_handler,
    ));
    let game = tokio::spawn(net::serve(net::Protocol::Game, game_addr));

    // If either listener exits (bind error), bring the whole process down.
    tokio::select! {
        res = login => res.context("login listener task panicked")?.context("login listener failed")?,
        res = game => res.context("game listener task panicked")?.context("game listener failed")?,
    }

    Ok(())
}

/// Initialise `tracing`. `RUST_LOG` takes precedence over the config filter.
fn init_tracing(default_filter: &str) {
    let filter =
        EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(default_filter));
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(false)
        .init();
}
