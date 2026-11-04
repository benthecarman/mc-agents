//! One bot's runtime: receives azalea events, keeps the body's senses up to
//! date, and hands everything else to the agent loop.

use std::{
    sync::{Arc, Mutex},
    time::Instant,
};

use anyhow::Result;
use azalea::{
    chat::ChatPacket,
    prelude::*,
    protocol::packets::game::ClientboundGamePacket,
    Client,
};
use tokio::sync::mpsc;

use crate::{
    agent::{self, BotEvent},
    body::{Body, Senses},
    config::{BotConfig, Config},
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

/// Filled in once the bot has joined: its body and its agent's event queue.
pub type Handle = Arc<std::sync::OnceLock<(Body, mpsc::UnboundedSender<BotEvent>)>>;

pub fn spawn(bot: BotConfig, config: Config) -> Result<(BotLink, Handle)> {
    let (tx, mut rx) = mpsc::unbounded_channel::<(Client, Event)>();
    let handle: Handle = Arc::default();
    let ready = handle.clone();
    let bot_names: Vec<String> = config.bots.iter().map(|b| b.name.clone()).collect();
    tokio::spawn(async move {
        let senses = Arc::new(Mutex::new(Senses::default()));
        let mut events: Option<mpsc::UnboundedSender<BotEvent>> = None;
        let mut body: Option<Body> = None;
        let mut last_death = Instant::now() - std::time::Duration::from_secs(60);
        while let Some((client, event)) = rx.recv().await {
            if body.is_none() {
                let b = Body::new(bot.name.clone(), client.clone(), senses.clone());
                let queue = agent::start(b.clone(), bot.clone(), config.clone());
                let _ = ready.set((b.clone(), queue.clone()));
                events = Some(queue);
                body = Some(b);
            }
            let events = events.as_ref().unwrap();
            let send = |e: BotEvent| {
                let _ = events.send(e);
            };
            match event {
                Event::Spawn => send(BotEvent::Spawned),
                Event::Death(_) => {
                    // azalea can report one death twice.
                    if last_death.elapsed() > std::time::Duration::from_secs(5) {
                        last_death = Instant::now();
                        send(BotEvent::Death);
                    }
                }
                Event::Chat(ChatPacket::System(packet)) => {
                    let text = packet.content.to_string();
                    let mut s = senses.lock().unwrap();
                    s.system_messages.push_back((Instant::now(), text));
                    while s.system_messages.len() > 20 {
                        s.system_messages.pop_front();
                    }
                }
                Event::Chat(chat) => {
                    let (sender, content) = chat.split_sender_and_content();
                    let Some(sender) = sender else { continue };
                    if sender == bot.name {
                        continue;
                    }
                    if let Some(command) = debug_command(&bot.name, &content) {
                        send(BotEvent::Debug { command });
                        continue;
                    }
                    let from_bot = bot_names.contains(&sender);
                    send(BotEvent::Chat {
                        from: sender,
                        text: content,
                        from_bot,
                    });
                }
                Event::Packet(packet) => match &*packet {
                    ClientboundGamePacket::SetTime(p) => senses.lock().unwrap().day_time = p.day_time,
                    ClientboundGamePacket::DamageEvent(p) => {
                        let me = client.minecraft_entity_by_ecs_entity(client.entity);
                        if Some(p.entity_id) == me {
                            if let Some(source) = p.source_cause_id.0 {
                                senses.lock().unwrap().last_attacker = Some((source, Instant::now()));
                            }
                            send(BotEvent::Damage {
                                attacker: p.source_cause_id.0,
                            });
                        }
                    }
                    _ => {}
                },
                _ => {}
            }
        }
    });
    Ok((BotLink { tx: Some(tx) }, handle))
}

/// `!Ada mine log 3`, `!all look_around`, `!Ada dream`: test commands.
fn debug_command(name: &str, content: &str) -> Option<String> {
    let rest = content.strip_prefix('!')?;
    let (target, command) = rest.split_once(' ')?;
    (target.eq_ignore_ascii_case(name) || target == "all").then(|| command.trim().to_string())
}
