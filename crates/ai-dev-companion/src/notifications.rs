//! The inbox: what background work found for you (a conflict resolution ready
//! to review, an analysis, a review request, the morning digest); port of
//! `core/notifications.js`. The extension badge counts the unseen items, the
//! menu lists them under "Ready for you", `companion inbox` prints them, and
//! the extension turns new ones into Chrome notifications: batched, at most
//! one every `notify.minIntervalMinutes` unless one is urgent, and none in
//! quiet hours (held for the digest). Saved to `notifications.json` (0600) on
//! the installed copy, in memory otherwise. Titles and bodies come from PR and
//! ticket text, so they are clipped plain text and links must be https.
//! Redacting secrets is the caller's job.

use crate::ids::{is_lower_uuid, uuid_v4};
use crate::js_text::{is_js_space, js_to_string, js_trim, slice_utf16, utf16_len};
use serde::Serialize;
use serde_json::Value;
use std::io;
use std::path::{Path, PathBuf};
use url::Url;

pub const KINDS: [&str; 4] = ["conflict", "analysis", "review-request", "digest"];
pub const MAX_ITEMS: usize = 100;
const DAY_MS: i64 = 24 * 60 * 60 * 1000;
const SEEN_TTL_MS: i64 = 7 * DAY_MS;
const UNSEEN_TTL_MS: i64 = 14 * DAY_MS;
const TITLE_MAX: usize = 200;
const BODY_MAX: usize = 1000;
const KEY_MAX: usize = 400;
const DEFAULT_MIN_INTERVAL_MS: f64 = 30.0 * 60.0 * 1000.0;

/// Control, C1 and invisible/bidi characters out (newlines stay for multiline
/// text), then whitespace collapsed (or runs of blank lines), trimmed, and cut
/// to `max` UTF-16 units with an ellipsis.
fn clip(text: Option<&Value>, max: usize, multiline: bool) -> String {
    let Some(Value::String(text)) = text else { return String::new() };
    let cleaned: String = text
        .chars()
        .map(|c| if matches!(c, '\u{0}'..='\u{8}' | '\u{b}'..='\u{1f}' | '\u{7f}'..='\u{9f}') { ' ' } else { c })
        .filter(|c| !matches!(c, '\u{200b}'..='\u{200f}' | '\u{202a}'..='\u{202e}' | '\u{2060}'..='\u{2064}' | '\u{2066}'..='\u{2069}' | '\u{feff}'))
        .collect();
    let shaped = if multiline {
        let mut out = String::with_capacity(cleaned.len());
        let mut newlines = 0;
        for c in cleaned.chars() {
            if c == '\n' {
                newlines += 1;
                if newlines <= 2 {
                    out.push(c);
                }
            } else {
                newlines = 0;
                out.push(c);
            }
        }
        out
    } else {
        cleaned.split(is_js_space).filter(|p| !p.is_empty()).collect::<Vec<_>>().join(" ")
    };
    let clean = js_trim(&shaped);
    if utf16_len(clean) > max { format!("{}\u{2026}", slice_utf16(clean, max - 1)) } else { clean.to_string() }
}

fn https_url(value: &str) -> Option<String> {
    let u = Url::parse(value).ok()?;
    (u.scheme() == "https" && u.username().is_empty() && u.password().is_none_or(str::is_empty)).then(|| u.to_string())
}

fn optional_string(v: Option<&Value>, max: usize) -> Option<String> {
    v.and_then(Value::as_str).filter(|s| !s.is_empty() && utf16_len(s) <= max).map(str::to_string)
}

