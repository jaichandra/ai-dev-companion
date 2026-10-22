//! The handler behind `POST /features/<id>/:jobId/actions/:action` (port of
//! `core/feature-actions.ts`): 404 for an unknown job, a job of another
//! feature, or an action name the feature doesn't define (own names only, so
//! "constructor" or "__proto__" are unknown too); otherwise runs the action and
//! answers with its result. Split out so the route's checks are testable
//! without starting the server.

use crate::jobs::{Feature, FeatureContext, FeatureError, JobStore};
use crate::js_text::slice_utf16;
use serde_json::Value;

#[derive(Debug, PartialEq)]
pub enum ActionError {
    /// Answer 404 with `{ error }`.
    NotFound(String),
    /// Let the server's error mapping answer.
    Feature(FeatureError),
}

/// Runs `action` on `job_id`. A result that is the job itself (same id) goes through `present`.
pub async fn feature_action(store: &JobStore, feature: &dyn Feature, job_id: &str, action: &str, ctx: FeatureContext, present: impl Fn(Value) -> Value) -> Result<Value, ActionError> {
    let job = match store.get(job_id) {
        Some(j) if j["featureId"] == feature.id() => j,
        _ => return Err(ActionError::NotFound("unknown job id".into())),
    };
    if !feature.action_names().contains(&action) {
        return Err(ActionError::NotFound(format!("{} has no action \"{}\".", feature.label(), slice_utf16(action, 40))));
    }
    let id = job["id"].clone();
    let result = feature.run_action(action, job, ctx).await.map_err(ActionError::Feature)?;
    Ok(if result.is_object() && result["id"] == id { present(result) } else { result })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::jobs::FeatureResult;
    use async_trait::async_trait;
    use serde_json::{json, Map};

    struct F;
    #[async_trait]
    impl Feature for F {
        fn id(&self) -> &str {
            "f"
        }
        fn label(&self) -> &str {
            "Feature F"
        }
        async fn start(&self, _: Value, _: FeatureContext) -> FeatureResult<Value> {
            Ok(json!({}))
        }
        async fn approve(&self, _: Value, _: FeatureContext) -> FeatureResult<()> {
            Ok(())
        }
        async fn reject(&self, _: Value, _: FeatureContext) -> FeatureResult<()> {
            Ok(())
        }
        fn action_names(&self) -> Vec<&'static str> {
            vec!["echo", "job", "boom"]
        }
        async fn run_action(&self, name: &str, job: Value, ctx: FeatureContext) -> FeatureResult<Value> {
            match name {
                "echo" => Ok(json!({ "body": ctx.body })),
                "job" => Ok(job),
                _ => Err(FeatureError::JobStatus("busy".into())),
            }
        }
    }

    fn setup() -> (JobStore, String) {
        let s = JobStore::new();
        let id = s.create("f", Map::new())["id"].as_str().unwrap().to_string();
        (s, id)
    }
    fn tag(mut v: Value) -> Value {
        v["presented"] = json!(true);
        v
    }

    #[tokio::test]
    async fn an_unknown_job_or_a_job_of_another_feature_is_a_404() {
        let (s, _) = setup();
        let other = s.create("g", Map::new())["id"].as_str().unwrap().to_string();
        for id in ["nope", other.as_str()] {
            assert_eq!(feature_action(&s, &F, id, "echo", FeatureContext::default(), tag).await, Err(ActionError::NotFound("unknown job id".into())));
        }
    }

    #[tokio::test]
    async fn only_declared_actions_exist_and_the_name_is_clipped_in_the_message() {
        let (s, id) = setup();
        for name in ["constructor", "__proto__", "toString"] {
            assert!(matches!(feature_action(&s, &F, &id, name, FeatureContext::default(), tag).await, Err(ActionError::NotFound(m)) if m.starts_with("Feature F has no action")));
        }
        let long = "x".repeat(100);
        let Err(ActionError::NotFound(m)) = feature_action(&s, &F, &id, &long, FeatureContext::default(), tag).await else { panic!() };
        assert_eq!(m, format!("Feature F has no action \"{}\".", "x".repeat(40)));
    }

    #[tokio::test]
    async fn the_result_is_returned_as_is_unless_it_is_the_job_which_is_presented() {
        let (s, id) = setup();
        let ctx = FeatureContext { body: Some(json!({ "a": 1 })), ..Default::default() };
        assert_eq!(feature_action(&s, &F, &id, "echo", ctx, tag).await.unwrap(), json!({ "body": { "a": 1 } }));
        let j = feature_action(&s, &F, &id, "job", FeatureContext::default(), tag).await.unwrap();
        assert_eq!((j["id"].as_str(), j["presented"].clone()), (Some(id.as_str()), json!(true)));
    }

    #[tokio::test]
    async fn a_failing_action_is_handed_to_the_servers_error_mapping() {
        let (s, id) = setup();
        assert_eq!(feature_action(&s, &F, &id, "boom", FeatureContext::default(), tag).await, Err(ActionError::Feature(FeatureError::JobStatus("busy".into()))));
    }
}
