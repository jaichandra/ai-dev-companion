const test = require("node:test");
const assert = require("node:assert/strict");
const { isValidTerminalLocation, parseTerminalLocation, focusCommand, FOCUS_SCRIPTS } = require("./terminal-focus.js");

const SESSION = "0F3B5B28-6C1D-4E0A-9E2B-5A1C2D3E4F50";

test("parseTerminalLocation reads iTerm's window and session ids", () => {
  assert.deepEqual(parseTerminalLocation(`iterm 12345 ${SESSION}\n`), {
    app: "iterm",
    windowId: "12345",
    sessionId: SESSION,
  });
});

test("parseTerminalLocation reads Terminal's window id and tty", () => {
  assert.deepEqual(parseTerminalLocation("terminal 678 /dev/ttys003\n"), {
    app: "terminal",
    windowId: "678",
    tty: "/dev/ttys003",
  });
});

test("parseTerminalLocation rejects anything else", () => {
  for (const text of [
    "",
    "\n",
    "iterm 1",
    "iterm x " + SESSION,
    "iterm 1 not-a-uuid",
    `iterm 1 ${SESSION}" & do shell script "id`,
    "terminal 1 /dev/ttys003 extra",
    "terminal 1 /etc/passwd",
    "terminal 1 /dev/ttys003\"",
    "xterm 1 /dev/ttys003",
    `iterm 12345678901 ${SESSION}`,
  ]) {
    assert.equal(parseTerminalLocation(text), null, text);
  }
});

test("isValidTerminalLocation checks every field's shape", () => {
  assert.equal(isValidTerminalLocation({ app: "iterm", windowId: "1", sessionId: SESSION }), true);
  assert.equal(isValidTerminalLocation({ app: "terminal", windowId: 2, tty: "/dev/ttys12" }), true);
  assert.equal(isValidTerminalLocation({ app: "terminal", windowId: "2", tty: "/dev/ttys12;rm" }), false);
  assert.equal(isValidTerminalLocation({ app: "iterm", windowId: "1", sessionId: 5 }), false);
  assert.equal(isValidTerminalLocation({ app: "iterm", sessionId: SESSION }), false);
  assert.equal(isValidTerminalLocation(null), false);
  assert.equal(isValidTerminalLocation("iterm 1 x"), false);
});

test("focusCommand passes the id as an argument, never inside the script", () => {
  assert.deepEqual(focusCommand({ app: "iterm", windowId: "1", sessionId: SESSION }), [
    "-e",
    FOCUS_SCRIPTS.iterm,
    SESSION,
  ]);
  assert.deepEqual(focusCommand({ app: "terminal", windowId: "1", tty: "/dev/ttys003" }), [
    "-e",
    FOCUS_SCRIPTS.terminal,
    "/dev/ttys003",
  ]);
  assert.throws(() => focusCommand({ app: "terminal", windowId: "1", tty: "x" }), /Invalid terminal location/);
});

test("the focus scripts never start a terminal that isn't running", () => {
  assert.match(FOCUS_SCRIPTS.iterm, /if application "iTerm" is not running then return "missing"/);
  assert.match(FOCUS_SCRIPTS.terminal, /if application "Terminal" is not running then return "missing"/);
});
