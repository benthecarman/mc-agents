//! The benchmark: timed scenarios played by one bot through the real agent
//! loop, scored as JSON.
//!
//!     MC_CONSOLE=fifo:/run/mc-agents-server.stdin cargo run --release --bin bench
//!     cargo run --release --bin bench -- --only wood,sleep
//!
//! Stop the demo bots first: they would hear the benchmark bot's chat.
//! The bot starts with an empty memory in bench/memory/ every run, so skills
//! it learns in early scenarios help it in later ones, but not across runs.
//!
//! Score per scenario: 100 for passing, plus up to 50 for speed, minus 2 per
//! model call, never below 0. The last stdout line is `BENCH_SCORE <json>`.

use std::{
    sync::atomic::Ordering,
    time::{Duration, Instant},
};

use anyhow::{bail, Context, Result};
use azalea::{prelude::*, swarm::prelude::*};
use mc_agents::{
    agent::{learn::{verify, WorldState}, BotEvent},
    brain::MODEL_CALLS,
    config::{BotConfig, Config},
    console::Console,
    log::{self, Kind},
    runtime::{self, BotLink, Handle},
};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

#[derive(Deserialize)]
struct Suite {
    bot: String,
    personality: String,
    #[serde(default)]
    setup: Vec<String>,
    #[serde(default)]
    teardown: Vec<String>,
    #[serde(default)]
    cleanup: Vec<String>,
    scenario: Vec<Scenario>,
}

#[derive(Deserialize)]
struct Scenario {
    name: String,
    goal: String,
    commands: Vec<String>,
    timeout_secs: u64,
    check: Value,
}

#[derive(Serialize)]
struct Outcome {
    name: String,
    passed: bool,
    secs: f64,
    model_calls: usize,
    score: f64,
    detail: String,
}

fn main() -> Result<()> {
    // Before any threads start: the bench bot gets its own memory.
    // SAFETY: single-threaded here.
    unsafe {
        std::env::set_var("MC_MEMORY_DIR", "bench/memory");
        if std::env::var_os("RUST_LOG").is_none() {
            std::env::set_var("RUST_LOG", "warn,azalea=error,azalea_client=error");
        }
    }
    tokio::runtime::Builder::new_multi_thread().enable_all().build()?.block_on(run())
}

async fn run() -> Result<()> {
    let mut args = std::env::args().skip(1);
    let mut only: Option<Vec<String>> = None;
    let mut suite_path = "bench/scenarios.toml".to_string();
    let mut config_path = "config.toml".to_string();
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--only" => only = Some(args.next().context("--only needs names")?.split(',').map(String::from).collect()),
            "--suite" => suite_path = args.next().context("--suite needs a path")?,
            "--config" => config_path = args.next().context("--config needs a path")?,
            other => bail!("unknown argument {other}"),
        }
    }
    let mut suite: Suite = toml::from_str(&std::fs::read_to_string(&suite_path).with_context(|| format!("reading {suite_path}"))?)?;
    if let Some(only) = &only {
        suite.scenario.retain(|s| only.contains(&s.name));
    }
    let console = Console::from_env()?;

    let mut config = Config::load(&config_path)?;
    config.limits.idle_goal_secs = 0;
    config.limits.hardcore = false;
    config.bots = vec![BotConfig {
        name: suite.bot.clone(),
        personality: suite.personality.clone(),
        risk_appetite: 0.5,
        role: Default::default(),
        fast_model: None,
        strong_model: None,
    }];
    log::init("logs/bench")?;
    let _ = std::fs::remove_dir_all("bench/memory");

    let bot = config.bots[0].clone();
    let (link, handle) = runtime::spawn(bot.clone(), config.clone())?;
    let address = config.server.address.clone();
    tokio::spawn(async move {
        let result = drive(&suite, &console, &handle).await;
        let code = match result {
            Ok(()) => 0,
            Err(e) => {
                log::log("bench", Kind::Fail, format!("{e:#}"));
                1
            }
        };
        std::process::exit(code);
    });
    SwarmBuilder::new()
        .set_handler(handle_event)
        .set_swarm_handler(swarm_handle)
        .add_account_with_state(Account::offline(&bot.name), link)
        .start(address.as_str())
        .await;
    Ok(())
}

