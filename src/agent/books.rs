//! `write_book` and `read_book`: skills passed on through books in chests.
//!
//! A book holds the bot's rules and the code of the chosen skills in a plain
//! text format. Reading a book puts its skills into working memory, unsaved:
//! the reader still has to sleep to keep them, and each one is compiled first.

use std::sync::{Arc, Weak};

use serde_json::json;

use crate::{
    body::Body,
    brain::tools::{object_schema, ToolDef, ToolSet},
    log::{self, Kind},
    memory::{Memory, Skill},
    skills::{self, SkillLibrary},
};

/// Renders rules and skills as book text.
pub fn format_book(author: &str, generation: u32, rules: &[String], skills: &[Skill]) -> String {
    let mut text = format!("Skills of {author}, generation {generation}\n");
    for rule in rules {
        text.push_str(&format!("@rule {}\n", rule.replace('\n', " ")));
    }
    for skill in skills {
        text.push_str(&format!(
            "@skill {}\n@desc {}\n{}\n@end\n",
            skill.name,
            skill.description.replace('\n', " "),
            skill.code.trim()
        ));
    }
    text
}

/// Reads rules and skills back out of book text.
pub fn parse_book(text: &str) -> (Vec<String>, Vec<Skill>) {
    let mut rules = Vec::new();
    let mut skills = Vec::new();
    let mut current: Option<Skill> = None;
    for line in text.lines() {
        if let Some(rule) = line.strip_prefix("@rule ") {
            rules.push(rule.trim().to_string());
        } else if let Some(name) = line.strip_prefix("@skill ") {
            current = Some(Skill {
                name: name.trim().to_string(),
                description: String::new(),
                code: String::new(),
                saved: false,
            });
        } else if let (Some(desc), Some(skill)) = (line.strip_prefix("@desc "), current.as_mut()) {
            skill.description = desc.trim().to_string();
        } else if line.trim() == "@end" {
            skills.extend(current.take());
        } else if let Some(skill) = current.as_mut() {
            skill.code.push_str(line);
            skill.code.push('\n');
        }
    }
    (rules, skills)
}

pub fn register(set: &Arc<ToolSet>, library: &Arc<SkillLibrary>, body: &Body, memory: &Memory) {
    {
        let (body, memory) = (body.clone(), memory.clone());
        set.register(ToolDef::new(
            "write_book",
            "Write your rules and skills into a book and put it in the nearest chest, so another bot (or your next \
             generation) can read_book them. Needs a writable_book and a chest within 32 blocks.",
            object_schema(&[("skills", "string", "\"all\", or skill names separated by commas")]),
            move |args| {
                let (body, memory) = (body.clone(), memory.clone());
                async move {
                    let wanted = args["skills"].as_str().unwrap_or("all").trim().to_string();
                    let known = memory.skills();
                    let chosen: Vec<Skill> = if wanted.is_empty() || wanted == "all" {
                        known
                    } else {
                        let names: Vec<&str> = wanted.split(',').map(str::trim).collect();
                        let missing: Vec<&str> =
                            names.iter().copied().filter(|n| !known.iter().any(|k| k.name == *n)).collect();
                        if !missing.is_empty() {
                            return Err(format!("unknown skills: {}", missing.join(", ")));
                        }
                        known.into_iter().filter(|k| names.contains(&k.name.as_str())).collect()
                    };
                    if chosen.is_empty() && memory.rules().is_empty() {
                        return Err("nothing to write: no skills and no rules yet".into());
                    }
                    let generation = memory.generation();
                    let text = format_book(&body.name, generation, &memory.rules(), &chosen);
                    let title = format!("{} skills g{generation}", body.name);
                    let result = body.store_book(&title, &text).await?;
                    let names: Vec<&str> = chosen.iter().map(|s| s.name.as_str()).collect();
                    log::log(&body.name, Kind::Memory, format!("wrote a book: {}", names.join(", ")));
                    Ok(json!({"result": result, "skills": names}).to_string())
                }
            },
        ));
    }
    let weak: Weak<ToolSet> = Arc::downgrade(set);
    let (body, memory, library) = (body.clone(), memory.clone(), library.clone());
    set.register(ToolDef::new(
        "read_book",
        "Read the written books in your inventory and in chests nearby. Their skills join your working memory \
         (unsaved until you sleep) and become tools; their rules become lessons.",
        object_schema(&[]),
        move |_| {
            let (body, memory, library, weak) = (body.clone(), memory.clone(), library.clone(), weak.clone());
            async move {
                let books = body.read_books().await?;
                if books.is_empty() {
                    return Err("found no written books nearby".into());
                }
                let mut learned = Vec::new();
                let mut skipped = Vec::new();
                let mut lessons = 0;
                let rules = memory.rules();
                for book in &books {
                    let (book_rules, book_skills) = parse_book(&book.text);
                    for rule in book_rules {
                        if !rules.contains(&rule) {
                            let _ = memory.add_lesson(&format!("from {}'s book: {rule}", book.author));
                            lessons += 1;
                        }
                    }
                    for skill in book_skills {
                        if library.get(&skill.name).is_some() || learned.contains(&skill.name) {
                            skipped.push(format!("{} (already known)", skill.name));
                            continue;
                        }
                        if let Err(e) = skills::check_syntax(&skill.code, &skill.name) {
                            skipped.push(format!("{} (does not compile: {e})", skill.name));
                            continue;
                        }
                        memory.save_working_skill(&skill).map_err(|e| format!("could not remember {}: {e}", skill.name))?;
                        library.insert(skill.clone());
                        if let Some(set) = weak.upgrade() {
                            skills::register(&set, &library, &body, &skill);
                        }
                        log::log(&body.name, Kind::Skill, format!("read skill {} in {}'s book (unsaved)", skill.name, book.author));
                        learned.push(skill.name.clone());
                    }
                }
                let titles: Vec<String> = books.iter().map(|b| format!("{} by {}", b.title, b.author)).collect();
                Ok(json!({
                    "books": titles,
                    "learned_skills": learned,
                    "skipped": skipped,
                    "new_lessons": lessons,
                    "note": if learned.is_empty() { "" } else { "new skills are unsaved until you sleep" },
                })
                .to_string())
            }
        },
    ));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn book_round_trip() {
        let skills = vec![Skill {
            name: "gather_wood".into(),
            description: "gets logs".into(),
            code: "fn gather_wood(count) {\n    mine(\"log\", count)\n}".into(),
            saved: true,
        }];
        let text = format_book("Ada", 2, &["sleep before dark".into()], &skills);
        let (rules, read) = parse_book(&text);
        assert_eq!(rules, vec!["sleep before dark"]);
        assert_eq!(read.len(), 1);
        assert_eq!(read[0].name, "gather_wood");
        assert_eq!(read[0].description, "gets logs");
        assert_eq!(read[0].code.trim(), skills[0].code);
        assert!(!read[0].saved);
    }
}
