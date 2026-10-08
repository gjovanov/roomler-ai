// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 G ROX EOOD
//
// FR-90 P1f — an edit, line by line: kept, removed and added lines, with
// long runs of kept lines folded.
import { describe, expect, it } from 'vitest'
import { diffLines, MAX_DIFF_LINES, withContext } from '@/utils/lineDiff'

const kinds = (ops: { kind: string; text?: string }[] | null) => (ops ?? []).map((o) => `${o.kind}:${o.text ?? ''}`)

describe('diffLines (FR-90 P1f)', () => {
  it('keeps what is the same, and marks what changed', () => {
    expect(kinds(diffLines('a\nb\nc\n', 'a\nB\nc\n'))).toEqual(['same:a', 'del:b', 'add:B', 'same:c'])
  })

  it('reads a pure insertion and a pure deletion', () => {
    expect(kinds(diffLines('a\nc', 'a\nb\nc'))).toEqual(['same:a', 'add:b', 'same:c'])
    expect(kinds(diffLines('a\nb\nc', 'a\nc'))).toEqual(['same:a', 'del:b', 'same:c'])
    expect(kinds(diffLines('', 'new\nfile'))).toEqual(['add:new', 'add:file'])
    expect(kinds(diffLines('gone', ''))).toEqual(['del:gone'])
  })

  it('finds the common lines inside a changed middle', () => {
    expect(kinds(diffLines('x\n1\n2\n3\ny', 'x\n1\nnew\n3\ny'))).toEqual(['same:x', 'same:1', 'del:2', 'add:new', 'same:3', 'same:y'])
    expect(kinds(diffLines('a\nb\nc\nd', 'b\nc\nd\ne'))).toEqual(['del:a', 'same:b', 'same:c', 'same:d', 'add:e'])
  })

  it('treats a final newline as the end of the last line, and CRLF as LF', () => {
    expect(kinds(diffLines('a\r\nb\r\n', 'a\nb'))).toEqual(['same:a', 'same:b'])
  })

  it('does not diff a side longer than its limit', () => {
    const long = Array.from({ length: MAX_DIFF_LINES + 1 }, (_, i) => `l${i}`).join('\n')
    expect(diffLines(long, 'x')).toBeNull()
    expect(diffLines('x', long)).toBeNull()
  })

  it('shows a middle too large to table as removed then added — still the truth', () => {
    const a = Array.from({ length: 600 }, (_, i) => `a${i}`).join('\n')
    const b = Array.from({ length: 600 }, (_, i) => `b${i}`).join('\n')
    const ops = diffLines(a, b)!
    expect(ops.filter((o) => o.kind === 'del')).toHaveLength(600)
    expect(ops.filter((o) => o.kind === 'add')).toHaveLength(600)
    expect(ops.findIndex((o) => o.kind === 'add')).toBe(600)
  })
})

describe('withContext (FR-90 P1f)', () => {
  it('folds long runs of kept lines, keeping context next to each change', () => {
    const before = Array.from({ length: 20 }, (_, i) => `l${i}`).join('\n')
    const after = before.replace('l10', 'L10')
    const rows = withContext(diffLines(before, after)!, 2)
    expect(rows[0]).toEqual({ kind: 'skip', count: 8 })
    expect(rows.slice(1, 7).map((r) => ('text' in r ? r.text : ''))).toEqual(['l8', 'l9', 'l10', 'L10', 'l11', 'l12'])
    expect(rows[7]).toEqual({ kind: 'skip', count: 7 })
  })

  it('never folds a single line', () => {
    const rows = withContext(diffLines('a\nb\nc\nd\ne', 'a\nb\nc\nd\nE')!, 3)
    expect(rows.every((r) => r.kind !== 'skip')).toBe(true)
  })
})
