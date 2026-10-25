// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 G ROX EOOD
//
// FR-120 P6 — on a touch screen a target is at least 36 × 36 px.
//
// The rule lives in `ui/src/styles/responsive.css`: under `(pointer: coarse)`
// every button and every tappable chip gets an invisible `::before` that grows
// its hit area to 36 px, centred, without moving its box. jsdom does no layout
// and matches no pointer media, so the stylesheet is parsed and its shape is
// locked: the hit area exists only under a coarse pointer (a desktop stays
// pixel-identical), sits under the control's own content (a chip's close button
// keeps its taps), and survives a chip's clipping. The field proof is the sweep
// on production (FR-120 §8).
//
// Also here, the open question FR-120 answered with it: the viewer's Settings
// covers the screen on a phone (portrait or sideways), not below 1280 px.
import { describe, expect, it } from 'vitest'
import { readFileSync } from 'node:fs'
import { resolve } from 'node:path'
import postcss, { type AtRule, type Rule } from 'postcss'
import { parse as parseSfc } from '@vue/compiler-sfc'
import { NodeTypes, type ElementNode, type TemplateChildNode } from '@vue/compiler-dom'

// Vitest runs from `ui/` (where its config lives).
const read = (path: string) => readFileSync(resolve(process.cwd(), path), 'utf8')

const css = postcss.parse(read('src/styles/responsive.css'))
const INSET = 'min(0px, (100% - 36px) / 2)'

/** The rules directly inside `@media (pointer: coarse)`. */
function coarseRules(): Rule[] {
  const out: Rule[] = []
  css.walkAtRules('media', (at: AtRule) => {
    if (at.params.replace(/\s+/g, ' ').trim() === '(pointer: coarse)') {
      at.walkRules((r) => {
        out.push(r)
      })
    }
  })
  return out
}

const decls = (r: Rule) => Object.fromEntries(r.nodes.filter((n) => n.type === 'decl').map((d) => [d.prop, d.value]))
const ruleFor = (rules: Rule[], selector: string) => rules.find((r) => r.selectors.map((s) => s.trim()).includes(selector))

describe('FR-120 P6 — touch targets of at least 36 px on a coarse pointer', () => {
  const rules = coarseRules()

  it('gives every button and tappable chip a centred 36 px hit area', () => {
    for (const sel of ['.v-btn::before', '.v-chip--link::before']) {
      const r = ruleFor(rules, sel)
      expect(r, `${sel} under (pointer: coarse)`).toBeTruthy()
      const d = decls(r!)
      expect(d.content).toBe("''")
      expect(d.position).toBe('absolute')
      for (const side of ['top', 'bottom', 'left', 'right']) expect(d[side], `${sel} ${side}`).toBe(INSET)
    }
  })

  it('keeps the hit area under the control’s own content, inside its own stacking context', () => {
    // `z-index: -1` without `isolation` would drop the hit area behind the row
    // around the control, where no tap reaches it.
    expect(decls(ruleFor(rules, '.v-btn::before')!)['z-index']).toBe('-1')
    for (const sel of ['.v-btn', '.v-chip--link']) {
      const r = rules.find((x) => x.selectors.map((s) => s.trim()).includes(sel) && 'isolation' in decls(x))
      expect(r && decls(r).isolation, `${sel} isolates`).toBe('isolate')
    }
  })

  it('lets a tappable chip’s hit area out of its box, clipping its label instead', () => {
    // Two classes, so it outranks Vuetify's `.v-chip { overflow: hidden }`
    // whichever stylesheet loads last.
    const chip = rules.find((x) => x.selector.trim() === '.v-chip.v-chip--link' && 'overflow' in decls(x))
    expect(chip && decls(chip).overflow).toBe('visible')
    const label = decls(ruleFor(rules, '.v-chip--link > .v-chip__content')!)
    expect(label.overflow).toBe('hidden')
    expect(label['min-width']).toBe('0')
  })

  it('touches nothing a mouse sees: no button or chip pseudo-element outside a coarse pointer', () => {
    const outside: string[] = []
    css.walkRules((r) => {
      const inCoarse = r.parent?.type === 'atrule' && (r.parent as AtRule).params.includes('pointer: coarse')
      if (!inCoarse && /\.v-(btn|chip)[^,]*::?before/.test(r.selector)) outside.push(r.selector)
    })
    expect(outside).toEqual([])
  })

  it('reaches exactly 36 px for a small control and leaves a big one alone', () => {
    // The inset, evaluated: half the shortfall, negative; 0 when there is none.
    const hit = (px: number) => px - 2 * Math.min(0, (px - 36) / 2)
    expect([20, 26, 28, 32, 36, 40, 48].map(hit)).toEqual([36, 36, 36, 36, 36, 40, 48])
  })

  it('gives the mesh graph’s carrier chips room for their hit areas inside the chip group’s scroller', () => {
    // A `v-chip-group` scrolls sideways (`overflow-y: hidden`), so a hit area
    // grown past its 20 px chips was clipped to 28 px; on a touch screen each
    // chip gets 10 px above and below: the 8 px (half of 36 − 20) its hit
    // area needs, and 4 px between two rows' hit areas.
    const { descriptor } = parseSfc(read('src/components/stats/MeshGraph.vue'), { filename: 'MeshGraph.vue' })
    const style = postcss.parse(descriptor.styles.map((s) => s.content).join('\n'))
    let margin: string | null = null
    style.walkAtRules('media', (at) => {
      if (at.params.trim() !== '(pointer: coarse)') return
      at.walkRules((r) => {
        if (r.selector.includes('.v-chip-group .v-chip')) margin = decls(r)['margin-block'] ?? null
      })
    })
    expect(margin).toBe('10px')
  })
})