fn finite(v: Option<&Value>) -> Option<f64> {
    v.and_then(Value::as_f64).filter(|f| f.is_finite())
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Item {
    pub id: String,
    pub key: String,
    pub kind: String,
    pub title: String,
    pub body: String,
    pub url: Option<String>,
    pub urgent: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub watcher: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub scope_key: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub job_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub feature_id: Option<String>,
    pub created_at: i64,
    pub updated_at: i64,
    /// When the thing the item is about happened (a PR's last update), if the
    /// producer knows; display only.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub event_at: Option<i64>,
    pub seen_at: Option<i64>,
    pub opened_at: Option<i64>,
    pub announced_at: Option<i64>,
}

/// A new item from untrusted parts; an error on a missing key, kind or title.
pub fn clean_item(input: &Value, now: i64, id: &str) -> Result<Item, String> {
    let Some(obj) = input.as_object() else { return Err("A notification must be an object.".into()) };
    let key = obj.get("key").and_then(Value::as_str).map_or("", js_trim);
    if key.is_empty() || utf16_len(key) > KEY_MAX {
        return Err("A notification needs a key.".into());
    }
    let kind = obj.get("kind").and_then(Value::as_str).filter(|k| KINDS.contains(k));
    let Some(kind) = kind else {
        let shown = obj.get("kind").map_or_else(|| "undefined".to_string(), js_to_string);
        return Err(format!("Unknown notification kind: {}", slice_utf16(&shown, 30)));
    };
    let title = clip(obj.get("title"), TITLE_MAX, false);
    if title.is_empty() {
        return Err("A notification needs a title.".into());
    }
    let event_at = finite(obj.get("eventAt")).filter(|e| *e > 0.0).map(|e| e.min(now as f64) as i64);
    Ok(Item {
        id: id.to_string(),
        key: key.to_string(),
        kind: kind.to_string(),
        title,
        body: clip(obj.get("body"), BODY_MAX, true),
        url: crate::js_text::js_truthy(obj.get("url")).then(|| obj.get("url").and_then(Value::as_str).and_then(https_url)).flatten(),
        urgent: obj.get("urgent") == Some(&Value::Bool(true)),
        watcher: optional_string(obj.get("watcher"), 40),
        scope_key: optional_string(obj.get("scopeKey"), 300),
        job_id: obj.get("jobId").and_then(Value::as_str).filter(|j| is_lower_uuid(j)).map(str::to_string),
        feature_id: optional_string(obj.get("featureId"), 60),
        created_at: now,
        updated_at: now,
        event_at,
        seen_at: None,
        opened_at: None,
        announced_at: None,
    })
}

fn seen(i: &Item) -> bool {
    i.seen_at.is_some_and(|s| s != 0)
}

/// Drops seen items after a week, unseen ones after two, and keeps the newest 100.
pub fn prune(items: &[Item], now: i64) -> Vec<Item> {
    let mut kept: Vec<Item> = items
        .iter()
        .filter(|i| match i.seen_at.filter(|s| *s != 0) {
            Some(s) => now - s < SEEN_TTL_MS,
            None => now - i.created_at < UNSEEN_TTL_MS,
        })
        .cloned()
        .collect();
    kept.sort_by_key(|i| std::cmp::Reverse(i.updated_at));
    kept.truncate(MAX_ITEMS);
    kept
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Announcement {
    pub ids: Vec<String>,
    pub urgent: bool,
    pub title: String,
    pub message: String,
    pub url: Option<String>,
}

/// What to announce as one Chrome notification now, or `None`: nothing new,
/// quiet hours, or the last one was under `min_interval_ms` ago, unless a new
/// item is urgent (a conflict on a PR that already has approvals).
pub fn plan_announcement(items: &[Item], last_announced_at: i64, now: i64, min_interval_ms: f64, quiet: bool) -> Option<Announcement> {
    let pending: Vec<&Item> = items.iter().filter(|i| !seen(i) && i.announced_at.is_none_or(|a| a == 0)).collect();
    if pending.is_empty() || quiet {
        return None;
    }
    let urgent = pending.iter().any(|i| i.urgent);
    if !urgent && ((now - last_announced_at) as f64) < min_interval_ms {
        return None;
    }
    let mut ordered = pending;
    ordered.sort_by(|a, b| b.urgent.cmp(&a.urgent).then(b.updated_at.cmp(&a.updated_at)));
    let single = (ordered.len() == 1).then(|| ordered[0]);
    Some(Announcement {
        ids: ordered.iter().map(|i| i.id.clone()).collect(),
        urgent,
        title: single.map_or_else(|| format!("{} things are ready for you", ordered.len()), |s| s.title.clone()),
        message: match single {
            Some(s) if !s.body.is_empty() => s.body.clone(),
            Some(_) => "Open \u{2728} to see it.".into(),
            None => ordered.iter().take(3).map(|i| format!("\u{2022} {}", i.title)).collect::<Vec<_>>().join("\n"),
        },
        url: single.and_then(|s| s.url.clone()),
    })
}

/// An item read back from the file goes through the same cleaning as a new one
/// (a hand-edited file must not smuggle in a long title or a javascript: link);
/// ids and times are kept. `None` when it isn't a usable item.
fn clean_loaded(raw: &Value) -> Option<Item> {
    let obj = raw.as_object()?;
    let id = obj.get("id").and_then(Value::as_str).filter(|s| !s.is_empty() && utf16_len(s) <= 100)?;
    let created = finite(obj.get("createdAt"))? as i64;
    let item = clean_item(raw, created, id).ok()?;
    let time = |k: &str| finite(obj.get(k)).map(|f| f as i64);
    Some(Item {
        updated_at: time("updatedAt").unwrap_or(created),
        seen_at: time("seenAt"),
        opened_at: time("openedAt"),
        announced_at: time("announcedAt"),
        ..item
    })
}

struct State {
    items: Vec<Item>,
    last_announced_at: i64,
}

fn load_state(file: Option<&Path>, fs: &dyn InboxFs) -> State {
    let empty = State { items: vec![], last_announced_at: 0 };
    let Some(file) = file else { return empty };
    let Ok(text) = fs.read(file) else { return empty };
    let Ok(raw) = serde_json::from_str::<Value>(&text) else { return empty };
    State {
        items: raw.get("items").and_then(Value::as_array).map(|a| a.iter().filter_map(clean_loaded).collect()).unwrap_or_default(),
        last_announced_at: finite(raw.get("lastAnnouncedAt")).map_or(0, |f| f as i64),
    }
}

/// The few file operations the inbox needs, so tests can make them fail.
pub trait InboxFs: Send + Sync {
    fn read(&self, path: &Path) -> io::Result<String>;
    fn create_dir_all_private(&self, dir: &Path) -> io::Result<()>;
    fn write_private(&self, path: &Path, contents: &[u8]) -> io::Result<()>;
    fn rename(&self, from: &Path, to: &Path) -> io::Result<()>;
    fn remove(&self, path: &Path) -> io::Result<()>;
}

pub struct StdFs;

impl InboxFs for StdFs {
    fn read(&self, path: &Path) -> io::Result<String> {
        std::fs::read(path).map(|b| String::from_utf8_lossy(&b).into_owned())
    }

    fn create_dir_all_private(&self, dir: &Path) -> io::Result<()> {
        #[cfg(unix)]
        {
            use std::os::unix::fs::DirBuilderExt;
            std::fs::DirBuilder::new().recursive(true).mode(0o700).create(dir)
        }
        #[cfg(not(unix))]
        {
            std::fs::create_dir_all(dir)
        }
    }

    fn write_private(&self, path: &Path, contents: &[u8]) -> io::Result<()> {
        use std::io::Write;
        let mut opts = std::fs::OpenOptions::new();
        opts.write(true).create(true).truncate(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            opts.mode(0o600);
        }
        opts.open(path)?.write_all(contents)
    }

    fn rename(&self, from: &Path, to: &Path) -> io::Result<()> {
        std::fs::rename(from, to)
    }

    fn remove(&self, path: &Path) -> io::Result<()> {
        match std::fs::remove_file(path) {
            Err(e) if e.kind() != io::ErrorKind::NotFound => Err(e),
            _ => Ok(()),
        }
    }
}

pub enum SeenIds<'a> {
    All,
    Ids(&'a [String]),
}

pub struct NotificationStore {
    file: Option<PathBuf>,
    now: Box<dyn Fn() -> i64 + Send + Sync>,
    id_gen: Box<dyn FnMut() -> String + Send>,
    fs: Box<dyn InboxFs>,
    log: Box<dyn Fn(&str) + Send + Sync>,
    state: State,
}

fn system_now() -> i64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_millis() as i64).unwrap_or(0)
}

