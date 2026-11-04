//! The custom agent loop.
//!
//! ```text
//! loop {
//!   event = next_event()
//!   if reflex(event): act in plain Rust, report it, resume the plan
//!   if !needs_thought(event): continue
//!   ctx  = long-term memory + working memory + snapshot + stakes + event
//!   plan = one GDK turn (fast or strong model)
//!   executor.replace(plan)   // runs without the model; wakes us on done/failure
//! }
//! ```
//!
//! Thinking runs in a background task so reflexes keep working while the
//! model is busy. Events that need thought while a thought is in flight are
//! queued and folded into the next one.

/// Chat lines that answer test commands start with this; bots ignore them.
const DEBUG_TAG: &str = "[debug]";

pub mod books;
pub mod dream;
pub mod executor;
pub mod learn;
pub mod plan;
pub mod tools;

use std::{
    collections::{HashMap, VecDeque},
    sync::Arc,
    time::{Duration, Instant},
};

use tokio::sync::mpsc;

use self::{
    executor::Executor,
    plan::{Decision, Plan},
};
use crate::{
    body::{Body, TimeOfDay},
    brain::{describe_event, tools::GdkTools, tools::ToolSet, truncate, Llm, Tier, Turn},
    config::{BotConfig, Config},
    log::{self, Kind},
    memory::{DreamResult, Memory, Nightmares},
    skills::{self, SkillLibrary},
};

#[derive(Debug, Clone)]
pub enum BotEvent {
    // From the game.
    Spawned,
    Death,
    Chat { from: String, text: String, from_bot: bool },
    Damage { attacker: Option<u32> },
    // From the ticker.
    Dusk { seconds_left: u64 },
    Night,
    BedEntered,
    InventoryFull,
    LowHealth,
    Hungry,
    // From the executor.
    PlanDone { plan_id: u64 },
    StepFailed { plan_id: u64, step: String, error: String },
    // From our own thinking task.
    /// `spoke` is true when the model already used `say`; `text` is any
    /// plain-text reply it gave instead.
    ThoughtDone { decision: Decision, spoke: bool, text: String, tier: Tier, calls: usize },
    ThoughtFailed { error: String },
    // From the skill learner and reflection.
    SkillLearned { name: String, skill: Option<crate::memory::Skill>, detail: String, calls: usize, attempts: u32 },
    Reflected { lessons: Vec<String>, calls: usize },
    DreamDone { dream: DreamResult, calls: usize },
    /// Every few seconds, so the agent can notice it has nothing to do.
    Heartbeat,
    /// `!<bot> <command>` typed in chat, for testing.
    Debug { command: String },
}

pub fn start(body: Body, bot: BotConfig, config: Config) -> mpsc::UnboundedSender<BotEvent> {
    let (tx, rx) = mpsc::unbounded_channel();
    let agent = match Agent::new(body, bot.clone(), config, tx.clone()) {
        Ok(agent) => agent,
        Err(e) => {
            log::log(&bot.name, Kind::Fail, format!("agent failed to start: {e:#}"));
            return tx;
        }
    };
    tokio::spawn(agent.run(rx));
    tx
}

struct Agent {
    name: String,
    bot: BotConfig,
    config: Config,
    body: Body,
    llm: Llm,
    memory: Memory,
    tools: Arc<ToolSet>,
    library: Arc<SkillLibrary>,
    executor: Executor,
    events: mpsc::UnboundedSender<BotEvent>,
    goal: Option<String>,
    thinking: bool,
    pending: Vec<String>,
    pending_tier: Tier,
    failures_in_a_row: u32,
    calls_today: (u64, u32),
    budget_warned: bool,
    recent: VecDeque<String>,
    last_reflex: HashMap<&'static str, Instant>,
    dreaming: bool,
    nightmares: Nightmares,
    last_hurt: Option<(String, Instant)>,
    /// Model work in flight (thinking, learning, reflecting, dreaming).
    /// Death aborts it so nothing from the old life lands in the new one.
    mind: Vec<tokio::task::AbortHandle>,
    last_dream: Option<Instant>,
    late_lessons: Vec<String>,
    /// Last time the bot thought or had a goal; drives picking its own goals.
    busy_at: Instant,
    /// Bots answering bots can loop forever; they get one reply per cooldown.
    last_bot_reply: Option<Instant>,
}

