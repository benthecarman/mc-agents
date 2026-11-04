//! The core bot tools, registered once in a [`ToolSet`].
//!
//! The same set serves three callers: the GDK agent (tool calling), the plan
//! executor, and the Rhai skill sandbox. Each tool is a thin JSON adapter over
//! a [`Body`] method.

use std::sync::Arc;

use serde_json::{json, Value};

use crate::{
    body::Body,
    brain::tools::{object_schema, ToolDef, ToolSet},
};

/// Tools that only read the world. They run immediately during planning.
pub const SENSE_TOOLS: [&str; 4] = ["look_around", "inventory", "list_skills", "where_is"];

fn int(args: &Value, key: &str) -> Result<i32, String> {
    args.get(key)
        .and_then(|v| v.as_i64().or_else(|| v.as_f64().map(|f| f.floor() as i64)).or_else(|| v.as_str().and_then(|s| s.parse().ok())))
        .map(|v| v as i32)
        .ok_or_else(|| format!("missing integer argument '{key}'"))
}

fn int_or(args: &Value, key: &str, default: i32) -> i32 {
    int(args, key).unwrap_or(default)
}

fn text(args: &Value, key: &str) -> Result<String, String> {
    args.get(key)
        .and_then(|v| v.as_str())
        .map(str::to_string)
        .ok_or_else(|| format!("missing text argument '{key}'"))
}

/// Order of positional arguments, for `!bot tool a b c` chat commands and Rhai calls.
pub fn positional(tool: &str) -> &'static [&'static str] {
    match tool {
        "look_around" => &["radius"],
        "goto" => &["x", "y", "z"],
        "follow" => &["player", "seconds"],
        "where_is" | "go_to_player" => &["player"],
        "give" => &["player", "item", "count"],
        "mine" => &["block", "count"],
        "place" => &["block", "x", "y", "z"],
        "craft" | "smelt" => &["item", "count"],
        "say" => &["text"],
        "attack" => &["target"],
        "write_book" => &["skills"],
        _ => &[],
    }
}

