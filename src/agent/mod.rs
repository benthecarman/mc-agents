//! The custom agent loop.

pub mod tools;

use tokio::sync::mpsc;

use crate::{
    body::Body,
    config::{BotConfig, Config},
    log::{self, Kind},
};

#[derive(Debug, Clone)]
pub enum BotEvent {
    Spawned,
    Death,
    Chat { from: String, text: String, from_bot: bool },
    Damage { attacker: Option<u32> },
}

pub fn start(body: Body, bot: BotConfig, _config: Config) -> mpsc::UnboundedSender<BotEvent> {
    let (tx, mut rx) = mpsc::unbounded_channel();
    tokio::spawn(async move {
        while let Some(event) = rx.recv().await {
            log::log(&bot.name, Kind::Event, format!("{event:?}"));
            let _ = &body;
        }
    });
    tx
}
