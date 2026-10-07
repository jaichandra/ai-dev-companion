// The "digest" feature of the ✨ menu: its helpers and its registry entry (the contract is in registry.js).

// A short badge per digest tone (core's features/digest/plan.js sets the tone).
const DIGEST_ITEM_BADGES = { bad: "needs you", warn: "check", busy: "running", ok: "ok" };

function renderDigestPanel(job) {
  const container = document.createElement("div");
  container.className = "digest";
  const digest = job.data?.digest;
  if (job.status === "failed" || !digest) {
    const box = document.createElement("pre");
    box.className = "action-error";
    box.textContent = job.error || "The digest couldn't be put together.";
    container.appendChild(box);
    return container;
  }
  const headline = document.createElement("div");
  headline.className = "digest-headline";
  headline.textContent = typeof digest.headline === "string" && digest.headline ? digest.headline : "Your digest";
  container.appendChild(headline);
  for (const section of Array.isArray(digest.sections) ? digest.sections : []) {
    // Every string is server data (PR titles, ticket summaries): textContent
    // only, and a link only for an https address (ticketWorkspaceLine checks).
    const mkRow = (item) => {
      const row = ticketWorkspaceLine(DIGEST_ITEM_BADGES[item.tone] || null, item.tone, String(item.text ?? ""), item.url);
      row.className = `digest-row ${item.tone || "neutral"}`;
      row.removeAttribute("style");
      return row;
    };
    const lines = (Array.isArray(section?.items) ? section.items : [])
      .filter((item) => item && typeof item === "object")
      .map((item) => mkRow(item));
    const extra = (Array.isArray(section?.more) ? section.more : []).filter((item) => item && typeof item === "object");
    if (extra.length > 0 && lines.length > 0) {
      // The last row is the server's "…and N more": make it the toggle.
      const toggle = document.createElement("button");
      toggle.type = "button";
      toggle.className = "digest-more";
      let rows = null;
      const label = () => {
        toggle.textContent = rows ? "Show fewer" : `Show ${extra.length} more`;
      };
      label();
      toggle.addEventListener("click", () => {
        if (rows) {
          for (const r of rows) r.remove();
          rows = null;
        } else {
          rows = extra.map(mkRow);
          toggle.before(...rows);
        }
        label();
      });
      lines[lines.length - 1] = toggle;
    }
    const node = renderSection(section.title, lines);
    if (node) {
      node.removeAttribute("style");
      node.classList.add("digest-section");
      const count = document.createElement("span");
      count.className = "digest-count";
      count.textContent = String(lines.length);
      node.firstElementChild.appendChild(count);
      container.appendChild(node);
    }
  }
  const generated = new Date(digest.generatedAt);
  if (digest.generatedAt && !Number.isNaN(generated.getTime())) {
    const when = document.createElement("p");
    when.className = "digest-when";
    when.textContent = `Put together ${generated.toLocaleTimeString()}.`;
    container.appendChild(when);
  }
  return container;
}

PaiRegistry.register({
  id: "digest",
  settingsGroups: ["jira", "git"],
  menuLabel: "Morning digest",
  // Not tied to a page: the menu shows it as a header shortcut, not a row.
  global: true,
  readOnly: true,
  // Reads live PRs/reviews/tickets, so nothing is kept: every click runs
  // it again and opens the panel when it finishes (content.js).
  alwaysFresh: true,

  // Every covered page; nothing is read until it's clicked.
  condition() {
    return true;
  },

  progressSteps: [{ id: "collect", label: "Read your PRs, reviews and tickets" }],

  renderPanel(job) {
    return renderDigestPanel(job);
  },
});
