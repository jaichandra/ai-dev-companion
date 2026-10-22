//! A seam for an LLM the distribution may provide: cheap on-device-or-on-prem model calls the
//! service makes on its own (the watchers' "is this worth a Claude run?" triage, embeddings for the
//! similar-items search) as opposed to the Claude Code runs the features start. The framework knows
//! nothing about any particular provider: a pack supplies a factory that builds a [`LlmBackend`]
//! from the live config and the credential store ([`LlmContext`]), and the framework asks it
//! questions. With no backend ([`NoLlm`]) every feature degrades to rule-based triage and
//! word-overlap similarity, exactly as it does when a backend has no key.

use crate::config::ConfigFn;
use crate::similar_search::Embedder;
use crate::watcher_runner::WatcherLlm;
use async_trait::async_trait;
use serde_json::{Map, Value};
use std::sync::Arc;

/// One message of a chat request (`role`: system, user or assistant).
#[derive(Debug, Clone, PartialEq)]
pub struct ChatMessage {
    pub role: String,
    pub content: String,
}

impl ChatMessage {
    pub fn new(role: &str, content: &str) -> Self {
        Self { role: role.into(), content: content.into() }
    }
}

/// How `companion doctor` shows the backend. The wording is the backend's own (it knows what to tell
/// the user to fix); the framework only places it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LlmView {
    /// `ready`, `no-key`, `unreachable`, ... (the backend's states; only `ready` means usable).
    pub state: String,
    /// The embedding model, so the doctor can count how many history items carry its vectors.
    pub embedding_model: String,
    /// `OK` (true) or `WARN` (false) for `line`.
    pub ok: bool,
    /// The whole line, without its level.
    pub line: String,
    /// What to do when the line is a warning.
    pub fix: String,
    /// How similar-items search works while the backend is ready ("by meaning (...) and shared words").
    pub similar_when_ready: String,
    /// ... and while it isn't ("by shared words only, until ... is ready").
    pub similar_when_not_ready: String,
}

/// Answers the service's own LLM questions.
#[async_trait]
pub trait LlmBackend: Send + Sync {
    /// Ready to be asked right now (a usable key, reachable, ...).
    async fn available(&self) -> bool;
    /// The backend's state word (`ready`, `no-key`, ...), cached by the backend.
    async fn detect_state(&self) -> String;
    /// The configured embedding model; an error when the configuration is unusable.
    fn embedding_model(&self) -> Result<String, String>;
    async fn embed(&self, texts: &[String]) -> Result<Vec<Vec<f64>>, String>;
    /// The parsed JSON answer to a chat request, if the model gave one.
    async fn chat(&self, system: &str, messages: &[ChatMessage], schema: &Value, max_tokens: f64) -> Result<Option<Value>, String>;
    /// Entries merged into `GET /settings`' `background` object (a backend's own key and its
    /// detection, for its Settings section to show).
    async fn background_status(&self) -> Map<String, Value>;
    /// The doctor's view of it.
    /// `None` when there is nothing to show.
    async fn doctor_view(&self) -> Result<Option<LlmView>, String>;
    /// Why the history-embedding task did nothing when the backend isn't ready (a short phrase kept
    /// in the task's result).
    fn skip_reason(&self) -> String;
}

/// Reads a saved credential by name.
pub type TokenFn = Arc<dyn Fn(&str) -> Option<String> + Send + Sync>;

/// What a pack's factory builds a backend from: the same live config and credentials the rest of
/// the service reads.
pub struct LlmContext {
    /// Read afresh on every use, so a Settings save applies live.
    pub config: ConfigFn,
    /// A saved credential by name (see the profile's `credentialNames`).
    pub get_token: TokenFn,
    /// The operating-system user name (the fallback for a sign-on name).
    pub os_user: String,
}

/// Builds the backend once at start-up.
pub type LlmFactory = Arc<dyn Fn(&LlmContext) -> Result<Arc<dyn LlmBackend>, String> + Send + Sync>;

/// No backend: never available, nothing to say.
pub struct NoLlm;

#[async_trait]
impl LlmBackend for NoLlm {
    async fn available(&self) -> bool {
        false
    }
    async fn detect_state(&self) -> String {
        "no-key".into()
    }
    fn embedding_model(&self) -> Result<String, String> {
        Ok(String::new())
    }
    async fn embed(&self, _: &[String]) -> Result<Vec<Vec<f64>>, String> {
        Err("no LLM is configured".into())
    }
    async fn chat(&self, _: &str, _: &[ChatMessage], _: &Value, _: f64) -> Result<Option<Value>, String> {
        Ok(None)
    }
    async fn background_status(&self) -> Map<String, Value> {
        Map::new()
    }
    async fn doctor_view(&self) -> Result<Option<LlmView>, String> {
        Ok(None)
    }
    fn skip_reason(&self) -> String {
        "no llm".into()
    }
}

/// A cheaply cloned handle on a backend, which the generic consumers (watchers, similar search,
/// the embedding task) take: they are written against [`WatcherLlm`] and [`Embedder`].
#[derive(Clone)]
pub struct LlmHandle(pub Arc<dyn LlmBackend>);

#[async_trait]
impl WatcherLlm for LlmHandle {
    async fn available(&self) -> bool {
        self.0.available().await
    }
    async fn chat(&self, system: &str, messages: &[ChatMessage], schema: &Value, max_tokens: f64) -> Result<Option<Value>, String> {
        self.0.chat(system, messages, schema, max_tokens).await
    }
}

impl Embedder for LlmHandle {
    fn embedding_model(&self) -> Result<String, String> {
        self.0.embedding_model()
    }
    async fn available(&self) -> bool {
        self.0.available().await
    }
    async fn detect_state(&self) -> String {
        self.0.detect_state().await
    }
    async fn embed(&self, texts: &[String]) -> Result<Vec<Vec<f64>>, String> {
        self.0.embed(texts).await
    }
    fn skip_reason(&self) -> String {
        self.0.skip_reason()
    }
}
