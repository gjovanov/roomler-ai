// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 G ROX EOOD
//
// #3258 — the ended page offers Force control (admin break-glass) only where
// it can do something. Its reason rides `rc:session.request`, which an
// outsider's session (FR-52, the `connect-session` route) never sends, so the
// external viewer must not show the button at all. FR-27 P10's rule stays:
// not right after the person at the device ended the session.
//
// The rule is `offersBreakGlass`. RemoteControl.vue is too large to mount, so
// its template is parsed, as rcRatingMount.spec.ts does, to pin that the
// button renders under that rule and under nothing looser.
import { describe, expect, it } from 'vitest'
import { readFileSync } from 'node:fs'
import { resolve } from 'node:path'
import { parse as parseSfc } from '@vue/compiler-sfc'
import { NodeTypes, type ElementNode, type TemplateChildNode } from '@vue/compiler-dom'
import { offersBreakGlass } from '@/composables/breakGlass'

describe('#3258: offersBreakGlass', () => {
  it('offers it to a member whose session ended any way but the host\'s Disconnect', () => {
    expect(offersBreakGlass({ external: false, endedBy: null })).toBe(true)
  })

  it('does not offer it right after the person at the device ended the session (FR-27 P10)', () => {
    expect(offersBreakGlass({ external: false, endedBy: 'host' })).toBe(false)
  })

  it('never offers it on the external viewer, however the session ended', () => {
    expect(offersBreakGlass({ external: true, endedBy: null })).toBe(false)
    expect(offersBreakGlass({ external: true, endedBy: 'host' })).toBe(false)
  })
})

const source = readFileSync(resolve(process.cwd(), 'src/views/remote/RemoteControl.vue'), 'utf8')
const { descriptor } = parseSfc(source, { filename: 'RemoteControl.vue' })
const root = descriptor.template!.ast as unknown as ElementNode
const script = descriptor.scriptSetup?.content ?? ''

function elements(node: ElementNode | TemplateChildNode, out: ElementNode[] = []): ElementNode[] {
  if (node.type === NodeTypes.ELEMENT) out.push(node as ElementNode)
  if ('children' in node && Array.isArray(node.children)) {
    for (const c of node.children as TemplateChildNode[]) elements(c, out)
  }
  return out
}

function staticAttr(el: ElementNode, name: string): string | null {
  for (const p of el.props) {
    if (p.type === NodeTypes.ATTRIBUTE && p.name === name) return p.value?.content ?? ''
  }
  return null
}

function vIf(el: ElementNode): string | null {
  for (const p of el.props) {
    if (p.type === NodeTypes.DIRECTIVE && p.name === 'if') {
      return p.exp && 'content' in p.exp ? p.exp.content.replace(/\s+/g, ' ').trim() : ''
    }
  }
  return null
}

describe('#3258: RemoteControl.vue renders Force control under that rule', () => {
  const all = elements(root)

  it('has one Force control button, and it renders only where offersBreakGlass says so', () => {
    const buttons = all.filter((el) => staticAttr(el, 'data-testid') === 'rc-force-control')
    expect(buttons).toHaveLength(1)
    expect(vIf(buttons[0])).toBe('offersBreakGlass({ external: isExternal, endedBy: rc.endedBy.value })')
  })

  it('is the only control that opens the break-glass dialog', () => {
    // A second opener would be a way around the rule above.
    const openers = all.filter((el) =>
      el.props.some(
        (p) =>
          p.type === NodeTypes.DIRECTIVE &&
          p.name === 'on' &&
          p.exp &&
          'content' in p.exp &&
          /forceDialogOpen\s*=\s*true/.test(p.exp.content),
      ),
    )
    expect(openers).toHaveLength(1)
    expect(staticAttr(openers[0], 'data-testid')).toBe('rc-force-control')
    expect(script).not.toMatch(/forceDialogOpen\.value\s*=\s*true/)
  })

  it('takes isExternal from the route the external viewer runs on', () => {
    expect(script).toMatch(/const isExternal = route\.name === 'connect-session'/)
  })
})
