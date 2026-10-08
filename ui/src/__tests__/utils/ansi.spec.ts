// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 G ROX EOOD
//
// FR-90 P1f — a tool's output as styled runs: SGR read, every other escape
// dropped, a carriage return starting the line over.
import { describe, expect, it } from 'vitest'
import { hasAnsi, MAX_SEGMENTS, parseAnsi } from '@/utils/ansi'

const E = '\u001b'
const plain = (s: string) => parseAnsi(s).map((r) => r.text).join('')

describe('parseAnsi (FR-90 P1f)', () => {
  it('leaves plain text as one run', () => {
    expect(parseAnsi('one\ntwo\n')).toEqual([{ text: 'one\ntwo\n', style: {} }])
    expect(hasAnsi('one\ntwo')).toBe(false)
  })

  it('reads the palette, bright colours, flags and their resets', () => {
    const runs = parseAnsi(`${E}[1;31merror${E}[0m: ${E}[92mok${E}[39m ${E}[4;3mu${E}[24;23m end`)
    expect(runs).toEqual([
      { text: 'error', style: { bold: true, fg: 1 } },
      { text: ': ', style: {} },
      { text: 'ok', style: { fg: 10 } },
      { text: ' ', style: {} },
      { text: 'u', style: { underline: true, italic: true } },
      { text: ' end', style: {} },
    ])
  })

  it('reads 256 colours and true colour, clamped to integers', () => {
    expect(parseAnsi(`${E}[38;5;208mo${E}[m`)[0].style.fg).toEqual([255, 135, 0])
    expect(parseAnsi(`${E}[38;5;3my`)[0].style.fg).toBe(3)
    expect(parseAnsi(`${E}[48;2;10;20;30mb`)[0].style.bg).toEqual([10, 20, 30])
    expect(parseAnsi(`${E}[38:2::1:2:3mc`)[0].style.fg).toEqual([1, 2, 3])
    expect(parseAnsi(`${E}[38;2;999;300;7mx`)[0].style.fg).toEqual([255, 255, 7])
  })

  it('reads a malformed sequence as text rather than guessing', () => {
    // `-` is not a parameter byte: this is no SGR, and nothing is styled.
    expect(parseAnsi(`${E}[38;2;1;-5;1mx`).every((r) => Object.keys(r.style).length === 0)).toBe(true)
  })

  it('drops every escape that is not SGR, keeping the text around it', () => {
    expect(plain(`a${E}[2K${E}[1Ab${E}[?25lc${E}(Bd${E}=e`)).toBe('abcde')
    // An OSC title, and a hyperlink: the link's text stays, its target goes.
    expect(plain(`${E}]0;title\u0007x ${E}]8;;https://example.com${E}\\link${E}]8;;${E}\\ y`)).toBe('x link y')
    // An unterminated OSC, and a lone ESC at the end.
    expect(plain(`z${E}]0;never ends`)).toBe('z')
    expect(plain(`end${E}`)).toBe('end')
  })

  it('drops control characters a terminal would not print', () => {
    expect(plain('a\u0000b\u0007c\u0008d\u007fe\tf')).toBe('abcde\tf')
  })

  it('starts a line over at a carriage return, keeping the style', () => {
    expect(plain('progress 10%\rprogress 50%\rprogress 100%\ndone')).toBe('progress 100%\ndone')
    expect(plain('crlf\r\nnext')).toBe('crlf\nnext')
    const runs = parseAnsi(`${E}[32mgreen 1\rgreen 2`)
    expect(runs).toEqual([{ text: 'green 2', style: { fg: 2 } }])
  })

  it('stops styling past its run limit, and keeps the rest as plain text', () => {
    const many = Array.from({ length: MAX_SEGMENTS + 50 }, (_, i) => `${E}[3${i % 2}m${i}`).join('')
    const runs = parseAnsi(many)
    expect(runs.length).toBeLessThanOrEqual(MAX_SEGMENTS + 1)
    expect(runs.map((r) => r.text).join('')).toBe(Array.from({ length: MAX_SEGMENTS + 50 }, (_, i) => String(i)).join(''))
  })
})
