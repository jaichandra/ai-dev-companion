//! Runs `cursor-agent` headlessly in a folder (port of `core/cursor-agent.ts`).
//! `--force` lets it edit and run commands without asking (there is nobody to
//! ask), so the run is confined the only ways the CLI offers: `--sandbox
//! enabled`, and the caller's use of a disposable worktree that nothing pushes
//! until a human approves its diff. Unlike the Claude runner there are no
//! per-tool allow-lists. `--trust` skips the interactive "trust this workspace"
//! prompt, which a new worktree always triggers.

use crate::exec::{self, ExecError, RunOptions};
use serde_json::Value;
use std::path::PathBuf;
use std::sync::Arc;
use tokio_util::sync::CancellationToken;

pub struct CursorAgentRunOptions {
    /// Directory the agent runs in: in practice a disposable worktree.
    pub cwd: String,
    pub prompt: String,
    /// Passed as `--model` only when set; unset means Cursor's own default.
    pub model: Option<String>,
    pub cancel: Option<CancellationToken>,
    /// Called with the CLI's pid as soon as it spawns.
    pub on_spawn: Option<Arc<dyn Fn(u32) + Send + Sync>>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CursorAgentRunResult {
    /// The agent's final text response.
    pub text: String,
    pub raw: String,
    /// The chat id, when the CLI reported one.
    pub session_id: Option<String>,
}

/// The argv for a run (without the command).
pub fn cursor_agent_args(model: Option<&str>, prompt: &str) -> Vec<String> {
    let mut args: Vec<String> = ["-p", "--trust", "--force", "--sandbox", "enabled", "--output-format", "json"].map(String::from).to_vec();
    if let Some(m) = model.filter(|m| !m.is_empty()) {
        args.extend(["--model".into(), m.into()]);
    }
    args.push(prompt.into());
    args
}

pub async fn run_cursor_agent(command: &str, opts: CursorAgentRunOptions) -> Result<CursorAgentRunResult, ExecError> {
    let args = cursor_agent_args(opts.model.as_deref(), &opts.prompt);
    let on_spawn = opts.on_spawn.map(|f| -> Box<dyn Fn(u32) + Send + Sync> { Box::new(move |pid| f(pid)) });
    let result = exec::run(command, &args, RunOptions { cwd: Some(PathBuf::from(opts.cwd)), cancel: opts.cancel, on_spawn, ..Default::default() }).await?;
    Ok(parse_cursor_agent_output(&result.stdout))
}

/// `--output-format json` prints one result object; its field names have varied between versions,
/// so anything unexpected degrades to the raw text.
pub fn parse_cursor_agent_output(stdout: &str) -> CursorAgentRunResult {
    let raw = stdout.to_string();
    let Ok(parsed) = serde_json::from_str::<Value>(stdout) else {
        return CursorAgentRunResult { text: raw.clone(), raw, session_id: None };
    };
    if parsed.is_null() {
        return CursorAgentRunResult { text: raw.clone(), raw, session_id: None };
    }
    let text = parsed["result"].as_str().or_else(|| parsed["text"].as_str()).map(str::to_string).unwrap_or_else(|| raw.clone());
    let id = ["session_id", "chatId", "chat_id"].iter().map(|k| &parsed[*k]).find(|v| !v.is_null());
    CursorAgentRunResult { text, raw, session_id: id.and_then(Value::as_str).map(str::to_string) }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    #[test]
    fn the_argv_is_fixed_with_the_model_only_when_set_and_the_prompt_last() {
        assert_eq!(cursor_agent_args(None, "do it"), ["-p", "--trust", "--force", "--sandbox", "enabled", "--output-format", "json", "do it"]);
        assert_eq!(cursor_agent_args(Some("gpt-5"), "p")[7..], ["--model", "gpt-5", "p"]);
        assert_eq!(cursor_agent_args(Some(""), "p").len(), 8);
    }

    #[test]
    fn output_parsing_takes_result_or_text_and_whatever_session_field_is_there_and_degrades_to_raw() {
        let p = parse_cursor_agent_output;
        assert_eq!(p(r#"{"result":"done","session_id":"s1"}"#), CursorAgentRunResult { text: "done".into(), raw: r#"{"result":"done","session_id":"s1"}"#.into(), session_id: Some("s1".into()) });
        assert_eq!(p(r#"{"text":"older","chatId":"c1"}"#).session_id.as_deref(), Some("c1"));
        assert_eq!(p(r#"{"result":"x","chat_id":"c2"}"#).session_id.as_deref(), Some("c2"));
        assert_eq!(p(r#"{"result":"x","session_id":5,"chatId":"c1"}"#).session_id, None, "the first present field decides, and a non-string isn't an id");
        assert_eq!(p(r#"{"other":1}"#).text, r#"{"other":1}"#);
        assert_eq!(p("plain").text, "plain");
        assert_eq!(p("null"), CursorAgentRunResult { text: "null".into(), raw: "null".into(), session_id: None });
    }

    #[tokio::test]
    async fn the_agent_runs_in_the_folder_with_the_prompt_and_its_json_is_parsed() {
        let d = tempfile::tempdir().unwrap();
        let script = d.path().join("cursor-agent");
        std::fs::write(&script, "#!/bin/sh\nprintf '{\"result\":\"ran in %s with %s args\",\"session_id\":\"s-7\"}' \"$(basename \"$PWD\")\" \"$#\"\n").unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        let pid = Arc::new(std::sync::Mutex::new(None));
        let p = pid.clone();
        let r = run_cursor_agent(
            &script.to_string_lossy(),
            CursorAgentRunOptions { cwd: d.path().to_string_lossy().into_owned(), prompt: "p".into(), model: Some("m".into()), cancel: None, on_spawn: Some(Arc::new(move |n| *p.lock().unwrap() = Some(n))) },
        )
        .await
        .unwrap();
        let folder = d.path().file_name().unwrap().to_string_lossy().into_owned();
        assert_eq!((r.text, r.session_id.as_deref()), (format!("ran in {folder} with 10 args"), Some("s-7")));
        assert!(pid.lock().unwrap().is_some());
        let missing = run_cursor_agent("/no/cursor-agent", CursorAgentRunOptions { cwd: "/tmp".into(), prompt: "p".into(), model: None, cancel: None, on_spawn: None }).await;
        assert!(matches!(missing, Err(ExecError::Spawn { .. })));
    }
}
