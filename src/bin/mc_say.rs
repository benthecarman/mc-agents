//! Test helper: joins as a player, sends chat lines, prints what it hears.
//!
//!     cargo run --bin mc-say -- [--name Tester] [--wait 30] "Ada, hello" ["/time set night"]

use std::time::{Duration, Instant};

use azalea::prelude::*;

#[derive(Clone, Component, Default)]
struct State;

static SCRIPT: std::sync::OnceLock<(Vec<String>, u64)> = std::sync::OnceLock::new();

#[tokio::main]
async fn main() -> AppExit {
    let mut args = std::env::args().skip(1).peekable();
    let mut name = "Tester".to_string();
    let mut wait = 30;
    let mut lines = Vec::new();
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--name" => name = args.next().expect("--name value"),
            "--wait" => wait = args.next().expect("--wait value").parse().expect("seconds"),
            _ => lines.push(arg),
        }
    }
    SCRIPT.set((lines, wait)).unwrap();
    let address = std::env::var("MC_ADDRESS").unwrap_or_else(|_| "localhost:25565".into());
    ClientBuilder::new()
        .set_handler(handle)
        .start(Account::offline(&name), address.as_str())
        .await
}

async fn handle(bot: Client, event: Event, _state: State) -> anyhow::Result<()> {
    match event {
        Event::Spawn => {
            let (lines, wait) = SCRIPT.get().unwrap().clone();
            tokio::spawn(async move {
                tokio::time::sleep(Duration::from_secs(2)).await;
                for line in lines {
                    println!(">> {line}");
                    bot.chat(line);
                    tokio::time::sleep(Duration::from_secs(1)).await;
                }
                let start = Instant::now();
                while start.elapsed() < Duration::from_secs(wait) {
                    tokio::time::sleep(Duration::from_millis(250)).await;
                }
                bot.disconnect();
                std::process::exit(0);
            });
        }
        Event::Chat(m) => println!("<< {}", m.message().to_ansi()),
        _ => {}
    }
    Ok(())
}
