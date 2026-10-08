<!-- SPDX-License-Identifier: AGPL-3.0-only -->
<!-- Copyright (C) 2026 G ROX EOOD -->
<!--
  FR-90 P1f — a tool's output, coloured as a terminal would show it
  (`utils/ansi.ts`). Each run of text is a text node in a span whose classes
  come from a fixed set and whose inline colour, if any, is built from three
  integers: the output can neither inject markup nor pick a style. Built
  with a render function, so no template whitespace leaks into the <pre>.
-->
<script lang="ts">
import { computed, defineComponent, h } from 'vue'
import { hasAnsi, parseAnsi, type AnsiColor, type AnsiStyle } from '@/utils/ansi'

const rgb = (c: [number, number, number]) => `rgb(${c[0]}, ${c[1]}, ${c[2]})`
const palette = (c: AnsiColor | undefined): c is number => typeof c === 'number' && Number.isInteger(c) && c >= 0 && c < 16

function present(s: AnsiStyle): { class: string[]; style: Record<string, string> } {
  let fg = s.fg
  let bg = s.bg
  const cls: string[] = []
  if (s.inverse) {
    ;[fg, bg] = [bg, fg]
    if (fg === undefined) cls.push('ansi-fg-inverse')
    if (bg === undefined) cls.push('ansi-bg-inverse')
  }
  const style: Record<string, string> = {}
  if (palette(fg)) cls.push(`ansi-fg-${fg}`)
  else if (Array.isArray(fg)) style.color = rgb(fg)
  if (palette(bg)) cls.push(`ansi-bg-${bg}`)
  else if (Array.isArray(bg)) style.backgroundColor = rgb(bg)
  if (s.bold) cls.push('ansi-bold')
  if (s.dim) cls.push('ansi-dim')
  if (s.italic) cls.push('ansi-italic')
  if (s.underline) cls.push('ansi-underline')
  return { class: cls, style }
}

export default defineComponent({
  name: 'HiveAnsi',
  props: {
    text: { type: String, required: true },
    error: { type: Boolean, default: false },
  },
  setup(props) {
    const runs = computed(() => (hasAnsi(props.text) ? parseAnsi(props.text) : null))
    return () =>
      h(
        'pre',
        { class: ['hive-pre', 'hive-ansi', { 'hive-error': props.error }], 'data-testid': 'hive-ansi' },
        runs.value === null
          ? props.text
          : runs.value.map((r) => {
              const p = present(r.style)
              return p.class.length || Object.keys(p.style).length ? h('span', p, r.text) : r.text
            }),
      )
  },
})
</script>

<style>
/* The terminal palette, light then dark (GitHub's). Unscoped so the classes
   apply to the runs the render function builds; every rule sits under
   .hive-ansi, so nothing leaks. The block's own look is here too: a scoped
   rule of the transcript does not reach a component nested in another. */
