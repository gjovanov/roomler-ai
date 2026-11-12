// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 G ROX EOOD
//
// #3786 — the viewer's "This device cannot capture its screen." overlay
// (FR-80) leaves what still works without a picture to the device. Only the
// agent knows its OS and its install, so it composes that sentence itself
// (`agents/roomlerd/src/capture/mod.rs`, `CaptureUnavailableCode::hint`), and
// the answer differs from host to host. With nobody signed in at the screen,
// a Linux system install browses and sends no files (#3688, said in its hint
// since #3711), a Mac's root daemon declines file transfer (#3757), and a
// Windows system install refuses it (#3669). The overlay used to end with a
// fixed line of its own, "Remote shell, file transfer and the other channels
// are unaffected.", right under a hint that said the opposite.
//
// RemoteControl.vue is too large to mount, so the overlay's own template is
// cut out of it and rendered with the reason a device sends.
import { describe, expect, it } from 'vitest'
import { readFileSync } from 'node:fs'
import { resolve } from 'node:path'
import { defineComponent, ref } from 'vue'
import { mount } from '@vue/test-utils'
import { parse as parseSfc } from '@vue/compiler-sfc'
import { NodeTypes, type ElementNode, type TemplateChildNode } from '@vue/compiler-dom'
import { mediaUnavailableHint } from '@/composables/macPermissionHint'
import type { RcMediaUnavailable } from '@/composables/useRemoteControl'

const source = readFileSync(resolve(process.cwd(), 'src/views/remote/RemoteControl.vue'), 'utf8')
const { descriptor } = parseSfc(source, { filename: 'RemoteControl.vue' })

/** Every element shown under `v-if="rc.mediaUnavailable.value"`. */
function overlays(node: TemplateChildNode, out: ElementNode[] = []): ElementNode[] {
  if (node.type !== NodeTypes.ELEMENT) return out
  const shownWhen = node.props.find((p) => p.type === NodeTypes.DIRECTIVE && p.name === 'if')
  if (
    shownWhen?.type === NodeTypes.DIRECTIVE &&
    shownWhen.exp &&
    'content' in shownWhen.exp &&
    shownWhen.exp.content.trim() === 'rc.mediaUnavailable.value'
  ) {
    out.push(node)
  }
  for (const child of node.children) overlays(child, out)
  return out
}

const found = (descriptor.template?.ast?.children ?? []).flatMap((c) => overlays(c))

/** The overlay rendered with `reason`, as its text with whitespace collapsed. */
function render(reason: RcMediaUnavailable): string {
  const Overlay = defineComponent({
    template: found[0].loc.source,
    setup: () => ({ rc: { mediaUnavailable: ref<RcMediaUnavailable | null>(reason) }, mediaUnavailableHint }),
  })
  const wrapper = mount(Overlay, { global: { stubs: { VIcon: true } } })
  const text = wrapper.text().replace(/\s+/g, ' ').trim()
  wrapper.unmount()
  return text
}

/** What the overlay says in its own words: the text without the device's. */
function ownWords(text: string, reason: RcMediaUnavailable): string {
  let own = text
  for (const theirs of [reason.hint, reason.detail]) if (theirs) own = own.split(theirs).join(' ')
  return own.replace(/\s+/g, ' ').trim()
}

/** A Linux system install with nobody signed in at its screen
 *  (`no_display_hint`, file transfer following the screen: #3711), word for
 *  word, and the X server's refusal scrap reports there. */
const LINUX_SYSTEM_NO_DISPLAY: RcMediaUnavailable = {
  code: 'no_display',
  detail: 'no primary display: connection refused',
  hint:
    'This host has no display to capture (headless, or no desktop session). Remote shell still works. ' +
    'File transfer works as the person signed in at the screen: with nobody signed in there, this ' +
    "computer's files cannot be browsed or downloaded, and a file sent to it is kept where only root can read it.",
}

/** A Mac's root daemon, which has no desktop session and declines file
 *  transfer (#3757). */
const MAC_SYSTEM_DAEMON: RcMediaUnavailable = {
  code: 'no_display',
  detail: '',
  hint:
    'This agent runs as a system daemon, which has no desktop session and therefore no screen to capture. ' +
    "Connect to this machine's user-session agent instead. Remote shell works here.",
}

/** Windows says `backend_error` when every backend fails, and its sentence
 *  names no other channel. */
const WINDOWS_BACKEND_ERROR: RcMediaUnavailable = {
  code: 'backend_error',
  detail: '',
  hint: "The host's capture backend failed to start. Its own error is in the detail; the agent log has the full line.",
}

/** An agent from before FR-80's sentences: a code, and nothing else. */
const NO_SENTENCE: RcMediaUnavailable = { code: 'no_display', detail: '', hint: '' }

describe('#3786: the cannot-capture overlay leaves what still works to the device', () => {
  it('is one element in RemoteControl.vue', () => {
    expect(found).toHaveLength(1)
  })

  it("a Linux system install with nobody at the screen: the device's own words say how file transfer behaves, and nothing else on the overlay does", () => {
    const text = render(LINUX_SYSTEM_NO_DISPLAY)
    expect(text).toContain('This device cannot capture its screen.')
    expect(text).toContain(LINUX_SYSTEM_NO_DISPLAY.hint)
    expect(text).toContain('Reported by the device as no_display.')
    expect(ownWords(text, LINUX_SYSTEM_NO_DISPLAY)).not.toMatch(/file transfer/i)
  })

  const hosts: [string, RcMediaUnavailable][] = [
    ['a Linux system install with nobody at the screen', LINUX_SYSTEM_NO_DISPLAY],
    ["a Mac's root daemon", MAC_SYSTEM_DAEMON],
    ['a Windows capture backend that failed', WINDOWS_BACKEND_ERROR],
    ['an agent that sent no sentence', NO_SENTENCE],
  ]
  it.each(hosts)('%s: the overlay names, in its own words, no channel that still works', (_host, reason) => {
    const own = ownWords(render(reason), reason)
    expect(own).not.toMatch(/file transfer|files|remote shell|clipboard|channels|unaffected|still works?/i)
  })
})
