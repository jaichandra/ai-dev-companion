import { expect, test } from 'claude-code/testing'
import type { On } from 'claude-code'

const PANE = {
  plugin: 'companion-panes',
  component: 'Pane',
  requestId: 'run',
  props: {
    title: 'Run',
    isFocused: false,
    bodyColumns: 60,
    placement: 'dock',
    scroll: { offset: 0, bodyRows: 30 },
    view: {},
  },
} as const

const status: (string | undefined)[] = []

const typed = (command: string, args: string) =>
  ({ command, args, origin: { kind: 'composer' }, presentation: { isFullscreen: true, columns: 120 } }) as const

// Stands for the engine beneath the mod: the calls the mod makes are answered here.
function engine(
  on: On,
  process: { calls: string[][]; lines: string[]; remote?: string; opened?: string[] },
  files: Record<string, string> = { 'package.json': '{"scripts":{"dev":"vite","test":"vitest"}}' },
) {
  on('command.register', () => ({ value: { command: 'dev' } }))
  on('tool.register', () => ({ value: { tool: 'mcp__companion-panes__run_output' } }))
  on('ui.open', (_, e) => {
    process.opened?.push(e.id)
    return { value: { isPlaced: true } }
  })
  on('ui.close', () => ({ value: undefined }))
  on('ui.status', (_, e) => {
    status.push(e.text)
    return { value: undefined }
  })
  on('session.start', (_, e) => ({ cwd: e.cwd }))
  on('session.cwd', () => ({ value: '/w/sample-app' }))
  on('fs.read', (_, e) => {
    const name = Object.keys(files).find(file => e.path.endsWith(file))
    if (name !== undefined) return { value: files[name] as string }
    throw new Error('missing')
  })
  on('process.run', (_, e) => {
    process.calls.push([...e.argv])
    if (e.argv[0] === 'git') return { value: { exitCode: 0, stdout: process.remote ?? '', stderr: '', isStdoutTruncated: false, isStderrTruncated: false } }
    if (e.argv[0] === 'ps') return { value: { exitCode: 0, stdout: ' 4242 1\n 4243 4242\n', stderr: '', isStdoutTruncated: false, isStderrTruncated: false } }
    return { value: { exitCode: 0, stdout: '', stderr: '', isStdoutTruncated: false, isStderrTruncated: false } }
  })
  on('process.spawn', async function* () {
    yield { stream: 'stderr' as const, text: '@@companion-pid 4242\n' }
    for (const line of process.lines) yield { stream: 'stdout' as const, text: line }
    // A dev server that goes quiet and stays up.
    await new Promise<void>(() => undefined)
  })
}

test('/dev starts a command and the pane lists it with its output and a Stop', async ($, on) => {
  const process = { calls: [] as string[][], lines: ['ready on :3000\n'] }
  engine(on, process)
  await $.session.start({ cwd: '/w/sample-app', surface: 'terminal', isInteractive: true })

  const ran = await $.command.run(typed('dev', 'npm run dev'))
  expect(ran.text).toContain('Started #1: npm run dev')

  const ui = await $.ui.mount({ ...PANE, surface: 'terminal' })
  expect((await ui.find({ type: 'Text', text: /ready on :3000/ }))?.text).toBe('ready on :3000')
  expect(await ui.find({ key: 'stop:1' })).toBeDefined()
  expect(await ui.find({ key: 'hint:npm run test' })).toBeDefined()
  await ui.unmount()
})

test('the status line keeps /shell while a command runs', async ($, on) => {
  const process = { calls: [] as string[][], lines: ['ready\n'] }
  engine(on, process)
  status.length = 0
  await $.session.start({ cwd: '/w/sample-app', surface: 'terminal', isInteractive: true })
  await $.command.run(typed('dev', 'npm run dev'))
  expect(status.at(-1)).toBe('\u25b6 1 running \u00b7 /dev \u00b7 /shell')
  expect(status[0]).toBe('\u25b6 /dev \u00b7 /shell')
})

