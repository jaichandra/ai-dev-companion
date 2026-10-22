//! Pure rules for the background scheduler (port of `core/schedule.js`): which
//! tasks are due, quiet hours, the daily Claude-run budget and the weekday
//! digest time. No timers and no I/O: every function takes `now` (epoch ms,
//! read in local time), so tests drive it with plain numbers.

use crate::js_number::json_number;
use crate::js_text::js_trim;
use crate::local_time::{local_day_at, local_parts};
use crate::similar::similar_enabled;
use serde_json::{json, Map, Value};

pub const WATCHERS: [&str; 3] = ["conflicts", "assignedBugs", "reviewRequests"];
pub const WATCHER_DEFAULT_MINUTES: [i64; 3] = [15, 60, 30];
pub const MIN_INTERVAL_MINUTES: i64 = 1;
pub const MAX_INTERVAL_MINUTES: i64 = 1440;
pub const DEFAULT_CLAUDE_RUNS_PER_DAY: i64 = 5;
pub const MAX_CLAUDE_RUNS_PER_DAY: i64 = 50;
pub const DEFAULT_DIGEST_TIME: &str = "08:30";
pub const DEFAULT_NOTIFY_MIN_INTERVAL_MINUTES: i64 = 30;
const MINUTE_MS: f64 = 60.0 * 1000.0;
pub const MAINTENANCE_INTERVAL_MS: f64 = 24.0 * 60.0 * MINUTE_MS;
const MAINTENANCE_FIRST_DELAY_MS: f64 = MINUTE_MS;
// history.embed: embeds new history items for similar-item search.
pub const EMBED_INTERVAL_MS: f64 = 30.0 * MINUTE_MS;
const EMBED_FIRST_DELAY_MS: f64 = 2.0 * MINUTE_MS;
const MAX_WAKE_MS: f64 = MINUTE_MS;
const MIN_WAKE_MS: f64 = 1000.0;

/// The scheduler's task names, in the order a tick runs them.
pub const TASKS: [&str; 6] = ["maintenance", "watcher.conflicts", "watcher.assignedBugs", "watcher.reviewRequests", "digest", "history.embed"];

fn is_watcher(task: &str) -> bool {
    task.starts_with("watcher.")
}

/// "08:30" becomes 510 (minutes after midnight); `None` for anything else.
pub fn parse_hm(text: Option<&str>) -> Option<i32> {
    let t = js_trim(text?);
    let b = t.as_bytes();
    if b.len() != 5 || b[2] != b':' || !b.iter().enumerate().all(|(i, c)| i == 2 || c.is_ascii_digit()) {
        return None;
    }
    let (h, m) = (i32::from(b[0] - b'0') * 10 + i32::from(b[1] - b'0'), i32::from(b[3] - b'0') * 10 + i32::from(b[4] - b'0'));
    (h <= 23 && m <= 59).then_some(h * 60 + m)
}

fn minutes_of_day(now: f64) -> i32 {
    let p = local_parts(now as i64);
    p.hours * 60 + p.minutes
}

/// Local calendar day, "2026-09-29".
pub fn local_day_key(now: f64) -> String {
    let p = local_parts(now as i64);
    format!("{:04}-{:02}-{:02}", p.year, p.month0 + 1, p.day)
}

