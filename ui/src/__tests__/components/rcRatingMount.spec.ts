// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 G ROX EOOD
//
// FR-102 P9b — at most ONE rating prompt ever renders. `RemoteControl.vue`
// mounts `<session-rating-prompt>` in the closed ending and in the error
// ending; the two must be branches of one `v-if` / `v-else-if` chain, which
// is what makes them mutually exclusive. The error ending cannot be produced
// against the e2e harness (it needs a failure the synthetic agent never
// has), so this locks the structure itself: the template is parsed and
// every mount's branch is checked, and a third mount outside the chain, or
// a mount under a sibling `v-if` that is not part of it, fails here.
import { describe, it, expect } from 'vitest'
import { readFileSync } from 'node:fs'
import { resolve } from 'node:path'
import { parse as parseSfc } from '@vue/compiler-sfc'
import { NodeTypes, type ElementNode, type TemplateChildNode } from '@vue/compiler-dom'

// Vitest runs from `ui/` (where its config lives); under jsdom
// `import.meta.url` is not a `file:` URL, so the SFC is resolved from there.
const source = readFileSync(resolve(process.cwd(), 'src/views/remote/RemoteControl.vue'), 'utf8')
const PROMPT_TAG = 'session-rating-prompt'

type Branch = { parent: ElementNode; node: ElementNode; kind: 'if' | 'else-if' | 'else' }

function directiveKind(el: ElementNode): Branch['kind'] | null {
  for (const p of el.props) {
    if (p.type !== NodeTypes.DIRECTIVE) continue
    if (p.name === 'if') return 'if'
    if (p.name === 'else-if') return 'else-if'
    if (p.name === 'else') return 'else'
  }
  return null
}

/** Every `<session-rating-prompt>` with the chain of element ancestors above it. */
function findMounts(node: TemplateChildNode, ancestors: ElementNode[], out: { el: ElementNode; ancestors: ElementNode[] }[]) {
  if (node.type !== NodeTypes.ELEMENT) return
  if (node.tag === PROMPT_TAG) out.push({ el: node, ancestors: [...ancestors] })
  for (const child of node.children) findMounts(child, [...ancestors, node], out)
}

/** The nearest ancestor that is a branch of some chain, with that chain's parent. */
function nearestBranch(ancestors: ElementNode[]): Branch | null {
  for (let i = ancestors.length - 1; i >= 1; i--) {
    const kind = directiveKind(ancestors[i])
    if (kind) return { parent: ancestors[i - 1], node: ancestors[i], kind }
  }
  return null
}

/** The element children of `parent`, in order, skipping whitespace and comments. */
function elementChildren(parent: ElementNode): ElementNode[] {
  return parent.children.filter((c): c is ElementNode => c.type === NodeTypes.ELEMENT)
}

describe('RemoteControl.vue mounts the rating prompt in mutually exclusive branches', () => {
  const { descriptor } = parseSfc(source, { filename: 'RemoteControl.vue' })
  const template = descriptor.template?.ast
  expect(template, 'the SFC has a template').toBeTruthy()

  const mounts: { el: ElementNode; ancestors: ElementNode[] }[] = []
  for (const child of template!.children) findMounts(child, [template as unknown as ElementNode], mounts)

  it('mounts the prompt in the closed ending and the error ending, and nowhere else', () => {
    expect(mounts.length).toBe(2)
  })

  it('every mount sits in a branch of ONE v-if chain, so at most one renders', () => {
    const branches = mounts.map((m) => nearestBranch(m.ancestors))
    for (const b of branches) expect(b, 'a mount outside any v-if branch').not.toBeNull()
    const parents = new Set(branches.map((b) => b!.parent))
    expect(parents.size, 'the branches hang off one parent').toBe(1)
    const parent = [...parents][0]
    const siblings = elementChildren(parent)
    const indexes = branches.map((b) => siblings.indexOf(b!.node)).sort((a, b) => a - b)
    expect(indexes.every((i) => i >= 0)).toBe(true)
    // Walk from the first branch to the last: the first must open the chain
    // (`v-if`) or continue one, and every element between must continue it
    // (`v-else-if` / `v-else`) — an element without a directive, or a fresh
    // `v-if`, would start a new chain and the mounts could render together.
    const first = indexes[0]
    const last = indexes[indexes.length - 1]
    // Find the chain's own `v-if`: walk back from the first branch over
    // continuing branches until the opener.
    let opener = first
    while (opener > 0 && directiveKind(siblings[opener]) !== 'if') {
      const k = directiveKind(siblings[opener])
      expect(k, `a branch at ${opener} without a directive`).not.toBeNull()
      opener -= 1
    }
    expect(directiveKind(siblings[opener])).toBe('if')
    for (let i = opener + 1; i <= last; i++) {
      const k = directiveKind(siblings[i])
      expect(k, `element ${i} between the mounts breaks the chain`).not.toBeNull()
      expect(k, `element ${i} opens a new chain`).not.toBe('if')
    }
    // The branches are distinct: two mounts in one branch would both render.
    expect(new Set(indexes).size).toBe(indexes.length)
  })

  it('both mounts are guarded by the same pending ref', () => {
    for (const m of mounts) {
      const cond = m.el.props.find((p) => p.type === NodeTypes.DIRECTIVE && p.name === 'if')
      expect(cond, 'a mount without v-if').toBeTruthy()
      const exp = cond && cond.type === NodeTypes.DIRECTIVE && cond.exp && 'content' in cond.exp ? cond.exp.content : ''
      expect(exp.trim()).toBe('ratingPending')
    }
  })
})