test('the buttons are on top and again below the output', async ($, on) => {
  const process = { calls: [] as string[][], lines: ['ready\n'] }
  engine(on, process)
  await $.session.start({ cwd: '/w/sample-app', surface: 'terminal', isInteractive: true })
  await $.command.run(typed('dev', 'docker compose up'))

  const ui = await $.ui.mount({ ...PANE, surface: 'terminal' })
  expect(await ui.find({ key: 'stop:1' })).toBeDefined()
  const keys = (await ui.findAll({ type: 'Button' })).map(button => button.key)
  for (const key of ['stop:1', 'clear:1', 'top', 'end', 'shell', 'close']) {
    expect(keys, key).toContain(key)
    expect(keys, `${key}-b`).toContain(key === 'stop:1' || key === 'clear:1' ? `${key}-b` : `${key}-b`)
  }
  // Ctrl+C is part of Stop, not a button of its own.
  expect(keys.some(key => key?.startsWith('int:'))).toBe(false)
  await ui.unmount()
})

test('Stop signals the whole process tree and the run reads stopped', async ($, on) => {
  const process = { calls: [] as string[][], lines: ['ready\n'] }
  engine(on, process)
  on('clock.sleep', () => ({ value: undefined }))
  await $.session.start({ cwd: '/w/sample-app', surface: 'terminal', isInteractive: true })
  await $.command.run(typed('dev', 'npm run dev'))

  const ui = await $.ui.mount({ ...PANE, surface: 'terminal' })
  await ui.press({ key: 'stop:1' })

  // Ctrl+C first, then the signals that force it, each to the whole tree.
  const sent = process.calls.filter(argv => argv[0] === 'kill' && argv[1] !== '-0').map(argv => argv[1])
  expect(sent).toEqual(['-INT', '-TERM', '-KILL'])
  expect(process.calls.find(argv => argv[1] === '-INT')).toEqual(['kill', '-INT', '4242', '4243'])
  expect(await ui.find({ type: 'Text', text: /stopped/ })).toBeDefined()
  expect(await ui.find({ key: 'restart:1' })).toBeDefined()
  await ui.unmount()
})

test('the run_output tool returns the tail of a run for Claude', async ($, on) => {
  const process = { calls: [] as string[][], lines: ['one\n', 'two\n', 'Error: port 3000 in use\n'] }
  engine(on, process)
  await $.session.start({ cwd: '/w/sample-app', surface: 'terminal', isInteractive: true })
  await $.command.run(typed('dev', 'npm run dev'))
  const ui = await $.ui.mount({ ...PANE, surface: 'terminal' })
  await ui.find({ type: 'Text', text: /port 3000/ })
  await ui.unmount()

  const none = await $.tool.call({ tool: 'mcp__companion-panes__run_output', run: 'nothing like it' })
  expect(String(none.result)).toContain('No run matches')

  const got = await $.tool.call({ tool: 'mcp__companion-panes__run_output', lines: 2 })
  const text = String(got.result)
  expect(text).toContain('#1 npm run dev (running)')
  expect(text).toContain('Error: port 3000 in use')
  expect(text).not.toContain('one')
})

test('the pane closes from /dev close and from its Close button, and the runs go on', async ($, on) => {
  const process = { calls: [] as string[][], lines: ['ready\n'] }
  engine(on, process)
  await $.session.start({ cwd: '/w/sample-app', surface: 'terminal', isInteractive: true })
  await $.command.run(typed('dev', 'npm run dev'))

  const ran = await $.command.run(typed('dev', 'close'))
  expect(ran.text).toContain('Run pane closed')
  expect(ran.text).not.toContain('Started')

  const ui = await $.ui.mount({ ...PANE, surface: 'terminal' })
  expect(await ui.find({ key: 'close' })).toBeDefined()
  expect(await ui.find({ type: 'Text', text: /Esc leaves runs going/ })).toBeDefined()
  expect(await ui.find({ key: 'shell' })).toBeDefined()
  await ui.press({ key: 'close' })
  await ui.unmount()
})

