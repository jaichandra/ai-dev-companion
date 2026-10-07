// The "create-jira-subtasks" feature of the ✨ menu: its helpers and its registry entry (the contract is in registry.js).

// Wraps `input` in a `.assignee-field` container (styling lives in
// content.js's shared panel <style>) and, when `issueKey` is given, wires
// it up to /rest/api/2/user/picker — the same endpoint Jira Server's own
// Assignee field calls while you type, doing real substring matching
// across display name, username, and email (not just a username prefix).
// Selecting a suggestion fills the input with that user's username (Jira
// Server's assignee.name); typing a raw username and ignoring the
// dropdown still works exactly as before. Without an issueKey (shouldn't
// happen in practice — every caller has one) this degrades to a plain
// input with no search, so the placeholder set by the caller stays the
// fallback guidance.
function attachAssigneeAutocomplete(input, issueKey) {
  const wrap = document.createElement("div");
  wrap.className = "assignee-field";
  const dropdown = document.createElement("div");
  dropdown.className = "assignee-suggestions";
  dropdown.style.display = "none";
  wrap.append(input, dropdown);

  if (!issueKey) return wrap;

  let debounceTimer = null;
  let requestToken = 0;
  let items = []; // [{ user, el }], in display order — for arrow-key navigation
  let activeIndex = -1;

  function hide() {
    dropdown.style.display = "none";
    dropdown.textContent = "";
    items = [];
    activeIndex = -1;
  }

  function setActive(index) {
    activeIndex = index;
    items.forEach(({ el }, i) => el.classList.toggle("active", i === activeIndex));
    if (activeIndex >= 0) {
      items[activeIndex].el.scrollIntoView({ block: "nearest" });
    }
  }

  // "Jai Langoju (jlangoju)" — same format the dropdown itself renders
  // (as two separately styled spans there; here as one string, since an
  // <input> can't hold rich markup).
  function formatUserDisplay(user) {
    return user.displayName && user.displayName !== user.name
      ? `${user.displayName} (${user.name})`
      : user.name;
  }

  function selectUser(user) {
    input.value = formatUserDisplay(user);
    // The *visible* text is the formatted "Name (ssoid)" string, but the
    // actual value a subtask gets assigned to must be just the username —
    // stashed here since there's nowhere else to keep it once the input
    // no longer holds it verbatim. getPayload() (below, in
    // renderStartForm) reads this instead of input.value when present.
    input.dataset.username = user.name;
    hide();
  }

  function renderSuggestions(users) {
    dropdown.textContent = "";
    items = [];
    activeIndex = -1;
    if (users.length === 0) {
      hide();
      return;
    }
    for (const user of users) {
      const item = document.createElement("div");
      item.className = "assignee-suggestion";
      const name = document.createElement("span");
      name.className = "assignee-suggestion-name";
      name.textContent = user.displayName || user.name;
      item.appendChild(name);
      if (user.displayName && user.displayName !== user.name) {
        const id = document.createElement("span");
        id.className = "assignee-suggestion-id";
        id.textContent = `(${user.name})`;
        item.appendChild(id);
      }
      // mousedown (not click) fires before the input's blur below, so the
      // selection registers before the blur-triggered hide() can race it.
      item.addEventListener("mousedown", (e) => {
        e.preventDefault();
        selectUser(user);
      });
      const entry = { user, el: item };
      item.addEventListener("mouseenter", () => setActive(items.indexOf(entry)));
      items.push(entry);
      dropdown.appendChild(item);
    }
    dropdown.style.display = "block";
    // Auto-highlight the first match so Enter picks it immediately without
    // needing an extra ArrowDown first — matches Jira's own picker.
    setActive(0);
  }

  input.addEventListener("input", () => {
    clearTimeout(debounceTimer);
    // Whatever the input now shows no longer corresponds to a previous
    // selection (if any) — the user is typing something new. Otherwise an
    // edited-but-not-reselected value would silently submit a stale
    // username that no longer matches what's displayed.
    delete input.dataset.username;
    const query = input.value.trim();
    if (!query) {
      hide();
      return;
    }
    debounceTimer = setTimeout(async () => {
      const token = ++requestToken;
      try {
        // Same-origin call to the Jira page's own host, session cookie sent
        // by default — same pattern as this file's condition()s.
        //
        // Deliberately /rest/api/2/user/picker, not .../user/assignable/search:
        // the latter's `query` only prefix-matches the *username*, so
        // searching a real display name ("Jai" for "Jai Langoju") returns
        // an unfiltered/default list instead of a match. /user/picker is
        // what Jira Server's own Assignee field calls while you type, and
        // does real substring matching across display name, username, and
        // email — exactly the "full text filter on name/sso id" behavior
        // being replicated here.
        const res = await fetch(
          `${location.origin}/rest/api/2/user/picker?` +
            `query=${encodeURIComponent(query)}&maxResults=20&showAvatar=false` +
            `&issueKey=${encodeURIComponent(issueKey)}`,
          { credentials: "include" },
        );
        if (token !== requestToken) return; // a newer keystroke already superseded this
        if (!res.ok) {
          hide();
          return;
        }
        const data = await res.json();
        if (token !== requestToken) return;
        renderSuggestions(Array.isArray(data?.users) ? data.users : []);
      } catch {
        hide();
      }
    }, 250);
  });

  input.addEventListener("blur", () => setTimeout(hide, 150));

  // Arrow-key navigation + Enter-to-select, same as Jira's own picker.
  // panel-level keydown handling (see content.js's openOverlayPanel) only
  // stops these from leaking to the host page's shortcut listeners — it
  // doesn't consume them, so they still reach this handler first (input is
  // the actual event target, ahead of anything the event bubbles up to).
  input.addEventListener("keydown", (e) => {
    if (items.length === 0) return;
    if (e.key === "ArrowDown") {
      e.preventDefault();
      setActive(Math.min(activeIndex + 1, items.length - 1));
    } else if (e.key === "ArrowUp") {
      e.preventDefault();
      setActive(Math.max(activeIndex - 1, 0));
    } else if (e.key === "Enter") {
      if (activeIndex >= 0) {
        e.preventDefault();
        selectUser(items[activeIndex].user);
      }
    } else if (e.key === "Escape") {
      hide();
    }
  });

  return wrap;
}

