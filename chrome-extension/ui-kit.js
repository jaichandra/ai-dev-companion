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
