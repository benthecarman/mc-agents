//! A bot's memory: a git repo under `memory/<bot>/`.
//!
//! ```text
//! long_term/skills/*.rhai   committed skills
//! long_term/rules.md        durable rules distilled from lessons
//! long_term/map.md          places worth remembering (beds, tables, ore)
//! long_term/journal.md      dream journal
//! working/skills/*.rhai     skills learned since the last sleep (untracked)
//! working/lessons.md        lessons since the last sleep (untracked)
//! ```
//!
//! Sleeping commits: the dream rewrites `long_term/` from both trees and
//! empties `working/`. Dying forgets: `git checkout -- . && git clean -fd`
//! puts the tree back to the last commit, which deletes `working/`.

use std::{
    fs,
    path::{Path, PathBuf},
    process::Command,
};

use anyhow::{bail, Context, Result};

#[derive(Debug, Clone)]
pub struct Skill {
    pub name: String,
    pub description: String,
    pub code: String,
    /// In long-term memory (committed), as opposed to working memory.
    pub saved: bool,
}

impl Skill {
    pub fn to_file(&self) -> String {
        format!(
            "// skill: {}\n// description: {}\n{}\n",
            self.name,
            self.description.replace('\n', " "),
            self.code.trim_end()
        )
    }

    fn from_file(text: &str, saved: bool) -> Option<Self> {
        let mut name = None;
        let mut description = String::new();
        let mut code = Vec::new();
        for line in text.lines() {
            if let Some(n) = line.strip_prefix("// skill: ") {
                name = Some(n.trim().to_string());
            } else if let Some(d) = line.strip_prefix("// description: ") {
                description = d.trim().to_string();
            } else {
                code.push(line);
            }
        }
        Some(Self {
            name: name?,
            description,
            code: code.join("\n"),
            saved,
        })
    }
}

#[derive(Clone)]
pub struct Memory {
    root: PathBuf,
    bot: String,
}

#[derive(Debug, Clone, Copy, Default)]
pub struct Unsaved {
    pub skills: usize,
    pub lessons: usize,
}

