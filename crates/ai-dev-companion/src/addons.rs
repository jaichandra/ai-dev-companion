//! Job add-ons: something a pack attaches to a finished job's panel (a button that starts a side
//! process, a link to what it serves). The framework knows only this contract; the pack decides when
//! to offer one and what it does.

use async_trait::async_trait;
use serde_json::{json, Value};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AddonState {
    Idle,
    Starting,
    Ready,
    Failed,
}

impl AddonState {
    pub fn as_str(self) -> &'static str {
        match self {
            AddonState::Idle => "idle",
            AddonState::Starting => "starting",
            AddonState::Ready => "ready",
            AddonState::Failed => "failed",
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct AddonView {
    pub id: String,
    pub label: String,
    pub state: AddonState,
    /// Where "ready" leads (opened in a new tab by the panel).
    pub href: Option<String>,
    pub error: Option<String>,
    /// Whether the panel starts it by itself when it first sees it idle.
    pub auto_start: bool,
}

impl AddonView {
    pub fn to_json(&self) -> Value {
        json!({ "id": self.id, "label": self.label, "state": self.state.as_str(), "href": self.href, "error": self.error, "autoStart": self.auto_start })
    }
}

#[async_trait]
pub trait JobAddon: Send + Sync {
    fn id(&self) -> &str;
    /// How the add-on looks for this job; `None` when it isn't offered for it.
    async fn view(&self, job: &Value, config: &Value) -> Option<AddonView>;
    /// Starts it (idempotent while it runs for this job).
    async fn start(&self, job: &Value, config: &Value) -> Result<AddonView, String>;
    async fn stop(&self, job: &Value, config: &Value) -> Result<AddonView, String>;
    /// A page at `origin` says it is still open; whether that origin belongs to a running add-on.
    fn heartbeat(&self, origin: &str) -> bool;
    /// The service is stopping: stop whatever this add-on runs. Called once, bounded by the
    /// caller's timeout.
    async fn shutdown(&self) {}
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn a_view_serializes_for_the_extension() {
        let v = AddonView { id: "x".into(), label: "X".into(), state: AddonState::Ready, href: Some("http://a.localhost/".into()), error: None, auto_start: false };
        assert_eq!(v.to_json(), json!({ "id": "x", "label": "X", "state": "ready", "href": "http://a.localhost/", "error": null, "autoStart": false }));
    }
}