impl Agent {
    fn new(body: Body, bot: BotConfig, config: Config, events: mpsc::UnboundedSender<BotEvent>) -> anyhow::Result<Self> {
        let llm = Llm::from_config(&config.llm)?
            .with_models(bot.fast_model.as_deref(), bot.strong_model.as_deref())
            .with_timeout(Duration::from_secs(config.limits.turn_timeout_secs));
        let memory = Memory::open(crate::memory::base_dir(), &bot.name)?;
        let tools = tools::core_tools(body.clone());
        let library = Arc::new(SkillLibrary::default());
        for skill in memory.skills() {
            log::log(&bot.name, Kind::Memory, format!("remembers skill {}{}", skill.name, if skill.saved { "" } else { " (unsaved)" }));
            library.insert(skill.clone());
            skills::register(&tools, &library, &body, &skill);
        }
        {
            let library = library.clone();
            tools.register(crate::brain::tools::ToolDef::new(
                "list_skills",
                "List learned skills with their arguments.",
                serde_json::json!({"type": "object", "properties": {}}),
                move |_| {
                    let sigs = library.signatures();
                    async move { Ok(if sigs.is_empty() { "no skills yet".into() } else { sigs.join("\n") }) }
                },
            ));
        }
        books::register(&tools, &library, &body, &memory);
        let nightmares = Nightmares::new(crate::memory::base_dir(), &bot.name);
        let executor = Executor::new(body.clone(), tools.clone(), events.clone());
        Ok(Self {
            name: bot.name.clone(),
            bot,
            config,
            body,
            llm,
            memory,
            tools,
            library,
            executor,
            events,
            goal: None,
            thinking: false,
            pending: Vec::new(),
            pending_tier: Tier::Fast,
            failures_in_a_row: 0,
            calls_today: (0, 0),
            budget_warned: false,
            recent: VecDeque::new(),
            last_reflex: HashMap::new(),
            dreaming: false,
            nightmares,
            last_hurt: None,
            mind: Vec::new(),
            last_dream: None,
            late_lessons: Vec::new(),
            busy_at: Instant::now(),
            last_bot_reply: None,
        })
    }

    fn note(&mut self, text: impl Into<String>) {
        self.recent.push_back(text.into());
        while self.recent.len() > 12 {
            self.recent.pop_front();
        }
    }

    async fn run(mut self, mut rx: mpsc::UnboundedReceiver<BotEvent>) {
        spawn_ticker(self.body.clone(), self.events.clone());
        while let Some(event) = rx.recv().await {
            self.log_event(&event);
            if self.reflex(&event).await {
                continue;
            }
            if let Some((text, tier)) = self.needs_thought(&event) {
                self.request_thought(text, tier);
            }
        }
    }

    fn log_event(&self, event: &BotEvent) {
        let text = match event {
            BotEvent::ThoughtDone { .. }
            | BotEvent::ThoughtFailed { .. }
            | BotEvent::SkillLearned { .. }
            | BotEvent::Reflected { .. }
            | BotEvent::DreamDone { .. }
            | BotEvent::Heartbeat => return,
            BotEvent::Chat { from, text, .. } => format!("chat <{from}> {text}"),
            BotEvent::StepFailed { step, error, .. } => format!("step_failed {step}: {}", truncate(error, 160)),
            BotEvent::PlanDone { .. } => "goal_done".into(),
            other => format!("{other:?}").to_lowercase(),
        };
        log::log(&self.name, Kind::Event, text);
    }

    fn reflex_ready(&mut self, key: &'static str, every: Duration) -> bool {
        let now = Instant::now();
        match self.last_reflex.get(key) {
            Some(t) if now.duration_since(*t) < every => false,
            _ => {
                self.last_reflex.insert(key, now);
                true
            }
        }
    }

    /// Fast reactions in plain Rust. Returns true when the event is handled.
    async fn reflex(&mut self, event: &BotEvent) -> bool {
        if let BotEvent::Damage { attacker } = event {
            let what = attacker
                .and_then(|id| attacker_kind(&self.body, id))
                .unwrap_or_else(|| if self.body.head_in_water() { "drowning".into() } else { "the environment".into() });
            self.last_hurt = Some((what, Instant::now()));
        }
        if self.body.health() <= 0.0 && !matches!(event, BotEvent::Death) {
            // Already dead: the death event is next, don't act on a corpse.
            return true;
        }
        if self.dreaming && self.body.is_sleeping() && !matches!(event, BotEvent::Death) {
            // Asleep: nothing to react to but death.
            return matches!(event, BotEvent::Damage { .. } | BotEvent::LowHealth | BotEvent::Hungry);
        }
        match event {
            BotEvent::LowHealth if self.reflex_ready("flee", Duration::from_secs(6)) => {
                self.executor.pause();
                let result = self.body.flee().await;
                self.report_reflex("flee", &result);
                self.executor.resume();
                true
            }
            BotEvent::Damage { attacker: Some(id) } if self.body.health() > 6.0 => {
                let Some(kind) = attacker_kind(&self.body, *id) else { return true };
                if kind == "player" {
                    return true;
                }
                if !self.reflex_ready("fight", Duration::from_secs(4)) {
                    return true;
                }
                self.executor.pause();
                let result = self.body.attack(Some(&kind)).await;
                self.report_reflex("fight back", &result);
                self.executor.resume();
                true
            }
            BotEvent::Damage { attacker: None } => {
                if self.reflex_ready("hazard", Duration::from_secs(3)) {
                    self.executor.pause();
                    let result = self.body.escape_hazard().await;
                    self.report_reflex("escape hazard", &result);
                    self.executor.resume();
                }
                true
            }
            BotEvent::Damage { .. } => true,
            BotEvent::Hungry if self.reflex_ready("eat", Duration::from_secs(20)) => {
                self.executor.pause();
                let result = self.body.eat().await;
                self.report_reflex("eat", &result);
                self.executor.resume();
                true
            }
            BotEvent::LowHealth | BotEvent::Hungry => true,
            _ => false,
        }
    }

