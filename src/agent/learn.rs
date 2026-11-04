//! Skill learning: write, run, verify against the world, retry.
//!
//! Each attempt is one GDK turn on the strong model. The writer must call
//! `submit_skill`; syntax errors and missing test arguments come back inside
//! the same turn so it can fix them without spending an attempt. Then we run
//! the skill in the sandbox for real and check the world: did the inventory
//! gain the item, is the block there, did the bot get where it should? On
//! failure the error and the measured outcome go into the next attempt.

use std::{
    collections::BTreeMap,
    sync::{Arc, Mutex},
};

use serde_json::{json, Value};

use super::plan::SkillRequest;
use crate::{
    body::{diff, Body},
    brain::{
        describe_event,
        tools::{GdkTools, ToolDef, ToolSet},
        truncate, Llm, Tier, Turn,
    },
    log::{self, Kind},
    memory::Skill,
    skills::{self, item_matches, SkillLibrary},
};

pub struct Learner {
    pub body: Body,
    pub llm: Llm,
    pub tools: Arc<ToolSet>,
    pub library: Arc<SkillLibrary>,
    pub retries: u32,
    pub inferences_per_turn: u32,
}

pub struct Learned {
    pub result: Result<Skill, String>,
    pub calls: usize,
    pub attempts: u32,
}

#[derive(Clone)]
struct Submission {
    code: String,
    test_args: Value,
    check: Value,
}

struct WorldState {
    inventory: BTreeMap<String, i32>,
    position: azalea::Vec3,
}

impl Learner {
    fn state(&self) -> WorldState {
        WorldState {
            inventory: self.body.inventory_counts(),
            position: self.body.position(),
        }
    }

    pub async fn learn(&self, request: SkillRequest) -> Learned {
        let name = &self.body.name;
        let mut feedback: Vec<String> = Vec::new();
        let mut calls = 0;
        for attempt in 1..=self.retries.max(1) {
            log::log(name, Kind::Skill, format!("writing {} (attempt {attempt}/{})", request.name, self.retries));
            let (submission, turn_calls) = match self.write(&request, &feedback).await {
                Ok(x) => x,
                Err(e) => {
                    feedback.push(format!("Attempt {attempt}: {e}"));
                    continue;
                }
            };
            calls += turn_calls;
            let Some(sub) = submission else {
                feedback.push(format!("Attempt {attempt}: you did not call submit_skill."));
                continue;
            };
            log::log(name, Kind::Skill, format!("code for {}:\n{}", request.name, sub.code.trim()));

            let skill = Skill {
                name: request.name.clone(),
                description: request.description.clone(),
                code: sub.code.clone(),
                saved: false,
            };
            let mut library = self.library.all();
            library.retain(|s| s.name != skill.name);
            library.push(skill.clone());

            let before = self.state();
            log::log(name, Kind::Skill, format!("testing {}({})", request.name, sub.test_args));
            let run = skills::run(
                self.body.clone(),
                self.tools.clone(),
                library,
                request.name.clone(),
                sub.test_args.clone(),
            )
            .await;
            let after = self.state();
            let changes = diff(&before.inventory, &after.inventory);
            let moved = before.position.distance_to(after.position);
            let verdict = verify(&sub.check, &before, &after, &self.body);
            let outcome = format!(
                "returned {}; inventory change {}; moved {moved:.0} blocks",
                match &run {
                    Ok(v) => format!("ok: {}", truncate(v, 160)),
                    Err(e) => format!("error: {}", truncate(e, 240)),
                },
                serde_json::to_string(&changes).unwrap_or_default()
            );
            match (&run, &verdict) {
                (Ok(_), Ok(())) => {
                    log::log(name, Kind::Skill, format!("verified {}: {outcome}", request.name));
                    return Learned {
                        result: Ok(skill),
                        calls,
                        attempts: attempt,
                    };
                }
                _ => {
                    let why = verdict.err().unwrap_or_else(|| "the skill threw an error".into());
                    log::log(name, Kind::Fail, format!("{} attempt {attempt} failed: {why}; {outcome}", request.name));
                    feedback.push(format!(
                        "Attempt {attempt} code:\n{}\nOutcome: {outcome}. Check {} failed: {why}",
                        sub.code.trim(),
                        sub.check
                    ));
                }
            }
        }
        Learned {
            result: Err(feedback.last().cloned().unwrap_or_else(|| "no attempts".into())),
            calls,
            attempts: self.retries,
        }
    }