impl NotificationStore {
    /// An in-memory store (`file: None`) or one saved to `file`.
    pub fn new(file: Option<PathBuf>) -> Self {
        Self::with(file, system_now, uuid_v4, Box::new(StdFs), |m| println!("{m}"))
    }

    pub fn with(
        file: Option<PathBuf>,
        now: impl Fn() -> i64 + Send + Sync + 'static,
        id_gen: impl FnMut() -> String + Send + 'static,
        fs: Box<dyn InboxFs>,
        log: impl Fn(&str) + Send + Sync + 'static,
    ) -> Self {
        let mut state = load_state(file.as_deref(), fs.as_ref());
        state.items = prune(&state.items, now());
        state.last_announced_at = state.last_announced_at.min(now());
        NotificationStore { file, now: Box::new(now), id_gen: Box::new(id_gen), fs, log: Box::new(log), state }
    }

    /// Expired items go the moment anything looks at the inbox, not only on the
    /// next write.
    fn live(&mut self) -> &mut Vec<Item> {
        self.state.items = prune(&self.state.items, (self.now)());
        &mut self.state.items
    }

    fn save(&mut self) {
        self.state.items = prune(&self.state.items, (self.now)());
        let Some(file) = self.file.clone() else { return };
        let dir = file.parent().map_or_else(|| PathBuf::from("."), Path::to_path_buf);
        let base = file.file_name().map(|f| f.to_string_lossy().into_owned()).unwrap_or_default();
        let tmp = dir.join(format!(".{base}.{}.tmp", std::process::id()));
        let body = serde_json::to_string_pretty(&serde_json::json!({
            "version": 1,
            "items": self.state.items,
            "lastAnnouncedAt": self.state.last_announced_at,
        }))
        .unwrap_or_default()
            + "\n";
        let result = self
            .fs
            .create_dir_all_private(&dir)
            .and_then(|()| self.fs.write_private(&tmp, body.as_bytes()))
            .and_then(|()| self.fs.rename(&tmp, &file));
        if let Err(e) = result {
            let _ = self.fs.remove(&tmp);
            (self.log)(&format!("[notifications] WARN - couldn't save the inbox: {e}"));
        }
    }

