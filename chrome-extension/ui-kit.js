// Small DOM helpers shared by several feature panels (loaded before the feature files; all top-level
// declarations share the content-script global scope, as they did when this was one file).

/** Local date/time for a cache completedAt ISO string; empty if unusable. */
function formatAnalysisSavedAt(iso) {
  if (!iso || typeof iso !== "string") return "";
  const d = new Date(iso);
  if (Number.isNaN(d.getTime())) return "";
  try {
    return d.toLocaleString(undefined, {
      dateStyle: "medium",
      timeStyle: "short",
    });
  } catch {
    return d.toISOString();
  }
}

/** A titled section that only renders when it has children to show. */
function renderSection(title, children) {
  if (!children || (Array.isArray(children) && children.length === 0)) return null;
  const section = document.createElement("section");
  section.style.marginBottom = "16px";
  const heading = document.createElement("div");
  heading.className = "report-headline";
  heading.textContent = title;
  section.appendChild(heading);
  if (Array.isArray(children)) {
    for (const child of children) {
      if (child) section.appendChild(child);
    }
  } else {
    section.appendChild(children);
  }
  return section;
}

/** A plain <ul> of text items (or nodes). Skips blanks. */
function renderList(items, mapItem) {
  if (!Array.isArray(items) || items.length === 0) return null;
  const list = document.createElement("ul");
  list.style.margin = "0";
  list.style.paddingLeft = "18px";
  for (const item of items) {
    const mapped = mapItem ? mapItem(item) : item;
    if (mapped == null || mapped === "") continue;
    const li = document.createElement("li");
    li.style.marginBottom = "6px";
    li.style.lineHeight = "1.45";
    if (typeof mapped === "string") {
      li.textContent = mapped;
    } else {
      li.appendChild(mapped);
    }
    list.appendChild(li);
  }
  return list.childNodes.length ? list : null;
}

// The report panels of the AI Assistant design: a bordered card per section (icon, title, count) holding rows
// (icon tile, title, subtitle, status tag). Styled by .pai-card / .pai-row in content.js. Every string is service
// or model output, so textContent only.

/** A status tag; tone is one of ok | bad | warn | busy | neutral. */
function paiTag(text, tone) {
  const tag = document.createElement("span");
  tag.className = `status-badge ${tone || "neutral"}`;
  tag.textContent = text;
  return tag;
}

/** One row of a card. opts: icon (PaiIcons name), tone (ok|bad|warn|info|mute: the icon's colour), title, sub,
 * href (https only: makes the title a link) or onClick (makes it a link that runs this instead), tag + tagTone,
 * extra (nodes appended under the subtitle). */
function paiRow(opts) {
  const row = document.createElement("div");
  row.className = `pai-row ${opts.tone || "mute"}`;
  const tile = document.createElement("span");
  tile.className = "pai-row-tile";
  tile.appendChild(PaiIcons.el(opts.icon || "info", 14.4));
  const text = document.createElement("div");
  text.className = "pai-row-text";
  const title = document.createElement(opts.href || opts.onClick ? "a" : "div");
  title.className = "pai-row-title";
  title.textContent = opts.title || "";
  if (opts.href) {
    title.href = opts.href;
    title.target = "_blank";
    title.rel = "noopener noreferrer";
  } else if (opts.onClick) {
    title.href = "#";
    if (opts.hint) title.title = opts.hint;
    title.addEventListener("click", (event) => {
      event.preventDefault();
      opts.onClick();
    });
  }
  text.appendChild(title);
  if (opts.sub) {
    const sub = document.createElement("div");
    sub.className = "pai-row-sub";
    sub.textContent = opts.sub;
    text.appendChild(sub);
  }
  for (const node of opts.extra || []) text.appendChild(node);
  row.append(tile, text);
  if (opts.tag) row.appendChild(paiTag(opts.tag, opts.tagTone));
  return row;
}

/** A titled card of rows (or any nodes); null when there is nothing to show. */
function paiCard(title, icon, rows) {
  const kids = (rows || []).filter(Boolean);
  if (kids.length === 0) return null;
  const card = document.createElement("section");
  card.className = "pai-card";
  const head = document.createElement("div");
  head.className = "pai-card-head";
  head.appendChild(PaiIcons.el(icon || "info", 14.4));
  const name = document.createElement("span");
  name.className = "pai-card-title";
  name.textContent = title;
  head.appendChild(name);
  if (kids.length > 1) {
    const count = document.createElement("span");
    count.className = "pai-card-count";
    count.textContent = String(kids.length);
    head.appendChild(count);
  }
  card.appendChild(head);
  const list = document.createElement("div");
  list.className = "pai-card-rows";
  for (const kid of kids) list.appendChild(kid);
  card.appendChild(list);
  return card;
}

/** A card holding a paragraph of prose (an assessment, a summary). */
function paiProseCard(title, icon, text) {
  if (!text) return null;
  const p = document.createElement("p");
  p.className = "pai-prose";
  p.textContent = text;
  return paiCard(title, icon, [p]);
}

/** A card of plain list items (decisions, next steps) as rows with one icon. */
function paiListCard(title, icon, items, rowIcon, tone) {
  const strings = (Array.isArray(items) ? items : []).map((s) => String(s ?? "").trim()).filter(Boolean);
  return paiCard(title, icon, strings.map((s) => paiRow({ icon: rowIcon, tone, title: s })));
}

/** The red box a failed or empty report shows. */
function paiErrorBox(text) {
  const box = document.createElement("pre");
  box.className = "action-error";
  box.textContent = text;
  return box;
}

/** The note at the foot of a report (what it reads and that it changes nothing). */
function paiFootNote(text) {
  const note = document.createElement("div");
  note.className = "pai-foot";
  note.appendChild(PaiIcons.el("privacy", 12));
  const span = document.createElement("span");
  span.textContent = text;
  note.appendChild(span);
  return note;
}

/** A report's body: its cards (nulls skipped) and the foot note. */
function paiPage(cards, foot) {
  const page = document.createElement("div");
  page.className = "pai-page";
  for (const card of cards) if (card) page.appendChild(card);
  if (foot) page.appendChild(paiFootNote(foot));
  return page;
}

/** The row of buttons under a report's header: [{ label, icon, primary, onClick }]. */
function paiActions(items) {
  const row = document.createElement("div");
  row.className = "pai-actions";
  for (const item of items.filter(Boolean)) {
    const btn = document.createElement("button");
    btn.type = "button";
    btn.className = `pai-btn${item.primary ? " primary" : ""}`;
    if (item.icon) btn.appendChild(PaiIcons.el(item.icon, 14.4));
    btn.appendChild(document.createTextNode(item.label));
    btn.addEventListener("click", async () => {
      btn.disabled = true;
      try {
        await item.onClick();
      } finally {
        btn.disabled = false;
      }
    });
    row.appendChild(btn);
  }
  return row;
}
