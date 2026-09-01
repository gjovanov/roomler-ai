// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 G ROX EOOD
/**
 * FR-58 (#1165) — the frontmatter parser's contract.
 *
 * This parser is deliberately a YAML SUBSET rather than a YAML engine, so
 * what it refuses matters as much as what it accepts: a key that quietly
 * misparses would ship a broken <title> or a wrong description to
 * production, where nothing would fail loudly.
 */
import { describe, expect, it } from 'vitest'
import {
  parseFrontmatter,
  requireString,
  requireStringArray,
  optionalBoolean,
  optionalNumber,
  optionalString,
} from '../theme/frontmatter.ts'

const F = 'test.md'

describe('parseFrontmatter', () => {
  it('parses scalars, inline lists and block lists', () => {
    const { data, body } = parseFrontmatter(
      [
        '---',
        'title: Install on Windows',
        'description: "A quoted one"',
        'tags: [install, windows]',
        'order: 10',
        'faq: true',
        'noindex: false',
        'extra:',
        '  - one',
        '  - two',
        '---',
        '',
        '# Body',
      ].join('\n'),
      F,
    )

    expect(data.title).toBe('Install on Windows')
    expect(data.description).toBe('A quoted one')
    expect(data.tags).toEqual(['install', 'windows'])
    expect(data.order).toBe(10)
    expect(data.faq).toBe(true)
    expect(data.noindex).toBe(false)
    expect(data.extra).toEqual(['one', 'two'])
    expect(body.trim()).toBe('# Body')
  })

  it('tolerates CRLF and a BOM', () => {
    // This repo is checked out with CRLF on Windows. An unhandled \r turns
    // every value into "value\r", which is invisible in a diff and shows up
    // as a stray character in a rendered <title>.
    const src = '﻿---\r\ntitle: T\r\ndescription: D\r\ntags: [a]\r\n---\r\n\r\nbody\r\n'
    const { data } = parseFrontmatter(src, F)
    expect(data.title).toBe('T')
    expect(data.tags).toEqual(['a'])
  })

  it('treats a key with no items as an EMPTY list, not an absent one', () => {
    // Some([]) vs None. `tags:` with nothing under it is "explicitly none",
    // and the required-field check must then fail loudly rather than the
    // key silently reading as absent-and-defaulted.
    const { data } = parseFrontmatter('---\ntitle: T\ntags:\n---\nbody', F)
    expect(data.tags).toEqual([])
    expect(() => requireStringArray(data, 'tags', F)).toThrow(/required/)
  })

  it('ignores comments and blank lines', () => {
    const { data } = parseFrontmatter(
      '---\n# a comment\n\ntitle: T\ndescription: D\ntags: [a]\n---\nbody',
      F,
    )
    expect(data.title).toBe('T')
  })

  it('refuses a file with no frontmatter block', () => {
    expect(() => parseFrontmatter('# Just a heading\n', F)).toThrow(/must open with/)
  })

  it('refuses an unclosed frontmatter block', () => {
    expect(() => parseFrontmatter('---\ntitle: T\n\nbody', F)).toThrow(/never closed/)
  })

  it('refuses a line it cannot parse instead of guessing', () => {
    expect(() => parseFrontmatter('---\ntitle: T\nthis is not a key\n---\nbody', F)).toThrow(
      /cannot parse/,
    )
  })

  it('refuses a list item with no key above it', () => {
    expect(() => parseFrontmatter('---\n  - orphan\n---\nbody', F)).toThrow(/no key above it/)
  })

  it('keeps a decimal as a string so a version is not mangled', () => {
    // 0.40 must not become 0.4.
    const { data } = parseFrontmatter('---\nversion: 0.40\n---\nbody', F)
    expect(data.version).toBe('0.40')
  })
})

describe('typed accessors fail loudly', () => {
  const { data } = parseFrontmatter('---\ntitle: T\ncount: 3\nflag: true\n---\nb', F)

  it('requireString rejects a missing or empty value', () => {
    expect(requireString(data, 'title', F)).toBe('T')
    expect(() => requireString(data, 'nope', F)).toThrow(/required/)
  })

  it('optional accessors return undefined rather than throwing', () => {
    expect(optionalString(data, 'nope')).toBeUndefined()
    expect(optionalNumber(data, 'count')).toBe(3)
    expect(optionalBoolean(data, 'flag')).toBe(true)
    expect(optionalBoolean(data, 'nope')).toBeUndefined()
  })
})
