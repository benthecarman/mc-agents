//! Plans and the planning turn.
//!
//! The model never drives the body step by step. In one GDK turn it may look
//! around and check the inventory (those tools run at once), then it must call
//! `submit_plan` with a list of tool steps. Our `StopWhen` operation ends the
//! turn as soon as a valid plan is in. The executor then runs the steps with
//! no model in the loop.

use std::sync::{Arc, Mutex};

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::brain::tools::{ToolDef, ToolSet};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Step {
    pub tool: String,
    #[serde(default)]
    pub args: Value,
}

impl Step {
    pub fn describe(&self) -> String {
        let args = match &self.args {
            Value::Object(map) if map.is_empty() => String::new(),
            Value::Object(map) => map
                .iter()
                .map(|(k, v)| match v {
                    Value::String(s) => format!("{k}={s}"),
                    other => format!("{k}={other}"),
                })
                .collect::<Vec<_>>()
                .join(" "),
            Value::Null => String::new(),
            other => other.to_string(),
        };
        if args.is_empty() {
            self.tool.clone()
        } else {
            format!("{}({args})", self.tool)
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct Plan {
    pub goal: String,
    pub steps: Vec<Step>,
    /// True when finishing the steps completes the goal. False means "plan the
    /// next part when these steps are done".
    pub completes_goal: bool,
}

impl Plan {
    pub fn describe(&self) -> String {
        self.steps
            .iter()
            .map(Step::describe)
            .collect::<Vec<_>>()
            .join(" → ")
    }
}

/// What the model decided in one turn.
#[derive(Debug, Clone, Default)]
pub struct Decision {
    pub plan: Option<Plan>,
    /// The model chose to keep the current plan.
    pub keep: bool,
    /// The fast model handed a goal to the strong planner.
    pub escalate: Option<String>,
    /// The model wants to write a new skill (handled by the skill learner).
    pub learn: Option<SkillRequest>,
}

#[derive(Debug, Clone)]
pub struct SkillRequest {
    pub name: String,
    pub description: String,
    pub success: String,
}

impl Decision {
    pub fn decided(&self) -> bool {
        self.plan.is_some() || self.keep || self.escalate.is_some() || self.learn.is_some()
    }
}

/// Builds the tool set for a planning turn: the read-only tools, `say`, and
/// the decision tools that end the turn.
pub fn planning_tools(
    core: &ToolSet,
    sense: &[&str],
    action_names: Vec<String>,
    allow_escalate: bool,
) -> (Arc<ToolSet>, Arc<Mutex<Decision>>) {
    let set = ToolSet::new();
    for name in sense.iter().chain(["say"].iter()) {
        if let Some(tool) = core.get(name) {
            set.register(tool);
        }
    }
    let decision = Arc::new(Mutex::new(Decision::default()));
    let actions = Arc::new(action_names);

    {
        let decision = decision.clone();
        let actions = actions.clone();
        let core_schemas: Vec<(String, Value)> = actions
            .iter()
            .filter_map(|n| core.get(n).map(|t| (n.clone(), t.schema.clone())))
            .collect();
        set.register(ToolDef::new(
            "submit_plan",
            "Submit the plan the bot will carry out. Each step is one action tool with its arguments. This ends your turn.",
            json!({
                "type": "object",
                "properties": {
                    "goal": {"type": "string", "description": "The goal this plan works toward, in a few words"},
                    "steps": {
                        "type": "array",
                        "description": "Ordered actions, e.g. [{\"tool\":\"mine\",\"args\":{\"block\":\"log\",\"count\":4}}]",
                        "items": {
                            "type": "object",
                            "properties": {
                                "tool": {"type": "string", "description": "An action tool or skill name"},
                                "args": {"type": "object", "description": "That tool's arguments"}
                            },
                            "required": ["tool"]
                        }
                    },
                    "completes_goal": {"type": "boolean", "description": "true if these steps finish the goal; false to plan more afterwards"}
                },
                "required": ["goal", "steps", "completes_goal"]
            }),
            move |args| {
                let decision = decision.clone();
                let actions = actions.clone();
                let schemas = core_schemas.clone();
                async move {
                    let plan = parse_plan(&args, &actions, &schemas)?;
                    let n = plan.steps.len();
                    decision.lock().unwrap().plan = Some(plan);
                    Ok(format!("plan accepted ({n} steps)"))
                }
            },
        ));
    }
    {
        let decision = decision.clone();
        set.register(ToolDef::new(
            "keep_plan",
            "Keep doing the current plan unchanged. This ends your turn.",
            json!({"type": "object", "properties": {}}),
            move |_| {
                let decision = decision.clone();
                async move {
                    decision.lock().unwrap().keep = true;
                    Ok("keeping the current plan".into())
                }
            },
        ));
    }
    {
        let decision = decision.clone();
        set.register(ToolDef::new(
            "learn_skill",
            "Write a new reusable skill (a script over the action tools) when the goal needs a capability you do not have yet. A stronger model writes and tests it. This ends your turn.",
            json!({
                "type": "object",
                "properties": {
                    "name": {"type": "string", "description": "snake_case skill name, e.g. gather_wood"},
                    "description": {"type": "string", "description": "What the skill does and its arguments"},
                    "success": {"type": "string", "description": "How to check it worked, in inventory or world terms, e.g. 'inventory gains 4+ oak_log'"}
                },
                "required": ["name", "description", "success"]
            }),
            move |args| {
                let decision = decision.clone();
                async move {
                    let name = args["name"].as_str().unwrap_or_default().trim().to_string();
                    if name.is_empty() || !name.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_') {
                        return Err("name must be snake_case letters, digits and _".into());
                    }
                    decision.lock().unwrap().learn = Some(SkillRequest {
                        name,
                        description: args["description"].as_str().unwrap_or_default().to_string(),
                        success: args["success"].as_str().unwrap_or_default().to_string(),
                    });
                    Ok("a skill writer will take it from here".into())
                }
            },
        ));
    }
    if allow_escalate {
        let decision = decision.clone();
        set.register(ToolDef::new(
            "set_goal",
            "Hand a multi-step goal to the planner, who will make a full plan. Use this when a player asks for something that needs several actions. This ends your turn.",
            json!({
                "type": "object",
                "properties": {"goal": {"type": "string", "description": "The goal, e.g. 'build a crafting table and a wooden pickaxe'"}},
                "required": ["goal"]
            }),
            move |args| {
                let decision = decision.clone();
                async move {
                    let goal = args["goal"].as_str().unwrap_or_default().trim().to_string();
                    if goal.is_empty() {
                        return Err("goal is empty".into());
                    }
                    decision.lock().unwrap().escalate = Some(goal);
                    Ok("the planner will take it from here".into())
                }
            },
        ));
    }
    (set, decision)
}

fn parse_plan(args: &Value, actions: &[String], schemas: &[(String, Value)]) -> Result<Plan, String> {
    let goal = args["goal"].as_str().unwrap_or("").trim().to_string();
    let completes_goal = args["completes_goal"].as_bool().unwrap_or(true);
    let raw_steps = match &args["steps"] {
        Value::Array(steps) => steps.clone(),
        Value::String(s) => serde_json::from_str::<Vec<Value>>(s).map_err(|e| format!("steps is not a JSON list: {e}"))?,
        _ => return Err("steps must be a list of {tool, args}".into()),
    };
    let mut steps = Vec::new();
    let mut problems = Vec::new();
    for (i, raw) in raw_steps.into_iter().enumerate() {
        let tool = raw["tool"].as_str().unwrap_or_default().to_string();
        let mut step_args = raw.get("args").cloned().unwrap_or(json!({}));
        if let Value::String(s) = &step_args {
            step_args = serde_json::from_str(s).unwrap_or(json!({}));
        }
        if !actions.contains(&tool) {
            problems.push(format!("step {}: '{tool}' is not an action tool or skill", i + 1));
            continue;
        }
        if let Some((_, schema)) = schemas.iter().find(|(n, _)| *n == tool) {
            for required in schema["required"].as_array().into_iter().flatten() {
                let key = required.as_str().unwrap_or_default();
                if step_args.get(key).is_none() {
                    problems.push(format!("step {}: {tool} needs '{key}'", i + 1));
                }
            }
        }
        steps.push(Step { tool, args: step_args });
    }
    if !problems.is_empty() {
        return Err(format!(
            "plan rejected: {}. Action tools: {}",
            problems.join("; "),
            actions.join(", ")
        ));
    }
    Ok(Plan {
        goal,
        steps,
        completes_goal,
    })
}

/// Lists tools for the system prompt: `name(arg, arg) - description`.
pub fn describe_tools(set: &ToolSet, names: &[String]) -> String {
    names
        .iter()
        .filter_map(|n| set.get(n))
        .map(|t| {
            let args: Vec<String> = t.schema["properties"]
                .as_object()
                .map(|p| p.keys().cloned().collect())
                .unwrap_or_default();
            format!("- {}({}): {}", t.name, args.join(", "), t.description)
        })
        .collect::<Vec<_>>()
        .join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_unknown_tools_and_missing_args() {
        let actions = vec!["mine".to_string()];
        let schemas = vec![("mine".to_string(), json!({"required": ["block"]}))];
        let err = parse_plan(
            &json!({"goal": "g", "steps": [{"tool": "fly"}, {"tool": "mine", "args": {}}], "completes_goal": true}),
            &actions,
            &schemas,
        )
        .unwrap_err();
        assert!(err.contains("'fly'") && err.contains("needs 'block'"));
    }

    #[test]
    fn accepts_stringified_steps() {
        let actions = vec!["mine".to_string()];
        let plan = parse_plan(
            &json!({"goal": "wood", "steps": "[{\"tool\":\"mine\",\"args\":{\"block\":\"log\"}}]", "completes_goal": false}),
            &actions,
            &[],
        )
        .unwrap();
        assert_eq!(plan.steps.len(), 1);
        assert!(!plan.completes_goal);
    }
}