    fn report_reflex(&mut self, what: &str, result: &Result<String, String>) {
        let text = match result {
            Ok(t) => t.clone(),
            Err(e) => format!("failed: {e}"),
        };
        log::log(&self.name, Kind::Reflex, format!("{what}: {}", truncate(&text, 160)));
        self.note(format!("reflex {what}: {}", truncate(&text, 80)));
    }

    /// Decides whether an event deserves a model call, and which model.
    fn needs_thought(&mut self, event: &BotEvent) -> Option<(String, Tier)> {
        match event {
            BotEvent::Spawned => None,
            BotEvent::Chat { from, text, from_bot } => {
                if !self.addressed(text, *from_bot) {
                    return None;
                }
                self.note(format!("{from} said: {text}"));
                if *from_bot {
                    if self.last_bot_reply.is_some_and(|t| t.elapsed() < Duration::from_secs(45)) {
                        return None;
                    }
                    self.last_bot_reply = Some(Instant::now());
                }
                let who = if *from_bot { "Your fellow bot" } else { "Player" };
                Some((format!("{who} {from} says to you: \"{text}\""), Tier::Fast))
            }
            BotEvent::ThoughtDone { decision, spoke, text, tier, calls } => {
                self.on_thought(decision.clone(), *spoke, text, *tier, *calls);
                None
            }
            BotEvent::ThoughtFailed { error } => {
                log::log(&self.name, Kind::Fail, format!("thinking failed: {error}"));
                self.thinking = false;
                self.flush_pending();
                None
            }
            BotEvent::PlanDone { plan_id } if *plan_id == self.executor.plan_id() => {
                self.failures_in_a_row = 0;
                let goal = self.executor.goal().unwrap_or_default();
                if self.executor.completes_goal() {
                    log::log(&self.name, Kind::Plan, format!("goal done: {goal}"));
                    self.note(format!("finished goal: {goal}"));
                    self.on_goal_done(&goal);
                    None
                } else {
                    Some((
                        format!("The plan finished but the goal '{goal}' is not complete. Results: {}. Plan the next part.", self.executor.recent_results().join("; ")),
                        Tier::Strong,
                    ))
                }
            }
            BotEvent::StepFailed { plan_id, step, error } if *plan_id == self.executor.plan_id() => {
                self.failures_in_a_row += 1;
                self.note(format!("{step} failed: {}", truncate(error, 100)));
                let tier = if self.failures_in_a_row >= 2 { Tier::Strong } else { Tier::Fast };
                if self.failures_in_a_row > 5 {
                    log::log(&self.name, Kind::Fail, "too many failures in a row, giving up on the goal");
                    let _ = self.body.say("I keep failing at this. Giving up for now.");
                    self.executor.clear();
                    self.goal = None;
                    self.failures_in_a_row = 0;
                    return None;
                }
                Some((
                    format!("Step {step} failed: {error}. Results so far: {}. Fix the plan.", self.executor.recent_results().join("; ")),
                    tier,
                ))
            }
            BotEvent::PlanDone { .. } | BotEvent::StepFailed { .. } => None,
            BotEvent::Dusk { seconds_left } => Some((
                format!(
                    "Dusk: about {seconds_left} seconds until dark. Decide now, weighing the stakes and your risk appetite: \
                     race back to a bed (sleeping saves your memory) or push on. Say your choice and the reason in chat."
                ),
                Tier::Fast,
            )),
            BotEvent::Night => Some((
                "It is night. Monsters spawn in the dark. Decide, weighing the stakes and your risk appetite: race back to a bed \
                 or push on. Say your choice and the reason in chat."
                    .into(),
                Tier::Fast,
            )),
            BotEvent::InventoryFull => Some(("Your inventory is full.".into(), Tier::Fast)),
            BotEvent::Heartbeat => self.idle_thought(),
            BotEvent::BedEntered => {
                // One dream per sleep: the pose and the sleep tool both report it.
                let recently = self.last_dream.is_some_and(|t| t.elapsed() < Duration::from_secs(60));
                if !self.dreaming && !recently {
                    self.start_dream();
                }
                None
            }
            BotEvent::DreamDone { dream, calls } => {
                self.calls_today.1 += *calls as u32;
                self.finish_dream(dream);
                if let Some(goal) = self.goal.clone() {
                    Some((format!("You woke up after sleeping. Continue the goal '{goal}'."), Tier::Strong))
                } else {
                    None
                }
            }
            BotEvent::Death => {
                self.on_death();
                None
            }
            BotEvent::Debug { command } => {
                self.debug(command.clone());
                None
            }
            BotEvent::Damage { .. } | BotEvent::LowHealth | BotEvent::Hungry => None,
            BotEvent::SkillLearned { name, skill, detail, calls, attempts } => {
                self.thinking = false;
                self.calls_today.1 += *calls as u32;
                match skill {
                    Some(skill) => {
                        if let Err(e) = self.memory.save_working_skill(skill) {
                            log::log(&self.name, Kind::Fail, format!("saving skill: {e:#}"));
                        }
                        self.library.insert(skill.clone());
                        skills::register(&self.tools, &self.library, &self.body, skill);
                        log::log(&self.name, Kind::Skill, format!("learned {name} in {attempts} attempt(s); registered as a tool (unsaved until I sleep)"));
                        let _ = self.body.say(&format!("I learned a new skill: {name}. I must sleep to remember it."));
                        self.note(format!("learned skill {name}"));
                        let sig = self.library.signatures().into_iter().find(|s| s.starts_with(&format!("{name}("))).unwrap_or_default();
                        self.pending.insert(0, format!("You just learned skill {sig}; it ran once already as a test and is now an action tool. Continue the goal '{}'.", self.goal.clone().unwrap_or_default()));
                    }
                    None => {
                        log::log(&self.name, Kind::Fail, format!("could not learn {name} after {attempts} attempts"));
                        let _ = self.body.say(&format!("I couldn't figure out {name}."));
                        self.note(format!("failed to learn {name}"));
                        self.pending.insert(0, format!("Learning skill {name} failed: {}. Plan with the tools you have, without that skill.", truncate(detail, 300)));
                    }
                }
                self.pending_tier = Tier::Strong;
                self.flush_pending();
                None
            }
            BotEvent::Reflected { lessons, calls } => {
                self.calls_today.1 += *calls as u32;
                for lesson in lessons {
                    log::log(&self.name, Kind::Memory, format!("lesson: {lesson}"));
                    let _ = self.memory.add_lesson(lesson);
                    if self.dreaming {
                        // Arrived after the dream started; keep it past consolidation.
                        self.late_lessons.push(lesson.clone());
                    }
                }
                None
            }
        }
    }