.hive-ansi {
  white-space: pre-wrap;
  word-break: break-word;
  font-family: ui-monospace, SFMono-Regular, Menlo, monospace;
  font-size: 0.8rem;
  max-height: 24rem;
  overflow: auto;
  margin: 0;
  padding: 0.5rem;
  border-radius: 4px;
  background: rgba(var(--v-theme-on-surface), 0.05);
}
.hive-ansi.hive-error { border-left: 3px solid rgb(var(--v-theme-error)); }
.hive-ansi .ansi-bold { font-weight: 700; }
.hive-ansi .ansi-dim { opacity: 0.7; }
.hive-ansi .ansi-italic { font-style: italic; }
.hive-ansi .ansi-underline { text-decoration: underline; }
.hive-ansi .ansi-fg-inverse { color: rgb(var(--v-theme-surface)); }
.hive-ansi .ansi-bg-inverse { background-color: rgb(var(--v-theme-on-surface)); }
.hive-ansi .ansi-fg-0 { color: #24292f; } .hive-ansi .ansi-bg-0 { background-color: #24292f; }
.hive-ansi .ansi-fg-1 { color: #cf222e; } .hive-ansi .ansi-bg-1 { background-color: #cf222e; }
.hive-ansi .ansi-fg-2 { color: #116329; } .hive-ansi .ansi-bg-2 { background-color: #116329; }
.hive-ansi .ansi-fg-3 { color: #7d4e00; } .hive-ansi .ansi-bg-3 { background-color: #7d4e00; }
.hive-ansi .ansi-fg-4 { color: #0969da; } .hive-ansi .ansi-bg-4 { background-color: #0969da; }
.hive-ansi .ansi-fg-5 { color: #8250df; } .hive-ansi .ansi-bg-5 { background-color: #8250df; }
.hive-ansi .ansi-fg-6 { color: #1b7c83; } .hive-ansi .ansi-bg-6 { background-color: #1b7c83; }
.hive-ansi .ansi-fg-7 { color: #6e7781; } .hive-ansi .ansi-bg-7 { background-color: #6e7781; }
.hive-ansi .ansi-fg-8 { color: #57606a; } .hive-ansi .ansi-bg-8 { background-color: #57606a; }
.hive-ansi .ansi-fg-9 { color: #a40e26; } .hive-ansi .ansi-bg-9 { background-color: #a40e26; }
.hive-ansi .ansi-fg-10 { color: #1a7f37; } .hive-ansi .ansi-bg-10 { background-color: #1a7f37; }
.hive-ansi .ansi-fg-11 { color: #633c01; } .hive-ansi .ansi-bg-11 { background-color: #633c01; }
.hive-ansi .ansi-fg-12 { color: #218bff; } .hive-ansi .ansi-bg-12 { background-color: #218bff; }
.hive-ansi .ansi-fg-13 { color: #a475f9; } .hive-ansi .ansi-bg-13 { background-color: #a475f9; }
.hive-ansi .ansi-fg-14 { color: #3192aa; } .hive-ansi .ansi-bg-14 { background-color: #3192aa; }
.hive-ansi .ansi-fg-15 { color: #8c959f; } .hive-ansi .ansi-bg-15 { background-color: #8c959f; }
.v-theme--dark .hive-ansi .ansi-fg-0 { color: #6e7681; } .v-theme--dark .hive-ansi .ansi-bg-0 { background-color: #484f58; }
.v-theme--dark .hive-ansi .ansi-fg-1 { color: #ff7b72; } .v-theme--dark .hive-ansi .ansi-bg-1 { background-color: #ff7b72; }
.v-theme--dark .hive-ansi .ansi-fg-2 { color: #3fb950; } .v-theme--dark .hive-ansi .ansi-bg-2 { background-color: #3fb950; }
.v-theme--dark .hive-ansi .ansi-fg-3 { color: #d29922; } .v-theme--dark .hive-ansi .ansi-bg-3 { background-color: #d29922; }
.v-theme--dark .hive-ansi .ansi-fg-4 { color: #58a6ff; } .v-theme--dark .hive-ansi .ansi-bg-4 { background-color: #58a6ff; }
.v-theme--dark .hive-ansi .ansi-fg-5 { color: #bc8cff; } .v-theme--dark .hive-ansi .ansi-bg-5 { background-color: #bc8cff; }
.v-theme--dark .hive-ansi .ansi-fg-6 { color: #39c5cf; } .v-theme--dark .hive-ansi .ansi-bg-6 { background-color: #39c5cf; }
.v-theme--dark .hive-ansi .ansi-fg-7 { color: #b1bac4; } .v-theme--dark .hive-ansi .ansi-bg-7 { background-color: #b1bac4; }
.v-theme--dark .hive-ansi .ansi-fg-8 { color: #8b949e; } .v-theme--dark .hive-ansi .ansi-bg-8 { background-color: #6e7681; }
.v-theme--dark .hive-ansi .ansi-fg-9 { color: #ffa198; } .v-theme--dark .hive-ansi .ansi-bg-9 { background-color: #ffa198; }
.v-theme--dark .hive-ansi .ansi-fg-10 { color: #56d364; } .v-theme--dark .hive-ansi .ansi-bg-10 { background-color: #56d364; }
.v-theme--dark .hive-ansi .ansi-fg-11 { color: #e3b341; } .v-theme--dark .hive-ansi .ansi-bg-11 { background-color: #e3b341; }
.v-theme--dark .hive-ansi .ansi-fg-12 { color: #79c0ff; } .v-theme--dark .hive-ansi .ansi-bg-12 { background-color: #79c0ff; }
.v-theme--dark .hive-ansi .ansi-fg-13 { color: #d2a8ff; } .v-theme--dark .hive-ansi .ansi-bg-13 { background-color: #d2a8ff; }
.v-theme--dark .hive-ansi .ansi-fg-14 { color: #56d4dd; } .v-theme--dark .hive-ansi .ansi-bg-14 { background-color: #56d4dd; }
.v-theme--dark .hive-ansi .ansi-fg-15 { color: #ffffff; } .v-theme--dark .hive-ansi .ansi-bg-15 { background-color: #f0f6fc; }
</style>
