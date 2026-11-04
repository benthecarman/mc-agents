//! The god bot: it joins like any bot but does not live in the world. It
//! runs the world through the server console: weather, time of day,
//! lightning, teleports and gifts.
//!
//! It acts when a player or bot says its name in chat, and now and then on a
//! whim (`limits.god_whim_secs`). Every power is a fixed command template
//! with checked arguments: player names must be online players, and items
//! and numbers are validated. The model never writes a console command.

use std::{
    sync::Arc,
    time::{Duration, Instant},
};

use serde_json::{json, Value};
use tokio::sync::mpsc;

use super::BotEvent;
use crate::{
    body::Body,
    brain::{
        describe_event,
        tools::{object_schema, GdkTools, ToolDef, ToolSet},
        truncate, Llm, Tier, Turn,
    },
    config::{BotConfig, Config, Role},
    console::Console,
    log::{self, Kind},
};

pub async fn run(
    body: Body,
    bot: BotConfig,
    config: Config,
    events: mpsc::UnboundedSender<BotEvent>,
    mut rx: mpsc::UnboundedReceiver<BotEvent>,
) {
    let name = bot.name.clone();
    let llm = match Llm::from_config(&config.llm) {
        Ok(llm) => llm
            .with_models(bot.fast_model.as_deref(), bot.strong_model.as_deref())
            .with_timeout(Duration::from_secs(config.limits.turn_timeout_secs)),
        Err(e) => {
            log::log(&name, Kind::Fail, format!("god failed to start: {e:#}"));
            return;
        }
    };
    let console = match Console::from_env() {
        Ok(c) => Some(Arc::new(c)),
        Err(_) => {
            log::log(&name, Kind::Fail, "MC_CONSOLE is not set, so the god has no powers");
            None
        }
    };
    let tools = console.as_ref().map(|c| powers(&body, c.clone()));
    let mut thinking = false;
    let mut calls: (u64, u32) = (0, 0);
    let mut last_whim = Instant::now();
    let whim_every = config.limits.god_whim_secs;
    let mut last_bot_reply: Option<Instant> = None;
    {
        let events = events.clone();
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(Duration::from_secs(10)).await;
                if events.send(BotEvent::Heartbeat).is_err() {
                    return;
                }
            }
        });
    }

    while let Some(event) = rx.recv().await {
        let prompt = match event {
            BotEvent::Spawned => {
                if let Some(console) = &console {
                    // Gods do not starve, drown or get eaten.
                    let _ = console.run(&format!("gamemode creative {name}")).await;
                }
                log::log(&name, Kind::Event, "spawned");
                None
            }
            BotEvent::Chat { from, text, from_bot } => {
                if !super::names(&text, &name) || text.starts_with(super::DEBUG_TAG) {
                    continue;
                }
                if from_bot {
                    // Bots may pray too, but not in a loop: one answer per 45 s.
                    if last_bot_reply.is_some_and(|t: Instant| t.elapsed() < Duration::from_secs(45)) {
                        continue;
                    }
                    last_bot_reply = Some(Instant::now());
                }
                body.allow_speech(Duration::from_secs(120));
                log::log(&name, Kind::Event, format!("chat <{from}> {text}"));
                let who = if from_bot { "The bot" } else { "The player" };
                let master = !from_bot && bot.obeys.iter().any(|m| m.eq_ignore_ascii_case(&from));
                let order = if master { " This is your master: carry it out." } else { "" };
                Some(format!("{who} {from} says: \"{text}\"{order}"))
            }
            BotEvent::Heartbeat => {
                if whim_every == 0 || last_whim.elapsed() < Duration::from_secs(whim_every) || thinking {
                    continue;
                }
                last_whim = Instant::now();
                Some(
                    "No one has called on you for a while. Look at who is around and decide whether to do ONE thing \
                     to make the world more interesting (weather, time, a gift, a scare), or nothing. Do not use say: nobody called you."
                        .into(),
                )
            }
            BotEvent::ThoughtDone { calls: n, text, spoke, .. } => {
                thinking = false;
                calls.1 += n as u32;
                if !spoke && !text.is_empty() {
                    let _ = body.say(&truncate(&text, 200));
                }
                None
            }
            BotEvent::ThoughtFailed { error } => {
                thinking = false;
                log::log(&name, Kind::Fail, format!("thinking failed: {error}"));
                None
            }
            _ => None,
        };
        let Some(prompt) = prompt else { continue };
        let Some(tools) = &tools else {
            body.allow_speech(Duration::from_secs(5));
            let _ = body.say("I have no power here. (Set MC_CONSOLE.)");
            continue;
        };
        if thinking {
            continue;
        }
        let day = body.time().day();
        if calls.0 != day {
            calls = (day, 0);
        }
        if calls.1 >= config.limits.llm_calls_per_day {
            log::log(&name, Kind::Llm, "daily model-call budget spent");
            continue;
        }
        thinking = true;
        think(&llm, &body, &bot, &config, tools.clone(), prompt, events.clone());
    }
}