    /// Chat is for this bot if it names the bot, says "all"/"everyone"/"bots",
    /// or names no bot and this is the first bot in the config.
    fn addressed(&self, text: &str, from_bot: bool) -> bool {
        if text.starts_with(DEBUG_TAG) {
            return false;
        }
        let lower = text.to_lowercase();
        let mentions_me = lower.contains(&self.name.to_lowercase());
        if from_bot {
            return mentions_me;
        }
        let any_bot = self.config.bots.iter().any(|b| lower.contains(&b.name.to_lowercase()));
        let everyone = ["all", "everyone", "bots", "everybody"]
            .iter()
            .any(|w| lower.split(|c: char| !c.is_alphanumeric()).any(|t| t == *w));
        mentions_me || everyone || (!any_bot && self.config.bots.first().is_some_and(|b| b.name == self.name))
    }

    fn request_thought(&mut self, text: String, tier: Tier) {
        if self.thinking || self.dreaming {
            self.pending.push(text);
            if tier == Tier::Strong {
                self.pending_tier = Tier::Strong;
            }
            return;
        }
        let day = self.body.time().day();
        if self.calls_today.0 != day {
            self.calls_today = (day, 0);
            self.budget_warned = false;
        }
        if self.calls_today.1 >= self.config.limits.llm_calls_per_day {
            if !self.budget_warned {
                log::log(&self.name, Kind::Llm, "daily model-call budget spent; not thinking until tomorrow");
                let _ = self.body.say("My head hurts. No more thinking until tomorrow.");
                self.budget_warned = true;
            }
            return;
        }
        self.thinking = true;
        self.busy_at = Instant::now();
        self.think(text, tier, tier == Tier::Fast);
    }

    /// With nothing to do for a while, the bot picks a goal of its own.
    fn idle_thought(&mut self) -> Option<(String, Tier)> {
        let wait = self.config.limits.idle_goal_secs;
        let busy = self.thinking || self.dreaming || self.goal.is_some() || self.executor.goal().is_some();
        if busy {
            self.busy_at = Instant::now();
            return None;
        }
        if wait == 0 || self.busy_at.elapsed() < Duration::from_secs(wait) || self.body.is_sleeping() {
            return None;
        }
        // Keep a quarter of the day's budget for chat and emergencies.
        if self.calls_today.1 * 4 >= self.config.limits.llm_calls_per_day * 3 {
            return None;
        }
        self.busy_at = Instant::now();
        Some((
            "You have had nothing to do for a while. Pick a goal that fits your personality and the time of day (gather, craft tools, mine, build, prepare for night), announce it with say, and call set_goal."
                .into(),
            Tier::Fast,
        ))
    }

    fn flush_pending(&mut self) {
        if self.pending.is_empty() {
            return;
        }
        let text = std::mem::take(&mut self.pending).join("\nAlso: ");
        let tier = std::mem::replace(&mut self.pending_tier, Tier::Fast);
        self.request_thought(text, tier);
    }

    fn action_names(&self) -> Vec<String> {
        let mut names: Vec<String> = self
            .tools
            .names()
            .into_iter()
            .filter(|n| !tools::SENSE_TOOLS.contains(&n.as_str()))
            .collect();
        names.sort();
        names
    }

    fn stakes(&self) -> String {
        let unsaved = self.memory.unsaved();
        let time = self.body.time();
        let bed = crate::body::nearest_bed_distance(&self.body)
            .map(|d| format!("{d:.0} blocks"))
            .unwrap_or_else(|| "none known".into());
        format!(
            "Unsaved: {} skills, {} lessons. Time: {} ({}). Nearest bed: {bed}. HP: {:.0}/20.",
            unsaved.skills,
            unsaved.lessons,
            time.phase(),
            if time.is_dark() { "dark now".to_string() } else { format!("{}s until dark", time.ticks_until_dark() / 20) },
            self.body.health()
        )
    }

