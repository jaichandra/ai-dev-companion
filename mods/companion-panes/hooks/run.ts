// The Run pane's logic with no engine in it: what a command prints, which commands to suggest
// and which processes belong to a run.

export const MAX_LINES = 2000
export const MAX_SUGGESTIONS = 10
/** What the wrapper prints first, so the mod knows the shell's pid to stop the whole tree by. */
export const PID_MARKER = '@@companion-pid '

/** Runs `$1` in a login shell, after saying which process that is. `exec` keeps the pid. */
export const WRAPPER = `printf '${PID_MARKER}%s\\n' "$$" >&2; exec sh -lc "$1"`

// CSI sequences, OSC sequences (title, hyperlinks) and the two-byte escapes.
const ANSI = /\u001b\[[0-9;?]*[ -/]*[@-~]|\u001b\][^\u0007\u001b]*(?:\u0007|\u001b\\)|\u001b[@-Z\\-_]/g
const CONTROLS = /[\u0000-\u0008\u000b\u000c\u000e-\u001f\u007f-\u009f]/g

export const stripAnsi = (text: string): string => text.replace(ANSI, '')

/** A progress bar redraws its line with `\r`: only what is left after the last one shows. */
const afterReturn = (line: string): string => line.slice(line.lastIndexOf('\r') + 1).replace(CONTROLS, '')

/** The last `MAX_LINES` lines a command printed, held as text arrives in pieces. */
export class RunLog {
  private lines: string[] = []
  private partial = ''

  push(text: string): void {
    const parts = (this.partial + stripAnsi(text)).split('\n')
    this.partial = parts.pop() ?? ''
    for (const part of parts) this.lines.push(afterReturn(part))
    if (this.lines.length > MAX_LINES) this.lines.splice(0, this.lines.length - MAX_LINES)
  }

  get size(): number {
    return this.lines.length + (this.partial ? 1 : 0)
  }

  /** The last `count` lines, an unfinished one included. */
  tail(count: number): string[] {
    const all = this.partial ? [...this.lines, afterReturn(this.partial)] : this.lines
    return count >= all.length ? [...all] : all.slice(all.length - count)
  }

  clear(): void {
    this.lines = []
    this.partial = ''
  }
}

const PREFERRED = ['dev', 'start', 'serve', 'watch', 'test', 'build', 'lint']

/** `npm run <script>` for each script of a package.json, the usual ones first. */
export function packageScripts(text: string): string[] {
  let scripts: unknown
  try {
    scripts = (JSON.parse(text) as { scripts?: unknown }).scripts
  } catch {
    return []
  }
  if (typeof scripts !== 'object' || scripts === null) return []
  const names = Object.keys(scripts).filter(name => /^[\w:.-]+$/.test(name))
  const rank = (name: string): number => {
    const at = PREFERRED.indexOf(name.split(':')[0] ?? '')
    return at < 0 ? PREFERRED.length : at
  }
  return names
    .map((name, at) => ({ name, at }))
    .sort((a, b) => rank(a.name) - rank(b.name) || a.at - b.at)
    .map(({ name }) => `npm run ${name}`)
}

/** `make <target>` for each plain target of a Makefile: not a pattern, a variable or a special one. */
export function makeTargets(text: string): string[] {
  const found: string[] = []
  for (const line of text.split('\n')) {
    const match = /^([A-Za-z0-9][\w.-]*)\s*:(?![=:])/.exec(line)
    const target = match?.[1]
    if (target && !found.includes(target)) found.push(target)
  }
  return found.map(target => `make ${target}`)
}

export type ProjectFiles = { packageJson?: string; makefile?: string; hasCargo?: boolean; extra?: readonly string[] }

export function suggestions(files: ProjectFiles): string[] {
  // What the installation adds (hooks/companion.json) comes first: it is what its team runs most.
  const all = [
    ...(files.extra ?? []),
    ...(files.packageJson === undefined ? [] : packageScripts(files.packageJson)),
    ...(files.hasCargo ? ['cargo run', 'cargo test'] : []),
    ...(files.makefile === undefined ? [] : makeTargets(files.makefile)),
  ]
  return [...new Set(all)].slice(0, MAX_SUGGESTIONS)
}

/** The pid the wrapper reported, and the text after its line. Undefined while the line is incomplete. */
export function readPid(head: string): { pid?: number; rest: string } | undefined {
  if (!head.startsWith(PID_MARKER)) return head.length < PID_MARKER.length && PID_MARKER.startsWith(head) ? undefined : { rest: head }
  const end = head.indexOf('\n')
  if (end < 0) return undefined
  const pid = Number(head.slice(PID_MARKER.length, end))
  return { ...(Number.isInteger(pid) && pid > 1 ? { pid } : {}), rest: head.slice(end + 1) }
}

/** `root` and everything below it, from `ps -A -o pid=,ppid=`. */
export function descendants(ps: string, root: number): number[] {
  const children = new Map<number, number[]>()
  for (const line of ps.split('\n')) {
    const [pid, parent] = line.trim().split(/\s+/).map(Number)
    if (pid === undefined || parent === undefined || !Number.isInteger(pid) || !Number.isInteger(parent)) continue
    children.set(parent, [...(children.get(parent) ?? []), pid])
  }
  const found = [root]
  for (let at = 0; at < found.length; at++) {
    for (const child of children.get(found[at] as number) ?? []) if (!found.includes(child)) found.push(child)
  }
  return found
}

export type RunStatus = 'running' | 'exited' | 'stopped' | 'failed'

export function statusMark(status: RunStatus, code: number | null): string {
  if (status === 'running') return '●'
  if (status === 'stopped') return '■'
  return status === 'exited' && code === 0 ? '✓' : '✗'
}

export function statusWords(status: RunStatus, code: number | null): string {
  if (status === 'running') return 'running'
  if (status === 'stopped') return 'stopped'
  if (status === 'failed') return 'could not start'
  return code === null ? 'killed' : `exit ${code}`
}

/** Whether any pattern occurs in the text, ignoring case. No patterns match nothing. */
export function matchesAny(text: string, patterns: readonly string[]): boolean {
  const lower = text.toLowerCase()
  return patterns.some(pattern => pattern.trim() !== '' && lower.includes(pattern.trim().toLowerCase()))
}