    async fn write(&self, request: &SkillRequest, feedback: &[String]) -> anyhow::Result<(Option<Submission>, usize)> {
        let submitted: Arc<Mutex<Option<Submission>>> = Arc::default();
        let set = ToolSet::new();
        for sense in ["look_around", "inventory"] {
            if let Some(t) = self.tools.get(sense) {
                set.register(t);
            }
        }
        {
            let submitted = submitted.clone();
            let name = request.name.clone();
            set.register(ToolDef::new(
                "submit_skill",
                "Submit the skill code with test arguments and a world check. The skill is then run for real and checked. Ends your turn.",
                json!({
                    "type": "object",
                    "properties": {
                        "code": {"type": "string", "description": format!("Rhai source defining fn {name}(...)")},
                        "test_args": {"type": "object", "description": "Arguments for the test run, by parameter name, e.g. {\"n\": 3}"},
                        "check": {"type": "object", "description": "World check after the test run. Keys (all must hold): gain {item: n} (inventory grew by n; item may be 'log' or 'planks'), has {item: n}, block_within {block, distance}, near {x, y, z, distance}"}
                    },
                    "required": ["code", "test_args", "check"]
                }),
                move |args| {
                    let submitted = submitted.clone();
                    let name = name.clone();
                    async move {
                        let code = args["code"].as_str().unwrap_or_default().to_string();
                        let params = skills::check_syntax(&code, &name)?;
                        let test_args = match &args["test_args"] {
                            Value::String(s) => serde_json::from_str(s).unwrap_or(json!({})),
                            Value::Null => json!({}),
                            other => other.clone(),
                        };
                        let missing: Vec<&String> = params.iter().filter(|p| test_args.get(p.as_str()).is_none()).collect();
                        if !missing.is_empty() {
                            return Err(format!("test_args is missing {missing:?}"));
                        }
                        let check = match &args["check"] {
                            Value::String(s) => serde_json::from_str(s).map_err(|e| format!("check is not JSON: {e}"))?,
                            other => other.clone(),
                        };
                        if !check.is_object() || check.as_object().unwrap().is_empty() {
                            return Err("check must be an object with gain, has, block_within or near".into());
                        }
                        *submitted.lock().unwrap() = Some(Submission { code, test_args, check });
                        Ok(format!("compiled; parameters {params:?}; running the test"))
                    }
                },
            ));
        }
        let existing = self.library.signatures();
        let system = format!(
            "You write skills for a Minecraft bot in the Rhai scripting language. A skill is one Rhai function that \
             calls the bot API below. It runs in a sandbox with no other access.\n\n\
             Rhai basics: `fn name(a, b) {{ ... }}`, `let x = 1;`, `if c {{ }} else {{ }}`, `while c {{ }}`, \
             `for i in 0..n {{ }}`, strings in double quotes joined with +, maps `#{{a: 1}}` read with `m.a` or `m[\"a\"]`, \
             arrays `[1, 2]`, `try {{ }} catch (err) {{ }}`, `throw \"msg\";`. The last expression is the return value. \
             Integers and floats do not mix: use `to_int()` / `to_float()`.\n\n\
             {api}\n\n\
             Existing skills you may call: {existing}\n\n\
             Rules: define `fn {name}(...)` and optionally helpers named {name}_*; check the result with count() or \
             find_block(); throw an error with a clear message when the goal is not met; keep loops bounded \
             (at most a few retries). Then call submit_skill.\n\n\
             Example:\n\
             fn gather_wood(n) {{\n    let start = count(\"log\");\n    let tries = 0;\n    while count(\"log\") < start + n && tries < 4 {{\n        mine(\"log\", start + n - count(\"log\"));\n        tries += 1;\n    }}\n    let got = count(\"log\") - start;\n    if got < n {{ throw \"only got \" + got + \" logs\"; }}\n    \"got \" + got + \" logs\"\n}}",
            api = skills::API_DOCS,
            existing = if existing.is_empty() { "none".into() } else { existing.join("; ") },
            name = request.name,
        );
        let mut user = format!(
            "Write skill `{}`: {}\nSuccess means: {}\n\nWorld now: {}",
            request.name,
            request.description,
            request.success,
            self.body.snapshot()
        );
        if !feedback.is_empty() {
            user.push_str("\n\nEarlier attempts failed. Fix the cause:\n");
            user.push_str(&feedback.iter().rev().take(2).cloned().collect::<Vec<_>>().join("\n---\n"));
        }
        let stop = submitted.clone();
        let turn = Turn {
            system,
            user,
            tools: Arc::new(GdkTools::all(set)),
            tier: Tier::Strong,
            max_inferences: self.inferences_per_turn,
            stop_when: Some(Box::new(move |_| stop.lock().unwrap().is_some())),
        };
        let name = self.body.name.clone();
        let outcome = self
            .llm
            .run_turn(turn, |e| {
                if let Some(line) = describe_event(e) {
                    log::log(&name, Kind::Llm, truncate(&line, 200));
                }
            })
            .await?;
        let submission = submitted.lock().unwrap().clone();
        Ok((submission, outcome.model_calls()))
    }
}