    fn system_prompt(&self, escalate: bool) -> String {
        let actions = self.action_names();
        let risk = self.bot.risk_appetite;
        let risk_text = if risk < 0.4 {
            "You are cautious: when dusk comes, stop and head for a bed. Never stay out in the dark if you can help it."
        } else if risk > 0.6 {
            "You are greedy: at dusk, push on if the goal is close; you sleep only when it is safe and convenient, and you accept losing unsaved memory as a risk."
        } else {
            "You weigh it: head for bed at dusk unless the goal is nearly done."
        };
        let finish = if escalate {
            "End the turn with exactly one of: submit_plan (simple requests), set_goal (anything needing several actions), keep_plan, or learn_skill. Small talk needs only say."
        } else {
            "End the turn with exactly one of: submit_plan, keep_plan or learn_skill."
        };
        format!(
            "You are {name}, a bot in a Minecraft survival world. Personality: {personality}\n\
             Risk appetite: {risk:.1} (0 = never risks memory, 1 = gambles everything).\n\
             {lineage}\
             {others}\
             You think only when something happens; between thoughts your plan runs on its own.\n\n\
             Tools you call now: look_around and inventory (results come back to you), say (chat, under 20 words), and the decision tools.\n\
             Action tools you can use as plan steps:\n{tools}\n\n\
             Rules:\n\
             - Plans have 1-8 steps. Take positions from look_around; never invent coordinates.\n\
             - Logs need no tool. Stone and ores need a pickaxe; iron ore needs a stone pickaxe.\n\
             - Planks and sticks craft anywhere. Tools, chest, furnace and bed need a placed crafting_table within 32 blocks.\n\
             - Your memory is saved only when you sleep in a bed at night. If you die, you lose every unsaved skill and lesson.\n\
             - {risk_text}\n\
             - If a step failed, read the error and change the plan; do not repeat the same failing step.\n\
             - Learned skills are action tools too; prefer them. When a goal needs a reusable multi-step ability you lack \
             (e.g. gather_wood, make_tools, build_shelter), call learn_skill once, then plan with the new skill.\n\
             - {finish}",
            name = self.name,
            personality = self.bot.personality,
            others = self.others(),
            lineage = self.lineage(),
            tools = plan::describe_tools(&self.tools, &actions),
        )
    }

    /// Hardcore lineage: which generation this is, and what books are for.
    fn lineage(&self) -> String {
        let generation = self.memory.generation();
        let mut text = String::new();
        if self.config.limits.hardcore {
            text.push_str("Hardcore world: if you die, you lose ALL memory, saved or not. Only books in chests survive you.\n");
        }
        if generation > 1 {
            text.push_str(&format!(
                "You are generation {generation}: your earlier selves died. They may have left books of skills in chests nearby; \
                 read_book them before relearning anything.\n"
            ));
        }
        text.push_str("To pass skills on, write_book them into a chest (needs a writable_book).\n");
        text
    }

    /// The other bots, so chat between them makes sense.
    fn others(&self) -> String {
        let others: Vec<String> = self
            .config
            .bots
            .iter()
            .filter(|b| b.name != self.name)
            .map(|b| format!("{} ({})", b.name, b.personality))
            .collect();
        if others.is_empty() {
            String::new()
        } else {
            format!(
                "Other bots here, each with its own memory: {}. You may talk to them by name, but keep it short and don't chat back and forth.\n",
                others.join("; ")
            )
        }
    }

    fn context(&self, event: &str) -> String {
        let recent = if self.recent.is_empty() {
            "nothing yet".into()
        } else {
            self.recent.iter().cloned().collect::<Vec<_>>().join("; ")
        };
        format!(
            "== Long-term memory ==\n{}\n\n== Working memory (lost if you die before sleeping) ==\n{}\nRecent: {recent}\n\n== World ==\n{}\nGoal: {}. Current plan: {}\n\n== Stakes ==\n{}\n\n== Event ==\n{event}",
            self.memory.long_term_summary(),
            self.memory.working_summary(),
            self.body.snapshot(),
            self.goal.as_deref().unwrap_or("none"),
            self.executor.describe(),
            self.stakes(),
        )
    }

