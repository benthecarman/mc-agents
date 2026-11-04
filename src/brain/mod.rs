//! The bot brain, built on the goose GDK (`goose-agent` + `goose-providers`).
//!
//! The GDK gives us the agent loop as a state machine of steps. We assemble
//! our own machine for each "thought":
//!
//! ```text
//! [ ToolOperation(our bot tools) , StopWhen , TurnBudget , InferenceRunner ]
//! ```
//!
//! The machine starts over from the top after every step that applies:
//! pending tool calls run first, then our operations can end the turn (a plan
//! was submitted, or the call budget is spent), and only then does the model
//! get called. The bot's outer loop (events, reflexes, executor) is ours too.
//! It calls [`Llm::run_turn`] only when an event needs thought, never per tick.

pub mod tools;

/// Model calls made by this process, for the benchmark.
pub static MODEL_CALLS: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

use std::sync::{Arc, Mutex};

use anyhow::{bail, Result};
use async_trait::async_trait;
use goose_agent::{
    events::AgentEvent,
    inference::{InferenceEffect, InferenceRequestPreparer, InferenceRunner, PreparedInferenceRequest},
    machine::{EffectHandler, MachineSession, SessionLoader, StateMachine, Step},
    operation::{
        applied, assistant_turn_count, last_effective_role, messages_since_kickoff, not_applicable,
        yielded, Emitter, InferenceInput, MachineEffect, Operation, OperationResult,
    },
    tool::{ToolOperation, ToolProvider},
};
use goose_providers::{
    anthropic::AnthropicProviderBuilder,
    api_client::{ApiClient, AuthMethod},
    base::Provider,
    conversation::{
        message::{Message, MessageContent},
        token_usage::ProviderUsage,
        Conversation, EffectiveRole,
    },
    model::ModelConfig,
    openai::OpenAiProvider,
};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::config::LlmConfig;

/// Which model a turn runs on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tier {
    /// Chat replies and simple replans.
    Fast,
    /// Planning, skill writing, dream consolidation.
    Strong,
}

/// One provider, two models. Cheap to clone.
#[derive(Clone)]
pub struct Llm {
    provider: Arc<dyn Provider>,
    fast: ModelConfig,
    strong: ModelConfig,
    fallback: Option<String>,
    timeout: std::time::Duration,
}

impl Llm {
    pub fn from_config(config: &LlmConfig) -> Result<Self> {
        let key = config.api_key()?;
        let provider: Arc<dyn Provider> = match config.kind.as_str() {
            "openai" => {
                let auth = key.map_or(AuthMethod::NoAuth, AuthMethod::BearerToken);
                let client = ApiClient::new_with_tls(config.base_url.clone(), auth, None)?;
                Arc::new(OpenAiProvider::new(client))
            }
            "anthropic" => {
                let auth = key.map_or(AuthMethod::NoAuth, |key| AuthMethod::ApiKey {
                    header_name: "x-api-key".into(),
                    key,
                });
                let client = ApiClient::new_with_tls(config.base_url.clone(), auth, None)?
                    .with_header("anthropic-version", "2023-06-01")?;
                Arc::new(AnthropicProviderBuilder::new(client).build())
            }
            other => bail!("unknown llm.kind '{other}' (use 'openai' or 'anthropic')"),
        };
        Ok(Self {
            provider,
            fast: with_params(ModelConfig::new(&config.fast_model), &config.fast_params),
            strong: with_params(ModelConfig::new(&config.strong_model), &config.strong_params),
            fallback: config.fallback_model.clone().filter(|m| !m.is_empty()),
            timeout: DEFAULT_TURN_TIMEOUT,
        })
    }
}

/// Adds `[llm] fast_params` / `strong_params` to every request for a tier.
fn with_params(model: ModelConfig, params: &std::collections::HashMap<String, toml::Value>) -> ModelConfig {
    if params.is_empty() {
        return model;
    }
    let params = params
        .iter()
        .filter_map(|(k, v)| serde_json::to_value(v).ok().map(|v| (k.clone(), v)))
        .collect();
    model.with_merged_request_params(params)
}

impl Llm {
    pub fn with_timeout(mut self, timeout: std::time::Duration) -> Self {
        self.timeout = timeout;
        self
    }

    /// The same provider with other models, for per-bot model choices.
    pub fn with_models(&self, fast: Option<&str>, strong: Option<&str>) -> Self {
        let mut llm = self.clone();
        // Keep the tier's request params when only the model changes.
        if let Some(fast) = fast {
            llm.fast = ModelConfig { model_name: fast.to_string(), ..llm.fast.clone() };
        }
        if let Some(strong) = strong {
            llm.strong = ModelConfig { model_name: strong.to_string(), ..llm.strong.clone() };
        }
        llm
    }