//
// Built with createElement/textContent throughout, never innerHTML — the
// same rule the rest of this extension follows, and it matters more here
// than anywhere else: every string below (test names, error details, job
// names) is untrusted text that came out of Jenkins.

PaiRegistry.register({
  site: "issues",
  id: "create-jira-subtasks", // internal id only — not shown to the user
  settingsGroups: ["jira"],
  menuLabel: "Create Subtasks",
  startLabel: "Create Subtasks",

  urlPattern: /\/browse\/([A-Z][A-Z0-9_]*-\d+)/,

  async condition(ctx) {
    const issueKey = ctx.match[1];

    // Same pattern as resolve-conflict's condition above: same-origin
    // call to the Jira page's own host, session cookie sent by default.
    const res = await fetch(`${ctx.origin}/rest/api/2/issue/${issueKey}?fields=issuetype`, {
      credentials: "include",
    });
    if (!res.ok) return null;
    const issue = await res.json();

    // Limited to Story/Epic tickets — a Sub-task can never itself be
    // typed Story/Epic, so that exclusion comes for free. Deliberately
    // NOT gated on "no subtasks yet": this is a repeatable "add more
    // subtasks" tool, not a one-time setup step.
    const typeName = issue.fields?.issuetype?.name;
    if (typeName !== "Story" && typeName !== "Epic") return null;

    return { issueKey };
  },

  // Opens the big overlay panel (content.js's openComposePanel) instead
  // of starting immediately — a repeatable row of {name, assignee}
  // inputs, one to start, up to 10. getPayload() is called on submit;
  // its return value POSTs verbatim (merged over { issueKey }) to /start,
  // which creates every row's subtask before responding — there's no
  // separate approve step for this feature.
  renderStartForm(payload) {
    const issueKey = payload?.issueKey;
    const MAX_ROWS = 10;
    const rows = [];

    const container = document.createElement("div");
    const rowsWrap = document.createElement("div");
    rowsWrap.className = "subtask-row-form";
    container.appendChild(rowsWrap);

    const addBtn = document.createElement("button");
    addBtn.type = "button";
    addBtn.className = "subtask-add-row";
    addBtn.textContent = "+ Add subtask";

    function updateControls() {
      addBtn.disabled = rows.length >= MAX_ROWS;
      for (const entry of rows) {
        entry.removeBtn.style.visibility = rows.length > 1 ? "visible" : "hidden";
      }
    }

    function addRow() {
      if (rows.length >= MAX_ROWS) return;
      const row = document.createElement("div");
      row.className = "subtask-row";

      const nameInput = document.createElement("input");
      nameInput.type = "text";
      nameInput.placeholder = "Subtask name";

      const assigneeInput = document.createElement("input");
      assigneeInput.type = "text";
      assigneeInput.autocomplete = "off";
      // Explicit about what the field wants even when the search below
      // is what's actually driving it — a raw SSO ID is always a valid
      // thing to type, dropdown or not.
      assigneeInput.placeholder = issueKey ? "Search assignee (name or SSO ID)…" : "Assignee SSO ID";
      const assigneeField = attachAssigneeAutocomplete(assigneeInput, issueKey);

      const removeBtn = document.createElement("button");
      removeBtn.type = "button";
      removeBtn.className = "subtask-row-remove";
      removeBtn.textContent = "×";
      removeBtn.title = "Remove this row";

      row.append(nameInput, assigneeField, removeBtn);
      rowsWrap.appendChild(row);

      const entry = { row, nameInput, assigneeInput, removeBtn };
      rows.push(entry);

      removeBtn.addEventListener("click", () => {
        rowsWrap.removeChild(row);
        rows.splice(rows.indexOf(entry), 1);
        updateControls();
      });

      updateControls();
    }

    addBtn.addEventListener("click", addRow);
    container.appendChild(addBtn);

    addRow(); // always start with exactly one row

    function getPayload() {
      const subtasks = rows.map(({ nameInput, assigneeInput }) => ({
        summary: nameInput.value.trim(),
        // A dropdown selection displays "Name (ssoid)" but stashes the
        // real username in dataset.username (see
        // attachAssigneeAutocomplete's selectUser) — that's what Jira
        // actually needs. Falls back to the raw typed value for anyone
        // who ignores the dropdown and just types their SSO ID directly.
        assignee: (assigneeInput.dataset.username || assigneeInput.value).trim(),
      }));
      if (subtasks.some((row) => !row.summary || !row.assignee)) {
        throw new Error("Every subtask needs both a name and an assignee.");
      }
      return { subtasks };
    }

    return { node: container, getPayload };
  },

  // No renderPanel: this feature never reaches the review-panel flow
  // (showPanel) that calls it — a successful submit closes the compose
  // panel and reloads the page immediately (see content.js's
  // openComposePanel), and a failed one is shown via the footer's
  // error box, not a re-render. See the registry doc comment above.
});