async fn drive(suite: &Suite, console: &Console, handle: &Handle) -> Result<()> {
    let start = Instant::now();
    let (body, events) = loop {
        if let Some(ready) = handle.get() {
            break ready.clone();
        }
        if start.elapsed() > Duration::from_secs(60) {
            bail!("the benchmark bot did not join within 60s");
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    };
    tokio::time::sleep(Duration::from_secs(3)).await;
    let bot = &suite.bot;
    let run = |commands: Vec<String>| async move {
        for command in commands {
            let command = command.replace("{bot}", bot);
            if let Err(e) = console.run(&command).await {
                log::log("bench", Kind::Fail, format!("console {command}: {e:#}"));
            }
            tokio::time::sleep(Duration::from_millis(300)).await;
        }
    };
    run(suite.setup.clone()).await;

    let mut outcomes = Vec::new();
    for scenario in &suite.scenario {
        run(suite.cleanup.clone()).await;
        let _ = events.send(BotEvent::Debug { command: "reset".into() });
        run(scenario.commands.clone()).await;
        tokio::time::sleep(Duration::from_secs(2)).await;
        log::log("bench", Kind::Plan, format!("scenario {}: {}", scenario.name, scenario.goal));

        let before = WorldState::of(&body);
        let calls_before = MODEL_CALLS.load(Ordering::Relaxed);
        let started = Instant::now();
        let start_tick = body.time().ticks();
        let _ = events.send(BotEvent::Chat {
            from: "Judge".into(),
            text: format!("{bot}, {}", scenario.goal),
            from_bot: false,
        });
        let timeout = Duration::from_secs(scenario.timeout_secs);
        let mut detail = String::from("timed out");
        let mut passed = false;
        while started.elapsed() < timeout {
            tokio::time::sleep(Duration::from_secs(2)).await;
            if body.health() <= 0.0 {
                detail = "died".into();
                break;
            }
            let result = if scenario.check.get("day").is_some() {
                // Started at dusk; passes once the night has been slept away.
                let ticks = body.time().ticks();
                if ticks < start_tick && !body.time().is_dark() {
                    Ok(())
                } else {
                    Err("still night".to_string())
                }
            } else {
                verify(&scenario.check, &before, &WorldState::of(&body), &body)
            };
            match result {
                Ok(()) => {
                    passed = true;
                    detail = "passed".into();
                    break;
                }
                Err(e) => detail = format!("timed out: {e}"),
            }
        }
        let secs = started.elapsed().as_secs_f64();
        let model_calls = MODEL_CALLS.load(Ordering::Relaxed) - calls_before;
        let speed = 1.0 - (secs / scenario.timeout_secs as f64).min(1.0);
        let score = if passed { (100.0 + 50.0 * speed - 2.0 * model_calls as f64).max(0.0) } else { 0.0 };
        log::log(
            "bench",
            if passed { Kind::Result } else { Kind::Fail },
            format!("{}: {detail} in {secs:.0}s, {model_calls} model calls, score {score:.1}", scenario.name),
        );
        outcomes.push(Outcome {
            name: scenario.name.clone(),
            passed,
            secs: (secs * 10.0).round() / 10.0,
            model_calls,
            score: (score * 10.0).round() / 10.0,
            detail,
        });
    }
    let _ = events.send(BotEvent::Debug { command: "reset".into() });
    run(suite.teardown.clone()).await;

    let total: f64 = outcomes.iter().map(|o| o.score).sum();
    let result = json!({
        "score": (total * 10.0).round() / 10.0,
        "max": 150 * outcomes.len(),
        "passed": outcomes.iter().filter(|o| o.passed).count(),
        "scenarios": outcomes,
        "secs": start.elapsed().as_secs(),
        "finished": chrono::Local::now().to_rfc3339(),
    });
    std::fs::create_dir_all("bench/results")?;
    std::fs::write("bench/results/latest.json", serde_json::to_string_pretty(&result)?)?;
    log::log("bench", Kind::Result, format!("total score {total:.1}"));
    println!("BENCH_SCORE {result}");
    Ok(())
}

async fn handle_event(bot: Client, event: Event, link: BotLink) -> Result<()> {
    link.forward(bot, event);
    Ok(())
}

#[derive(Clone, Default, Resource)]
struct SwarmState;

async fn swarm_handle(_swarm: Swarm, _event: SwarmEvent, _state: SwarmState) -> Result<()> {
    Ok(())
}
