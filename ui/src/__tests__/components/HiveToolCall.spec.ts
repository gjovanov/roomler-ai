// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 G ROX EOOD
//
// FR-90 P1f — a tool call in the transcript, by its tool: a command line, an
// edit as a diff, a to-do list, output coloured as a terminal shows it. All
// of it is the model's or a tool's text, and stays text.
import { describe, expect, it } from 'vitest'
import { mount } from '@vue/test-utils'
import { createVuetify } from 'vuetify'
import * as components from 'vuetify/components'
import * as directives from 'vuetify/directives'
import { createI18n } from 'vue-i18n'
import en from '@/locales/en.json'
import HiveToolCall from '@/components/hive/HiveToolCall.vue'

const vuetify = createVuetify({ components, directives })
const HOSTILE = '<img src=x onerror="alert(1)">'
const E = '\u001b'

interface Call {
  name: string
  input: unknown
  result?: { ok: boolean; output: string; truncated?: boolean }
}

function render(props: Call) {
  const i18n = createI18n({ legacy: false, locale: 'en', messages: { en } })
  return mount(HiveToolCall, { props, global: { plugins: [vuetify, i18n] } })
}

describe('HiveToolCall (FR-90 P1f)', () => {
  it('shows a command line and its output, coloured', () => {
    const w = render({
      name: 'Bash',
      input: { command: 'cargo test', description: 'Run the tests' },
      result: { ok: true, output: `${E}[32mok${E}[0m 12 passed\n` },
    })
    expect(w.find('[data-testid="hive-command"]').text()).toBe('$ cargo test')
    expect(w.text()).toContain('Run the tests')
    const green = w.find('[data-testid="hive-ansi"] .ansi-fg-2')
    expect(green.text()).toBe('ok')
    expect(w.find('[data-testid="hive-ansi"]').text()).not.toContain('[32m')
  })

  it('shows an edit as removed and added lines, and its result as one line', () => {
    const w = render({
      name: 'Edit',
      input: { file_path: '/w/src/main.rs', old_string: 'fn a() {}\nfn b() {}', new_string: 'fn a() {}\nfn c() {}' },
      result: { ok: true, output: 'The file /w/src/main.rs has been updated.\nmore detail' },
    })
    const rows = w.findAll('[data-testid="hive-diff"] .hive-diff-row')
    expect(rows.map((r) => r.attributes('data-kind'))).toEqual(['same', 'del', 'add'])
    expect(rows[1].text()).toContain('fn b() {}')
    expect(rows[2].text()).toContain('fn c() {}')
    expect(w.find('[data-testid="hive-tool-line"]').text()).toBe('The file /w/src/main.rs has been updated.')
  })

  it('folds a file read behind its line count, and shows a failure in full', () => {
    const read = render({
      name: 'Read',
      input: { file_path: '/w/README.md', offset: 3, limit: 2 },
      result: { ok: true, output: 'line three\nline four\n' },
    })
    expect(read.text()).toContain('lines 3–4')
    expect(read.find('[data-testid="hive-tool-folded"] summary').text()).toBe('Output · lines: 2')
    const failed = render({ name: 'Read', input: { file_path: '/w/nope' }, result: { ok: false, output: 'File does not exist.' } })
    expect(failed.find('[data-testid="hive-tool-folded"]').exists()).toBe(false)
    expect(failed.find('[data-testid="hive-ansi"]').classes()).toContain('hive-error')
  })

  it('shows a to-do list with each item’s state, and hides its result', () => {
    const w = render({
      name: 'TodoWrite',
      input: { todos: [{ content: 'write it', status: 'completed' }, { content: 'test it', status: 'in_progress' }] },
      result: { ok: true, output: 'Todos have been modified successfully.' },
    })
    expect(w.findAll('.hive-todos li').map((li) => li.attributes('data-status'))).toEqual(['completed', 'in_progress'])
    expect(w.text()).not.toContain('Todos have been modified')
  })

  it('keeps every string the model or a tool wrote as text — never markup', () => {
    const cases = [
      { name: 'Bash', input: { command: HOSTILE, description: HOSTILE }, result: { ok: true, output: `${E}[31m${HOSTILE}` } },
      { name: 'Edit', input: { file_path: HOSTILE, old_string: 'a', new_string: HOSTILE }, result: { ok: true, output: HOSTILE } },
      { name: 'Write', input: { file_path: '/w/x.html', content: HOSTILE } },
      { name: 'TodoWrite', input: { todos: [{ content: HOSTILE, status: 'pending' }] } },
      { name: 'WebFetch', input: { url: 'javascript:alert(1)', prompt: HOSTILE } },
      { name: 'mcp__other__tool', input: { x: HOSTILE }, result: { ok: false, output: HOSTILE } },
    ]
    for (const c of cases) {
      const w = render(c)
      expect(w.find('img').exists(), c.name).toBe(false)
      expect(w.find('a').exists(), c.name).toBe(false)
      expect(w.text(), c.name).toContain('<img src=x onerror=')
    }
  })
})
