//! The inbox routes the server mounts behind the Host guard and the shared
//! secret (port of `core/background-routes.js`), as plain functions returning
//! `(HTTP status, JSON body)` so they're testable without a server and the web
//! layer is a thin adapter:
//!
//! - `GET  /notifications`            unseen items (`?all=1`: seen too), the count, the scheduler's status
//! - `POST /notifications/seen`       `{ ids: [...] }` or `{ all: true }`
//! - `POST /notifications/announce`   what (if anything) to show as one Chrome notification now
//! - `POST /notifications/:id/open`   the user clicked an item: seen, opened, and for a
//!   pre-warmed read-only result a "used" history event
//!
//! Every dependency is injected. Inbox ids are random UUIDs (see `notifications`).

use crate::notifications::{NotificationStore, SeenIds};
use serde_json::{Value, json};
use std::sync::{Arc, Mutex};

const MAX_IDS: usize = 100;

/// A UUID in either case (`/i`).
fn is_id(s: &str) -> bool {
    let groups: Vec<&str> = s.split('-').collect();
    groups.len() == 5 && groups.iter().zip([8, 4, 4, 4, 12]).all(|(g, n)| g.len() == n && g.bytes().all(|b| b.is_ascii_hexdigit()))
}

/// What the routes need from the background scheduler.
pub trait SchedulerView: Send + Sync {
    fn status(&self) -> Value;
    fn quiet_now(&self) -> bool;
}

/// Where "used" events go (the history store, when it is on).
pub trait EventSink: Send + Sync {
    fn record_event(&self, event: Value);
}

/// The inbox, shared with the watchers and the digest task that add to it.
pub type SharedInbox = Arc<Mutex<NotificationStore>>;

pub type Reply = (u16, Value);

/// `(job, now) -> history event` for an opened pre-warmed result.
pub type UsedEvent = Box<dyn Fn(&Value, i64) -> Option<Value> + Send + Sync>;
/// Looks a job up by id.
pub type GetJob = Box<dyn Fn(&str) -> Option<Value> + Send + Sync>;

pub struct BackgroundRoutes {
    pub inbox: SharedInbox,
    pub scheduler: Box<dyn SchedulerView>,
    pub history: Option<Box<dyn EventSink>>,
    /// `(job, now) -> history event`, for an opened pre-warmed result.
    pub used_event: UsedEvent,
    pub get_job: GetJob,
    pub min_interval_ms: Box<dyn Fn() -> Option<f64> + Send + Sync>,
    pub now: Box<dyn Fn() -> i64 + Send + Sync>,
    pub log: Box<dyn Fn(&str) + Send + Sync>,
}

impl BackgroundRoutes {
    pub fn list(&self, all: bool) -> Reply {
        let mut inbox = self.inbox.lock().unwrap();
        let items = inbox.list(all, 50);
        (200, json!({ "items": items, "unseen": inbox.unseen_count(), "background": self.scheduler.status() }))
    }

    /// Exactly one of `ids` / `all`, and nothing else: an ambiguous or padded
    /// body is a mistake, not a request.
    pub fn seen(&self, body: &Value) -> Reply {
        let request = match body.as_object() {
            Some(o) if o.len() == 1 => {
                if o.get("all") == Some(&Value::Bool(true)) {
                    Some(None)
                } else {
                    o.get("ids")
                        .and_then(Value::as_array)
                        .filter(|a| a.len() <= MAX_IDS)
                        .and_then(|a| a.iter().map(|id| id.as_str().filter(|s| is_id(s)).map(str::to_string)).collect::<Option<Vec<_>>>())
                        .map(Some)
                }
            }
            _ => None,
        };
        let Some(request) = request else {
            return (400, json!({ "error": "Send { ids: [...] } (at most 100 ids) or { all: true }." }));
        };
        let mut inbox = self.inbox.lock().unwrap();
        let n = match &request {
            None => inbox.mark_seen(SeenIds::All),
            Some(ids) => inbox.mark_seen(SeenIds::Ids(ids)),
        };
        (200, json!({ "ok": true, "seen": n, "unseen": inbox.unseen_count() }))
    }

    pub fn announce(&self) -> Reply {
        let mut inbox = self.inbox.lock().unwrap();
        let plan = inbox.announce(self.scheduler.quiet_now(), (self.min_interval_ms)());
        (200, json!({ "announce": plan, "unseen": inbox.unseen_count() }))
    }

