//! Where a piece of AI work runs (port of `core/route-policy.js`): the
//! on-prem models through the LLM ("onprem") or Claude through the
//! user's own Claude Code login ("claude"). Explicit rules, never a learned
//! router: code changes, anything that needs tools, diagnosis and analysis
//! always go to Claude; the small text-only steps (triage, classifying,
//! condensing, embedding) go on-prem when the LLM was detected, otherwise to
//! Claude. Callers record the answer in `events.metrics_json.tier`.

pub const ONPREM_TASKS: [&str; 4] = ["triage", "classify", "condense", "embed"];
pub const CLAUDE_TASKS: [&str; 3] = ["code-change", "diagnosis", "analysis"];
/// The on-prem chat models take 262K tokens in; stay well inside that.
pub const MAX_ONPREM_BYTES: f64 = 200.0 * 1024.0;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tier {
    Onprem,
    Claude,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TierChoice {
    pub tier: Tier,
    pub reason: String,
}

#[derive(Debug, Clone, Copy, Default)]
pub struct TierRequest<'a> {
    pub task: Option<&'a str>,
    /// Size of the text; NaN stands for "not a number" and is treated as too
    /// large, like infinity. `None` means unspecified (0).
    pub bytes: Option<f64>,
    pub needs_tools: bool,
}

fn choice(tier: Tier, reason: impl Into<String>) -> TierChoice {
    TierChoice { tier, reason: reason.into() }
}

pub fn choose_tier(req: TierRequest<'_>, onprem_available: bool) -> TierChoice {
    if req.needs_tools {
        return choice(Tier::Claude, "needs tools");
    }
    let task = req.task.unwrap_or("");
    if CLAUDE_TASKS.contains(&task) {
        return choice(Tier::Claude, format!("{task} always runs on Claude"));
    }
    if !ONPREM_TASKS.contains(&task) {
        let shown: String = req.task.unwrap_or("undefined").chars().take(40).collect();
        return choice(Tier::Claude, format!("no on-prem rule for \"{shown}\""));
    }
    if !onprem_available {
        return choice(Tier::Claude, "no LLM is available");
    }
    let bytes = req.bytes.unwrap_or(0.0);
    if !bytes.is_finite() || bytes > MAX_ONPREM_BYTES {
        return choice(Tier::Claude, "too large for the on-prem model");
    }
    choice(Tier::Onprem, format!("{task} runs on-prem"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tier(task: &str, bytes: Option<f64>, on: bool) -> Tier {
        choose_tier(TierRequest { task: Some(task), bytes, needs_tools: false }, on).tier
    }

    #[test]
    fn code_changes_tool_use_diagnosis_and_analysis_always_go_to_claude() {
        for task in ["code-change", "diagnosis", "analysis"] {
            assert_eq!(tier(task, None, true), Tier::Claude, "{task}");
        }
        let r = TierRequest { task: Some("triage"), needs_tools: true, ..Default::default() };
        assert_eq!(choose_tier(r, true).tier, Tier::Claude);
        assert_eq!(tier("something-new", None, true), Tier::Claude);
    }

    #[test]
    fn the_small_text_only_steps_go_on_prem_only_when_the_llm_is_available_and_the_text_fits() {
        for task in ["triage", "classify", "condense", "embed"] {
            assert_eq!(tier(task, Some(1000.0), true), Tier::Onprem, "{task}");
            assert_eq!(tier(task, Some(1000.0), false), Tier::Claude, "{task}");
        }
        assert_eq!(tier("triage", Some(MAX_ONPREM_BYTES + 1.0), true), Tier::Claude);
        assert_eq!(tier("triage", None, false), Tier::Claude, "no LLM info means Claude");
        let r = choose_tier(TierRequest { task: Some("triage"), ..Default::default() }, false);
        assert!(r.reason.contains("is available"));
    }

    #[test]
    fn the_byte_limit_is_inclusive_and_odd_sizes_are_handled_without_panicking() {
        assert_eq!(tier("triage", Some(MAX_ONPREM_BYTES), true), Tier::Onprem, "exactly the limit still fits");
        assert_eq!(tier("triage", Some(MAX_ONPREM_BYTES + 1.0), true), Tier::Claude);
        for bytes in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY] {
            let r = choose_tier(TierRequest { task: Some("triage"), bytes: Some(bytes), ..Default::default() }, true);
            assert_eq!(r.tier, Tier::Claude, "{bytes}");
            assert!(r.reason.contains("too large"), "{bytes}");
        }
        assert_eq!(tier("triage", Some(0.0), true), Tier::Onprem);
        assert_eq!(tier("triage", Some(-5.0), true), Tier::Onprem, "a negative size is just small");
        assert_eq!(choose_tier(TierRequest::default(), true).tier, Tier::Claude, "no task, no on-prem rule");
        let long = "x".repeat(500);
        let r = choose_tier(TierRequest { task: Some(&long), ..Default::default() }, true);
        assert!(r.reason.len() < 80, "an odd task name is clipped in the reason");
        assert_eq!(r.reason, format!("no on-prem rule for \"{}\"", "x".repeat(40)));
    }
}