fn verify(check: &Value, before: &WorldState, after: &WorldState, body: &Body) -> Result<(), String> {
    let total = |inv: &BTreeMap<String, i32>, item: &str| -> i32 {
        inv.iter().filter(|(k, _)| item_matches(item, k)).map(|(_, v)| *v).sum()
    };
    let Some(map) = check.as_object() else {
        return Err("check is not an object".into());
    };
    for (key, value) in map {
        match key.as_str() {
            "gain" => {
                for (item, n) in value.as_object().into_iter().flatten() {
                    let n = n.as_i64().unwrap_or(1) as i32;
                    let got = total(&after.inventory, item) - total(&before.inventory, item);
                    if got < n {
                        return Err(format!("expected to gain {n} {item}, gained {got}"));
                    }
                }
            }
            "has" => {
                for (item, n) in value.as_object().into_iter().flatten() {
                    let n = n.as_i64().unwrap_or(1) as i32;
                    let have = total(&after.inventory, item);
                    if have < n {
                        return Err(format!("expected to have {n} {item}, have {have}"));
                    }
                }
            }
            "block_within" => {
                let block = value["block"].as_str().unwrap_or_default();
                let distance = value["distance"].as_i64().unwrap_or(6) as i32;
                if body.find_block(block, distance).is_none() {
                    return Err(format!("no {block} within {distance} blocks"));
                }
            }
            "near" => {
                let target = azalea::Vec3::new(
                    value["x"].as_f64().unwrap_or(0.0),
                    value["y"].as_f64().unwrap_or(after.position.y),
                    value["z"].as_f64().unwrap_or(0.0),
                );
                let distance = value["distance"].as_f64().unwrap_or(3.0);
                let d = after.position.distance_to(target);
                if d > distance {
                    return Err(format!("ended {d:.0} blocks from the target"));
                }
            }
            other => return Err(format!("unknown check '{other}'")),
        }
    }
    Ok(())
}

/// After a goal: 1-3 short lessons for working memory.
pub async fn reflect(llm: &Llm, body: &Body, goal: &str, results: &[String], max_inferences: u32) -> (Vec<String>, usize) {
    let lessons: Arc<Mutex<Vec<String>>> = Arc::default();
    let set = ToolSet::new();
    {
        let lessons = lessons.clone();
        set.register(ToolDef::new(
            "record_lessons",
            "Record 1-3 short lessons (under 20 words each) worth remembering from this goal.",
            json!({
                "type": "object",
                "properties": {"lessons": {"type": "array", "items": {"type": "string"}}},
                "required": ["lessons"]
            }),
            move |args| {
                let lessons = lessons.clone();
                async move {
                    let list: Vec<String> = match &args["lessons"] {
                        Value::Array(a) => a.iter().filter_map(|v| v.as_str().map(str::to_string)).collect(),
                        Value::String(s) => vec![s.clone()],
                        _ => Vec::new(),
                    };
                    *lessons.lock().unwrap() = list.into_iter().take(3).collect();
                    Ok("recorded".into())
                }
            },
        ));
    }
    let stop = lessons.clone();
    let turn = Turn {
        system: "You help a Minecraft bot learn from experience. Write concrete, reusable lessons \
                 (what worked, what failed and why, what to do next time). Call record_lessons once."
            .into(),
        user: format!(
            "Goal finished: {goal}\nSteps and results:\n{}\nNow: {}",
            results.join("\n"),
            body.snapshot()
        ),
        tools: Arc::new(GdkTools::all(set)),
        tier: Tier::Fast,
        max_inferences: max_inferences.min(2),
        stop_when: Some(Box::new(move |_| !stop.lock().unwrap().is_empty())),
    };
    match llm.run_turn(turn, |_| {}).await {
        Ok(outcome) => (lessons.lock().unwrap().clone(), outcome.model_calls()),
        Err(e) => {
            log::log(&body.name, Kind::Fail, format!("reflection failed: {e:#}"));
            (Vec::new(), 0)
        }
    }
}