    /// Adds an item, or refreshes an unseen one with the same key. An item
    /// already seen is never brought back (`Ok(None)`).
    pub fn add(&mut self, input: &Value) -> Result<Option<Item>, String> {
        let at = (self.now)();
        let id = (self.id_gen)();
        let item = clean_item(input, at, &id)?;
        let existing = self.live().iter().position(|i| i.key == item.key);
        let Some(pos) = existing else {
            self.state.items.push(item.clone());
            self.save();
            return Ok(Some(item));
        };
        let e = &mut self.state.items[pos];
        if seen(e) {
            return Ok(None);
        }
        // A refresh that turns urgent (an approval landed) is worth telling again.
        if item.urgent && !e.urgent {
            e.announced_at = None;
        }
        e.title = item.title;
        e.body = item.body;
        e.url = item.url;
        e.urgent = item.urgent;
        e.job_id = item.job_id;
        e.feature_id = item.feature_id;
        e.updated_at = at;
        if item.event_at.is_some_and(|v| v != 0) {
            e.event_at = item.event_at;
        }
        let out = e.clone();
        self.save();
        Ok(Some(out))
    }

    pub fn list(&self, include_seen: bool, limit: usize) -> Vec<Item> {
        prune(&self.state.items, (self.now)())
            .into_iter()
            .filter(|i| include_seen || !seen(i))
            .take(limit.clamp(1, MAX_ITEMS))
            .collect()
    }

    pub fn unseen_count(&mut self) -> usize {
        self.live().iter().filter(|i| !seen(i)).count()
    }

    pub fn get(&mut self, id: &str) -> Option<Item> {
        self.live().iter().find(|i| i.id == id).cloned()
    }

    /// How many became seen.
    pub fn mark_seen(&mut self, ids: SeenIds<'_>) -> usize {
        let at = (self.now)();
        let mut n = 0;
        for i in self.live().iter_mut() {
            let wanted = match &ids {
                SeenIds::All => true,
                SeenIds::Ids(list) => list.contains(&i.id),
            };
            if seen(i) || !wanted {
                continue;
            }
            i.seen_at = Some(at);
            n += 1;
        }
        if n > 0 {
            self.save();
        }
        n
    }

    /// The user clicked an item: seen and opened (the first time is kept).
    pub fn open(&mut self, id: &str) -> Option<Item> {
        let at = (self.now)();
        let item = self.live().iter_mut().find(|i| i.id == id)?;
        item.seen_at = Some(item.seen_at.filter(|s| *s != 0).unwrap_or(at));
        item.opened_at = Some(item.opened_at.filter(|s| *s != 0).unwrap_or(at));
        let out = item.clone();
        self.save();
        Some(out)
    }

    /// Plans and records one announcement (see [`plan_announcement`]). `None`
    /// or a nonsense interval fails closed: the default spacing, never "no
    /// limit".
    pub fn announce(&mut self, quiet: bool, min_interval_ms: Option<f64>) -> Option<Announcement> {
        let at = (self.now)();
        let spacing = min_interval_ms.filter(|m| m.is_finite() && *m >= 0.0).unwrap_or(DEFAULT_MIN_INTERVAL_MS);
        self.state.last_announced_at = self.state.last_announced_at.min(at);
        let last = self.state.last_announced_at;
        let plan = plan_announcement(self.live(), last, at, spacing, quiet)?;
        for i in self.state.items.iter_mut().filter(|i| plan.ids.contains(&i.id)) {
            i.announced_at = Some(at);
        }
        self.state.last_announced_at = at;
        self.save();
        Some(plan)
    }

