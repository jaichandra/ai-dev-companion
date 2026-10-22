//! One module per feature, mirroring `companion-service/features/<id>/`: the
//! pure planning logic (`plan`) and, as they are ported, the prompt builders and
//! the job runner (`index`).

pub mod address_review_comments;
pub mod analyze_issue;
pub mod create_jira_subtasks;
pub mod declare;
pub mod diagnose_build;
pub mod digest;
pub mod resolve_conflict;
pub mod review_in_editor;
pub mod summarize_comments;
pub mod ticket_to_pr;
pub mod ticket_workspace;
