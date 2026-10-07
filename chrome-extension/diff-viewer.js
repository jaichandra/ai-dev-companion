// Side-by-side unified-diff renderer. No external dependency — `git diff`
// output is a well-defined, simple grammar (hunk headers + " "/"+"/"-"
// prefixed lines), so a small hand-rolled parser avoids vendoring a
// third-party diff library into the extension bundle just for this.
//
// Exposes window.renderDiff(diffText) -> DOM node.
(function () {
  const HUNK_HEADER_RE = /^@@ -(\d+)(?:,(\d+))? \+(\d+)(?:,(\d+))? @@(.*)$/;

  // Split the raw `git diff` text into hunks, each a flat list of rows in
  // original file order: { type: "ctx"|"add"|"del", text, oldLine, newLine }.
  function parseHunks(diffText) {
    const hunks = [];
    let current = null;
    let oldLine = 0;
    let newLine = 0;

    for (const line of diffText.split("\n")) {
      const headerMatch = line.match(HUNK_HEADER_RE);
      if (headerMatch) {
        oldLine = parseInt(headerMatch[1], 10);
        newLine = parseInt(headerMatch[3], 10);
        current = { header: line, rows: [] };
        hunks.push(current);
        continue;
      }
      if (!current) continue; // skip "diff --git", "index", "---", "+++" preamble
      if (line.startsWith("+")) {
        current.rows.push({ type: "add", text: line.slice(1), oldLine: null, newLine: newLine++ });
      } else if (line.startsWith("-")) {
        current.rows.push({ type: "del", text: line.slice(1), oldLine: oldLine++, newLine: null });
      } else if (line.startsWith(" ")) {
        current.rows.push({ type: "ctx", text: line.slice(1), oldLine: oldLine++, newLine: newLine++ });
      }
      // lines like "\ No newline at end of file" are ignored
    }
    return hunks;
  }

  // Pair up consecutive del/add runs so they render side by side (like
  // GitHub/Bitbucket's split view), padding the shorter side with blanks.
  function pairRows(rows) {
    const pairs = [];
    let i = 0;
    while (i < rows.length) {
      if (rows[i].type === "ctx") {
        pairs.push({ left: rows[i], right: rows[i] });
        i++;
        continue;
      }
      const dels = [];
      while (i < rows.length && rows[i].type === "del") dels.push(rows[i++]);
      const adds = [];
      while (i < rows.length && rows[i].type === "add") adds.push(rows[i++]);
      const max = Math.max(dels.length, adds.length);
      for (let j = 0; j < max; j++) {
        pairs.push({ left: dels[j] || null, right: adds[j] || null });
      }
    }
    return pairs;
  }

  function cell(row, side) {
    const td = document.createElement("td");
    const numTd = document.createElement("td");
    numTd.className = "diff-linenum";
    if (!row) {
      td.className = "diff-cell diff-empty";
      numTd.textContent = "";
      return [numTd, td];
    }
    const lineNo = side === "left" ? row.oldLine : row.newLine;
    numTd.textContent = lineNo == null ? "" : String(lineNo);
    td.className =
      "diff-cell " + (row.type === "add" ? "diff-add" : row.type === "del" ? "diff-del" : "diff-ctx");
    td.textContent = row.text;
    return [numTd, td];
  }

  function renderDiff(diffText) {
    const container = document.createElement("div");
    container.className = "diff-viewer";

    if (!diffText || !diffText.trim()) {
      const empty = document.createElement("p");
      empty.textContent = "(no textual diff)";
      container.appendChild(empty);
      return container;
    }

    const hunks = parseHunks(diffText);
    if (hunks.length === 0) {
      // Not a recognizable unified diff (e.g. binary file) — fall back to
      // showing the raw text rather than an empty box.
      const pre = document.createElement("pre");
      pre.className = "diff-raw-fallback";
      pre.textContent = diffText;
      container.appendChild(pre);
      return container;
    }

    for (const hunk of hunks) {
      const header = document.createElement("div");
      header.className = "diff-hunk-header";
      header.textContent = hunk.header;
      container.appendChild(header);

      const table = document.createElement("table");
      table.className = "diff-table";
      const tbody = document.createElement("tbody");
      for (const pair of pairRows(hunk.rows)) {
        const tr = document.createElement("tr");
        const [leftNum, leftCell] = cell(pair.left, "left");
        const [rightNum, rightCell] = cell(pair.right, "right");
        tr.append(leftNum, leftCell, rightNum, rightCell);
        tbody.appendChild(tr);
      }
      table.appendChild(tbody);

      const scrollWrap = document.createElement("div");
      scrollWrap.className = "diff-table-scroll";
      scrollWrap.appendChild(table);
      container.appendChild(scrollWrap);
    }

    return container;
  }

  window.renderDiff = renderDiff;
})();
