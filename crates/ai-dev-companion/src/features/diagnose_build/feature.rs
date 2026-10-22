//! `diagnose-build`: everything the framework needs to know about this feature in one place (port of
//! `features/diagnose-build/feature.js`): its setup descriptor, the MCP tools, scope-key rule, persistence and
//! history behaviour, and where its extension file sits. Listed in a pack (`packs::builtin`).

use super::super::declare::*;
use crate::packs::*;
use serde_json::json;
use std::sync::Arc;

pub fn feature_spec<F>(factory: F, _setup: &SetupContext) -> FeatureSpec<F> {
    let mut d = FeatureDescriptor::new("diagnose-build", "Diagnose failed builds");
    d.summary = Some("On a failed Jenkins build, find out why with Claude".into());
    d.description = "On a failed Jenkins build, work out why with a read-only Claude run \u{2014} through a pack's diagnose command when one is installed \u{2014} and show the report in the panel. Continue in Claude Code picks the session up in a terminal.".into();
    d.enabled_by_default = Some(true);
    d.required_checks = vec!["claudeCli".into()];
    d.prompt_setup = Some(Arc::new(|_, _| json!({})));
    let mut spec = FeatureSpec::new(d, factory);
    spec.extension = Some(extension("features/diagnose-build.js", 40.0));
    spec
}
