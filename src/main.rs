//! Launches every bot in `config.toml` into the Minecraft server.
//!
//!     cargo run --release [-- path/to/config.toml]

use anyhow::Result;
use azalea::{prelude::*, swarm::prelude::*};
use mc_agents::{
    config::{Config, ServerConfig},
    console::Console,
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
        let (link, _) = runtime::spawn(bot.clone(), config.clone())?;
        builder = builder.add_account_with_state(Account::offline(&bot.name), link);
        log::log(&bot.name, Kind::Info, format!("joining {}", config.server.address));
    }
    match Console::from_env() {
        Ok(console) => {
            tokio::spawn(keep_world(console, config.server.clone()));
        }
        Err(_) => log::log(
            "server",
            Kind::Info,
            "MC_CONSOLE not set: not applying gamerules or day length (see README)",
        ),
    }
    builder.start(config.server.address.as_str()).await;
    Ok(())
}

/// Applies the start-up commands, then shortens the day by adding time.
async fn keep_world(console: Console, server: ServerConfig) {
    tokio::time::sleep(std::time::Duration::from_secs(5)).await;
    for command in &server.on_start {
        match console.run(command).await {
            Ok(reply) if !reply.is_empty() => log::log("server", Kind::Info, format!("{command}: {reply}")),
            Ok(_) => log::log("server", Kind::Info, command),
            Err(e) => log::log("server", Kind::Fail, format!("{command}: {e:#}")),
        }
    }
    if server.day_minutes >= 20.0 || server.day_minutes <= 0.0 {
        return;
    }
    // Vanilla runs 20 ticks a second; a day is 24000 ticks.
    let extra_per_second = 24000.0 / (server.day_minutes * 60.0) - 20.0;
    log::log("server", Kind::Info, format!("day length {} minutes", server.day_minutes));
    let mut owed = 0.0;
    loop {
        tokio::time::sleep(std::time::Duration::from_secs(5)).await;
        owed += extra_per_second * 5.0;
        let ticks = owed.floor();
        owed -= ticks;
        if let Err(e) = console.run(&format!("time add {ticks}")).await {
            log::log("server", Kind::Fail, format!("day keeper stopped: {e:#}"));
            return;
        }
    }
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
