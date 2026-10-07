// Splits a stream of text chunks into lines. Used by core/exec.ts to hand
// a child's stdout to a listener line by line while still buffering the
// whole output for the caller. A listener that throws must never break the
// run it is observing, so exceptions are swallowed.
function createLineSplitter(onLine) {
  let pending = "";
  const emit = (line) => {
    if (!line) return;
    try {
      onLine(line);
    } catch {
      /* a progress listener failing must not affect the command */
    }
  };
  return {
    write(chunk) {
      pending += chunk;
      let i;
      while ((i = pending.indexOf("\n")) !== -1) {
        const line = pending.slice(0, i).replace(/\r$/, "");
        pending = pending.slice(i + 1);
        emit(line);
      }
    },
    flush() {
      const rest = pending.replace(/\r$/, "");
      pending = "";
      emit(rest);
    },
  };
}

module.exports = { createLineSplitter };