fn think(
    llm: &Llm,
    body: &Body,
    bot: &BotConfig,
    config: &Config,
    tools: Arc<ToolSet>,
    prompt: String,
    events: mpsc::UnboundedSender<BotEvent>,
) {
    let spoke = Arc::new(std::sync::atomic::AtomicBool::new(false));
    {
        let spoke = spoke.clone();
        tools.set_observer(Arc::new(move |name, _, result| {
            if name == "say" && result.is_ok() {
                spoke.store(true, std::sync::atomic::Ordering::SeqCst);
            }
        }));
    }
    let mortals: Vec<String> = config
        .bots
        .iter()
        .filter(|b| b.role == Role::Worker)
        .map(|b| format!("{} ({}; risk appetite {:.1})", b.name, b.personality, b.risk_appetite))
        .collect();
    let system = format!(
        "You are {name}, the god of this Minecraft world. Personality: {personality}\n\
         You do not live in the world; you rule it with your powers: set_weather, set_time, smite (lightning), \
         teleport, give. players lists who is online and where. say speaks in chat (under 20 words, in character).\n\
         The bots here are mortals with their own memories: {mortals}. They save memory only by sleeping at night \
         and lose unsaved memory when they die, so night and lightning are high stakes for them.\n\
         {masters}\
         Rules: do what other players ask when it is reasonable. Use at most 2 powers per turn. Unless your master \
         orders it, smite only someone who asked for it, insulted you, or truly deserves it, and say why. \
         Always say what you did.",
        name = bot.name,
        personality = if bot.personality.is_empty() { "Grand, theatrical, fair." } else { &bot.personality },
        mortals = if mortals.is_empty() { "none".into() } else { mortals.join("; ") },
        masters = if bot.obeys.is_empty() {
            String::new()
        } else {
            format!(
                "You serve {}. Whatever they command, you do at once, using your powers, smiting included. \
                 You may grumble or add flourish in character, but you never refuse them or water it down.\n",
                bot.obeys.join(" and ")
            )
        },
    );
    let time = body.time();
    let user = format!("Time: {} ({}).\n\n{prompt}", time.describe(), time.phase());
    let turn = Turn {
        system,
        user,
        tools: Arc::new(GdkTools::all(tools)),
        tier: Tier::Fast,
        max_inferences: config.limits.inferences_per_turn,
        stop_when: None,
    };
    log::log(&bot.name, Kind::Think, truncate(&prompt, 160));
    let llm = llm.clone();
    let name = bot.name.clone();
    tokio::spawn(async move {
        let log_name = name.clone();
        let outcome = llm
            .run_turn(turn, move |e| {
                if let Some(line) = describe_event(e) {
                    log::log(&log_name, Kind::Llm, truncate(&line, 220));
                }
            })
            .await;
        let event = match outcome {
            Ok(o) => BotEvent::ThoughtDone {
                decision: Default::default(),
                spoke: spoke.load(std::sync::atomic::Ordering::SeqCst),
                text: o.text.clone(),
                tier: Tier::Fast,
                calls: o.model_calls(),
                chat: true,
            },
            Err(e) => BotEvent::ThoughtFailed { error: format!("{e:#}") },
        };
        let _ = events.send(event);
    });
}

