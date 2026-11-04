//! The dream: consolidating working memory while the bot sleeps.
//!
//! One GDK turn (strong tier) reads everything the bot knows: saved and
//! unsaved skills with their code, rules, lessons, places, recent events and
//! any nightmares (other bots' deaths). It must call `finish_dream` with the
//! skills to keep in long-term memory (merged, fixed or as they are), the
//! skills to drop, the new rule list, the new place list, a journal entry and
//! a one-line chat summary. Skill code is compiled inside the turn, so broken
//! fixes come straight back to the model.
//!
//! If the model fails or the budget is spent, the bot still sleeps: verified
//! working skills are kept as they are and lessons become rules.

use std::sync::{Arc, Mutex};

use serde_json::{json, Value};

use crate::{
    body::Body,
    brain::{
        describe_event,
        tools::{GdkTools, ToolDef, ToolSet},
        truncate, Llm, Tier, Turn,
    },
    log::{self, Kind},
    memory::{DreamResult, Memory, Skill},
    skills,
};

pub struct DreamInput {
    pub memory: Memory,
    pub recent: Vec<String>,
    pub nightmares: Vec<String>,
    pub places: Vec<String>,
}

/// Consolidation without a model: keep verified skills, lessons become rules.
pub fn plain_dream(input: &DreamInput) -> DreamResult {
    let memory = &input.memory;
    let mut rules = memory.rules();
    for lesson in memory.lessons().into_iter().chain(input.nightmares.iter().cloned()) {
        if !rules.contains(&lesson) {
            rules.push(lesson);
        }
    }
    let keep = rules.len().saturating_sub(15);
    rules.drain(..keep);
    let mut map = memory.map_notes();
    for place in &input.places {
        if !map.contains(place) {
            map.push(place.clone());
        }
    }
    let skills = memory.working_skills();
    let names: Vec<String> = skills.iter().map(|s| s.name.clone()).collect();
    DreamResult {
        journal: format!(
            "A dreamless sleep. Kept {} and wrote down what I learned.",
            if names.is_empty() { "nothing new".into() } else { names.join(", ") }
        ),
        summary: format!("saved {} skills, {} rules", names.len(), rules.len()),
        skills,
        drop: Vec::new(),
        rules,
        map,
    }
}