    pub fn model_name(&self, tier: Tier) -> &str {
        &self.model(tier).model_name
    }

    fn model(&self, tier: Tier) -> &ModelConfig {
        match tier {
            Tier::Fast => &self.fast,
            Tier::Strong => &self.strong,
        }
    }

    /// Runs one agent turn on a fresh conversation and returns when the model
    /// stops calling tools, `stop_when` holds, or the budget runs out.
    pub async fn run_turn(&self, turn: Turn, on_event: impl Fn(&AgentEvent)) -> Result<TurnOutcome> {
        self.run_turn_steered(turn, None, on_event).await
    }

    /// Like [`Llm::run_turn`], but messages pushed into `steer` while the turn
    /// runs are added to its conversation before the next model call.
    pub async fn run_turn_steered(
        &self,
        turn: Turn,
        steer: Option<Steer>,
        on_event: impl Fn(&AgentEvent),
    ) -> Result<TurnOutcome> {
        let Turn {
            system,
            user,
            tools,
            tier,
            max_inferences,
            stop_when,
        } = turn;
        let stop_when: Option<Arc<dyn Fn(&Conversation) -> bool + Send + Sync>> = stop_when.map(Arc::from);
        let steer = steer.map(Arc::new);
        let model = self.model(tier).clone();
        let run = |model: ModelConfig| {
            self.run_once(model, &system, &user, tools.clone(), max_inferences, stop_when.clone(), steer.clone(), &on_event)
        };
        match (run(model.clone()).await, &self.fallback) {
            // Only when no model call succeeded, so no tool ran twice.
            (Err((error, 0)), Some(fallback)) if *fallback != model.model_name => {
                crate::log::log("llm", crate::log::Kind::Fail, format!("{}: {error:#}; trying {fallback}", model.model_name));
                let fallback = ModelConfig { model_name: fallback.clone(), ..model };
                run(fallback).await.map_err(|(e, _)| e)
            }
            (result, _) => result.map_err(|(e, _)| e),
        }
    }

    /// One attempt at a turn. On failure, also returns how many model calls
    /// succeeded first.
    #[allow(clippy::too_many_arguments)]
    async fn run_once(
        &self,
        model: ModelConfig,
        system: &str,
        user: &str,
        tools: Arc<dyn ToolProvider<TurnSession>>,
        max_inferences: u32,
        stop_when: Option<Arc<dyn Fn(&Conversation) -> bool + Send + Sync>>,
        steer: Option<Arc<Steer>>,
        on_event: &impl Fn(&AgentEvent),
    ) -> std::result::Result<TurnOutcome, (anyhow::Error, usize)> {
        let (system, user) = (system.to_string(), user.to_string());
        let cancel = CancellationToken::new();
        let inference = InferenceRunner::<TurnSession, Effect>::new(self.provider.clone(), model)
            .with_request_preparer(Arc::new(SystemPrompt(system)));

        let mut steps: Vec<Step<'_, TurnSession, Effect>> = vec![Step::Operation(Arc::new(
            ToolOperation::new().with_provider(tools),
        ))];
        // After pending tool calls are answered, so the conversation stays valid.
        if let Some(steer) = steer {
            steps.push(Step::Operation(Arc::new(SteerIn(steer))));
        }
        if let Some(predicate) = stop_when {
            steps.push(Step::Operation(Arc::new(StopWhen(predicate))));
        }
        steps.push(Step::Operation(Arc::new(TurnBudget {
            max_inferences,
        })));
        steps.push(Step::Inference(Arc::new(inference)));
        let machine = StateMachine::new(steps, cancel.clone());

        let id = format!("turn_{}", uuid_like());
        let store = Store {
            session: Mutex::new(TurnSession {
                id: id.clone(),
                conversation: Conversation::new_unvalidated([Message::user().with_text(user)]),
            }),
            usage: Mutex::new(Vec::new()),
        };

        let (tx, mut rx) = mpsc::unbounded_channel();
        let emit = Emitter::new(tx, cancel);
        let run = async {
            let emit = emit;
            machine.run(&store, &id, &emit).await
        };
        let drain = async {
            while let Some(event) = rx.recv().await {
                on_event(&event);
            }
        };
        let timeout = self.timeout;
        let run = async move {
            match tokio::time::timeout(timeout, run).await {
                Ok(result) => result,
                Err(_) => Err(anyhow::anyhow!("model turn took longer than {}s", timeout.as_secs())),
            }
        };
        let (session, ()) = tokio::join!(run, drain);
        let calls = || store.usage.lock().unwrap().len();
        let session = session.map_err(|e| (e, calls()))?;

        let conversation = session.conversation;
        let text = conversation
            .messages()
            .iter()
            .rev()
            .find(|m| m.role == rmcp::model::Role::Assistant && !m.as_concat_text().trim().is_empty())
            .map(|m| strip_safety_tags(&m.as_concat_text()))
            .unwrap_or_default();
        if let Some(kind) = conversation.last().and_then(Message::error_kind) {
            // The detail is in the error block; as_concat_text skips those.
            let detail = conversation
                .last()
                .unwrap()
                .content
                .iter()
                .find_map(|c| match c {
                    goose_provider_types::conversation::message::MessageContentBlock::Error(e) => Some(e.message.clone()),
                    _ => None,
                })
                .unwrap_or_default();
            let error = anyhow::anyhow!("model call failed: {kind:?}: {detail}");
            return Err((error, calls()));
        }
        let usage = store.usage.into_inner().unwrap();
        MODEL_CALLS.fetch_add(usage.len().max(1), std::sync::atomic::Ordering::Relaxed);
        Ok(TurnOutcome {
            conversation,
            text,
            usage,
        })
    }
}

