//! The Rhai skill sandbox.
//!
//! A skill is a Rhai function written by the model, e.g.
//!
//! ```rhai
//! fn gather_wood(n) {
//!     let start = count("log");
//!     mine("log", n);
//!     if count("log") < start + n { throw "not enough logs"; }
//!     "got logs"
//! }
//! ```
//!
//! Generated code runs only here. The engine has no file, shell or network
//! access, no module imports and no `eval`; the only side effects it can reach
//! are the bot API functions registered below, which call the same tools as
//! the planner. Operation count, call depth, string and collection sizes, and
//! wall-clock time are capped.

use std::{
    any::TypeId,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, RwLock, Weak,
    },
    time::{Duration, Instant},
};

use rhai::{Dynamic, Engine, EvalAltResult, Map, Scope, AST};
use serde_json::{json, Value};

use crate::{
    agent::tools::positional,
    body::Body,
    brain::tools::{ToolDef, ToolSet},
    log::{self, Kind},
    memory::Skill,
};

/// Bot tools exposed to skills, with their positional parameters.
pub const BOT_API: [&str; 17] = [
    "look_around", "inventory", "goto", "follow", "where_is", "go_to_player", "give", "mine", "place",
    "craft", "smelt", "say", "eat", "attack", "sleep", "write_book", "read_book",
];

pub const API_DOCS: &str = "\
Bot API (every call blocks until the action is done; on failure it throws an error string you may catch):
  look_around(radius) -> #{position: [x,y,z], blocks: [#{block, count, nearest: [x,y,z], distance}], entities: [...]}
  inventory() -> #{items: #{name: count}, held, free_slots}
  count(item) -> int          // item name, or group \"log\" / \"planks\"
  position() -> #{x, y, z}
  find_block(name) -> #{x, y, z} or ()  // nearest block within 64, name or group like \"log\"
  go_to(x, y, z)              // walk there (`goto` is a reserved word in Rhai)
  follow(player, seconds)
  where_is(player) -> #{player, position: [x,y,z], distance}; go_to_player(player)
  give(player, item, count)   // walk to a player and toss them items
  mine(block, count) -> #{mined, gained}
  place(block, x, y, z)
  craft(item, count) -> #{crafted, count}    // makes missing planks/sticks itself
  smelt(item, count)
  say(text), eat(), attack(target), sleep()
  write_book(skills)          // \"all\" or \"a,b\": into the nearest chest; read_book() loads books nearby
  time() -> #{phase, tick}, hp() -> float, food() -> int
  log(text)                   // note for the debug log";

pub const MAX_RUN: Duration = Duration::from_secs(240);

/// Every skill the bot knows right now, compiled on demand.
#[derive(Default)]
pub struct SkillLibrary {
    skills: RwLock<Vec<Skill>>,
}

impl SkillLibrary {
    pub fn all(&self) -> Vec<Skill> {
        self.skills.read().unwrap().clone()
    }

    pub fn get(&self, name: &str) -> Option<Skill> {
        self.skills.read().unwrap().iter().find(|s| s.name == name).cloned()
    }

    pub fn insert(&self, skill: Skill) {
        let mut skills = self.skills.write().unwrap();
        skills.retain(|s| s.name != skill.name);
        skills.push(skill);
    }

    pub fn remove(&self, name: &str) {
        self.skills.write().unwrap().retain(|s| s.name != name);
    }

    /// One-line signatures for prompts: `gather_wood(n): gets n logs`.
    pub fn signatures(&self) -> Vec<String> {
        self.all()
            .iter()
            .map(|s| {
                let params = params_of(&s.code, &s.name).unwrap_or_default();
                format!(
                    "{}({}){}: {}",
                    s.name,
                    params.join(", "),
                    if s.saved { "" } else { " [unsaved]" },
                    s.description
                )
            })
            .collect()
    }
}

