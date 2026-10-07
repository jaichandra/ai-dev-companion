// Small, reusable widget: "here's a local path, want to look at it
// yourself?" Any feature whose result includes a local filesystem path
// (not just resolve-conflict) can reuse
// window.renderOpenInEditor(path, jobId, opts).
//
// Two mechanisms back the buttons here:
//   1. Cursor registers a cursor:// URL scheme (it's a VS Code fork and
//      inherited the same file-opening convention) — confirmed empirically
//      on this machine: `open "cursor://file/<path>"` actually opens that
//      folder in Cursor. A plain <a href> covers it, no extra permissions.
//   2. "Open in Claude Code" has no URL scheme to piggyback on — opening a
//      terminal and running a command isn't something a browser link can
//      do. That goes through the companion service instead (which does
//      have OS access) via background.js -> POST /jobs/:jobId/open-in-claude-code.
//   Clipboard copy is the always-works fallback for anyone on neither.
//
// opts.claudeSession (job.data.claudeSession, truthy when the companion
// says this job's headless Claude run is still resumable — core/resume.js)
// swaps the Claude Code button for "Continue in Claude Code", which POSTs
// { resume: true } instead of opening a plain terminal at the worktree. A
// 410 there (core/resume.js's validateSession failed, or the session's
// cwd/transcript are gone) is expected and recoverable — not a bug to
// alert() about — so it renders inline instead, with the plain "Open in
// Claude Code" offered as a fallback for the caller who's willing to lose
// the earlier conversation and just open a fresh terminal there.
(function () {
  function sendMessage(message) {
    return new Promise((resolve, reject) => {
      chrome.runtime.sendMessage(message, (response) => {
        if (chrome.runtime.lastError) {
          reject(new Error(chrome.runtime.lastError.message));
          return;
        }
        if (response && response.error) {
          const err = new Error(response.error);
          // details.status (background.js's callService) is what tells a
          // 410 (expired session) apart from a 409 (job still running) or
          // a 400 (no/invalid session) — the response body itself is just
          // { error: "<message>" } for all three.
          err.details = response.details;
          reject(err);
          return;
        }
        resolve(response);
      });
    });
  }

  // opts.editor is the user's default editor ("cursor" | "claude-code"),
  // when the job knows it: the buttons then offer the other one instead of
  // repeating the one already in use. Without it (older jobs,
  // features that don't record it) the original Cursor + Claude Code pair
  // is shown.
  function renderOpenInEditor(path, jobId, opts) {
    const claudeSession = !!(opts && opts.claudeSession);
    const editor = opts && opts.editor;

    const box = document.createElement("div");
    box.className = "open-in-editor";

    const label = document.createElement("div");
    label.className = "open-in-editor-label";
    label.textContent = "Not sure? Open the working copy for a closer look before deciding:";
    box.appendChild(label);

    const pathEl = document.createElement("code");
    pathEl.className = "open-in-editor-path";
    pathEl.textContent = path;
    box.appendChild(pathEl);

    const actions = document.createElement("div");
    actions.className = "open-in-editor-actions";

    const cursorLink = document.createElement("a");
    cursorLink.className = "open-in-editor-btn";
    // Cursor's documented-by-behavior scheme is cursor://file<absolute-path>
    // — the path's own leading "/" is the only separator (same shape as
    // VS Code's vscode://file<path>; verified directly on this machine).
    // encodeURI (not encodeURIComponent) keeps "/" as path separators.
    cursorLink.href = `cursor://file${encodeURI(path)}`;
    cursorLink.textContent = "Open in Cursor";
    if (editor !== "cursor") actions.appendChild(cursorLink);

    // Shown only on a 410 while resuming — the server's own message plus
    // a fallback button, appended once. Plain text, never innerHTML: the
    // message is server-supplied.
    const claudeError = document.createElement("pre");
    claudeError.className = "action-error";
    claudeError.style.display = "none";
    claudeError.style.marginTop = "8px";

    let fallbackAdded = false;
    function addFallbackButton() {
      if (fallbackAdded) return;
      fallbackAdded = true;
      const fallbackBtn = document.createElement("button");
      fallbackBtn.className = "open-in-editor-btn open-in-editor-btn-secondary";
      fallbackBtn.textContent = "Open in Claude Code";
      fallbackBtn.addEventListener("click", () => runOpen(fallbackBtn, false));
      actions.appendChild(fallbackBtn);
    }

    // resume: true resends the resumed session; false is the plain
    // "open a terminal here" call (today's behavior, and the 410
    // fallback's).
    async function runOpen(btn, resume) {
      const original = btn.textContent;
      btn.disabled = true;
      btn.textContent = "Opening…";
      claudeError.style.display = "none";
      try {
        await sendMessage({
          type: "open-in-claude-code",
          jobId,
          ...(resume ? { body: { resume: true } } : {}),
        });
      } catch (err) {
        if (resume && err.details && err.details.status === 410) {
          claudeError.textContent = err.message;
          claudeError.style.display = "block";
          addFallbackButton();
        } else {
          alert(`Could not open Claude Code: ${err.message}`);
        }
      } finally {
        btn.disabled = false;
        btn.textContent = original;
      }
    }

    const claudeBtn = document.createElement("button");
    claudeBtn.className = "open-in-editor-btn";
    claudeBtn.textContent = claudeSession ? "Continue in Claude Code" : "Open in Claude Code";
    claudeBtn.addEventListener("click", () => runOpen(claudeBtn, claudeSession));
    // A resumable session is kept even when Claude Code is the default
    // editor: that button continues the run rather than repeating it.
    if (editor !== "claude-code" || claudeSession) {
      actions.appendChild(claudeBtn);
      if (editor) actions.prepend(claudeBtn);
    }

    const copyBtn = document.createElement("button");
    copyBtn.className = "open-in-editor-btn open-in-editor-btn-secondary";
    copyBtn.textContent = "Copy Path";
    copyBtn.addEventListener("click", async () => {
      const original = copyBtn.textContent;
      try {
        await navigator.clipboard.writeText(path);
        copyBtn.textContent = "Copied!";
      } catch {
        // Clipboard access can be denied in some contexts — the path is
        // already shown selectable above as a fallback either way.
        copyBtn.textContent = "Select path above";
      }
      setTimeout(() => {
        copyBtn.textContent = original;
      }, 1500);
    });
    actions.appendChild(copyBtn);

    box.appendChild(actions);
    box.appendChild(claudeError);
    return box;
  }

  window.renderOpenInEditor = renderOpenInEditor;
})();