/// DeepSeek models sometimes append a `<ds_safety>` classification to
/// their text; it must never reach chat.
fn strip_safety_tags(text: &str) -> String {
    text.split("<ds_safety>").next().unwrap_or_default().trim().to_string()
}

fn uuid_like() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let nanos = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos();
    format!("{nanos:x}")
}

pub type StopPredicate = Box<dyn Fn(&Conversation) -> bool + Send + Sync>;

/// One request for thought.
pub struct Turn {
    pub system: String,
    pub user: String,
    pub tools: Arc<dyn ToolProvider<TurnSession>>,
    pub tier: Tier,
    /// Hard cap on model calls in this turn.
    pub max_inferences: u32,
    /// Ends the turn early, e.g. once the model has submitted a plan.
    pub stop_when: Option<StopPredicate>,
}

/// Default wall-clock limit for one turn; see [`Llm::with_timeout`].
pub const DEFAULT_TURN_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(180);

pub struct TurnOutcome {
    pub conversation: Conversation,
    /// The last thing the model said in plain text.
    pub text: String,
    pub usage: Vec<ProviderUsage>,
}

impl TurnOutcome {
    pub fn model_calls(&self) -> usize {
        self.usage.len()
    }

    pub fn tokens(&self) -> (i64, i64) {
        self.usage.iter().fold((0, 0), |(i, o), u| {
            (
                i + u.usage.input_tokens.unwrap_or(0) as i64,
                o + u.usage.output_tokens.unwrap_or(0) as i64,
            )
        })
    }
}

/// Renders a GDK event as one short log line, or `None` to skip it.
pub fn describe_event(event: &AgentEvent) -> Option<String> {
    let AgentEvent::Message(message) = event else {
        return None;
    };
    let mut parts = Vec::new();
    for content in &message.content {
        match content {
            MessageContent::ToolRequest(request) => match &request.tool_call {
                Ok(call) => parts.push(format!(
                    "call {}({})",
                    call.name,
                    serde_json::Value::Object(call.arguments.clone().unwrap_or_default())
                )),
                Err(error) => parts.push(format!("bad tool call: {}", error.message)),
            },
            MessageContent::ToolResponse(response) => {
                let text = match &response.tool_result {
                    Ok(result) => result
                        .content
                        .iter()
                        .filter_map(|c| c.as_text().map(|t| t.text.clone()))
                        .collect::<Vec<_>>()
                        .join(" "),
                    Err(error) => error.message.to_string(),
                };
                parts.push(format!("result {}", truncate(&text, 200)));
            }
            _ => {}
        }
    }
    (!parts.is_empty()).then(|| parts.join("; "))
}

pub fn truncate(text: &str, max: usize) -> String {
    let text = text.replace('\n', " ");
    if text.chars().count() <= max {
        text
    } else {
        format!("{}…", text.chars().take(max).collect::<String>())
    }
}

// ---------------------------------------------------------------------------
// The pieces the GDK state machine needs from us: a session type, a store
// that loads and persists it, an effect type, and a request preparer.
// ---------------------------------------------------------------------------

#[derive(Clone)]
pub struct TurnSession {
    id: String,
    conversation: Conversation,
}

impl MachineSession for TurnSession {
    fn id(&self) -> &str {
        &self.id
    }

    fn conversation(&self) -> Option<&Conversation> {
        Some(&self.conversation)
    }
}

pub enum Effect {
    Message(Message),
    Usage(ProviderUsage),
}

impl From<Message> for Effect {
    fn from(message: Message) -> Self {
        Effect::Message(message)
    }
}

impl InferenceEffect for Effect {
    fn record_usage(usage: ProviderUsage) -> Self {
        Effect::Usage(usage)
    }
}

impl MachineEffect for Effect {
    fn ensure_message_ids(&mut self) {
        if let Effect::Message(message) = self {
            if message.id.is_none() {
                message.id = Some(format!("msg_{}", uuid_like()));
            }
        }
    }
}