test('suggestions the installation adds come first', async ($, on) => {
  const process = { calls: [] as string[][], lines: [] }
  engine(on, process, {
    'package.json': '{"scripts":{"dev":"vite"}}',
    'companion.json': '{"bin":"{{BIN}}","suggestions":["docker compose up","make seed"]}',
  })
  await $.session.start({ cwd: '/w/sample-app', surface: 'terminal', isInteractive: true })
  await $.command.run(typed('dev', ''))

  const ui = await $.ui.mount({ ...PANE, surface: 'terminal' })
  expect(await ui.find({ key: 'hint:docker compose up' })).toBeDefined()
  expect(await ui.find({ key: 'hint:make seed' })).toBeDefined()
  expect(await ui.find({ key: 'hint:npm run dev' })).toBeDefined()
  await ui.unmount()
})

test('the pane opens by itself in a folder the installation names, and in no other', async ($, on) => {
  const process = { calls: [] as string[][], lines: [], remote: 'ssh://git@host/team/Acme-rest.git\n', opened: [] as string[] }
  engine(on, process, { 'companion.json': '{"bin":"{{BIN}}","openFor":["acme"]}' })
  await $.session.start({ cwd: '/w/sample-app.worktrees/PROJ-1', surface: 'terminal', isInteractive: true })
  // Drawing the pane gives the start-up work the time to settle.
  await (await $.ui.mount({ ...PANE, surface: 'terminal' })).unmount()
  expect(process.opened).toEqual(['run'])
})

test('the pane stays closed elsewhere, and without any installed words', async ($, on) => {
  const other = { calls: [] as string[][], lines: [], remote: 'ssh://git@host/team/other-service.git\n', opened: [] as string[] }
  engine(on, other, { 'companion.json': '{"bin":"{{BIN}}","openFor":["acme"]}' })
  await $.session.start({ cwd: '/w/other-service', surface: 'terminal', isInteractive: true })
  // Drawing the pane gives the start-up work the time to settle.
  await (await $.ui.mount({ ...PANE, surface: 'terminal' })).unmount()
  expect(other.opened).toEqual([])
})

test('the output is coloured: a URL underlined, notes dim, a banner green', async ($, on) => {
  const process = {
    calls: [] as string[][],
    lines: ['\u25b6 OPEN THIS IN YOUR BROWSER:\n    http://app.localhost:3000/web/\n        # example web app\n'],
  }
  engine(on, process)
  await $.session.start({ cwd: '/w/sample-app', surface: 'terminal', isInteractive: true })
  await $.command.run(typed('dev', 'docker compose up'))

  for (const surface of ['terminal', 'desktop'] as const) {
    const ui = await $.ui.mount({ ...PANE, surface })
    expect(await ui.find({ type: 'Text', text: /example web app/ })).toBeDefined()
    const texts = (await ui.findAll({ type: 'Text' })).map(one => one.text)
    expect(texts).toContain('\u25b6 OPEN THIS IN YOUR BROWSER:')
    expect(texts).toContain('http://app.localhost:3000/web/')
    expect(texts).toContain('# example web app')
    await ui.unmount()
  }
})

test('/shell asks the companion binary for a shell tab', async ($, on) => {
  const process = { calls: [] as string[][], lines: [] }
  engine(on, process, { 'companion.json': '{"bin":"/opt/companion/bin/companion"}' })
  await $.session.start({ cwd: '/w/sample-app', surface: 'terminal', isInteractive: true })

  await $.command.run(typed('shell', ''))
  expect(process.calls).toContainEqual(['/opt/companion/bin/companion', 'open-shell', '--cwd', '/w/sample-app'])
})

test('/shell says so when the companion binary was never stamped in', async ($, on) => {
  const process = { calls: [] as string[][], lines: [] }
  engine(on, process, { 'companion.json': '{"bin":"{{BIN}}"}' })
  await $.session.start({ cwd: '/w/sample-app', surface: 'terminal', isInteractive: true })

  const ran = await $.command.run(typed('shell', ''))
  expect(ran.text).toContain('not set up')
  expect(process.calls.filter(argv => argv.includes('open-shell'))).toEqual([])
})