pub async fn dream(llm: &Llm, body: &Body, input: &DreamInput, max_inferences: u32) -> (DreamResult, usize) {
    let memory = &input.memory;
    let fallback = plain_dream(input);
    let result: Arc<Mutex<Option<DreamResult>>> = Arc::default();
    let set = ToolSet::new();
    {
        let result = result.clone();
        let known: Vec<String> = memory.skills().iter().map(|s| s.name.clone()).collect();
        set.register(ToolDef::new(
            "finish_dream",
            "Write the consolidated long-term memory. Ends the dream.",
            json!({
                "type": "object",
                "properties": {
                    "skills": {
                        "type": "array",
                        "description": "Every skill to keep in long-term memory, with final code. Include unchanged skills you keep. Merge duplicates into one; fix broken ones.",
                        "items": {
                            "type": "object",
                            "properties": {
                                "name": {"type": "string"},
                                "description": {"type": "string"},
                                "code": {"type": "string", "description": "Rhai source defining fn <name>(...)"}
                            },
                            "required": ["name", "description", "code"]
                        }
                    },
                    "drop": {"type": "array", "items": {"type": "string"}, "description": "Names of skills to forget (duplicates you merged, broken ones you could not fix)"},
                    "rules": {"type": "array", "items": {"type": "string"}, "description": "The full new list of durable rules, at most 15, each under 20 words"},
                    "map": {"type": "array", "items": {"type": "string"}, "description": "The full new list of places worth remembering, with coordinates"},
                    "journal": {"type": "string", "description": "A short dream journal entry in first person, 2-5 sentences: what happened today and what you will remember"},
                    "summary": {"type": "string", "description": "One chat line under 15 words saying what you consolidated"}
                },
                "required": ["skills", "drop", "rules", "journal", "summary"]
            }),
            move |args| {
                let result = result.clone();
                let known = known.clone();
                async move {
                    let mut kept = Vec::new();
                    let mut problems = Vec::new();
                    for s in args["skills"].as_array().into_iter().flatten() {
                        let name = s["name"].as_str().unwrap_or_default().to_string();
                        let code = s["code"].as_str().unwrap_or_default().to_string();
                        if let Err(e) = skills::check_syntax(&code, &name) {
                            problems.push(format!("{name}: {e}"));
                            continue;
                        }
                        kept.push(Skill {
                            name,
                            description: s["description"].as_str().unwrap_or_default().to_string(),
                            code,
                            saved: true,
                        });
                    }
                    if !problems.is_empty() {
                        return Err(format!("fix these skills and call finish_dream again: {}", problems.join("; ")));
                    }
                    let list = |v: &Value| -> Vec<String> {
                        v.as_array()
                            .into_iter()
                            .flatten()
                            .filter_map(|x| x.as_str().map(|s| s.trim().to_string()))
                            .filter(|s| !s.is_empty())
                            .collect()
                    };
                    let mut drop = list(&args["drop"]);
                    drop.retain(|d| known.contains(d) && !kept.iter().any(|k| &k.name == d));
                    let mut rules = list(&args["rules"]);
                    rules.truncate(15);
                    let mut map = list(&args["map"]);
                    map.truncate(15);
                    *result.lock().unwrap() = Some(DreamResult {
                        skills: kept,
                        drop,
                        rules,
                        map,
                        journal: args["journal"].as_str().unwrap_or_default().to_string(),
                        summary: args["summary"].as_str().unwrap_or_default().to_string(),
                    });
                    Ok("memory consolidated".into())
                }
            },
        ));
    }

    let skill_text = |list: Vec<Skill>| -> String {
        if list.is_empty() {
            return "none\n".into();
        }
        list.iter()
            .map(|s| format!("### {} - {}\n{}\n", s.name, s.description, s.code.trim()))
            .collect()
    };
    let user = format!(
        "You are {name}, asleep. Review today and rewrite your long-term memory.\n\n\
         == Saved skills ==\n{saved}\n== Unsaved skills learned today (verified when learned) ==\n{working}\n\
         == Rules ==\n{rules}\n\n== Lessons from today ==\n{lessons}\n\n== Places ==\n{places}\n\n\
         == Today ==\n{recent}\n\n== Nightmares (how other bots died) ==\n{nightmares}\n\n\
         Merge duplicate skills, fix or drop broken ones, keep good ones as they are, distill lessons and \
         nightmares into short durable rules, keep useful places, then call finish_dream.",
        name = body.name,
        saved = skill_text(memory.long_term_skills()),
        working = skill_text(memory.working_skills()),
        rules = bullet(&memory.rules()),
        lessons = bullet(&memory.lessons()),
        places = bullet(&memory.map_notes().into_iter().chain(input.places.iter().cloned()).collect::<Vec<_>>()),
        recent = bullet(&input.recent),
        nightmares = bullet(&input.nightmares),
    );
    let stop = result.clone();
    let turn = Turn {
        system: format!(
            "You consolidate a Minecraft bot's memory during sleep. Skills are Rhai functions over this API:\n{}\n\
             Be conservative with code: only change a skill to merge it or to fix a clear bug.",
            skills::API_DOCS
        ),
        user,
        tools: Arc::new(GdkTools::all(set)),
        tier: Tier::Strong,
        max_inferences,
        stop_when: Some(Box::new(move |_| stop.lock().unwrap().is_some())),
    };
    let name = body.name.clone();
    let outcome = llm
        .run_turn(turn, |e| {
            if let Some(line) = describe_event(e) {
                log::log(&name, Kind::Llm, truncate(&line, 200));
            }
        })
        .await;
    let calls = outcome.as_ref().map(|o| o.model_calls()).unwrap_or(0);
    if let Err(e) = &outcome {
        log::log(&body.name, Kind::Fail, format!("dream failed, sleeping without one: {e:#}"));
    }
    let mut dreamt = result.lock().unwrap().clone().unwrap_or(fallback);
    // A verified skill must never vanish just because the model left it out.
    for skill in memory.working_skills() {
        if !dreamt.skills.iter().any(|s| s.name == skill.name) && !dreamt.drop.contains(&skill.name) {
            dreamt.skills.push(Skill { saved: true, ..skill });
        }
    }
    if dreamt.journal.trim().is_empty() {
        dreamt.journal = "I slept.".into();
    }
    if dreamt.summary.trim().is_empty() {
        dreamt.summary = format!("saved {} skills", dreamt.skills.len());
    }
    (dreamt, calls)
}

fn bullet(items: &[String]) -> String {
    if items.is_empty() {
        "none".into()
    } else {
        items.iter().map(|i| format!("- {i}")).collect::<Vec<_>>().join("\n")
    }
}
