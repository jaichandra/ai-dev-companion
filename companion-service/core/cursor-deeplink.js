// The Cursor deeplink that starts a review chat in the IDE's Agent window.
// Plain JS so cursor-deeplink.test.js can require it without a build.
const path = require("path");

// Cursor's deeplink handler rejects longer prompts (its limit is 21 + the
// encoded length of the text, at most 10,000).
const MAX_ENCODED_PROMPT = 9900;

/** `workspace` routes the link to the open window whose folder is named like
 * `dir`; `mode=ask` keeps the chat read-only. Throws if Cursor would refuse
 * the prompt, so the caller can say so instead of the IDE failing silently. */
function buildCursorPromptUrl(dir, prompt) {
  const text = encodeURIComponent(prompt);
  if (text.length > MAX_ENCODED_PROMPT) {
    throw new Error("the review prompt is too long for a Cursor deeplink");
  }
  return `cursor://anysphere.cursor-deeplink/prompt?text=${text}&mode=ask&workspace=${encodeURIComponent(path.basename(dir))}`;
}

module.exports = { buildCursorPromptUrl };