pub fn register_core(set: &ToolSet, body: Body) {
    macro_rules! tool {
        ($name:expr, $desc:expr, $schema:expr, |$b:ident, $a:ident| $body:expr) => {{
            let body = body.clone();
            set.register(ToolDef::new($name, $desc, $schema, move |$a: Value| {
                let $b = body.clone();
                async move { $body }
            }));
        }};
    }

    tool!(
        "look_around",
        "Describe nearby blocks (with the nearest position of each kind), mobs, players, time, HP and food.",
        object_schema(&[("radius", "integer", "How far to look, 4-24 blocks. Use 16 if unsure.")]),
        |b, a| b.look_around(int_or(&a, "radius", 16))
    );
    tool!(
        "inventory",
        "List the items the bot carries.",
        json!({"type": "object", "properties": {}}),
        |b, _a| b.inventory()
    );
    tool!(
        "goto",
        "Walk to a block position. Fails with how far away the bot stopped.",
        object_schema(&[("x", "integer", "x"), ("y", "integer", "y"), ("z", "integer", "z")]),
        |b, a| b.goto(int(&a, "x")?, int(&a, "y")?, int(&a, "z")?).await
    );
    tool!(
        "follow",
        "Follow a player for some seconds.",
        object_schema(&[("player", "string", "Player name"), ("seconds", "integer", "How long, 3-120")]),
        |b, a| b.follow(&text(&a, "player")?, int_or(&a, "seconds", 30) as u64).await
    );
    tool!(
        "where_is",
        "Where a player is (position and distance). Leave player empty to list every player in sight.",
        object_schema(&[("player", "string", "Player name, or empty for everyone")]),
        |b, a| b.where_is(a.get("player").and_then(|v| v.as_str()).unwrap_or_default())
    );
    tool!(
        "go_to_player",
        "Walk to where a player is now. Use for \"come here\".",
        object_schema(&[("player", "string", "Player name")]),
        |b, a| b.go_to_player(&text(&a, "player")?).await
    );
    tool!(
        "give",
        "Walk to a player and drop items for them (the drop key). Item may be a name, a group (log, planks), or all for everything you carry.",
        json!({
            "type": "object",
            "properties": {
                "player": {"type": "string", "description": "Player name"},
                "item": {"type": "string", "description": "Item name, group (log, planks), or all"},
                "count": {"type": "integer", "description": "How many (default 1; ignored for all)"}
            },
            "required": ["player", "item"]
        }),
        |b, a| b.give_to(&text(&a, "player")?, &text(&a, "item")?, int_or(&a, "count", 1) as u32).await
    );
    tool!(
        "mine",
        "Mine the nearest blocks of a kind and pick up the drops. Accepts exact names (oak_log, iron_ore, coal_ore, stone) and groups (log, ore). Mining stone gives cobblestone and needs a pickaxe.",
        object_schema(&[("block", "string", "Block name or group"), ("count", "integer", "How many, 1-64")]),
        |b, a| b.mine(&text(&a, "block")?, int_or(&a, "count", 1) as u32).await
    );
    tool!(
        "place",
        "Place a block from the inventory at a position next to an existing solid block.",
        object_schema(&[
            ("block", "string", "Block item name, e.g. crafting_table"),
            ("x", "integer", "x"),
            ("y", "integer", "y"),
            ("z", "integer", "z"),
        ]),
        |b, a| b.place(&text(&a, "block")?, int(&a, "x")?, int(&a, "y")?, int(&a, "z")?).await
    );
    tool!(
        "craft",
        "Craft an item. Recipes that need a 3x3 grid (tools, chest, furnace, bed) need a crafting_table within 32 blocks. 'planks' crafts planks from whatever logs you have.",
        object_schema(&[("item", "string", "Item name, e.g. oak_planks, stick, wooden_pickaxe"), ("count", "integer", "How many items you want")]),
        |b, a| b.craft(&text(&a, "item")?, int_or(&a, "count", 1) as u32).await
    );
    tool!(
        "smelt",
        "Smelt items in a furnace within 32 blocks, e.g. raw_iron into iron_ingot. Uses fuel from the inventory.",
        object_schema(&[("item", "string", "Item to smelt"), ("count", "integer", "How many")]),
        |b, a| b.smelt(&text(&a, "item")?, int_or(&a, "count", 1) as u32).await
    );
    tool!(
        "say",
        "Say something in chat.",
        object_schema(&[("text", "string", "What to say")]),
        |b, a| b.say(&text(&a, "text")?)
    );
    tool!(
        "eat",
        "Eat food from the inventory.",
        json!({"type": "object", "properties": {}}),
        |b, _a| b.eat().await
    );
    tool!(
        "attack",
        "Fight the nearest hostile mob, or a given mob type, for up to 10 seconds.",
        json!({"type": "object", "properties": {"target": {"type": "string", "description": "Mob type, e.g. zombie. Omit for the nearest hostile."}}}),
        |b, a| b.attack(a.get("target").and_then(|t| t.as_str())).await
    );
    tool!(
        "sleep",
        "Walk to the nearest bed and sleep. Fails if it is not night or monsters are near. Sleeping saves your memory.",
        json!({"type": "object", "properties": {}}),
        |b, _a| b.sleep().await
    );
}

/// Parses `tool a b c` into the tool's JSON arguments.
pub fn parse_command(line: &str) -> Option<(String, Value)> {
    let mut parts = line.split_whitespace();
    let tool = parts.next()?.to_string();
    let names = positional(&tool);
    let rest: Vec<&str> = parts.collect();
    let mut args = serde_json::Map::new();
    for (i, name) in names.iter().enumerate() {
        let value = if i == names.len() - 1 && rest.len() > names.len() {
            rest[i..].join(" ")
        } else if let Some(v) = rest.get(i) {
            v.to_string()
        } else {
            continue;
        };
        let json = value
            .parse::<i64>()
            .map(Value::from)
            .unwrap_or(Value::String(value));
        args.insert(name.to_string(), json);
    }
    Some((tool, Value::Object(args)))
}

pub fn core_tools(body: Body) -> Arc<ToolSet> {
    let set = ToolSet::new();
    register_core(&set, body);
    set
}
