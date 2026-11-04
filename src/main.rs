//! Launches every bot in `config.toml` into the Minecraft server.
//!
//!     cargo run --release [-- path/to/config.toml]

use anyhow::Result;
use azalea::{prelude::*, swarm::prelude::*};
use mc_agents::{
    config::Config,
    log::{self, Kind},
    runtime::{self, BotLink},
};

#[tokio::main]
async fn main() -> Result<()> {
    let path = std::env::args().nth(1).unwrap_or_else(|| "config.toml".into());
    let config = Config::load(&path)?;
    log::init("logs")?;
    // azalea's bevy LogPlugin installs the tracing subscriber and reads RUST_LOG.
    if std::env::var_os("RUST_LOG").is_none() {
        // SAFETY: no other threads exist yet.
        unsafe { std::env::set_var("RUST_LOG", "warn,azalea=error,azalea_client=error") };
    }

    if config.bots.is_empty() {
        anyhow::bail!("config.toml has no [[bots]]");
    }

    let mut builder = SwarmBuilder::new()
        .set_handler(handle)
        .set_swarm_handler(swarm_handle)
        .join_delay(std::time::Duration::from_millis(1500));
    for bot in &config.bots {
        let link = runtime::spawn(bot.clone(), config.clone())?;
        builder = builder.add_account_with_state(Account::offline(&bot.name), link);
        log::log(&bot.name, Kind::Info, format!("joining {}", config.server.address));
    }
    builder.start(config.server.address.as_str()).await;
    Ok(())
}

/// azalea calls this for every event of every bot. We only forward the event
/// to that bot's own runtime, which owns all behavior.
async fn handle(bot: Client, event: Event, link: BotLink) -> Result<()> {
    link.forward(bot, event);
    Ok(())
}

#[derive(Clone, Default, Resource)]
struct SwarmState;

async fn swarm_handle(_swarm: Swarm, event: SwarmEvent, _state: SwarmState) -> Result<()> {
    if let SwarmEvent::Disconnect(account, _) = &event {
        log::log(&account.username, Kind::Fail, "disconnected, reconnecting");
    }
    Ok(())
}
