//! Milestone 1: a goose GDK agent running in-process with one custom Rust tool.
//!
//!     cargo run --bin gdk-hello [-- --strong]
//!
//! No Minecraft server needed. `look_around` returns a canned world snapshot,
//! so this checks only the model provider, tool calling, and our loop.

use std::sync::Arc;

use anyhow::Result;
use mc_agents::{
    brain::{
        describe_event,
        tools::{object_schema, GdkTools, ToolDef, ToolSet},
        Llm, Tier, Turn,
    },
    config::Config,
};
use serde_json::json;

#[tokio::main]
async fn main() -> Result<()> {
    let config = Config::load("config.toml")?;
    let llm = Llm::from_config(&config.llm)?;

    let tier = if std::env::args().any(|a| a == "--strong") {
        Tier::Strong
    } else {
        Tier::Fast
    };

    let tools = ToolSet::new();
    tools.register(ToolDef::new(
        "look_around",
        "Describe the blocks, mobs and players near the bot.",
        object_schema(&[("radius", "integer", "How far to look, in blocks (1-32)")]),
        |args| async move {
            let radius = args["radius"].as_i64().unwrap_or(16);
            Ok(json!({
                "radius": radius,
                "position": [12, 64, -40],
                "time": "dusk",
                "blocks": {"oak_log": 14, "stone": 220, "coal_ore": 3},
                "mobs": [{"type": "zombie", "distance": 18}],
                "players": [],
            })
            .to_string())
        },
    ));

    let turn = Turn {
        system: "You are Ada, a careful Minecraft bot. Use your tools to check the world \
                 before you answer. Answer in at most two sentences."
            .into(),
        user: "Is it safe to go chop trees right now?".into(),
        tools: Arc::new(GdkTools::all(tools)),
        tier,
        max_inferences: config.limits.inferences_per_turn,
        stop_when: None,
    };

    println!("model: {}", llm.model_name(tier));
    let outcome = llm
        .run_turn(turn, |event| {
            if let Some(line) = describe_event(event) {
                println!("  [gdk] {line}");
            }
        })
        .await?;

    let (input, output) = outcome.tokens();
    println!("answer: {}", outcome.text);
    println!(
        "model calls: {}, tokens in/out: {input}/{output}",
        outcome.model_calls()
    );
    Ok(())
}