/// In-memory session storage. Bot memory lives in git, not here: each turn
/// starts from a fresh conversation built from that memory.
struct Store {
    session: Mutex<TurnSession>,
    usage: Mutex<Vec<ProviderUsage>>,
}

#[async_trait]
impl SessionLoader<TurnSession> for Store {
    async fn load(&self, _session_id: &str) -> Result<TurnSession> {
        Ok(self.session.lock().unwrap().clone())
    }
}

#[async_trait]
impl EffectHandler<TurnSession, Effect> for Store {
    async fn apply_effects(
        &self,
        _session: &TurnSession,
        effects: &mut [Effect],
        _emit: &Emitter,
    ) -> Result<()> {
        let mut session = self.session.lock().unwrap();
        for effect in effects {
            match effect {
                Effect::Message(message) => session.conversation.push(message.clone()),
                Effect::Usage(usage) => self.usage.lock().unwrap().push(usage.clone()),
            }
        }
        Ok(())
    }
}

/// Puts our system prompt in front of whatever the operations contribute.
struct SystemPrompt(String);

#[async_trait]
impl InferenceRequestPreparer<TurnSession> for SystemPrompt {
    async fn prepare(
        &self,
        _session: &TurnSession,
        _conversation: &Conversation,
        input: InferenceInput,
    ) -> Result<PreparedInferenceRequest> {
        let mut system = self.0.clone();
        for (_, part) in input.prompt_parts {
            system.push_str("\n\n");
            system.push_str(&part);
        }
        Ok(PreparedInferenceRequest {
            system_prompt: system,
            tools: input.tools,
            additional_messages: Vec::new(),
        })
    }
}

/// Would the inference step fire next?
fn wants_inference(conversation: &Conversation) -> Result<bool> {
    let turn = messages_since_kickoff(conversation)?;
    Ok(matches!(
        last_effective_role(turn)?,
        EffectiveRole::User | EffectiveRole::Tool
    ))
}

/// Our operation: ends the turn as soon as a predicate on the conversation
/// holds. The planner uses it to stop right after `submit_plan`.
struct StopWhen(Arc<dyn Fn(&Conversation) -> bool + Send + Sync>);

#[async_trait]
impl Operation<TurnSession, Effect> for StopWhen {
    fn name(&self) -> &'static str {
        "stop_when"
    }

    async fn run(
        &self,
        _session: &TurnSession,
        conversation: &Conversation,
        _emit: &Emitter,
    ) -> Result<OperationResult<Effect>> {
        if (self.0)(conversation) {
            return yielded();
        }
        not_applicable()
    }
}

/// Messages waiting to be handed to a running turn.
pub type SteerInbox = Arc<Mutex<Vec<String>>>;

/// Messages that arrive while a turn runs. See [`Llm::run_turn_steered`].
pub struct Steer {
    pub inbox: SteerInbox,
    /// Runs when messages are handed over, e.g. to drop a decision the
    /// model made before it saw them.
    pub on_inject: Box<dyn Fn() + Send + Sync>,
}


/// Our operation: hands steering messages to the model mid-turn, so a player
/// can change the bot's mind without waiting for the turn to end.
struct SteerIn(Arc<Steer>);

#[async_trait]
impl Operation<TurnSession, Effect> for SteerIn {
    fn name(&self) -> &'static str {
        "steer"
    }

    async fn run(
        &self,
        _session: &TurnSession,
        _conversation: &Conversation,
        _emit: &Emitter,
    ) -> Result<OperationResult<Effect>> {
        let messages = std::mem::take(&mut *self.0.inbox.lock().unwrap());
        if messages.is_empty() {
            return not_applicable();
        }
        (self.0.on_inject)();
        let text = format!(
            "New message while you were working. It may change what you should do; your plan is paused until you decide again:\n{}",
            messages.join("\n")
        );
        applied([Effect::Message(Message::user().with_text(text))])
    }
}

/// Our operation: caps model calls per turn, so a confused model cannot loop
/// on tool calls and burn the budget.
struct TurnBudget {
    max_inferences: u32,
}

#[async_trait]
impl Operation<TurnSession, Effect> for TurnBudget {
    fn name(&self) -> &'static str {
        "turn_budget"
    }

    async fn run(
        &self,
        _session: &TurnSession,
        conversation: &Conversation,
        _emit: &Emitter,
    ) -> Result<OperationResult<Effect>> {
        let turn = messages_since_kickoff(conversation)?;
        if wants_inference(conversation)? && assistant_turn_count(turn) >= self.max_inferences {
            tracing::warn!("turn budget of {} model calls spent", self.max_inferences);
            return yielded();
        }
        not_applicable()
    }
}
