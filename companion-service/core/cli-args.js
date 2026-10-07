// Argument parsing for the `companion` command. Pure.
const COMMANDS = ["status", "jobs", "history", "similar", "resume", "forget", "open", "inbox", "digest", "precheck", "hooks", "doctor", "help"];
const VALUE_FLAGS = ["--feature", "--search", "--days"];
const BOOLEAN_FLAGS = { "--json": "json", "--yes": "yes", "-y": "yes", "--metrics": "metrics", "--all": "all", "--stdin": "stdin", "--brief": "brief" };

function parseArgs(argv) {
  const out = { command: "help", positional: [], flags: {}, errors: [] };
  const args = [...argv];
  if (args.length === 0) return out;
  if (!args[0].startsWith("-")) {
    const command = args.shift();
    if (!COMMANDS.includes(command)) {
      out.errors.push(`Unknown command "${command}".`);
      return out;
    }
    out.command = command;
  }
  for (let i = 0; i < args.length; i++) {
    const a = args[i];
    if (Object.prototype.hasOwnProperty.call(BOOLEAN_FLAGS, a)) out.flags[BOOLEAN_FLAGS[a]] = true;
    else if (a === "--help" || a === "-h") out.command = "help";
    else if (VALUE_FLAGS.includes(a)) {
      const value = args[i + 1];
      if (value === undefined || value.startsWith("-")) out.errors.push(`${a} needs a value.`);
      else {
        out.flags[a.slice(2)] = value;
        i++;
      }
    } else if (a.startsWith("-")) out.errors.push(`Unknown option "${a}".`);
    else out.positional.push(a);
  }
  return out;
}

module.exports = { COMMANDS, parseArgs };
