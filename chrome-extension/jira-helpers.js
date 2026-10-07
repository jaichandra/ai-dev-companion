// Helpers shared by the Jira-page features (ticket workspace, ticket to PR, digest): a linked-status line.

/** Only an https address is ever turned into a link or opened (the service
 * checks PR addresses too; this is the page's own second check). */
function ticketWorkspaceHttpsUrl(value) {
  if (typeof value !== "string") return null;
  try {
    return new URL(value).protocol === "https:" ? value : null;
  } catch {
    return null;
  }
}

/** One badge + text line; every string is server data, so textContent only. */
function ticketWorkspaceLine(badgeText, tone, text, href) {
  const row = document.createElement("div");
  row.style.margin = "4px 0";
  row.style.overflowWrap = "anywhere";
  if (badgeText) {
    const badge = document.createElement("span");
    badge.className = `status-badge ${tone || "neutral"}`;
    badge.textContent = badgeText;
    badge.style.marginRight = "6px";
    row.appendChild(badge);
  }
  if (ticketWorkspaceHttpsUrl(href)) {
    const a = document.createElement("a");
    a.href = href; // an https PR page on the configured Bitbucket (the service checked; so does the page)
    a.target = "_blank";
    a.rel = "noopener noreferrer";
    a.textContent = text;
    row.appendChild(a);
  } else {
    row.appendChild(document.createTextNode(text));
  }
  return row;
}
