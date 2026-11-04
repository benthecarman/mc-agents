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

pub mod executor;
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
    memory::Memory,
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
}

impl Agent {
    fn new(body: Body, bot: BotConfig, config: Config, events: mpsc::UnboundedSender<BotEvent>) -> anyhow::Result<Self> {
        let llm = Llm::from_config(&config.llm)?.with_models(bot.fast_model.as_deref(), bot.strong_model.as_deref());
        let memory = Memory::open("memory", &bot.name)?;
        let tools = tools::core_tools(body.clone());
        let executor = Executor::new(body.clone(), tools.clone(), events.clone());
        Ok(Self {
            name: bot.name.clone(),
            bot,
            config,
            body,
            llm,
            memory,
            tools,
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
            BotEvent::ThoughtDone { .. } | BotEvent::ThoughtFailed { .. } => return,
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
                format!("Dusk: about {seconds_left} seconds until dark. Decide now: head to a bed (sleeping saves your memory) or keep going."),
                Tier::Fast,
            )),
            BotEvent::Night => Some((
                "It is night. Monsters spawn in the dark. Decide: sleep now if a bed is reachable, or keep going.".into(),
                Tier::Fast,
            )),
            BotEvent::InventoryFull => Some(("Your inventory is full.".into(), Tier::Fast)),
            BotEvent::BedEntered => {
                log::log(&self.name, Kind::Memory, "in bed");
                None
            }
            BotEvent::Death => {
                log::log(&self.name, Kind::Death, "died");
                self.executor.clear();
                self.goal = None;
                None
            }
            BotEvent::Damage { .. } | BotEvent::LowHealth | BotEvent::Hungry => None,
        }
    }

    /// Chat is for this bot if it names the bot, says "all"/"everyone"/"bots",
    /// or names no bot and this is the first bot in the config.
    fn addressed(&self, text: &str, from_bot: bool) -> bool {
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
        if self.thinking {
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
        self.think(text, tier, tier == Tier::Fast);
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
             - {finish}",
            name = self.name,
            personality = self.bot.personality,
            tools = plan::describe_tools(&self.tools, &actions),
        )
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
        tokio::spawn(async move {
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
            self.pending.push(format!("You cannot write skills yet. Plan with the action tools for: {}", request.description));
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
        loop {
            tokio::time::sleep(Duration::from_secs(1)).await;
            if !body.client.logged_in() {
                continue;
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
            if sleeping && !was_sleeping {
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
