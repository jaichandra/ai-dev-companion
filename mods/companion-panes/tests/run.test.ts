import { expect, test } from 'claude-code/testing'

import {
  descendants,
  makeTargets,
  matchesAny,
  MAX_LINES,
  packageScripts,
  readPid,
  RunLog,
  statusMark,
  statusWords,
  stripAnsi,
  suggestions,
} from '../hooks/run'

test('colour and title escapes are stripped, the text stays', async () => {
  expect(stripAnsi('\u001b[32mready\u001b[0m \u001b]0;title\u0007on :3000')).toBe('ready on :3000')
})

test('a log joins pieces into lines and shows an unfinished one', async () => {
  const log = new RunLog()
  log.push('first\nsec')
  log.push('ond\nthi')
  expect(log.tail(10)).toEqual(['first', 'second', 'thi'])
  expect(log.size).toBe(3)
  expect(log.tail(2)).toEqual(['second', 'thi'])
})

test('a progress bar that redraws with a carriage return shows its last state', async () => {
  const log = new RunLog()
  log.push('10%\r50%\r100%\ndone\n')
  expect(log.tail(5)).toEqual(['100%', 'done'])
})

test('a log keeps the last lines only', async () => {
  const log = new RunLog()
  for (let at = 0; at < MAX_LINES + 50; at++) log.push(`line ${at}\n`)
  expect(log.size).toBe(MAX_LINES)
  expect(log.tail(1)).toEqual([`line ${MAX_LINES + 49}`])
  expect(log.tail(MAX_LINES + 100)[0]).toBe('line 50')
  log.clear()
  expect(log.size).toBe(0)
})

test('package scripts come as npm run, the usual ones first', async () => {
  const text = JSON.stringify({ scripts: { prepare: 'x', 'test:unit': 'y', dev: 'z', 'bad name': 'q', build: 'b' } })
  expect(packageScripts(text)).toEqual(['npm run dev', 'npm run test:unit', 'npm run build', 'npm run prepare'])
  expect(packageScripts('not json')).toEqual([])
  expect(packageScripts('{}')).toEqual([])
})

test('make targets skip patterns, variables and special targets', async () => {
  const text = ['CC := gcc', '.PHONY: all', 'all: build', 'build: main.o', '%.o: %.c', 'x = y', 'test:', 'build: again'].join('\n')
  expect(makeTargets(text)).toEqual(['make all', 'make build', 'make test'])
})

test('suggestions put package scripts first, then cargo, then make, up to ten', async () => {
  expect(suggestions({ packageJson: '{"scripts":{"dev":"x"}}', hasCargo: true, makefile: 'lint:\n' })).toEqual([
    'npm run dev',
    'cargo run',
    'cargo test',
    'make lint',
  ])
  expect(suggestions({})).toEqual([])
  const many = JSON.stringify({ scripts: Object.fromEntries(Array.from({ length: 30 }, (_, at) => [`s${at}`, 'x'])) })
  expect(suggestions({ packageJson: many })).toHaveLength(10)
})

test('the wrapper reports its pid before anything else', async () => {
  expect(readPid('@@companion-pid 4242\nhello')).toEqual({ pid: 4242, rest: 'hello' })
  expect(readPid('@@compan')).toBeUndefined()
  expect(readPid('@@companion-pid 42')).toBeUndefined()
  expect(readPid('plain output')).toEqual({ rest: 'plain output' })
  expect(readPid('@@companion-pid x\nrest')).toEqual({ rest: 'rest' })
})

test('every process below a pid is found, nothing else', async () => {
  const ps = [' 1 0', ' 100 1', ' 101 100', ' 102 101', ' 103 100', ' 200 1', '', 'junk'].join('\n')
  expect(descendants(ps, 100).sort()).toEqual([100, 101, 102, 103])
  expect(descendants(ps, 999)).toEqual([999])
})

test('a run is marked and described by how it ended', async () => {
  expect(statusMark('running', null)).toBe('●')
  expect(statusMark('exited', 0)).toBe('✓')
  expect(statusMark('exited', 2)).toBe('✗')
  expect(statusWords('exited', 2)).toBe('exit 2')
  expect(statusWords('exited', null)).toBe('killed')
  expect(statusWords('stopped', null)).toBe('stopped')
  expect(statusWords('failed', null)).toBe('could not start')
})

test('suggestions the installation adds come first and are not repeated', async () => {
  expect(suggestions({ extra: ['docker compose up', 'npm run dev'], packageJson: '{"scripts":{"dev":"x","test":"y"}}' })).toEqual([
    'docker compose up',
    'npm run dev',
    'npm run test',
  ])
})

test('a word matches a remote or a path whatever its case, and no words match nothing', async () => {
  expect(matchesAny('ssh://host/team/Acme-rest.git', ['acme'])).toBe(true)
  expect(matchesAny('/w/other', ['acme', ' other '])).toBe(true)
  expect(matchesAny('/w/other', ['acme'])).toBe(false)
  expect(matchesAny('/w/other', [])).toBe(false)
  expect(matchesAny('/w/other', ['', '  '])).toBe(false)
})
