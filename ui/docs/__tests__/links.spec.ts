// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 G ROX EOOD
/**
 * FR-60 (#1165) / FR-87 (#1776) — the internal-link gate, now testable.
 */
import { describe, expect, it } from 'vitest'
import { checkLinks, type LinkPage } from '../theme/links.ts'

const page = (url: string, html: string, anchors: string[] = []): LinkPage => ({
  id: `${url}.md`,
  url,
  html,
  anchors: new Set(anchors),
})
const docs = (p: string) => p.startsWith('/docs/')

describe('checkLinks', () => {
  const target = page('/docs/network/exit-nodes/', '', ['setup'])

  it('passes links that resolve, absolute or relative', () => {
    const from = page('/docs/network/', '<a href="/docs/network/exit-nodes/">a</a><a href="exit-nodes/">b</a><a href="./exit-nodes">c</a>')
    expect(checkLinks([from, target], docs)).toEqual([])
  })

  it('reports a page nobody generates', () => {
    const from = page('/docs/network/', '<a href="/docs/network/exit-node/">typo</a>')
    expect(checkLinks([from, target], docs)).toEqual([
      '/docs/network/.md — link "/docs/network/exit-node/" points at /docs/network/exit-node/, which no page generates',
    ])
  })

  it('reports a missing heading on another page', () => {
    const from = page('/docs/network/', '<a href="../network/exit-nodes/#install">x</a>')
    expect(checkLinks([from, target], docs)[0]).toMatch(/#install on \/docs\/network\/exit-nodes\//)
    const ok = page('/docs/network/', '<a href="exit-nodes/#setup">x</a>')
    expect(checkLinks([ok, target], docs)).toEqual([])
  })

  it('reports a missing heading on the SAME page, which FR-60 never checked', () => {
    const p = page('/docs/start/', '<a href="#later">x</a><a href="#here">y</a>', ['here'])
    expect(checkLinks([p], docs)).toEqual(['/docs/start/.md — link "#later" points at a heading this page does not have'])
  })

  it('skips what it cannot verify: other sites, other schemes, routes it does not own, files', () => {
    const p = page(
      '/docs/start/',
      '<a href="https://example.com/x">a</a><a href="mailto:x@y.z">b</a><a href="tel:1">c</a>' +
        '<a href="/register">d</a><a href="/docs/assets/guide.pdf">e</a><a href="#">f</a>',
    )
    expect(checkLinks([p], docs)).toEqual([])
  })

  it('checks a prefix it is told it owns — the blog will be one', () => {
    const p = page('/blog/post/', '<a href="/docs/nope/">x</a><a href="/blog/other/">y</a>')
    const owned = (x: string) => x.startsWith('/docs/') || x.startsWith('/blog/')
    expect(checkLinks([p], owned)).toHaveLength(2)
  })
})