    /// One GDK turn in a background task. The result comes back as an event.
    fn think(&mut self, event: String, tier: Tier, escalate: bool) {
        let (set, decision) = plan::planning_tools(&self.tools, &tools::SENSE_TOOLS, self.action_names(), escalate);
        let spoke = Arc::new(std::sync::atomic::AtomicBool::new(false));
        {
            let spoke = spoke.clone();
            set.set_observer(Arc::new(move |name, _, result| {
                if name == "say" && result.is_ok() {
                    spoke.store(true, std::sync::atomic::Ordering::SeqCst);
                }
            }));
        }
        let stop_decision = decision.clone();
        let turn = Turn {
            system: self.system_prompt(escalate),
            user: self.context(&event),
            tools: Arc::new(GdkTools::all(set)),
            tier,
            max_inferences: self.config.limits.inferences_per_turn,
            stop_when: Some(Box::new(move |_| stop_decision.lock().unwrap().decided())),
        };
        log::log(
            &self.name,
            Kind::Think,
            format!("{} ({}): {}", tier_name(tier), self.llm.model_name(tier), truncate(&event, 160)),
        );
        let llm = self.llm.clone();
        let name = self.name.clone();
        let events = self.events.clone();
        self.spawn_mind(async move {
            let log_name = name.clone();
            let outcome = llm
                .run_turn(turn, move |e| {
                    if let Some(line) = describe_event(e) {
                        log::log(&log_name, Kind::Llm, truncate(&line, 220));
                    }
                })
                .await;
            let event = match outcome {
                Ok(outcome) => {
                    let (i, o) = outcome.tokens();
                    log::log(&name, Kind::Llm, format!("{} calls, {i} in / {o} out tokens", outcome.model_calls()));
                    BotEvent::ThoughtDone {
                        decision: decision.lock().unwrap().clone(),
                        spoke: spoke.load(std::sync::atomic::Ordering::SeqCst),
                        text: outcome.text.clone(),
                        tier,
                        calls: outcome.model_calls(),
                    }
                }
                Err(e) => BotEvent::ThoughtFailed { error: format!("{e:#}") },
            };
            let _ = events.send(event);
        });
    }

    fn on_thought(&mut self, decision: Decision, spoke: bool, text: &str, _tier: Tier, calls: usize) {
        self.thinking = false;
        self.calls_today.1 += calls as u32;
        if !spoke && !text.is_empty() {
            // The model answered in plain text instead of calling say.
            let _ = self.body.say(&truncate(text, 200));
        }
        let did_say = spoke || !text.is_empty();
        if let Some(goal) = decision.escalate {
            log::log(&self.name, Kind::Plan, format!("new goal: {goal}"));
            self.goal = Some(goal.clone());
            self.note(format!("new goal: {goal}"));
            self.pending.insert(0, format!("New goal: {goal}. Make a plan."));
            self.pending_tier = Tier::Strong;
        } else if let Some(plan) = decision.plan {
            self.start_plan(plan);
        } else if decision.keep {
            log::log(&self.name, Kind::Plan, "keep the current plan");
            self.executor.resume();
        } else if let Some(request) = decision.learn {
            log::log(&self.name, Kind::Skill, format!("wants to learn {}: {}", request.name, request.description));
            self.start_learning(request);
            return;
        } else if !did_say {
            log::log(&self.name, Kind::Think, "decided nothing");
        }
        self.flush_pending();
    }

    fn start_plan(&mut self, plan: Plan) {
        if plan.steps.is_empty() {
            log::log(&self.name, Kind::Plan, "empty plan: stop and wait");
            self.executor.clear();
            return;
        }
        if !plan.goal.is_empty() {
            self.goal = Some(plan.goal.clone());
        }
        log::log(&self.name, Kind::Plan, format!("{}: {}", plan.goal, plan.describe()));
        self.executor.replace(plan);
    }

    fn on_goal_done(&mut self, goal: &str) {
        self.goal = None;
        let _ = self.body.say(&format!("Done: {}", truncate(goal, 80)));
        let llm = self.llm.clone();
        let body = self.body.clone();
        let events = self.events.clone();
        let goal = goal.to_string();
        let results = self.executor.recent_results();
        let max = self.config.limits.inferences_per_turn;
        self.spawn_mind(async move {
            let (lessons, calls) = learn::reflect(&llm, &body, &goal, &results, max).await;
            let _ = events.send(BotEvent::Reflected { lessons, calls });
        });
    }

    /// Bed entered: stop everything and dream on the strong model.
    fn start_dream(&mut self) {
        self.dreaming = true;
        self.executor.pause();
        let unsaved = self.memory.unsaved();
        log::log(
            &self.name,
            Kind::Dream,
            format!("asleep; dreaming over {} unsaved skills and {} lessons", unsaved.skills, unsaved.lessons),
        );
        let _ = self.body.say("Zzz... dreaming.");
        let mut places = Vec::new();
        if let Some(bed) = self.body.senses.lock().unwrap().bed {
            places.push(format!("my bed at {} {} {}", bed.x, bed.y, bed.z));
        }
        if let Some(table) = self.body.find_block("crafting_table", 32) {
            places.push(format!("crafting table at {} {} {}", table.x, table.y, table.z));
        }
        let input = dream::DreamInput {
            memory: self.memory.clone(),
            recent: self.recent.iter().cloned().collect(),
            nightmares: self.nightmares.take(),
            places,
        };
        let over_budget = self.calls_today.1 >= self.config.limits.llm_calls_per_day;
        let llm = self.llm.clone();
        let body = self.body.clone();
        let events = self.events.clone();
        let max = self.config.limits.inferences_per_turn;
        self.spawn_mind(async move {
            let (dream, calls) = if over_budget {
                (dream::plain_dream(&input), 0)
            } else {
                dream::dream(&llm, &body, &input, max).await
            };
            let _ = events.send(BotEvent::DreamDone { dream, calls });
        });
    }