/// The god's tools. Each one fills a fixed command template.
fn powers(body: &Body, console: Arc<Console>) -> Arc<ToolSet> {
    let set = ToolSet::new();
    macro_rules! power {
        ($name:expr, $desc:expr, $schema:expr, |$b:ident, $a:ident| $make:expr) => {{
            let (body, console) = (body.clone(), console.clone());
            set.register(ToolDef::new($name, $desc, $schema, move |$a: Value| {
                let ($b, console) = (body.clone(), console.clone());
                async move {
                    let (command, done): (String, String) = $make?;
                    log::log(&$b.name, Kind::Tool, format!("/{command}"));
                    let reply = console.run(&command).await.map_err(|e| format!("the console failed: {e:#}"))?;
                    Ok(if reply.is_empty() { done } else { format!("{done} ({})", truncate(&reply, 120)) })
                }
            }));
        }};
    }

    {
        let body = body.clone();
        set.register(ToolDef::new(
            "say",
            "Speak in chat.",
            object_schema(&[("text", "string", "what to say, under 20 words")]),
            move |a| {
                let body = body.clone();
                async move { body.say(a["text"].as_str().unwrap_or_default()) }
            },
        ));
    }
    {
        let body = body.clone();
        set.register(ToolDef::new(
            "players",
            "Who is online, and where the ones the bots can see are.",
            object_schema(&[]),
            move |_| {
                let body = body.clone();
                async move {
                    let online = online(&body);
                    let seen = body.where_is("").unwrap_or_else(|_| "[]".into());
                    Ok(json!({"online": online, "positions": serde_json::from_str::<Value>(&seen).unwrap_or_default()}).to_string())
                }
            },
        ));
    }
    power!(
        "set_weather",
        "Change the weather.",
        json!({"type": "object", "properties": {"weather": {"type": "string", "enum": ["clear", "rain", "thunder"]}}, "required": ["weather"]}),
        |_b, a| {
            let weather = a["weather"].as_str().unwrap_or_default();
            match weather {
                "clear" | "rain" | "thunder" => Ok::<_, String>((format!("weather {weather}"), format!("the weather is now {weather}"))),
                other => Err(format!("unknown weather '{other}'; use clear, rain or thunder")),
            }
        }
    );
    power!(
        "set_time",
        "Set the time of day: day, noon, night, midnight, or a tick from 0 to 23999 (12500 is dusk).",
        object_schema(&[("time", "string", "day, noon, night, midnight, or a number")]),
        |_b, a| {
            let time = a["time"].as_str().map(str::to_string).or_else(|| a["time"].as_i64().map(|n| n.to_string())).unwrap_or_default();
            let valid = matches!(time.as_str(), "day" | "noon" | "night" | "midnight")
                || time.parse::<u32>().is_ok_and(|n| n < 24000);
            if valid {
                Ok::<_, String>((format!("time set {time}"), format!("the time is now {time}")))
            } else {
                Err(format!("bad time '{time}'"))
            }
        }
    );
    power!(
        "smite",
        "Strike a player with lightning.",
        object_schema(&[("player", "string", "online player name")]),
        |b, a| {
            let player = online_player(&b, a["player"].as_str().unwrap_or_default())?;
            Ok::<_, String>((
                format!("execute at {player} run summon minecraft:lightning_bolt ~ ~ ~"),
                format!("smote {player}"),
            ))
        }
    );
    power!(
        "teleport",
        "Teleport a player to another player, or to coordinates \"x y z\".",
        object_schema(&[
            ("player", "string", "online player to move"),
            ("to", "string", "an online player's name, or \"x y z\""),
        ]),
        |b, a| {
            let player = online_player(&b, a["player"].as_str().unwrap_or_default())?;
            let to = a["to"].as_str().unwrap_or_default().trim().to_string();
            let coords: Vec<i64> = to.split_whitespace().filter_map(|n| n.parse().ok()).collect();
            let target = if coords.len() == 3 && to.split_whitespace().count() == 3 {
                if coords.iter().any(|c| c.abs() > 29_999_984) || !(-64..=320).contains(&coords[1]) {
                    return Err("coordinates out of range".into());
                }
                format!("{} {} {}", coords[0], coords[1], coords[2])
            } else {
                online_player(&b, &to)?
            };
            Ok::<_, String>((format!("tp {player} {target}"), format!("teleported {player} to {target}")))
        }
    );
    power!(
        "give",
        "Give a player items.",
        object_schema(&[
            ("player", "string", "online player name"),
            ("item", "string", "item id, e.g. bread, white_bed, iron_pickaxe"),
            ("count", "integer", "1-64"),
        ]),
        |b, a| {
            let player = online_player(&b, a["player"].as_str().unwrap_or_default())?;
            let item = a["item"].as_str().unwrap_or_default().trim().trim_start_matches("minecraft:").to_string();
            if item.is_empty() || item.len() > 40 || !item.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_') {
                return Err(format!("'{item}' is not an item id"));
            }
            let count = a["count"].as_i64().unwrap_or(1).clamp(1, 64);
            Ok::<_, String>((format!("give {player} minecraft:{item} {count}"), format!("gave {player} {count} {item}")))
        }
    );
    set
}

fn online(body: &Body) -> Vec<String> {
    let mut names: Vec<String> = body.client.tab_list().values().map(|p| p.profile.name.clone()).collect();
    names.sort();
    names
}

/// Returns the exact name of an online player, or an error for the model.
fn online_player(body: &Body, name: &str) -> Result<String, String> {
    let name = name.trim().trim_start_matches('@');
    if name.is_empty() || name.len() > 16 || !name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
        return Err(format!("'{name}' is not a player name"));
    }
    online(body)
        .into_iter()
        .find(|n| n.eq_ignore_ascii_case(name))
        .ok_or_else(|| format!("no player named {name} is online; online: {}", online(body).join(", ")))
}
