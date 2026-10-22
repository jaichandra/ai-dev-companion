//! Where install links the `companion` command, and whether it may (port of
//! `core/cli-link.js`). Pure.

use crate::node_path;

/// What the Node build linked to: `<stable dir>/companion-service/bin/companion.js`.
/// To be redesigned when the installer ships the binary itself.
pub const OWN_TARGET_SUFFIX: &str = "companion-service/bin/companion.js";

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Existing {
    None,
    /// A symlink; `target` is where it points, when readable.
    Symlink(Option<String>),
    Other,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LinkAction {
    Skip,
    Create,
    Leave,
    Replace,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LinkPlan {
    pub action: LinkAction,
    pub link_path: String,
    pub target: String,
    pub reason: Option<&'static str>,
}

pub fn plan_link(platform: &str, home: &str, stable_dir: &str, existing: &Existing) -> LinkPlan {
    let target = node_path::join(&[stable_dir, OWN_TARGET_SUFFIX]);
    let link_path = node_path::join(&[home, ".local", "bin", "companion"]);
    let plan = |action, reason| LinkPlan { action, link_path: link_path.clone(), target: target.clone(), reason };
    if platform == "win32" {
        return plan(LinkAction::Skip, Some("windows"));
    }
    match existing {
        Existing::None => plan(LinkAction::Create, None),
        Existing::Symlink(Some(t)) if *t == target => plan(LinkAction::Leave, Some("already-linked")),
        // A link left by an earlier install of this same tool, from another folder.
        Existing::Symlink(Some(t)) if t.ends_with(OWN_TARGET_SUFFIX) => plan(LinkAction::Replace, None),
        _ => plan(LinkAction::Leave, Some("something-else-is-there")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const TARGET: &str = "/Users/me/ai-dev-companion/companion-service/bin/companion.js";
    fn plan(platform: &str, existing: Existing) -> LinkPlan {
        plan_link(platform, "/Users/me", "/Users/me/ai-dev-companion", &existing)
    }

    #[test]
    fn creates_the_link_when_nothing_is_there() {
        assert_eq!(
            plan("darwin", Existing::None),
            LinkPlan {
                action: LinkAction::Create,
                link_path: "/Users/me/.local/bin/companion".into(),
                target: TARGET.into(),
                reason: None
            }
        );
    }

    #[test]
    fn leaves_an_existing_correct_link_alone_and_replaces_one_from_an_earlier_install_of_this_tool() {
        assert_eq!(plan("darwin", Existing::Symlink(Some(TARGET.into()))).action, LinkAction::Leave);
        assert_eq!(
            plan("darwin", Existing::Symlink(Some("/old/place/companion-service/bin/companion.js".into()))).action,
            LinkAction::Replace
        );
    }

    #[test]
    fn never_overwrites_something_else_that_is_called_companion() {
        let a = plan("darwin", Existing::Other);
        assert_eq!((a.action, a.reason), (LinkAction::Leave, Some("something-else-is-there")));
        assert_eq!(plan("darwin", Existing::Symlink(Some("/usr/bin/other-tool".into()))).action, LinkAction::Leave);
        assert_eq!(plan("darwin", Existing::Symlink(None)).action, LinkAction::Leave);
    }

    #[test]
    fn skips_windows_where_a_symlink_isnt_the_way() {
        assert_eq!(plan("win32", Existing::None).action, LinkAction::Skip);
    }
}