    /// The morning digest covers what quiet hours held: mark it announced so it
    /// doesn't also arrive as its own notification.
    pub fn absorb_pending(&mut self) -> Vec<String> {
        let at = (self.now)();
        let mut ids = Vec::new();
        for i in self.live().iter_mut() {
            if seen(i) || i.announced_at.is_some_and(|a| a != 0) || i.kind == "digest" {
                continue;
            }
            i.announced_at = Some(at);
            ids.push(i.id.clone());
        }
        if !ids.is_empty() {
            self.save();
        }
        ids
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::sync::atomic::{AtomicI64, AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};

    const MIN: i64 = 60 * 1000;
    const DAY: i64 = 24 * 60 * MIN;
    const T0: i64 = 1_790_668_800_000; // 2026-09-29T08:00:00Z
    const JOB: &str = "3f2b8c1e-1111-4222-8333-444455556666";

    struct Harness {
        s: NotificationStore,
        clock: Arc<AtomicI64>,
    }

    impl Harness {
        fn tick(&self, ms: i64) {
            self.clock.fetch_add(ms, Ordering::SeqCst);
        }
    }

    fn store_with(file: Option<PathBuf>, fs: Box<dyn InboxFs>, log: impl Fn(&str) + Send + Sync + 'static) -> Harness {
        let clock = Arc::new(AtomicI64::new(T0));
        let seq = Arc::new(AtomicUsize::new(0));
        let c = clock.clone();
        let s = NotificationStore::with(file, move || c.load(Ordering::SeqCst), move || format!("id-{}", seq.fetch_add(1, Ordering::SeqCst) + 1), fs, log);
        Harness { s, clock }
    }

    fn store() -> Harness {
        store_with(None, Box::new(StdFs), |_| {})
    }

    fn conflict(over: Value) -> Value {
        let mut v = json!({
            "key": "conflicts:bitbucket:CI/sample-app#12:a:b", "kind": "conflict",
            "title": "Conflict on CI/sample-app #12: fix\nlogin", "body": "Ready to review.",
            "url": "https://bb.example/projects/CI/repos/sample-app/pull-requests/12",
            "jobId": JOB, "featureId": "resolve-conflict", "watcher": "conflicts", "scopeKey": "bitbucket:CI/sample-app#12",
        });
        for (k, val) in over.as_object().unwrap() {
            v[k] = val.clone();
        }
        v
    }

    #[test]
    fn clean_item_flattens_titles_keeps_only_https_links_and_valid_job_ids_and_refuses_junk() {
        let item = clean_item(&conflict(json!({ "url": "javascript:alert(1)", "jobId": "not-a-uuid" })), T0, "x").unwrap();
        assert_eq!(item.title, "Conflict on CI/sample-app #12: fix login");
        assert_eq!((item.url, item.job_id), (None, None));
        assert_eq!(clean_item(&conflict(json!({ "url": "https://u:p@bb.example/x" })), T0, "x").unwrap().url, None);
        assert!(clean_item(&conflict(json!({ "kind": "spam" })), T0, "x").unwrap_err().starts_with("Unknown notification kind"));
        assert!(clean_item(&conflict(json!({ "title": "  " })), T0, "x").unwrap_err().contains("needs a title"));
        assert!(clean_item(&conflict(json!({ "key": "" })), T0, "x").unwrap_err().contains("needs a key"));
        assert_eq!(clean_item(&conflict(json!({ "title": "x".repeat(500) })), T0, "x").unwrap().title.chars().count(), 200);
        assert_eq!(clean_item(&json!("x"), T0, "x").unwrap_err(), "A notification must be an object.");
        assert_eq!(clean_item(&json!({ "key": "k", "title": "t" }), T0, "x").unwrap_err(), "Unknown notification kind: undefined");
        assert_eq!(clean_item(&conflict(json!({ "key": "k".repeat(401) })), T0, "x").unwrap_err(), "A notification needs a key.");
    }

    #[test]
    fn titles_lose_invisible_and_bidi_characters_and_keep_text_clean() {
        let clean = clean_item(&conflict(json!({ "title": "Fi\u{202e}x\u{200b}ed\u{85} it" })), T0, "x").unwrap();
        assert_eq!(clean.title, "Fixed it");
        let body = clean_item(&conflict(json!({ "body": "a\n\n\n\n\nb\u{7}c\td" })), T0, "x").unwrap().body;
        assert_eq!(body, "a\n\nb c\td", "blank lines collapse, other control characters become spaces, tabs survive");
        assert_eq!(clean_item(&conflict(json!({ "body": 5 })), T0, "x").unwrap().body, "");
    }

    #[test]
    fn add_dedupes_by_key_an_unseen_item_is_refreshed_a_seen_one_never_comes_back() {
        let mut h = store();
        let first = h.s.add(&conflict(json!({}))).unwrap().unwrap();
        assert_eq!(first.id, "id-1");
        let again = h.s.add(&conflict(json!({ "body": "Updated." }))).unwrap().unwrap();
        assert_eq!(again.id, "id-1");
        assert_eq!(h.s.list(false, 50).len(), 1);
        assert_eq!(h.s.list(false, 50)[0].body, "Updated.");
        assert_eq!(h.s.unseen_count(), 1);
        assert_eq!(h.s.mark_seen(SeenIds::All), 1);
        assert_eq!(h.s.add(&conflict(json!({}))).unwrap(), None);
        assert_eq!(h.s.unseen_count(), 0);
        assert_eq!(h.s.list(true, 50).len(), 1);
    }

    #[test]
    fn mark_seen_by_id_open_and_pruning_after_a_week_seen_or_two_unseen() {
        let mut h = store();
        let a = h.s.add(&conflict(json!({ "key": "a" }))).unwrap().unwrap();
        let b = h.s.add(&conflict(json!({ "key": "b" }))).unwrap().unwrap();
        assert_eq!(h.s.mark_seen(SeenIds::Ids(&[a.id.clone(), "nope".into()])), 1);
        let opened = h.s.open(&b.id).unwrap();
        assert!(opened.opened_at.is_some() && opened.seen_at.is_some());
        assert!(h.s.open("nope").is_none());
        h.s.add(&conflict(json!({ "key": "c" }))).unwrap();
        h.tick(8 * DAY);
        let keys: Vec<String> = h.s.list(true, 50).into_iter().map(|i| i.key).collect();
        assert_eq!(keys, ["c"]);
        h.tick(7 * DAY);
        assert!(h.s.list(true, 50).is_empty());
    }

    fn pending(id: &str, over: impl FnOnce(&mut Item)) -> Item {
        let mut i = Item {
            id: id.into(), key: id.into(), kind: "conflict".into(), title: format!("T{id}"), body: format!("B{id}"), url: None, urgent: false,
            watcher: None, scope_key: None, job_id: None, feature_id: None, created_at: T0, updated_at: T0, event_at: None,
            seen_at: None, opened_at: None, announced_at: None,
        };
        over(&mut i);
        i
    }

    #[test]
    fn plan_announcement_batches_one_per_interval_urgent_at_once_nothing_in_quiet_hours() {
        let plan = |items: &[Item], last, quiet| plan_announcement(items, last, T0, (30 * MIN) as f64, quiet);
        assert_eq!(plan(&[], 0, false), None);
        assert_eq!(
            plan(&[pending("1", |_| {})], 0, false),
            Some(Announcement { ids: vec!["1".into()], urgent: false, title: "T1".into(), message: "B1".into(), url: None })
        );
        assert_eq!(plan(&[pending("1", |_| {})], T0 - 10 * MIN, false), None, "too soon");
        let urgent = plan(&[pending("1", |_| {}), pending("2", |i| i.urgent = true)], T0 - MIN, false).unwrap();
        assert_eq!(urgent.ids, ["2", "1"]);
        assert_eq!((urgent.title.as_str(), urgent.message.as_str()), ("2 things are ready for you", "\u{2022} T2\n\u{2022} T1"));
        assert_eq!(plan(&[pending("1", |i| i.urgent = true)], 0, true), None);
        assert_eq!(plan(&[pending("1", |i| i.seen_at = Some(T0)), pending("2", |i| i.announced_at = Some(T0))], 0, false), None);
        let empty_body = plan(&[pending("1", |i| i.body = String::new())], 0, false).unwrap();
        assert_eq!(empty_body.message, "Open \u{2728} to see it.");
        let four: Vec<Item> = (1..=4).map(|n| pending(&n.to_string(), |_| {})).collect();
        assert_eq!(plan(&four, 0, false).unwrap().message.matches('\u{2022}').count(), 3, "only the first three titles");
    }

    #[test]
    fn announce_records_what_it_announced_absorb_pending_hands_quiet_hours_items_to_the_digest() {
        let mut h = store();
        h.s.add(&conflict(json!({ "key": "a" }))).unwrap();
        let first = h.s.announce(false, Some((30 * MIN) as f64)).unwrap();
        assert_eq!(first.title, "Conflict on CI/sample-app #12: fix login");
        assert_eq!(first.url.as_deref(), Some("https://bb.example/projects/CI/repos/sample-app/pull-requests/12"));
        assert_eq!(h.s.announce(false, Some((30 * MIN) as f64)), None, "nothing new");
        h.s.add(&conflict(json!({ "key": "b" }))).unwrap();
        h.tick(5 * MIN);
        assert_eq!(h.s.announce(false, Some((30 * MIN) as f64)), None, "held until the interval passes");
        assert_eq!(h.s.announce(true, Some(0.0)), None);
        h.s.add(&json!({ "key": "digest:1", "kind": "digest", "title": "Morning digest" })).unwrap();
        assert_eq!(h.s.absorb_pending().len(), 1, "only the non-digest item");
        assert_eq!(h.s.announce(false, Some(0.0)).unwrap().title, "Morning digest");
    }

    #[test]
    fn the_inbox_file_is_written_0600_and_read_back_a_broken_file_starts_empty() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("state").join("notifications.json");
        let mut h = store_with(Some(file.clone()), Box::new(StdFs), |_| {});
        h.s.add(&conflict(json!({}))).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(std::fs::metadata(&file).unwrap().permissions().mode() & 0o777, 0o600);
            assert_eq!(std::fs::metadata(file.parent().unwrap()).unwrap().permissions().mode() & 0o777, 0o700);
        }
        let reloaded = NotificationStore::with(Some(file.clone()), || T0, uuid_v4, Box::new(StdFs), |_| {});
        assert_eq!(reloaded.list(false, 50)[0].key, conflict(json!({}))["key"].as_str().unwrap());
        std::fs::write(&file, "{not json").unwrap();
        assert!(NotificationStore::with(Some(file), || T0, uuid_v4, Box::new(StdFs), |_| {}).list(false, 50).is_empty());
    }

    #[test]
    fn the_saved_file_has_the_node_layout() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("n.json");
        let mut h = store_with(Some(file.clone()), Box::new(StdFs), |_| {});
        h.s.add(&conflict(json!({ "eventAt": T0 - 1000 }))).unwrap();
        let text = std::fs::read_to_string(&file).unwrap();
        assert!(text.ends_with("}\n") && text.starts_with("{\n  \"version\": 1,\n  \"items\": ["), "{text}");
        let v: Value = serde_json::from_str(&text).unwrap();
        assert_eq!(v["lastAnnouncedAt"], 0);
        let item = &v["items"][0];
        assert_eq!(item["id"], "id-1");
        assert_eq!(item["eventAt"], T0 - 1000);
        assert_eq!(item["seenAt"], Value::Null);
        assert_eq!(item["jobId"], JOB);
        assert!(item.get("openedAt").is_some());
        // Optional fields that are absent are left out, as undefined was.
        h.s.add(&json!({ "key": "bare", "kind": "digest", "title": "t" })).unwrap();
        let v: Value = serde_json::from_str(&std::fs::read_to_string(&file).unwrap()).unwrap();
        let bare = v["items"].as_array().unwrap().iter().find(|i| i["key"] == "bare").unwrap();
        for k in ["watcher", "scopeKey", "jobId", "featureId", "eventAt"] {
            assert!(bare.get(k).is_none(), "{k}");
        }
    }

    #[test]
    fn only_the_newest_100_items_are_kept() {
        let mut h = store();
        for i in 0..105 {
            h.s.add(&conflict(json!({ "key": format!("k{i}") }))).unwrap();
            h.tick(1000);
        }
        let all = h.s.list(true, 200);
        assert_eq!(all.len(), 100);
        assert_eq!(all[0].key, "k104");
        assert!(!all.iter().any(|i| i.key == "k0"));
        assert_eq!(h.s.unseen_count(), 100);
    }

    #[test]
    fn expired_items_are_gone_from_the_count_get_add_refresh_and_announce_the_moment_they_expire() {
        let mut h = store();
        let a = h.s.add(&conflict(json!({ "key": "old" }))).unwrap().unwrap();
        h.tick(15 * DAY);
        assert_eq!(h.s.unseen_count(), 0);
        assert!(h.s.get(&a.id).is_none());
        assert_eq!(h.s.announce(false, Some(0.0)), None, "nothing expired is announced");
        assert!(h.s.open(&a.id).is_none());
        let again = h.s.add(&conflict(json!({ "key": "old" }))).unwrap().unwrap();
        assert_ne!(again.id, a.id, "an expired key is a new item, not a refresh");
    }

    #[test]
    fn announce_no_interval_means_the_default_spacing_a_future_last_announced_is_clamped_urgent_bypasses_spacing() {
        let mut h = store();
        h.s.add(&conflict(json!({ "key": "a" }))).unwrap();
        assert!(h.s.announce(false, None).is_some(), "first one goes out");
        h.tick(MIN);
        h.s.add(&conflict(json!({ "key": "b" }))).unwrap();
        assert_eq!(h.s.announce(false, None), None, "no interval: the 30-minute default, not unlimited");
        assert_eq!(h.s.announce(false, Some(f64::NAN)), None);
        assert_eq!(h.s.announce(false, Some(-5.0)), None);
        h.s.add(&conflict(json!({ "key": "c", "urgent": true }))).unwrap();
        let urgent = h.s.announce(false, None).unwrap();
        assert!(urgent.urgent, "urgent goes out at once");
        h.tick(31 * MIN);
        h.s.add(&conflict(json!({ "key": "d" }))).unwrap();
        assert!(h.s.announce(false, None).is_some(), "after 30 minutes the next batch goes");
    }

    #[test]
    fn last_announced_at_persists_and_a_future_value_from_the_file_is_clamped() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("n.json");
        let mut h = store_with(Some(file.clone()), Box::new(StdFs), |_| {});
        h.s.add(&conflict(json!({ "key": "a" }))).unwrap();
        h.s.announce(false, Some(0.0));
        let mut saved: Value = serde_json::from_str(&std::fs::read_to_string(&file).unwrap()).unwrap();
        assert_eq!(saved["lastAnnouncedAt"], T0);
        let mut reloaded = NotificationStore::with(Some(file.clone()), || T0 + 5 * MIN, || "z".into(), Box::new(StdFs), |_| {});
        reloaded.add(&conflict(json!({ "key": "b" }))).unwrap();
        assert_eq!(reloaded.announce(false, Some((30 * MIN) as f64)), None, "spacing survives a restart");

        saved["lastAnnouncedAt"] = json!(T0 + 10 * DAY);
        std::fs::write(&file, saved.to_string()).unwrap();
        let clock = Arc::new(AtomicI64::new(T0 + 60 * MIN));
        let c = clock.clone();
        let mut future = NotificationStore::with(Some(file), move || c.load(Ordering::SeqCst), || "y".into(), Box::new(StdFs), |_| {});
        future.add(&conflict(json!({ "key": "c" }))).unwrap();
        assert_eq!(future.announce(false, Some((30 * MIN) as f64)), None, "clamped to now: treated as just announced");
        clock.fetch_add(31 * MIN, Ordering::SeqCst);
        assert!(future.announce(false, Some((30 * MIN) as f64)).is_some(), "a future timestamp silences the inbox for one interval, not for days");
    }

    #[test]
    fn a_hand_edited_file_goes_through_the_same_cleaning_invalid_items_are_dropped() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("n.json");
        let mut good = conflict(json!({ "key": "good" }));
        good["id"] = json!("id-1");
        good["createdAt"] = json!(T0);
        good["updatedAt"] = json!(T0);
        good["seenAt"] = Value::Null;
        let with = |over: Value| {
            let mut v = good.clone();
            for (k, val) in over.as_object().unwrap() {
                v[k] = val.clone();
            }
            v
        };
        std::fs::write(
            &file,
            json!({
                "items": [
                    good,
                    with(json!({ "id": "id-2", "key": "js", "url": "javascript:alert(1)", "title": "x".repeat(5000) })),
                    with(json!({ "id": "id-3", "kind": "spam" })),
                    with(json!({ "id": "id-4", "title": "" })),
                    with(json!({ "id": 5 })),
                    with(json!({ "id": "id-6", "createdAt": "yesterday" })),
                    "junk",
                    null,
                ],
                "lastAnnouncedAt": "soon",
            })
            .to_string(),
        )
        .unwrap();
        let s = NotificationStore::with(Some(file), || T0, uuid_v4, Box::new(StdFs), |_| {});
        let items = s.list(false, 50);
        let mut keys: Vec<&str> = items.iter().map(|i| i.key.as_str()).collect();
        keys.sort();
        assert_eq!(keys, ["good", "js"]);
        let js = items.iter().find(|i| i.key == "js").unwrap();
        assert_eq!((js.url.clone(), js.title.chars().count()), (None, 200));
    }

    struct FailingFs {
        removed: Arc<Mutex<Vec<PathBuf>>>,
    }

    impl InboxFs for FailingFs {
        fn read(&self, _: &Path) -> io::Result<String> {
            Err(io::Error::other("none"))
        }
        fn create_dir_all_private(&self, _: &Path) -> io::Result<()> {
            Ok(())
        }
        fn write_private(&self, _: &Path, _: &[u8]) -> io::Result<()> {
            Err(io::Error::other("disk full"))
        }
        fn rename(&self, _: &Path, _: &Path) -> io::Result<()> {
            Ok(())
        }
        fn remove(&self, p: &Path) -> io::Result<()> {
            self.removed.lock().unwrap().push(p.to_path_buf());
            Ok(())
        }
    }

    #[test]
    fn a_failed_write_is_logged_the_temp_file_is_cleaned_up_and_the_item_is_still_in_memory() {
        let removed = Arc::new(Mutex::new(vec![]));
        let logs = Arc::new(Mutex::new(vec![]));
        let l = logs.clone();
        let mut h = store_with(Some(PathBuf::from("/no/such/dir/n.json")), Box::new(FailingFs { removed: removed.clone() }), move |m| l.lock().unwrap().push(m.to_string()));
        assert!(h.s.add(&conflict(json!({}))).unwrap().is_some());
        assert_eq!(h.s.unseen_count(), 1);
        let removed = removed.lock().unwrap();
        assert_eq!(removed.len(), 1);
        let name = removed[0].file_name().unwrap().to_string_lossy().into_owned();
        assert!(name.starts_with(".n.json.") && name.ends_with(".tmp") && name[".n.json.".len()..name.len() - 4].chars().all(|c| c.is_ascii_digit()), "{name}");
        assert!(logs.lock().unwrap().join("").contains("couldn't save the inbox: disk full"));
    }

    #[test]
    fn a_refresh_that_becomes_urgent_is_announced_again() {
        let mut h = store();
        h.s.add(&conflict(json!({ "key": "a" }))).unwrap();
        assert!(h.s.announce(false, Some(0.0)).is_some());
        h.tick(MIN);
        h.s.add(&conflict(json!({ "key": "a" }))).unwrap();
        assert_eq!(h.s.announce(false, Some((30 * MIN) as f64)), None, "a plain refresh is not news");
        h.s.add(&conflict(json!({ "key": "a", "urgent": true }))).unwrap();
        assert!(h.s.announce(false, Some((30 * MIN) as f64)).is_some(), "now urgent: tell again");
    }

    #[test]
    fn event_at_is_kept_for_display_never_in_the_future_survives_a_refresh_and_is_absent_when_not_given() {
        let mut h = store();
        let before = T0 - 3 * 3600 * 1000;
        assert_eq!(h.s.add(&conflict(json!({ "key": "a", "eventAt": before }))).unwrap().unwrap().event_at, Some(before));
        assert_eq!(h.s.add(&conflict(json!({ "key": "b", "eventAt": T0 + 99999 }))).unwrap().unwrap().event_at, Some(T0));
        assert_eq!(h.s.add(&conflict(json!({ "key": "c" }))).unwrap().unwrap().event_at, None);
        h.tick(1000);
        let later = T0 - 1000;
        let refreshed = h.s.add(&conflict(json!({ "key": "a", "eventAt": later }))).unwrap().unwrap();
        assert_eq!((refreshed.event_at, refreshed.created_at), (Some(later), T0));
        let kept = h.s.add(&conflict(json!({ "key": "a" }))).unwrap().unwrap();
        assert_eq!(kept.event_at, Some(later), "a refresh without eventAt keeps the old one");
    }
}