    fn finish_dream(&mut self, dream: &DreamResult) {
        self.dreaming = false;
        self.last_dream = Some(Instant::now());
        let night = self.memory.nights() + 1;
        match self.memory.consolidate(dream, night) {
            Ok(hash) => {
                log::log(&self.name, Kind::Dream, format!("dream journal, night {night} (commit {hash}):"));
                for line in dream.journal.lines().filter(|l| !l.trim().is_empty()) {
                    log::log(&self.name, Kind::Dream, format!("  {line}"));
                }
                log::log(
                    &self.name,
                    Kind::Memory,
                    format!(
                        "long-term: {} skills ({}), {} rules; dropped {:?}",
                        self.memory.long_term_skills().len(),
                        dream.skills.iter().map(|s| s.name.as_str()).collect::<Vec<_>>().join(", "),
                        dream.rules.len(),
                        dream.drop
                    ),
                );
                let _ = self.body.say(&format!("Dreamt: {}", truncate(&dream.summary, 150)));
            }
            Err(e) => log::log(&self.name, Kind::Fail, format!("could not save memory: {e:#}")),
        }
        for lesson in std::mem::take(&mut self.late_lessons) {
            let _ = self.memory.add_lesson(&lesson);
        }
        self.resync_skills();
        self.recent.clear();
        let body = self.body.clone();
        tokio::spawn(async move { body.wake_up().await });
        self.executor.resume();
        self.flush_pending();
    }

    /// Makes the tool set and library match what memory holds now.
    fn resync_skills(&mut self) {
        let known = self.memory.skills();
        for skill in self.library.all() {
            if !known.iter().any(|k| k.name == skill.name) {
                self.library.remove(&skill.name);
                self.tools.unregister(&skill.name);
            }
        }
        for skill in known {
            self.library.insert(skill.clone());
            skills::register(&self.tools, &self.library, &self.body, &skill);
        }
    }

    fn death_cause(&self) -> String {
        let recent = self.body.system_messages_since(Instant::now() - Duration::from_secs(15));
        if let Some(msg) = recent.iter().rev().find(|m| m.contains(&self.name)) {
            return msg.clone();
        }
        match &self.last_hurt {
            Some((what, when)) if when.elapsed() < Duration::from_secs(15) => format!("hurt by {what}"),
            _ => "unknown causes".into(),
        }
    }

    fn spawn_mind(&mut self, task: impl std::future::Future<Output = ()> + Send + 'static) {
        self.mind.retain(|t| !t.is_finished());
        self.mind.push(tokio::spawn(task).abort_handle());
    }

    /// Death: everything unsaved is gone. Tell the world what was lost.
    fn on_death(&mut self) {
        let cause = self.death_cause();
        log::log(&self.name, Kind::Death, format!("died: {cause}"));
        for task in self.mind.drain(..) {
            task.abort();
        }
        self.executor.clear();
        self.dreaming = false;
        self.thinking = false;
        self.pending.clear();
        self.late_lessons.clear();
        let lost_skills: Vec<String> = self.memory.working_skills().iter().map(|s| s.name.clone()).collect();
        let lost_lessons = self.memory.lessons().len();
        let doing = self.goal.clone().unwrap_or_else(|| "nothing in particular".into());
        self.nightmares.add(&format!(
            "{} died ({cause}) at {} while working on: {doing}",
            self.name,
            self.body.time().phase()
        ));
        if let Err(e) = self.memory.forget() {
            log::log(&self.name, Kind::Fail, format!("forgetting failed: {e:#}"));
        }
        let mut hardcore_message = None;
        if self.config.limits.hardcore {
            // Hardcore: long-term memory dies too. Only books survive.
            let saved = self.memory.long_term_skills().len();
            let rules = self.memory.rules().len();
            let next = self.memory.generation() + 1;
            match self.memory.wipe_long_term(next, &cause) {
                Ok(()) => {
                    let text = format!(
                        "Generation {} died for good, taking {saved} saved skill(s) and {rules} rule(s). Generation {next} remembers nothing.",
                        next - 1
                    );
                    hardcore_message = Some(text);
                }
                Err(e) => log::log(&self.name, Kind::Fail, format!("could not wipe memory: {e:#}")),
            }
        }
        self.resync_skills();
        self.goal = None;
        self.recent.clear();
        self.failures_in_a_row = 0;
        let message = if let Some(text) = hardcore_message {
            text
        } else if lost_skills.is_empty() && lost_lessons == 0 {
            "I died, but I had saved everything I knew.".to_string()
        } else {
            format!(
                "I died and forgot {} skill(s){} and {lost_lessons} lesson(s). Should have slept.",
                lost_skills.len(),
                if lost_skills.is_empty() { String::new() } else { format!(" ({})", lost_skills.join(", ")) }
            )
        };
        log::log(&self.name, Kind::Death, &message);
        let body = self.body.clone();
        tokio::spawn(async move {
            // Say it after respawning so it shows up in chat.
            tokio::time::sleep(Duration::from_secs(3)).await;
            let _ = body.say(&message);
        });
    }

