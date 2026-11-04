//! A tool set the GDK agent advertises and dispatches.
//!
//! Tools are plain async Rust closures over JSON. The same set backs the GDK
//! agent (through [`ToolProvider`]) and, later, the Rhai skill sandbox. Tools
//! can be added and removed while the bot runs, which is how learned skills
//! appear as new tools and how forgotten ones disappear.

use std::{future::Future, pin::Pin, sync::Arc};

use anyhow::Result;
use async_trait::async_trait;
use goose_agent::{operation::Emitter, tool::ToolProvider};
use rmcp::model::{CallToolRequestParams, CallToolResult, ContentBlock, ErrorData, JsonObject, Tool};
use serde_json::{json, Value};
use std::sync::RwLock;

use super::TurnSession;

/// Every tool returns short text: a JSON summary on success, or an error the
/// model can read and recover from.
pub type ToolResult = std::result::Result<String, String>;
pub type ToolFuture = Pin<Box<dyn Future<Output = ToolResult> + Send>>;
pub type ToolFn = Arc<dyn Fn(Value) -> ToolFuture + Send + Sync>;

#[derive(Clone)]
pub struct ToolDef {
    pub name: String,
    pub description: String,
    /// JSON Schema of the arguments object.
    pub schema: Value,
    pub handler: ToolFn,
}

impl ToolDef {
    pub fn new<F, Fut>(name: &str, description: &str, schema: Value, handler: F) -> Self
    where
        F: Fn(Value) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = ToolResult> + Send + 'static,
    {
        Self {
            name: name.into(),
            description: description.into(),
            schema,
            handler: Arc::new(move |args| Box::pin(handler(args))),
        }
    }

    fn to_rmcp(&self) -> Tool {
        let schema: JsonObject = match &self.schema {
            Value::Object(map) => map.clone(),
            _ => serde_json::from_value(json!({"type": "object", "properties": {}}))
                .expect("static schema"),
        };
        Tool::new(self.name.clone(), self.description.clone(), Arc::new(schema))
    }
}

/// Builds a JSON schema for an object from `(name, type, description)` triples.
/// Every listed property is required.
pub fn object_schema(props: &[(&str, &str, &str)]) -> Value {
    let properties: serde_json::Map<String, Value> = props
        .iter()
        .map(|(name, ty, desc)| (name.to_string(), json!({"type": ty, "description": desc})))
        .collect();
    let required: Vec<&str> = props.iter().map(|(name, _, _)| *name).collect();
    json!({"type": "object", "properties": properties, "required": required})
}

/// Called for every tool call with `(name, arguments, result)`, for logging.
pub type CallObserver = Arc<dyn Fn(&str, &Value, &ToolResult) + Send + Sync>;

#[derive(Default)]
pub struct ToolSet {
    tools: RwLock<Vec<ToolDef>>,
    observer: RwLock<Option<CallObserver>>,
}

impl ToolSet {
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    /// Adds a tool, replacing any tool with the same name.
    pub fn register(&self, tool: ToolDef) {
        let mut tools = self.tools.write().unwrap();
        tools.retain(|t| t.name != tool.name);
        tools.push(tool);
    }

    pub fn unregister(&self, name: &str) -> bool {
        let mut tools = self.tools.write().unwrap();
        let before = tools.len();
        tools.retain(|t| t.name != name);
        tools.len() != before
    }

    pub fn get(&self, name: &str) -> Option<ToolDef> {
        self.tools.read().unwrap().iter().find(|t| t.name == name).cloned()
    }

    pub fn names(&self) -> Vec<String> {
        self.tools.read().unwrap().iter().map(|t| t.name.clone()).collect()
    }

    pub fn set_observer(&self, observer: CallObserver) {
        *self.observer.write().unwrap() = Some(observer);
    }

    /// Runs a tool directly, without the model. The executor and the skill
    /// sandbox use this.
    pub async fn call(&self, name: &str, args: Value) -> ToolResult {
        let result = match self.get(name) {
            Some(tool) => (tool.handler)(args.clone()).await,
            None => Err(format!("unknown tool '{name}'")),
        };
        if let Some(observer) = self.observer.read().unwrap().clone() {
            observer(name, &args, &result);
        }
        result
    }
}

/// Wraps a [`ToolSet`] so a GDK agent can advertise and call it. The filter
/// picks which tools a given turn may see.
pub struct GdkTools {
    pub set: Arc<ToolSet>,
    pub filter: Box<dyn Fn(&str) -> bool + Send + Sync>,
}

impl GdkTools {
    pub fn all(set: Arc<ToolSet>) -> Self {
        Self {
            set,
            filter: Box::new(|_| true),
        }
    }
}

#[async_trait]
impl ToolProvider<TurnSession> for GdkTools {
    async fn tools(&self, _session: &TurnSession) -> Result<Vec<Tool>> {
        Ok(self
            .set
            .tools
            .read()
            .unwrap()
            .iter()
            .filter(|t| (self.filter)(&t.name))
            .map(ToolDef::to_rmcp)
            .collect())
    }

    async fn call(
        &self,
        _session: &TurnSession,
        _request_id: &str,
        call: CallToolRequestParams,
        _emit: &Emitter,
    ) -> std::result::Result<CallToolResult, ErrorData> {
        let args = Value::Object(call.arguments.unwrap_or_default());
        Ok(match self.set.call(&call.name, args).await {
            Ok(text) => CallToolResult::success(vec![ContentBlock::text(text)]),
            Err(text) => CallToolResult::error(vec![ContentBlock::text(format!("ERROR: {text}"))]),
        })
    }
}