/// Builds a locked-down engine whose only side effects are bot tool calls.
fn engine(body: Body, tools: Arc<ToolSet>, cancel: Arc<AtomicBool>, deadline: Instant) -> Engine {
    let mut engine = Engine::new();
    engine
        .set_max_operations(5_000_000)
        .set_max_call_levels(24)
        .set_max_expr_depths(64, 32)
        .set_max_string_size(20_000)
        .set_max_array_size(10_000)
        .set_max_map_size(2_000)
        .set_module_resolver(rhai::module_resolvers::DummyModuleResolver::new());
    engine.disable_symbol("eval");
    engine.disable_symbol("import");

    let name = body.name.clone();
    engine.on_print(move |text| log::log(&name, Kind::Skill, format!("print: {text}")));
    let name = body.name.clone();
    engine.on_debug(move |text, _, _| log::log(&name, Kind::Skill, format!("debug: {text}")));
    engine.on_progress(move |_| {
        if cancel.load(Ordering::Relaxed) {
            Some("skill cancelled".into())
        } else if Instant::now() > deadline {
            Some(format!("skill ran longer than {}s", MAX_RUN.as_secs()).into())
        } else {
            None
        }
    });

    let handle = tokio::runtime::Handle::current();
    for tool in BOT_API {
        let params = positional(tool);
        // `goto` is reserved in Rhai, so skills call it `go_to`.
        let script_name = if tool == "goto" { "go_to" } else { tool };
        for arity in 0..=params.len() {
            let tools = tools.clone();
            let handle = handle.clone();
            engine.register_raw_fn(script_name, vec![TypeId::of::<Dynamic>(); arity], move |_ctx, args| {
                let mut json_args = serde_json::Map::new();
                for (i, arg) in args.iter().enumerate() {
                    json_args.insert(params[i].to_string(), dynamic_to_json(arg));
                }
                let result = handle.block_on(tools.call(tool, Value::Object(json_args)));
                match result {
                    Ok(text) => Ok(text_to_dynamic(&text)),
                    Err(error) => Err(format!("{tool} failed: {error}").into()),
                }
            });
        }
    }

    let b = body.clone();
    engine.register_fn("count", move |item: &str| -> i64 {
        let inv = b.inventory_counts();
        inv.iter()
            .filter(|(k, _)| item_matches(item, k))
            .map(|(_, v)| *v as i64)
            .sum()
    });
    let b = body.clone();
    engine.register_fn("position", move || -> Map {
        let p = b.block_pos();
        let mut m = Map::new();
        m.insert("x".into(), (p.x as i64).into());
        m.insert("y".into(), (p.y as i64).into());
        m.insert("z".into(), (p.z as i64).into());
        m
    });
    let b = body.clone();
    engine.register_fn("find_block", move |name: &str| -> Dynamic {
        match b.find_block(name, 64) {
            Some(p) => {
                let mut m = Map::new();
                m.insert("x".into(), (p.x as i64).into());
                m.insert("y".into(), (p.y as i64).into());
                m.insert("z".into(), (p.z as i64).into());
                m.into()
            }
            None => Dynamic::UNIT,
        }
    });
    let b = body.clone();
    engine.register_fn("time", move || -> Map {
        let t = b.time();
        let mut m = Map::new();
        m.insert("phase".into(), t.phase().into());
        m.insert("tick".into(), (t.ticks() as i64).into());
        m
    });
    let b = body.clone();
    engine.register_fn("hp", move || -> f64 { b.health() as f64 });
    let b = body.clone();
    engine.register_fn("food", move || -> i64 { b.food() as i64 });
    let name = body.name.clone();
    engine.register_fn("log", move |text: &str| log::log(&name, Kind::Skill, format!("log: {text}")));
    engine
}

pub fn item_matches(query: &str, item: &str) -> bool {
    match query {
        "log" | "logs" => item.ends_with("_log"),
        "planks" => item.ends_with("_planks"),
        "wool" => item.ends_with("_wool"),
        "bed" => item.ends_with("_bed"),
        q => q == item,
    }
}

fn text_to_dynamic(text: &str) -> Dynamic {
    match serde_json::from_str::<Value>(text) {
        Ok(v @ (Value::Object(_) | Value::Array(_))) => rhai::serde::to_dynamic(v).unwrap_or_else(|_| text.into()),
        _ => text.into(),
    }
}

fn dynamic_to_json(value: &Dynamic) -> Value {
    if let Ok(i) = value.as_int() {
        return json!(i);
    }
    if let Ok(f) = value.as_float() {
        return json!(f);
    }
    if let Some(s) = value.clone().try_cast::<rhai::ImmutableString>() {
        return json!(s.to_string());
    }
    rhai::serde::from_dynamic::<Value>(value).unwrap_or_else(|_| json!(value.to_string()))
}

fn json_to_dynamic(value: &Value) -> Dynamic {
    match value {
        Value::String(s) => s.parse::<i64>().map(Dynamic::from).unwrap_or_else(|_| s.clone().into()),
        Value::Number(n) if n.is_i64() => n.as_i64().unwrap().into(),
        Value::Number(n) => n.as_f64().unwrap_or(0.0).into(),
        Value::Bool(b) => (*b).into(),
        Value::Null => Dynamic::UNIT,
        other => rhai::serde::to_dynamic(other).unwrap_or(Dynamic::UNIT),
    }
}

/// Compiles a skill on its own, to check syntax. Returns its parameter names.
pub fn check_syntax(code: &str, name: &str) -> Result<Vec<String>, String> {
    let engine = Engine::new_raw();
    let ast = engine.compile(code).map_err(|e| format!("syntax error: {e}"))?;
    let params = ast
        .iter_functions()
        .find(|f| f.name == name)
        .map(|f| f.params.iter().map(|p| p.to_string()).collect())
        .ok_or_else(|| format!("the code must define `fn {name}(...)`"))?;
    Ok(params)
}

pub fn params_of(code: &str, name: &str) -> Option<Vec<String>> {
    check_syntax(code, name).ok()
}

