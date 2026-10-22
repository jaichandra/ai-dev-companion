//! The Cursor deeplink that starts a review chat in the IDE's Agent window
//! (port of `core/cursor-deeplink.js`).

use crate::node_path;
use crate::uri::encode_uri_component;

/// Cursor's deeplink handler rejects longer prompts (its limit is 21 + the
/// encoded length of the text, at most 10,000).
const MAX_ENCODED_PROMPT: usize = 9900;

/// `workspace` routes the link to the open window whose folder is named like
/// `dir`; `mode=ask` keeps the chat read-only. Errors if Cursor would refuse
/// the prompt, so the caller can say so instead of the IDE failing silently.
pub fn build_cursor_prompt_url(dir: &str, prompt: &str) -> Result<String, String> {
    let text = encode_uri_component(prompt);
    if text.len() > MAX_ENCODED_PROMPT {
        return Err("the review prompt is too long for a Cursor deeplink".into());
    }
    Ok(format!(
        "cursor://anysphere.cursor-deeplink/prompt?text={text}&mode=ask&workspace={}",
        encode_uri_component(&node_path::basename(dir))
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use url::Url;

    #[test]
    fn targets_the_worktrees_window_in_ask_mode() {
        let built = build_cursor_prompt_url("/Users/me/wt/pr 42", "Review & don't edit").unwrap();
        let url = Url::parse(&built).unwrap();
        assert_eq!(url.scheme(), "cursor");
        assert_eq!(url.host_str(), Some("anysphere.cursor-deeplink"));
        assert_eq!(url.path(), "/prompt");
        let q: std::collections::HashMap<_, _> = url.query_pairs().into_owned().collect();
        assert_eq!(q["text"], "Review & don't edit");
        assert_eq!(q["mode"], "ask");
        assert_eq!(q["workspace"], "pr 42");
    }

    #[test]
    fn refuses_prompts_cursor_would_reject() {
        assert!(build_cursor_prompt_url("/wt", &"x".repeat(10000)).unwrap_err().contains("too long"));
        assert!(build_cursor_prompt_url("/wt", &"x".repeat(9900)).is_ok());
        // The limit is on the ENCODED length: 3300 spaces become 9900 chars of %20.
        assert!(build_cursor_prompt_url("/wt", &" ".repeat(3300)).is_ok());
        assert!(build_cursor_prompt_url("/wt", &" ".repeat(3301)).is_err());
    }
}
