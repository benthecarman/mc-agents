//! Runs a plan step by step with no model involved. It wakes the agent loop
//! only when the plan finishes or a step fails.

use std::sync::{Arc, Mutex};

use azalea::pathfinder::PathfinderClientExt;
use tokio::{sync::mpsc, task::JoinHandle};

use super::{plan::Plan, BotEvent};
use crate::{
    body::Body,
    brain::{tools::ToolSet, truncate},
    log::{self, Kind},
};

#[derive(Default)]
struct State {
    plan: Plan,
    next: usize,
    id: u64,
    /// One line per finished step, for the next prompt.
    results: Vec<String>,
}

pub struct Executor {
    name: String,
    body: Body,
    tools: Arc<ToolSet>,
    events: mpsc::UnboundedSender<BotEvent>,
    state: Arc<Mutex<State>>,
    task: Option<JoinHandle<()>>,
}

impl Executor {
    pub fn new(body: Body, tools: Arc<ToolSet>, events: mpsc::UnboundedSender<BotEvent>) -> Self {
        Self {
            name: body.name.clone(),
            body,
            tools,
            events,
            state: Arc::default(),
            task: None,
        }
    }

    pub fn is_running(&self) -> bool {
        self.task.as_ref().is_some_and(|t| !t.is_finished())
    }

    pub fn goal(&self) -> Option<String> {
        let s = self.state.lock().unwrap();
        (!s.plan.goal.is_empty()).then(|| s.plan.goal.clone())
    }

    pub fn plan_id(&self) -> u64 {
        self.state.lock().unwrap().id
    }

    pub fn completes_goal(&self) -> bool {
        self.state.lock().unwrap().plan.completes_goal
    }

    /// The current plan with finished steps marked, for the prompt.
    pub fn describe(&self) -> String {
        let s = self.state.lock().unwrap();
        if s.plan.steps.is_empty() {
            return "none".into();
        }
        s.plan
            .steps
            .iter()
            .enumerate()
            .map(|(i, step)| {
                let mark = if i < s.next { "done" } else if i == s.next && self.is_running() { "now" } else { "todo" };
                format!("[{mark}] {}", step.describe())
            })
            .collect::<Vec<_>>()
            .join(" → ")
    }

    pub fn recent_results(&self) -> Vec<String> {
        self.state.lock().unwrap().results.clone()
    }

    /// Replaces whatever is running with a new plan.
    pub fn replace(&mut self, plan: Plan) {
        self.stop();
        {
            let mut s = self.state.lock().unwrap();
            s.id += 1;
            s.plan = plan;
            s.next = 0;
            s.results.clear();
        }
        self.start();
    }

    /// Stops the running step. The plan stays, so `resume` can carry on.
    pub fn pause(&mut self) {
        self.stop();
    }

    pub fn resume(&mut self) {
        let has_work = {
            let s = self.state.lock().unwrap();
            s.next < s.plan.steps.len()
        };
        if has_work && !self.is_running() {
            self.start();
        }
    }

    /// Drops the plan entirely.
    pub fn clear(&mut self) {
        self.stop();
        let mut s = self.state.lock().unwrap();
        s.id += 1;
        s.plan = Plan::default();
        s.next = 0;
    }

    fn stop(&mut self) {
        if let Some(task) = self.task.take() {
            task.abort();
            self.body.client.stop_pathfinding();
        }
    }

    fn start(&mut self) {
        let state = self.state.clone();
        let tools = self.tools.clone();
        let events = self.events.clone();
        let name = self.name.clone();
        self.task = Some(tokio::spawn(async move {
            loop {
                let (step, index, id, total) = {
                    let s = state.lock().unwrap();
                    if s.next >= s.plan.steps.len() {
                        let _ = events.send(BotEvent::PlanDone { plan_id: s.id });
                        return;
                    }
                    (s.plan.steps[s.next].clone(), s.next, s.id, s.plan.steps.len())
                };
                log::log(&name, Kind::Tool, format!("step {}/{total} {}", index + 1, step.describe()));
                let result = tools.call(&step.tool, step.args.clone()).await;
                let mut s = state.lock().unwrap();
                if s.id != id {
                    return; // replaced while running
                }
                match result {
                    Ok(text) => {
                        log::log(&name, Kind::Result, truncate(&text, 300));
                        s.results.push(format!("{} -> {}", step.describe(), truncate(&text, 160)));
                        s.next += 1;
                    }
                    Err(error) => {
                        log::log(&name, Kind::Fail, format!("{}: {}", step.describe(), truncate(&error, 300)));
                        s.results.push(format!("{} -> FAILED: {}", step.describe(), truncate(&error, 200)));
                        let _ = events.send(BotEvent::StepFailed {
                            plan_id: id,
                            step: step.describe(),
                            error,
                        });
                        return;
                    }
                }
            }
        }));
    }
}