pub fn is_weekday(now: f64) -> bool {
    (1..=5).contains(&local_parts(now as i64).weekday)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct QuietHours {
    pub start: i32,
    pub end: i32,
}

/// `scheduler.quietHours`, or `None` when unset or malformed (a broken
/// setting never silences anything). Start == end is off.
pub fn quiet_hours_from(config: &Value) -> Option<QuietHours> {
    let q = &config["scheduler"]["quietHours"];
    if !q.is_object() {
        return None;
    }
    let (start, end) = (parse_hm(q["start"].as_str())?, parse_hm(q["end"].as_str())?);
    (start != end).then_some(QuietHours { start, end })
}

/// Whether `now` falls in the quiet hours; they may wrap past midnight.
pub fn in_quiet_hours(now: f64, quiet: Option<QuietHours>) -> bool {
    let Some(q) = quiet else { return false };
    let m = minutes_of_day(now);
    if q.start < q.end { m >= q.start && m < q.end } else { m >= q.start || m < q.end }
}

fn clamp_int(v: &Value, min: i64, max: i64, fallback: i64) -> i64 {
    match v.as_f64() {
        Some(n) if n.fract() == 0.0 && n >= min as f64 && n <= max as f64 => n as i64,
        _ => fallback,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WatcherSetting {
    pub enabled: bool,
    pub interval_minutes: i64,
}

/// Every watcher's effective settings, in `WATCHERS` order; off unless opted in.
pub fn watcher_settings(config: &Value) -> Vec<(&'static str, WatcherSetting)> {
    WATCHERS
        .iter()
        .zip(WATCHER_DEFAULT_MINUTES)
        .map(|(name, default)| {
            let w = &config["watchers"][*name];
            let w = if w.is_object() { w } else { &Value::Null };
            (*name, WatcherSetting { enabled: w["enabled"] == json!(true), interval_minutes: clamp_int(&w["intervalMinutes"], MIN_INTERVAL_MINUTES, MAX_INTERVAL_MINUTES, default) })
        })
        .collect()
}

pub fn budget_limit(config: &Value) -> i64 {
    clamp_int(&config["budget"]["claudeRunsPerDay"], 0, MAX_CLAUDE_RUNS_PER_DAY, DEFAULT_CLAUDE_RUNS_PER_DAY)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DigestSetting {
    pub enabled: bool,
    pub time: String,
}

pub fn digest_settings(config: &Value) -> DigestSetting {
    let d = &config["digest"];
    let time = d["time"].as_str().filter(|t| parse_hm(Some(t)).is_some()).map_or_else(|| DEFAULT_DIGEST_TIME.to_string(), |t| js_trim(t).to_string());
    DigestSetting { enabled: d["enabled"] == json!(true), time }
}

pub fn notify_min_interval_ms(config: &Value) -> f64 {
    clamp_int(&config["notify"]["minIntervalMinutes"], 0, 1440, DEFAULT_NOTIFY_MIN_INTERVAL_MINUTES) as f64 * MINUTE_MS
}

/// The scheduler's persisted state.
#[derive(Debug, Clone, PartialEq)]
pub struct SchedState {
    /// Task name to the epoch ms of its last run, in `TASKS` order.
    pub last_run: Vec<(String, f64)>,
    pub budget_day: String,
    pub budget_used: i64,
    pub last_digest_day: Option<String>,
    /// Per watcher, what it has already seen (key to signature); written by the watchers.
    pub seen: Map<String, Value>,
}

impl SchedState {
    pub fn last_run(&self, task: &str) -> Option<f64> {
        self.last_run.iter().find(|(t, _)| t == task).map(|(_, v)| *v)
    }

    pub fn set_last_run(&mut self, task: &str, at: f64) {
        match self.last_run.iter_mut().find(|(t, _)| t == task) {
            Some(e) => e.1 = at,
            None => self.last_run.push((task.to_string(), at)),
        }
    }

    pub fn to_json(&self) -> Value {
        let last_run: Map<String, Value> = self.last_run.iter().map(|(k, v)| (k.clone(), json_number(*v))).collect();
        json!({
            "version": 1,
            "lastRun": last_run,
            "budget": { "day": self.budget_day, "used": self.budget_used },
            "lastDigestDay": self.last_digest_day,
            "seen": self.seen,
        })
    }
}

/// A fresh scheduler state, or a loaded one with anything malformed dropped.
pub fn normalize_state(raw: &Value, now: f64) -> SchedState {
    let today = local_day_key(now);
    let mut last_run = vec![];
    for task in TASKS {
        // Maintenance always runs a minute after start (like the old startup
        // prune), so its last run is never carried across a restart.
        if task == "maintenance" {
            continue;
        }
        if let Some(v) = raw["lastRun"][task].as_f64().filter(|v| v.is_finite()) {
            last_run.push((task.to_string(), v.min(now)));
        }
    }
    let b = &raw["budget"];
    let (budget_day, budget_used) = match (b["day"].as_str(), b["used"].as_f64()) {
        (Some(day), Some(used)) if day <= today.as_str() && used.fract() == 0.0 && used >= 0.0 => (day.to_string(), used as i64),
        _ => (String::new(), 0),
    };
    let mut seen = Map::new();
    for name in WATCHERS {
        let mut entries = Map::new();
        if let Some(src) = raw["seen"][name].as_object() {
            for (k, v) in src.iter().take(500) {
                if v.as_str().is_some_and(|s| crate::js_text::utf16_len(s) <= 200) {
                    entries.insert(k.clone(), v.clone());
                }
            }
        }
        seen.insert(name.to_string(), Value::Object(entries));
    }
    let last_digest_day = raw["lastDigestDay"].as_str().filter(|d| *d <= today.as_str()).map(String::from);
    SchedState { last_run, budget_day, budget_used, last_digest_day, seen }
}

/// What the due-time rules read.
pub struct Ctx<'a> {
    pub config: &'a Value,
    pub state: &'a SchedState,
    pub now: f64,
    pub started_at: f64,
    /// When set, only these tasks count: an unregistered task must not keep
    /// the loop waking every second.
    pub registered: Option<&'a [&'a str]>,
}

/// When `task` is next due (epoch ms), or `None` when it isn't scheduled at all.
pub fn next_due_at(task: &str, ctx: &Ctx<'_>) -> Option<f64> {
    let last = ctx.state.last_run(task);
    if task == "maintenance" {
        return Some(last.map_or(ctx.started_at + MAINTENANCE_FIRST_DELAY_MS, |l| l + MAINTENANCE_INTERVAL_MS));
    }
    if let Some(name) = task.strip_prefix("watcher.") {
        let w = watcher_settings(ctx.config).into_iter().find(|(n, _)| *n == name)?.1;
        if !w.enabled {
            return None;
        }
        return Some(last.map_or(ctx.now, |l| l + w.interval_minutes as f64 * MINUTE_MS));
    }
    if task == "history.embed" {
        // On unless similar items are turned off; it only calls the LLM
        // when that is ready, and quiet hours don't stop it (it notifies no one).
        if !similar_enabled(ctx.config) {
            return None;
        }
        return Some(last.map_or(ctx.started_at + EMBED_FIRST_DELAY_MS, |l| l + EMBED_INTERVAL_MS));
    }
    if task == "digest" {
        let d = digest_settings(ctx.config);
        if !d.enabled {
            return None;
        }
        let minutes = parse_hm(Some(&d.time))?;
        let now = ctx.now as i64;
        if is_weekday(ctx.now) && ctx.state.last_digest_day.as_deref() != Some(local_day_key(ctx.now).as_str()) {
            return Some(local_day_at(now, minutes, 0) as f64);
        }
        // Tomorrow at the same time; the tick re-checks the weekday then.
        return Some(local_day_at(now, minutes, 1) as f64);
    }
    None
}

fn runnable_tasks(ctx: &Ctx<'_>) -> Vec<&'static str> {
    TASKS.iter().copied().filter(|t| ctx.registered.is_none_or(|r| r.contains(t))).collect()
}

/// The tasks due at `now`, in `TASKS` order. Quiet hours skip the watchers
/// (nothing burns the budget or wakes anyone at night); maintenance and the
/// digest still run.
pub fn due_tasks(ctx: &Ctx<'_>) -> Vec<&'static str> {
    let quiet = in_quiet_hours(ctx.now, quiet_hours_from(ctx.config));
    runnable_tasks(ctx)
        .into_iter()
        .filter(|task| !(quiet && is_watcher(task)) && next_due_at(task, ctx).is_some_and(|due| due <= ctx.now))
        .collect()
}

/// How long (ms) the scheduler may sleep before its next tick: until the next
/// due task, but never more than a minute, so a Settings change applies
/// within a minute with no restart.
pub fn next_wake_delay(ctx: &Ctx<'_>) -> f64 {
    let mut soonest = ctx.now + MAX_WAKE_MS;
    let quiet = quiet_hours_from(ctx.config);
    let in_quiet = in_quiet_hours(ctx.now, quiet);
    if let (true, Some(q)) = (in_quiet, quiet) {
        // Watchers can't run until the window ends: wake then, not every second.
        let mut end = local_day_at(ctx.now as i64, q.end, 0) as f64;
        if end <= ctx.now {
            end = local_day_at(ctx.now as i64, q.end, 1) as f64;
        }
        soonest = soonest.min(end);
    }
    for task in runnable_tasks(ctx) {
        if in_quiet && is_watcher(task) {
            continue;
        }
        if let Some(due) = next_due_at(task, ctx) {
            soonest = soonest.min(due);
        }
    }
    (soonest - ctx.now).clamp(MIN_WAKE_MS, MAX_WAKE_MS)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BudgetUsage {
    pub day: String,
    pub used: i64,
    pub limit: i64,
    pub remaining: i64,
}

/// Today's budget use. A new local day starts at zero.
pub fn budget_usage(state: &SchedState, now: f64, limit: i64) -> BudgetUsage {
    let day = local_day_key(now);
    let used = if state.budget_day == day { state.budget_used } else { 0 };
    BudgetUsage { day, used, limit, remaining: (limit - used).max(0) }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Grant {
    pub granted: bool,
    /// Why not: busy | quiet | budget.
    pub reason: Option<&'static str>,
    pub usage: BudgetUsage,
    pub state: SchedState,
}

/// May a background task start one more Claude run? Refused while another
/// background run is still going ("busy": one at a time), in quiet hours
/// ("quiet"), or when today's budget is spent ("budget"). A grant returns the
/// state with the run counted; a refusal returns it unchanged.
pub fn grant_claude_run(state: &SchedState, now: f64, limit: i64, busy: bool, quiet: bool) -> Grant {
    let usage = budget_usage(state, now, limit);
    let refuse = |reason| Grant { granted: false, reason: Some(reason), usage: usage.clone(), state: state.clone() };
    if busy {
        return refuse("busy");
    }
    if quiet {
        return refuse("quiet");
    }
    if usage.remaining <= 0 {
        return refuse("budget");
    }
    let mut next = state.clone();
    next.budget_day = usage.day.clone();
    next.budget_used = usage.used + 1;
    Grant { granted: true, reason: None, usage: budget_usage(&next, now, limit), state: next }
}

/// Gives back one run counted today (a run that never started). Never below zero.
pub fn refund_claude_run(state: &SchedState, now: f64) -> SchedState {
    let day = local_day_key(now);
    if state.budget_day != day || state.budget_used <= 0 {
        return state.clone();
    }
    SchedState { budget_day: day, budget_used: state.budget_used - 1, ..state.clone() }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::local_time::local_ms;

    // Local-time instants: Tuesday 29 Sep 2026 and Sunday 27 Sep 2026.
    fn at(day: i32, h: i32, m: i32) -> f64 {
        local_ms(2026, 8, day, h, m) as f64
    }
    fn tue_0800() -> f64 {
        at(29, 8, 0)
    }
    const MIN: f64 = 60.0 * 1000.0;

    /// The embed task is on by default; most tests are about the watchers and the digest.
    fn no_embed() -> Value {
        json!({ "similar": { "enabled": false } })
    }

    fn fresh() -> SchedState {
        normalize_state(&Value::Null, 0.0)
    }

    fn with_runs(runs: &[(&str, f64)]) -> SchedState {
        SchedState { last_run: runs.iter().map(|(t, v)| (t.to_string(), *v)).collect(), ..fresh() }
    }

    fn ctx<'a>(config: &'a Value, state: &'a SchedState, now: f64, started_at: f64) -> Ctx<'a> {
        Ctx { config, state, now, started_at, registered: None }
    }

    #[test]
    fn parse_hm_local_day_key_and_is_weekday_read_local_time() {
        assert_eq!(parse_hm(Some("08:30")), Some(510));
        assert_eq!(parse_hm(Some(" 23:59 ")), Some(1439));
        for bad in [Some("8:30"), Some("24:00"), Some("12:60"), Some(""), None, Some("08:3x"), Some("0830x")] {
            assert_eq!(parse_hm(bad), None, "{bad:?}");
        }
        assert_eq!(local_day_key(tue_0800()), "2026-09-29");
        assert!(is_weekday(tue_0800()));
        assert!(!is_weekday(at(27, 8, 0)));
    }

    #[test]
    fn quiet_hours_off_when_unset_malformed_or_empty_and_wrap_past_midnight() {
        assert_eq!(quiet_hours_from(&json!({})), None);
        assert_eq!(quiet_hours_from(&json!({ "scheduler": { "quietHours": { "start": "7pm", "end": "08:00" } } })), None);
        assert_eq!(quiet_hours_from(&json!({ "scheduler": { "quietHours": { "start": "08:00", "end": "08:00" } } })), None);
        let night = quiet_hours_from(&json!({ "scheduler": { "quietHours": { "start": "19:00", "end": "08:00" } } }));
        assert_eq!(night, Some(QuietHours { start: 1140, end: 480 }));
        assert!(in_quiet_hours(at(29, 22, 0), night));
        assert!(in_quiet_hours(at(29, 7, 59), night));
        assert!(!in_quiet_hours(tue_0800(), night));
        let lunch = Some(QuietHours { start: 720, end: 780 });
        assert!(in_quiet_hours(at(29, 12, 30), lunch));
        assert!(!in_quiet_hours(at(29, 13, 0), lunch));
        assert!(!in_quiet_hours(tue_0800(), None));
    }

    #[test]
    fn watchers_are_off_by_default_and_intervals_fall_back_to_the_defaults_when_out_of_range() {
        let d = watcher_settings(&json!({}));
        assert_eq!(d.iter().map(|(n, w)| (*n, w.enabled, w.interval_minutes)).collect::<Vec<_>>(), [("conflicts", false, 15), ("assignedBugs", false, 60), ("reviewRequests", false, 30)]);
        let w = watcher_settings(&json!({ "watchers": { "conflicts": { "enabled": true, "intervalMinutes": 1 }, "assignedBugs": { "enabled": "yes", "intervalMinutes": 0 } } }));
        assert_eq!(w[0].1, WatcherSetting { enabled: true, interval_minutes: 1 });
        assert_eq!(w[1].1, WatcherSetting { enabled: false, interval_minutes: 60 });
        assert_eq!(budget_limit(&json!({})), 5);
        assert_eq!(budget_limit(&json!({ "budget": { "claudeRunsPerDay": 0 } })), 0);
        assert_eq!(budget_limit(&json!({ "budget": { "claudeRunsPerDay": 99 } })), 5);
        assert_eq!(digest_settings(&json!({})), DigestSetting { enabled: false, time: "08:30".into() });
        assert_eq!(digest_settings(&json!({ "digest": { "enabled": true, "time": "09:15" } })), DigestSetting { enabled: true, time: "09:15".into() });
        assert_eq!(digest_settings(&json!({ "digest": { "time": " 09:15 " } })).time, "09:15");
        assert_eq!(notify_min_interval_ms(&json!({})), 30.0 * MIN);
        assert_eq!(notify_min_interval_ms(&json!({ "notify": { "minIntervalMinutes": 0 } })), 0.0);
    }

    #[test]
    fn normalize_state_keeps_valid_fields_drops_junk_and_never_carries_maintenance_over() {
        assert_eq!(
            fresh().to_json(),
            json!({ "version": 1, "lastRun": {}, "budget": { "day": "", "used": 0 }, "lastDigestDay": null, "seen": { "conflicts": {}, "assignedBugs": {}, "reviewRequests": {} } })
        );
        let loaded = normalize_state(
            &json!({
                "lastRun": { "maintenance": 5, "watcher.conflicts": 7, "bogus": 1 },
                "budget": { "day": "2026-09-29", "used": -1 },
                "lastDigestDay": "2026-09-28",
                "seen": { "conflicts": { "CI/sample-app#1": "a:b", "bad": 5 } },
            }),
            tue_0800(),
        );
        assert_eq!(loaded.last_run, [("watcher.conflicts".to_string(), 7.0)]);
        assert_eq!((loaded.budget_day.as_str(), loaded.budget_used), ("", 0));
        assert_eq!(loaded.last_digest_day.as_deref(), Some("2026-09-28"));
        assert_eq!(loaded.seen["conflicts"], json!({ "CI/sample-app#1": "a:b" }));
    }

    #[test]
    fn maintenance_runs_a_minute_after_start_and_then_daily_an_enabled_watcher_runs_at_once_then_on_its_interval() {
        let config = json!({ "similar": { "enabled": false }, "watchers": { "conflicts": { "enabled": true, "intervalMinutes": 15 } } });
        let started = tue_0800();
        assert_eq!(due_tasks(&ctx(&config, &fresh(), started, started)), ["watcher.conflicts"]);
        assert_eq!(due_tasks(&ctx(&config, &fresh(), started + MIN, started)), ["maintenance", "watcher.conflicts"]);
        let ran = with_runs(&[("maintenance", started + MIN), ("watcher.conflicts", started + MIN)]);
        assert!(due_tasks(&ctx(&config, &ran, started + 10.0 * MIN, started)).is_empty());
        assert_eq!(due_tasks(&ctx(&config, &ran, started + 16.0 * MIN, started)), ["watcher.conflicts"]);
        assert_eq!(due_tasks(&ctx(&config, &ran, started + 24.0 * 60.0 * MIN + MIN, started)), ["maintenance", "watcher.conflicts"]);
    }

    #[test]
    fn quiet_hours_skip_the_watchers_but_not_maintenance() {
        let config = json!({ "similar": { "enabled": false }, "watchers": { "conflicts": { "enabled": true } }, "scheduler": { "quietHours": { "start": "19:00", "end": "08:00" } } });
        let night = at(29, 23, 0);
        assert_eq!(due_tasks(&ctx(&config, &fresh(), night + MIN, night)), ["maintenance"]);
    }

    #[test]
    fn the_digest_is_due_once_per_weekday_after_its_time() {
        let config = json!({ "similar": { "enabled": false }, "digest": { "enabled": true, "time": "08:30" } });
        let state = fresh();
        let due = |now: f64, st: &SchedState| {
            let st = SchedState { last_run: vec![("maintenance".into(), now)], ..st.clone() };
            due_tasks(&ctx(&config, &st, now, now - 2.0 * MIN))
        };
        assert!(due(at(29, 8, 29), &state).is_empty());
        assert_eq!(due(at(29, 8, 30), &state), ["digest"]);
        let done = SchedState { last_digest_day: Some("2026-09-29".into()), ..state.clone() };
        assert!(due(at(29, 17, 0), &done).is_empty());
        assert!(due(at(27, 9, 0), &state).is_empty(), "not on a Sunday");
        let off = no_embed();
        assert!(due_tasks(&ctx(&off, &state, at(29, 9, 0), at(29, 9, 0))).is_empty());
    }

    #[test]
    fn next_wake_delay_sleeps_until_the_next_due_task_at_most_a_minute_and_at_least_a_second() {
        let state = with_runs(&[("maintenance", tue_0800()), ("watcher.conflicts", tue_0800())]);
        let config = json!({ "similar": { "enabled": false }, "watchers": { "conflicts": { "enabled": true, "intervalMinutes": 1 } } });
        assert_eq!(next_wake_delay(&ctx(&config, &state, tue_0800() + 30_000.0, tue_0800())), 30_000.0);
        let none = no_embed();
        assert_eq!(next_wake_delay(&ctx(&none, &state, tue_0800(), tue_0800())), MIN);
        assert_eq!(next_wake_delay(&ctx(&config, &state, tue_0800() + 5.0 * MIN, tue_0800())), 1000.0);
    }

    #[test]
    fn grant_claude_run_counts_runs_per_local_day_and_refuses_when_busy_quiet_or_spent() {
        let g = |s: &SchedState, now: f64, limit: i64, busy: bool, quiet: bool| grant_claude_run(s, now, limit, busy, quiet);
        let r1 = g(&fresh(), tue_0800(), 2, false, false);
        assert!(r1.granted);
        assert_eq!(r1.usage, BudgetUsage { day: "2026-09-29".into(), used: 1, limit: 2, remaining: 1 });
        let state = r1.state;
        assert_eq!(g(&state, tue_0800(), 2, true, false).reason, Some("busy"));
        assert_eq!(g(&state, tue_0800(), 2, false, true).reason, Some("quiet"));
        let state = g(&state, tue_0800(), 2, false, false).state;
        let spent = g(&state, tue_0800(), 2, false, false);
        assert!(!spent.granted);
        assert_eq!(spent.reason, Some("budget"));
        assert_eq!(spent.state, state, "a refusal leaves the state as it was");
        assert!(g(&state, at(30, 8, 0), 2, false, false).granted, "a new day starts at zero");
        assert_eq!(g(&fresh(), tue_0800(), 0, false, false).reason, Some("budget"));
    }

    #[test]
    fn refund_gives_back_one_run_today_and_never_goes_below_zero() {
        let state = grant_claude_run(&fresh(), tue_0800(), 5, false, false).state;
        let back = refund_claude_run(&state, tue_0800());
        assert_eq!(back.budget_used, 0);
        assert_eq!(refund_claude_run(&back, tue_0800()).budget_used, 0);
        assert_eq!(refund_claude_run(&state, at(30, 8, 0)), state, "another day's count is left alone");
    }

    #[test]
    fn next_wake_delay_does_not_spin_through_quiet_hours_with_a_watcher_enabled() {
        let config = json!({ "similar": { "enabled": false }, "watchers": { "conflicts": { "enabled": true, "intervalMinutes": 1 } }, "scheduler": { "quietHours": { "start": "19:00", "end": "08:00" } } });
        let night = at(29, 23, 0);
        let state = with_runs(&[("maintenance", night), ("watcher.conflicts", night - 10.0 * MIN)]);
        assert_eq!(next_wake_delay(&ctx(&config, &state, night, night - 60.0 * MIN)), MIN, "no 1 s spin");
        assert_eq!(next_wake_delay(&ctx(&config, &state, at(29, 7, 59) + 30_000.0, night - 600.0 * MIN)), 30_000.0, "wakes at the window end");
        assert_eq!(next_wake_delay(&ctx(&config, &state, at(30, 8, 0), night - 600.0 * MIN)), 1000.0, "after the window the watcher is due");
    }

    #[test]
    fn next_wake_delay_a_digest_already_done_today_is_not_a_past_wake_time() {
        let config = json!({ "similar": { "enabled": false }, "digest": { "enabled": true, "time": "08:30" } });
        let now = at(29, 17, 0);
        let state = SchedState { last_digest_day: Some("2026-09-29".into()), ..with_runs(&[("maintenance", now)]) };
        assert_eq!(next_wake_delay(&ctx(&config, &state, now, now - 600.0 * MIN)), MIN);
    }

    #[test]
    fn quiet_hours_boundaries_start_inclusive_end_exclusive_midnight_and_2359() {
        let night = Some(QuietHours { start: 1140, end: 480 });
        assert!(in_quiet_hours(at(29, 19, 0), night), "exactly start");
        assert!(!in_quiet_hours(at(29, 18, 59), night));
        assert!(in_quiet_hours(at(29, 0, 0), night), "00:00");
        assert!(in_quiet_hours(at(29, 23, 59), night), "23:59");
        assert!(!in_quiet_hours(at(29, 8, 0), night), "exactly end");
        let early = quiet_hours_from(&json!({ "scheduler": { "quietHours": { "start": "00:00", "end": "06:00" } } }));
        assert_eq!(early, Some(QuietHours { start: 0, end: 360 }));
        assert!(in_quiet_hours(at(29, 0, 0), early));
        assert!(in_quiet_hours(at(29, 5, 59), early));
        assert!(!in_quiet_hours(at(29, 6, 0), early));
        assert!(!in_quiet_hours(at(29, 23, 59), early));
    }

    #[test]
    fn normalize_state_clamps_a_future_last_run_and_resets_a_future_budget_day() {
        let now = tue_0800();
        let st = normalize_state(&json!({ "lastRun": { "watcher.conflicts": now + 5.0 * 60.0 * MIN }, "budget": { "day": "2026-10-05", "used": 0 }, "lastDigestDay": "2026-10-05" }), now);
        assert_eq!(st.last_run("watcher.conflicts"), Some(now));
        assert_eq!((st.budget_day.as_str(), st.budget_used), ("", 0));
        assert_eq!(st.last_digest_day, None);
        let g = grant_claude_run(&st, now, 1, false, false);
        assert!(g.granted);
        assert_eq!(grant_claude_run(&g.state, now, 1, false, false).reason, Some("budget"));
    }

    #[test]
    fn history_embed_runs_two_minutes_after_start_then_every_30_minutes_also_in_quiet_hours_unless_turned_off() {
        let state = fresh();
        let empty = json!({});
        let quiet = json!({ "scheduler": { "quietHours": { "start": "19:00", "end": "08:00" } } });
        let night = at(29, 23, 0);
        let s0 = tue_0800();
        assert_eq!(due_tasks(&ctx(&empty, &state, s0 + MIN, s0)), ["maintenance"]);
        assert_eq!(due_tasks(&ctx(&empty, &state, s0 + 2.0 * MIN, s0)), ["maintenance", "history.embed"]);
        let ran = with_runs(&[("maintenance", s0 + MIN), ("history.embed", s0 + 2.0 * MIN)]);
        assert!(due_tasks(&ctx(&empty, &ran, s0 + 31.0 * MIN, s0)).is_empty());
        assert_eq!(due_tasks(&ctx(&empty, &ran, s0 + 32.0 * MIN, s0)), ["history.embed"]);
        assert_eq!(due_tasks(&ctx(&quiet, &state, night + 2.0 * MIN, night)), ["maintenance", "history.embed"]);
        let off = no_embed();
        assert_eq!(due_tasks(&ctx(&off, &state, s0 + 2.0 * MIN, s0)), ["maintenance"]);
        assert_eq!(TASKS[TASKS.len() - 1], "history.embed", "it runs last in a tick");
        assert_eq!(normalize_state(&json!({ "lastRun": { "history.embed": 5 } }), s0).last_run("history.embed"), Some(5.0), "its last run survives a restart");
    }

    #[test]
    fn wake_computation_only_counts_registered_tasks() {
        let state = with_runs(&[("maintenance", tue_0800())]);
        let empty = json!({});
        let base = ctx(&empty, &state, tue_0800() + 60.0 * MIN, tue_0800());
        let only = ["maintenance"];
        let registered = Ctx { registered: Some(&only), ..ctx(&empty, &state, base.now, base.started_at) };
        assert_eq!(next_wake_delay(&registered), MIN);
        assert!(due_tasks(&registered).is_empty());
        assert_eq!(next_wake_delay(&base), 1000.0, "without a registered list every task counts (the old behaviour)");
    }

    #[test]
    fn a_registered_history_embed_is_due_at_start_plus_2_min_then_every_30_min() {
        let empty = json!({});
        let regs = ["maintenance", "history.embed"];
        fn reg<'a>(empty: &'a Value, regs: &'a [&'a str], state: &'a SchedState, now: f64) -> Ctx<'a> {
            Ctx { registered: Some(regs), ..ctx(empty, state, now, tue_0800()) }
        }
        let mut state = with_runs(&[("maintenance", tue_0800())]);
        assert!(due_tasks(&reg(&empty, &regs, &state, tue_0800() + MIN)).is_empty());
        assert_eq!(next_wake_delay(&reg(&empty, &regs, &state, tue_0800() + MIN)), MIN);
        assert_eq!(due_tasks(&reg(&empty, &regs, &state, tue_0800() + 2.0 * MIN)), ["history.embed"]);
        state.set_last_run("history.embed", tue_0800() + 2.0 * MIN);
        assert!(due_tasks(&reg(&empty, &regs, &state, tue_0800() + 31.0 * MIN)).is_empty());
        assert_eq!(due_tasks(&reg(&empty, &regs, &state, tue_0800() + 32.0 * MIN)), ["history.embed"]);
        assert_eq!(next_wake_delay(&reg(&empty, &regs, &state, tue_0800() + 3.0 * MIN)), MIN);
    }

    #[test]
    fn a_registered_task_that_keeps_failing_doesnt_spin() {
        let empty = json!({});
        let regs = ["maintenance", "history.embed"];
        let state = with_runs(&[("maintenance", tue_0800()), ("history.embed", tue_0800() + 2.0 * MIN)]);
        let c = Ctx { registered: Some(&regs), ..ctx(&empty, &state, tue_0800() + 3.0 * MIN, tue_0800()) };
        assert_eq!(next_wake_delay(&c), MIN);
    }
}