// --- the viewer's Settings: full screen on a phone, a card from 600 px ---

function templateOf(path: string): ElementNode {
  const { descriptor } = parseSfc(read(path), { filename: path })
  expect(descriptor.template?.ast, `${path} has a template`).toBeTruthy()
  return descriptor.template!.ast as unknown as ElementNode
}

function walk(node: ElementNode | TemplateChildNode, visit: (el: ElementNode) => void): void {
  if (!('children' in node) || !Array.isArray(node.children)) return
  if (node.type === NodeTypes.ELEMENT) visit(node)
  for (const c of node.children as TemplateChildNode[]) walk(c, visit)
}

/** A bound attribute's expression, or a static one's value. */
function attr(el: ElementNode, name: string): string | null {
  for (const p of el.props) {
    if (p.type === NodeTypes.ATTRIBUTE && p.name === name) return p.value?.content ?? ''
    if (p.type === NodeTypes.DIRECTIVE && p.name === 'bind' && p.arg && 'content' in p.arg && p.arg.content === name) {
      return p.exp && 'content' in p.exp ? p.exp.content : ''
    }
    if (p.type === NodeTypes.DIRECTIVE && p.name === 'model' && name === 'v-model') {
      return p.exp && 'content' in p.exp ? p.exp.content : ''
    }
  }
  return null
}

describe('FR-120 — the viewer’s Settings covers the screen on a phone only', () => {
  const VIEW = 'src/views/remote/RemoteControl.vue'

  it('binds the Settings dialog to settingsFullscreen, never to mobile (< 1280 px)', () => {
    const dialogs: ElementNode[] = []
    walk(templateOf(VIEW), (e) => {
      if (e.tag === 'v-dialog' && attr(e, 'v-model') === 'settingsOpen') dialogs.push(e)
    })
    expect(dialogs).toHaveLength(1)
    expect(attr(dialogs[0], 'fullscreen')).toBe('settingsFullscreen')
    expect(attr(dialogs[0], 'max-width')).toBe('settingsFullscreen ? undefined : 560')
  })

  it('means a phone: under 600 px wide, or under 600 px tall (held sideways)', () => {
    const src = read(VIEW)
    expect(src).toContain('const settingsFullscreen = computed(() => xs.value || displayHeight.value < 600)')
    expect(src).toMatch(/height: displayHeight \} = useDisplay\(\)/)
  })
})