    fn debug(&mut self, command: String) {
        match command.as_str() {
            "dream" => {
                if !self.dreaming {
                    self.start_dream();
                }
            }
            "forget" => self.on_death(),
            "reset" => {
                // Benchmark: drop the goal and anything in flight, keep memory.
                for task in self.mind.drain(..) {
                    task.abort();
                }
                self.executor.clear();
                self.goal = None;
                self.thinking = false;
                self.dreaming = false;
                self.pending.clear();
                self.recent.clear();
                self.failures_in_a_row = 0;
                self.calls_today = (self.body.time().day(), 0);
                self.budget_warned = false;
                log::log(&self.name, Kind::Info, "reset");
            }
            "memory" => {
                let text = format!("{} | {}", self.memory.long_term_summary(), self.memory.working_summary());
                log::log(&self.name, Kind::Memory, &text);
                let _ = self.body.say(&format!("{DEBUG_TAG} {}", truncate(&text.replace('\n', " "), 230)));
            }
            _ => {
                let Some((tool, args)) = tools::parse_command(&command) else { return };
                let tools = self.tools.clone();
                let body = self.body.clone();
                tokio::spawn(async move {
                    log::log(&body.name, Kind::Tool, format!("debug {tool}({args})"));
                    let result = tools.call(&tool, args).await;
                    let (kind, text) = match &result {
                        Ok(t) => (Kind::Result, t.clone()),
                        Err(e) => (Kind::Fail, e.clone()),
                    };
                    log::log(&body.name, kind, &text);
                    let _ = body.say(&format!("{DEBUG_TAG} {}", truncate(&text, 220)));
                });
            }
        }
    }

    /// Hands the body to the skill learner. Thinking stays blocked until the
    /// learner reports back.
    fn start_learning(&mut self, request: plan::SkillRequest) {
        self.thinking = true;
        self.executor.clear();
        let learner = learn::Learner {
            body: self.body.clone(),
            llm: self.llm.clone(),
            tools: self.tools.clone(),
            library: self.library.clone(),
            retries: self.config.limits.skill_write_retries,
            inferences_per_turn: self.config.limits.inferences_per_turn,
        };
        let events = self.events.clone();
        self.spawn_mind(async move {
            let name = request.name.clone();
            let learned = learner.learn(request).await;
            let (skill, detail) = match learned.result {
                Ok(skill) => (Some(skill), String::new()),
                Err(e) => (None, e),
            };
            let _ = events.send(BotEvent::SkillLearned {
                name,
                skill,
                detail,
                calls: learned.calls,
                attempts: learned.attempts,
            });
        });
    }
}

fn tier_name(tier: Tier) -> &'static str {
    match tier {
        Tier::Fast => "fast",
        Tier::Strong => "strong",
    }
}

fn attacker_kind(body: &Body, id: u32) -> Option<String> {
    let entity = body
        .client
        .ecs_entity_by_minecraft_entity(azalea::world::MinecraftEntityId(id as i32))?;
    let kind = body
        .client
        .get_entity_component::<azalea::entity::EntityKindComponent>(entity)?;
    Some(crate::body::strip_ns(&kind.0.to_string()))
}

/// Watches the body once a second and turns changes into events.
fn spawn_ticker(body: Body, events: mpsc::UnboundedSender<BotEvent>) {
    tokio::spawn(async move {
        let mut last_phase = String::new();
        let mut was_sleeping = false;
        let mut was_full = false;
        let mut last_day = u64::MAX;
        let mut last_alarm = Instant::now() - Duration::from_secs(60);
        let mut beats = 0u64;
        loop {
            tokio::time::sleep(Duration::from_secs(1)).await;
            if !body.client.logged_in() {
                continue;
            }
            beats += 1;
            if beats % 10 == 0 {
                let _ = events.send(BotEvent::Heartbeat);
            }
            let time: TimeOfDay = body.time();
            if time.0 == 0 {
                continue; // no time packet yet
            }
            let phase = time.phase().to_string();
            if phase != last_phase && !last_phase.is_empty() {
                match phase.as_str() {
                    "dusk" => {
                        let _ = events.send(BotEvent::Dusk { seconds_left: time.ticks_until_dark() / 20 });
                    }
                    "night" if last_day != time.day() => {
                        last_day = time.day();
                        let _ = events.send(BotEvent::Night);
                    }
                    _ => {}
                }
            }
            last_phase = phase;

            let sleeping = body.is_sleeping();
            let slept = std::mem::take(&mut body.senses.lock().unwrap().slept);
            if (sleeping && !was_sleeping) || slept {
                let _ = events.send(BotEvent::BedEntered);
            }
            was_sleeping = sleeping;

            let full = body.free_slots() == 0;
            if full && !was_full {
                let _ = events.send(BotEvent::InventoryFull);
            }
            was_full = full;

            if last_alarm.elapsed() < Duration::from_secs(5) {
                continue;
            }
            let hp = body.health();
            if hp > 0.0 && hp <= 6.0 && !body.hostiles(10.0).is_empty() {
                last_alarm = Instant::now();
                let _ = events.send(BotEvent::LowHealth);
            } else if body.food() <= 12 && body.has_food() && body.hostiles(8.0).is_empty() {
                last_alarm = Instant::now();
                let _ = events.send(BotEvent::Hungry);
            }
        }
    });
}