/// Runs skill `name` with JSON arguments. `extra` is a skill under test that
/// is not in the library yet.
pub async fn run(
    body: Body,
    tools: Arc<ToolSet>,
    library: Vec<Skill>,
    name: String,
    args: Value,
) -> Result<String, String> {
    let cancel = Arc::new(AtomicBool::new(false));
    // If the executor drops this future (plan replaced, bot died), stop the
    // script at its next operation.
    struct CancelOnDrop(Arc<AtomicBool>);
    impl Drop for CancelOnDrop {
        fn drop(&mut self) {
            self.0.store(true, Ordering::Relaxed);
        }
    }
    let _guard = CancelOnDrop(cancel.clone());
    let deadline = Instant::now() + MAX_RUN;
    let task = tokio::task::spawn_blocking(move || {
        let engine = engine(body, tools, cancel, deadline);
        let mut ast = AST::empty();
        for skill in &library {
            match engine.compile(&skill.code) {
                Ok(skill_ast) => {
                    ast.combine(skill_ast);
                }
                Err(e) if skill.name == name => return Err(format!("syntax error: {e}")),
                Err(_) => {}
            }
        }
        let params: Vec<String> = ast
            .iter_functions()
            .find(|f| f.name == name)
            .map(|f| f.params.iter().map(|p| p.to_string()).collect())
            .ok_or_else(|| format!("no skill named {name}"))?;
        let call_args: Vec<Dynamic> = params
            .iter()
            .enumerate()
            .map(|(i, p)| {
                let v = args.get(p).or_else(|| args.as_array().and_then(|a| a.get(i)));
                v.map(json_to_dynamic).unwrap_or(Dynamic::UNIT)
            })
            .collect();
        engine
            .call_fn::<Dynamic>(&mut Scope::new(), &ast, &name, call_args)
            .map(|v| if v.is_unit() { "done".to_string() } else { dynamic_to_json(&v).to_string().trim_matches('"').to_string() })
            .map_err(|e: Box<EvalAltResult>| describe_error(&e))
    });
    task.await.map_err(|e| format!("skill crashed: {e}"))?
}

fn describe_error(e: &EvalAltResult) -> String {
    match e {
        EvalAltResult::ErrorRuntime(value, pos) => format!("threw {value} ({pos})"),
        EvalAltResult::ErrorInFunctionCall(f, _, inner, _) => format!("in {f}: {}", describe_error(inner)),
        other => other.to_string(),
    }
}

/// Registers a skill as a tool in the bot's tool set, so the planner and
/// other skills' callers see it immediately.
pub fn register(set: &Arc<ToolSet>, library: &Arc<SkillLibrary>, body: &Body, skill: &Skill) {
    let params = params_of(&skill.code, &skill.name).unwrap_or_default();
    let properties: serde_json::Map<String, Value> = params
        .iter()
        .map(|p| {
            let numeric = matches!(p.as_str(), "n" | "count" | "amount" | "x" | "y" | "z" | "radius" | "seconds" | "size" | "width" | "height" | "depth" | "times");
            (p.clone(), json!({"type": if numeric { "integer" } else { "string" }, "description": p}))
        })
        .collect();
    let schema = json!({"type": "object", "properties": properties, "required": params});
    let weak: Weak<ToolSet> = Arc::downgrade(set);
    let library = library.clone();
    let body = body.clone();
    let name = skill.name.clone();
    set.register(ToolDef::new(
        &skill.name,
        &format!("Learned skill: {}", skill.description),
        schema,
        move |args| {
            let tools = weak.upgrade();
            let library = library.all();
            let body = body.clone();
            let name = name.clone();
            async move {
                let tools = tools.ok_or("bot is shutting down")?;
                run(body, tools, library, name, args).await
            }
        },
    ));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn syntax_check_finds_params() {
        let params = check_syntax("fn gather_wood(n, kind) { n + 1 }", "gather_wood").unwrap();
        assert_eq!(params, vec!["n", "kind"]);
        assert!(check_syntax("fn other() {}", "gather_wood").is_err());
        assert!(check_syntax("fn gather_wood( {", "gather_wood").unwrap_err().contains("syntax"));
    }

    #[test]
    fn sandbox_has_no_eval_or_imports() {
        let mut engine = Engine::new();
        engine.disable_symbol("eval");
        engine.set_module_resolver(rhai::module_resolvers::DummyModuleResolver::new());
        assert!(engine.run("eval(\"1\")").is_err());
        assert!(engine.run("import \"x\" as y;").is_err());
    }

    #[test]
    fn dynamic_params_accept_any_type() {
        let mut engine = Engine::new();
        engine.register_raw_fn("probe", vec![TypeId::of::<Dynamic>(); 2], |_ctx, args| {
            Ok(format!("{}|{}", dynamic_to_json(args[0]), dynamic_to_json(args[1])))
        });
        let out: String = engine.eval("probe(\"log\", 4)").unwrap();
        assert_eq!(out, "\"log\"|4");
    }
}