    pub fn open(&self, id: &str) -> Reply {
        let unknown = || (404, json!({ "error": "unknown notification" }));
        if !is_id(id) {
            return unknown();
        }
        let mut inbox = self.inbox.lock().unwrap();
        // Only the first open counts: clicking the same item again is not another use.
        let first_open = !inbox.get(id).is_some_and(|i| i.opened_at.is_some_and(|t| t != 0));
        let Some(item) = inbox.open(id) else { return unknown() };
        // A conflict resolution needs an approval to be "used" (the approve
        // records it); merely opening the item is not a use. Only read-only
        // results count.
        let needs_approval = item.kind == "conflict" || item.feature_id.as_deref() == Some("resolve-conflict");
        if let (Some(job_id), Some(history), true, false) = (item.job_id.as_deref(), self.history.as_ref(), first_open, needs_approval) {
            match (self.get_job)(job_id) {
                Some(job) if job.get("featureId").and_then(Value::as_str) == Some("resolve-conflict") => {}
                Some(job) => {
                    if let Some(event) = (self.used_event)(&job, (self.now)()) {
                        history.record_event(event);
                    }
                }
                None => {}
            }
        }
        (200, json!({ "ok": true, "item": item, "unseen": inbox.unseen_count() }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::notifications::StdFs;
    use std::sync::{Arc, Mutex as StdMutex};

    const T0: i64 = 1_790_668_800_000;
    const JOB_ID: &str = "3f2b8c1e-1111-4222-8333-444455556666";

    struct Sched {
        quiet: bool,
    }
    impl SchedulerView for Sched {
        fn status(&self) -> Value {
            json!({ "running": null, "quiet": self.quiet })
        }
        fn quiet_now(&self) -> bool {
            self.quiet
        }
    }

    struct Sink(Arc<StdMutex<Vec<Value>>>);
    impl EventSink for Sink {
        fn record_event(&self, event: Value) {
            self.0.lock().unwrap().push(event);
        }
    }

    struct Setup {
        routes: BackgroundRoutes,
        events: Arc<StdMutex<Vec<Value>>>,
    }

    impl Setup {
        fn add(&self, input: Value) -> Value {
            let item = self.routes.inbox.lock().unwrap().add(&input).unwrap().unwrap();
            serde_json::to_value(item).unwrap()
        }
    }

    fn setup(quiet: bool, history: bool) -> Setup {
        let seq = Arc::new(StdMutex::new(0));
        let inbox = NotificationStore::with(None, || T0, move || {
            let mut s = seq.lock().unwrap();
            *s += 1;
            format!("00000000-0000-4000-8000-{:012}", *s)
        }, Box::new(StdFs), |_| {});
        let events = Arc::new(StdMutex::new(vec![]));
        let routes = BackgroundRoutes {
            inbox: Arc::new(Mutex::new(inbox)),
            scheduler: Box::new(Sched { quiet }),
            history: history.then(|| Box::new(Sink(events.clone())) as Box<dyn EventSink>),
            // A stand-in for history-record's `usedEvent`: the shape the tests read.
            used_event: Box::new(|job, now| {
                Some(json!({ "jobId": job["id"], "status": "opened", "outcome": "used", "at": now, "metrics": { "watcher": job["watcher"] } }))
            }),
            get_job: Box::new(|id| (id == JOB_ID).then(|| json!({ "id": JOB_ID, "featureId": "analyze-issue", "scopeKey": "jira:PROJ-7", "startedVia": "watcher", "watcher": "assignedBugs", "createdAt": T0 - 60_000 }))),
            min_interval_ms: Box::new(|| Some(30.0 * 60.0 * 1000.0)),
            now: Box::new(|| T0),
            log: Box::new(|_| {}),
        };
        Setup { routes, events }
    }

    fn item(key: &str) -> Value {
        json!({ "key": key, "kind": "analysis", "title": format!("Analysis on the way for {key}"), "jobId": JOB_ID })
    }

    #[test]
    fn list_returns_unseen_items_with_the_count_and_the_schedulers_status_all_adds_seen_ones() {
        let s = setup(false, true);
        let a = s.add(item("a"));
        s.add(item("b"));
        s.routes.inbox.lock().unwrap().mark_seen(SeenIds::Ids(&[a["id"].as_str().unwrap().to_string()]));
        let (code, body) = s.routes.list(false);
        assert_eq!(code, 200);
        assert_eq!(body["unseen"], 1);
        assert_eq!(body["items"].as_array().unwrap().iter().map(|i| i["key"].clone()).collect::<Vec<_>>(), [json!("b")]);
        assert_eq!(body["background"], json!({ "running": null, "quiet": false }));
        assert_eq!(s.routes.list(true).1["items"].as_array().unwrap().len(), 2);
    }

    #[test]
    fn seen_takes_ids_or_all_and_refuses_anything_else() {
        let s = setup(false, true);
        let a = s.add(item("a"));
        s.add(item("b"));
        assert_eq!(s.routes.seen(&json!({ "ids": [a["id"]] })), (200, json!({ "ok": true, "seen": 1, "unseen": 1 })));
        assert_eq!(s.routes.seen(&json!({ "all": true })), (200, json!({ "ok": true, "seen": 1, "unseen": 0 })));
        let too_many: Vec<Value> = (0..101).map(|_| json!("a")).collect();
        for body in [json!({}), json!({ "ids": "x" }), json!({ "ids": ["../x"] }), json!({ "ids": ["id-1"] }), json!({ "ids": [{ "$ne": 1 }] }), json!({ "ids": too_many }), json!({ "all": "yes" })] {
            assert_eq!(s.routes.seen(&body).0, 400, "{}", body.to_string().chars().take(40).collect::<String>());
        }
        assert_eq!(s.routes.seen(&json!({ "all": true })).1["seen"], 0);
    }

    #[test]
    fn seen_is_strict_exactly_one_of_ids_or_all_no_extra_keys_no_arrays() {
        let s = setup(false, true);
        let a = s.add(item("a"));
        let id = a["id"].clone();
        for body in [json!({ "all": true, "ids": [id] }), json!({ "ids": [id], "extra": 1 }), json!({ "all": true, "extra": 1 }), json!([id]), json!(null), json!({ "all": false }), json!({ "all": 1 })] {
            assert_eq!(s.routes.seen(&body).0, 400, "{body}");
        }
        assert_eq!(s.routes.inbox.lock().unwrap().unseen_count(), 1);
        assert_eq!(s.routes.seen(&json!({ "ids": [id] })).0, 200);
    }

    #[test]
    fn announce_plans_one_notification_and_none_in_quiet_hours() {
        let s = setup(false, true);
        s.add(item("a"));
        let (code, body) = s.routes.announce();
        assert_eq!(code, 200);
        assert_eq!(body["announce"]["title"], "Analysis on the way for a");
        assert_eq!(s.routes.announce().1["announce"], Value::Null, "already announced");
        let q = setup(true, true);
        q.add(item("a"));
        assert_eq!(q.routes.announce().1["announce"], Value::Null);
    }

    #[test]
    fn open_marks_it_opened_and_records_a_pre_warmed_job_as_used() {
        let s = setup(false, true);
        let a = s.add(item("a"));
        let (code, body) = s.routes.open(a["id"].as_str().unwrap());
        assert_eq!(code, 200);
        assert_eq!(body["ok"], true);
        assert!(body["item"]["openedAt"].is_number());
        assert_eq!(body["unseen"], 0);
        let events = s.events.lock().unwrap().clone();
        assert_eq!(events.iter().map(|e| (e["jobId"].clone(), e["status"].clone(), e["outcome"].clone(), e["metrics"]["watcher"].clone())).collect::<Vec<_>>(), [(json!(JOB_ID), json!("opened"), json!("used"), json!("assignedBugs"))]);
        assert_eq!(s.routes.open("nope").0, 404);
        assert_eq!(s.routes.open("../../x").0, 404);
        assert_eq!(s.routes.open("00000000-0000-4000-8000-999999999999").0, 404);
        let plain = s.add(json!({ "key": "c", "kind": "analysis", "title": "no job" }));
        s.routes.open(plain["id"].as_str().unwrap());
        assert_eq!(s.events.lock().unwrap().len(), 1, "no job, no event");
        let off = setup(false, false);
        let b = off.add(item("a"));
        assert_eq!(off.routes.open(b["id"].as_str().unwrap()).0, 200, "works with the history off");
    }

    #[test]
    fn opening_a_conflict_item_is_not_a_use_only_the_approval_records_that() {
        let s = setup(false, true);
        let c = s.add(json!({ "key": "c1", "kind": "conflict", "title": "Conflict resolved", "jobId": "4f2b8c1e-1111-4222-8333-444455556666", "featureId": "resolve-conflict" }));
        assert_eq!(s.routes.open(c["id"].as_str().unwrap()).0, 200);
        assert!(s.events.lock().unwrap().is_empty(), "no `used` event for an approval-needing result");
    }

    #[test]
    fn opening_the_same_item_twice_records_one_used_event_and_keeps_the_first_opened_at() {
        let s = setup(false, true);
        let a = s.add(item("a"));
        let id = a["id"].as_str().unwrap();
        let first = s.routes.open(id).1;
        let second = s.routes.open(id).1;
        assert_eq!(s.events.lock().unwrap().len(), 1);
        assert_eq!(second["item"]["openedAt"], first["item"]["openedAt"]);
    }

    #[test]
    fn a_job_that_is_a_conflict_resolution_is_never_recorded_as_used_even_from_a_plain_item() {
        let mut s = setup(false, true);
        s.routes.get_job = Box::new(|_| Some(json!({ "id": "j", "featureId": "resolve-conflict" })));
        let a = s.add(item("a"));
        s.routes.open(a["id"].as_str().unwrap());
        assert!(s.events.lock().unwrap().is_empty());
    }
}
