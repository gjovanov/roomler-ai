// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 G ROX EOOD
/**
 * #4210: no source file under `ui/src` holds UTF-8 that was read as Latin-1
 * and saved again.
 *
 * Each pass of that damage turns every byte of a character's UTF-8 into one
 * Latin-1 character. A lead byte C2 or C3 becomes U+00C2 or U+00C3, and the
 * continuation byte after it (80-BF) becomes a character in U+0080-U+00BF.
 * Two passes turned an arrow (U+2192, bytes E2 86 92) into
 * U+00C3 U+00A2 U+00C2 U+0086 U+00C2 U+0092, with two invisible C1 controls
 * in it. From 2026-07-28 (#241) until #4210, `composables/useRemoteControl.ts`
 * carried such lines, 528 of them at the end.
 *
 * Correct text puts neither U+00C2 nor U+00C3 before a character in
 * U+0080-U+00BF, so any line that does is damaged. Text damaged by ONE pass
 * can lack that pair (an em dash, E2 80 94, becomes U+00E2 U+0080 U+0094), but
 * it still holds C1 controls (U+0080-U+009F), and UI source has no use for
 * one. So a line holding a C1 control fails too. The patterns are written
 * with escapes, so this file never matches itself.
 */
import { describe, expect, it } from 'vitest'
import { readdirSync, readFileSync } from 'node:fs'
import { dirname, extname, join, relative, sep } from 'node:path'
import { fileURLToPath } from 'node:url'

const SRC = join(dirname(fileURLToPath(import.meta.url)), '..')
const EXTENSIONS = new Set(['.ts', '.vue', '.js', '.json'])
const LEAD_BYTE_READ_AS_LATIN1 = /[ÂÃ][\u0080-¿]/
const C1_CONTROL = /[\u0080-\u009F]/

function sourceFiles(dir: string): string[] {
  return readdirSync(dir, { withFileTypes: true }).flatMap((entry) => {
    const path = join(dir, entry.name)
    if (entry.isDirectory()) return sourceFiles(path)
    return EXTENSIONS.has(extname(entry.name)) ? [path] : []
  })
}

const FILES = sourceFiles(SRC)
  .map((path) => relative(SRC, path).split(sep).join('/'))
  .sort()

/** Every file with a line `pattern` matches: `<file>: <n> line(s): <line>, ...`. */
function linesMatching(pattern: RegExp): { lines: number; files: string[] } {
  const files: string[] = []
  let lines = 0
  for (const file of FILES) {
    const hits: number[] = []
    readFileSync(join(SRC, file), 'utf8')
      .split(/\r?\n/)
      .forEach((line, i) => {
        if (pattern.test(line)) hits.push(i + 1)
      })
    if (hits.length > 0) {
      lines += hits.length
      files.push(`${file}: ${hits.length} line(s): ${hits.join(', ')}`)
    }
  }
  return { lines, files }
}

describe('source encoding (#4210)', () => {
  it('reads the source files under ui/src, nested ones included', () => {
    expect(FILES).toContain('App.vue')
    expect(FILES).toContain('composables/useRemoteControl.ts')
    expect(FILES).toContain('locales/en.json')
    expect(FILES).toContain('__tests__/sourceEncoding.spec.ts')
  })

  it('no line holds a UTF-8 lead byte read as Latin-1', () => {
    const { lines, files } = linesMatching(LEAD_BYTE_READ_AS_LATIN1)
    expect(
      files,
      `${lines} line(s) in ${files.length} file(s) put U+00C2 or U+00C3 before a ` +
        'character in U+0080-U+00BF: UTF-8 read as Latin-1 and saved again. ' +
        `Restore the characters they were (#4210).\n${files.join('\n')}`,
    ).toEqual([])
  })

  it('no line holds a C1 control character', () => {
    const { lines, files } = linesMatching(C1_CONTROL)
    expect(
      files,
      `${lines} line(s) in ${files.length} file(s) hold a C1 control character ` +
        '(U+0080-U+009F), which UI source has no use for: text read as Latin-1 ' +
        `and saved again leaves them. Restore the characters they were (#4210).\n${files.join('\n')}`,
    ).toEqual([])
  })
})
