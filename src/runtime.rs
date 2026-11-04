//! One bot's runtime: receives azalea events and decides what to do.

use anyhow::Result;
use azalea::{prelude::*, Client};
use tokio::sync::mpsc;

use crate::{
    config::{BotConfig, Config},
    log::{self, Kind},
};

/// azalea's per-bot state. It only carries a channel into the bot runtime.
#[derive(Clone, Component, Default)]
pub struct BotLink {
    tx: Option<mpsc::UnboundedSender<(Client, Event)>>,
}

impl BotLink {
    pub fn forward(&self, client: Client, event: Event) {
        if let Some(tx) = &self.tx {
            let _ = tx.send((client, event));
        }
    }
}

pub fn spawn(bot: BotConfig, config: Config) -> Result<BotLink> {
    let (tx, mut rx) = mpsc::unbounded_channel::<(Client, Event)>();
    let bot_names: Vec<String> = config.bots.iter().map(|b| b.name.clone()).collect();
    tokio::spawn(async move {
        while let Some((client, event)) = rx.recv().await {
            match event {
                Event::Spawn => log::log(&bot.name, Kind::Event, "spawned"),
                Event::Chat(chat) => {
                    let (sender, content) = chat.split_sender_and_content();
                    let Some(sender) = sender else { continue };
                    if bot_names.contains(&sender) {
                        continue;
                    }
                    log::log(&bot.name, Kind::Chat, format!("<{sender}> {content}"));
                    client.chat(format!("{sender} said: {content}"));
                }
                _ => {}
            }
        }
    });
    Ok(BotLink { tx: Some(tx) })
}
