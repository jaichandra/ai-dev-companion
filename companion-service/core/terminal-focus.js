// Which terminal window a review was opened in, and bringing it back to
// the front. Plain JS so terminal-focus.test.js runs with no build step.
//
// core/terminal.ts's launch AppleScript prints one line naming the new
// terminal ("iterm <window id> <session id>" or "terminal <window id>
// <tty>"); parseTerminalLocation turns that into a location, and
// focusTerminal selects it again. Locations are read back from disk, so
// every field is checked against its expected shape on the way in and
// passed to osascript as an argument, never pasted into the script.

const { execFile } = require("child_process");

const WINDOW_ID = /^\d{1,10}$/;
// iTerm's session ids are UUIDs.
const ITERM_SESSION_ID = /^[0-9A-Fa-f]{8}(-[0-9A-Fa-f]{4}){3}-[0-9A-Fa-f]{12}$/;
const TTY = /^\/dev\/ttys\d{1,4}$/;

function isValidTerminalLocation(loc) {
  if (!loc || typeof loc !== "object" || !WINDOW_ID.test(String(loc.windowId))) return false;
  if (loc.app === "iterm") return typeof loc.sessionId === "string" && ITERM_SESSION_ID.test(loc.sessionId);
  if (loc.app === "terminal") return typeof loc.tty === "string" && TTY.test(loc.tty);
  return false;
}

/** The location in the launch AppleScript's output, or null. */
function parseTerminalLocation(text) {
  const [app, windowId, id, ...rest] = String(text || "").trim().split(" ");
  if (rest.length > 0) return null;
  const loc = app === "iterm" ? { app, windowId, sessionId: id } : { app, windowId, tty: id };
  return isValidTerminalLocation(loc) ? loc : null;
}

// Neither launches its terminal if it isn't running (a closed review's
// terminal can't be there), and both print "found" or "missing".
const FOCUS_SCRIPTS = {
  iterm: `
    on run argv
      set wanted to item 1 of argv
      if application "iTerm" is not running then return "missing"
      tell application "iTerm"
        repeat with w in windows
          repeat with t in tabs of w
            repeat with s in sessions of t
              if (id of s) is wanted then
                tell w to select
                tell t to select
                tell s to select
                activate
                return "found"
              end if
            end repeat
          end repeat
        end repeat
      end tell
      return "missing"
    end run
  `,
  terminal: `
    on run argv
      set wanted to item 1 of argv
      if application "Terminal" is not running then return "missing"
      tell application "Terminal"
        repeat with w in windows
          repeat with t in tabs of w
            if (tty of t) is wanted then
              set selected of t to true
              try
                set index of w to 1
              end try
              activate
              return "found"
            end if
          end repeat
        end repeat
      end tell
      return "missing"
    end run
  `,
};

/** The osascript argv that focuses `loc`; throws for an invalid one. */
function focusCommand(loc) {
  if (!isValidTerminalLocation(loc)) throw new Error("Invalid terminal location.");
  return ["-e", FOCUS_SCRIPTS[loc.app], loc.app === "iterm" ? loc.sessionId : loc.tty];
}

/** Brings `loc`'s window/tab to the front; resolves whether it was found. */
function focusTerminal(loc, { osascript = "osascript" } = {}) {
  return new Promise((resolve, reject) => {
    let args;
    try {
      args = focusCommand(loc);
    } catch (err) {
      reject(err);
      return;
    }
    execFile(osascript, args, { timeout: 10000 }, (err, stdout, stderr) => {
      if (err) reject(new Error(`osascript failed: ${String(stderr || err.message).trim()}`));
      else resolve(stdout.trim() === "found");
    });
  });
}

module.exports = {
  FOCUS_SCRIPTS,
  isValidTerminalLocation,
  parseTerminalLocation,
  focusCommand,
  focusTerminal,
};