impl Memory {
    pub fn open(base: impl AsRef<Path>, bot: &str) -> Result<Self> {
        let root = base.as_ref().join(bot);
        fs::create_dir_all(root.join("long_term/skills"))?;
        fs::create_dir_all(root.join("working/skills"))?;
        let memory = Self {
            root,
            bot: bot.to_string(),
        };
        if !memory.root.join(".git").exists() {
            memory.git(&["init", "-q", "-b", "main"])?;
            for (file, text) in [
                ("long_term/rules.md", "# Rules\n"),
                ("long_term/map.md", "# Map\n"),
                ("long_term/journal.md", "# Dream journal\n"),
                ("long_term/skills/.keep", ""),
            ] {
                fs::write(memory.root.join(file), text)?;
            }
            // working/ is never committed directly.
            fs::write(memory.root.join(".gitignore"), "")?;
            memory.git(&["add", "-A"])?;
            memory.commit(&format!("{bot} is born"))?;
        }
        Ok(memory)
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn git(&self, args: &[&str]) -> Result<String> {
        let output = Command::new("git")
            .args(["-c", &format!("user.name={}", self.bot)])
            .args(["-c", &format!("user.email={}@mc-agents.local", self.bot.to_lowercase())])
            .args(["-c", "commit.gpgsign=false", "-c", "core.hooksPath=/dev/null"])
            .args(args)
            .current_dir(&self.root)
            .output()
            .with_context(|| format!("running git {}", args.join(" ")))?;
        if !output.status.success() {
            bail!(
                "git {} failed: {}",
                args.join(" "),
                String::from_utf8_lossy(&output.stderr).trim()
            );
        }
        Ok(String::from_utf8_lossy(&output.stdout).to_string())
    }

    pub fn commit(&self, message: &str) -> Result<()> {
        self.git(&["add", "-A", "long_term"])?;
        self.git(&["commit", "-q", "--allow-empty", "-m", message])?;
        Ok(())
    }

    fn read(&self, rel: &str) -> String {
        fs::read_to_string(self.root.join(rel)).unwrap_or_default()
    }

    fn bullets(&self, rel: &str) -> Vec<String> {
        self.read(rel)
            .lines()
            .filter_map(|l| l.strip_prefix("- ").map(str::to_string))
            .collect()
    }

    fn skills_in(&self, dir: &str, saved: bool) -> Vec<Skill> {
        let mut skills: Vec<Skill> = fs::read_dir(self.root.join(dir))
            .into_iter()
            .flatten()
            .flatten()
            .filter(|e| e.path().extension().is_some_and(|x| x == "rhai"))
            .filter_map(|e| Skill::from_file(&fs::read_to_string(e.path()).ok()?, saved))
            .collect();
        skills.sort_by(|a, b| a.name.cmp(&b.name));
        skills
    }

    pub fn long_term_skills(&self) -> Vec<Skill> {
        self.skills_in("long_term/skills", true)
    }

    pub fn working_skills(&self) -> Vec<Skill> {
        self.skills_in("working/skills", false)
    }

    /// All skills the bot knows; a working copy overrides a saved one.
    pub fn skills(&self) -> Vec<Skill> {
        let mut skills = self.long_term_skills();
        for skill in self.working_skills() {
            skills.retain(|s| s.name != skill.name);
            skills.push(skill);
        }
        skills
    }

    pub fn save_working_skill(&self, skill: &Skill) -> Result<()> {
        fs::write(
            self.root.join(format!("working/skills/{}.rhai", skill.name)),
            skill.to_file(),
        )?;
        Ok(())
    }

    pub fn lessons(&self) -> Vec<String> {
        self.bullets("working/lessons.md")
    }

    pub fn add_lesson(&self, lesson: &str) -> Result<()> {
        let lesson = lesson.trim().trim_start_matches("- ").replace('\n', " ");
        if lesson.is_empty() {
            return Ok(());
        }
        let mut text = self.read("working/lessons.md");
        if text.is_empty() {
            text.push_str("# Lessons\n");
        }
        text.push_str(&format!("- {lesson}\n"));
        fs::write(self.root.join("working/lessons.md"), text)?;
        Ok(())
    }

    pub fn rules(&self) -> Vec<String> {
        self.bullets("long_term/rules.md")
    }

    pub fn map_notes(&self) -> Vec<String> {
        self.bullets("long_term/map.md")
    }

    pub fn journal(&self) -> String {
        self.read("long_term/journal.md")
    }

    pub fn unsaved(&self) -> Unsaved {
        Unsaved {
            skills: self.working_skills().len(),
            lessons: self.lessons().len(),
        }
    }

    pub fn long_term_summary(&self) -> String {
        let rules = self.rules();
        let map = self.map_notes();
        let skills: Vec<String> = self.long_term_skills().iter().map(|s| s.name.clone()).collect();
        let mut out = String::new();
        out.push_str(&format!(
            "Rules: {}\n",
            if rules.is_empty() { "none yet".into() } else { rules.join("; ") }
        ));
        if !map.is_empty() {
            out.push_str(&format!("Places: {}\n", map.join("; ")));
        }
        out.push_str(&format!(
            "Saved skills: {}",
            if skills.is_empty() { "none".into() } else { skills.join(", ") }
        ));
        out
    }

    pub fn working_summary(&self) -> String {
        let skills: Vec<String> = self.working_skills().iter().map(|s| s.name.clone()).collect();
        let lessons = self.lessons();
        format!(
            "Unsaved skills: {}\nUnsaved lessons: {}",
            if skills.is_empty() { "none".into() } else { skills.join(", ") },
            if lessons.is_empty() { "none".into() } else { lessons.join("; ") }
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn working_memory_overrides_and_counts() {
        let dir = std::env::temp_dir().join(format!("mc-mem-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        let m = Memory::open(&dir, "Test").unwrap();
        m.save_working_skill(&Skill {
            name: "gather_wood".into(),
            description: "gets logs".into(),
            code: "mine(\"log\", 4);".into(),
            saved: false,
        })
        .unwrap();
        m.add_lesson("logs need no tool").unwrap();
        let u = m.unsaved();
        assert_eq!((u.skills, u.lessons), (1, 1));
        let skill = &m.skills()[0];
        assert_eq!(skill.name, "gather_wood");
        assert!(skill.code.contains("mine("));
        fs::remove_dir_all(&dir).unwrap();
    }
}
