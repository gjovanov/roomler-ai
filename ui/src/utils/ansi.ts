// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 G ROX EOOD
/**
 * FR-90 P1f — a tool's output with the colours a terminal would give it.
 *
 * Only SGR sequences (`ESC [ … m`) change the style. Every other escape — a
 * cursor move, an erase, a charset switch, an OSC title or hyperlink — is
 * dropped; an OSC hyperlink keeps its visible text, which sits outside the
 * sequence. A carriage return starts the line over, as on a terminal, so a
 * progress bar shows its last state.
 *
 * The result is DATA: runs of text, each with a style built only from a
 * fixed set — a palette index (0–15), an RGB triple of integers, and five
 * flags. The transcript renders it by text interpolation into spans whose
 * classes come from that set, never as an HTML string, so output can neither
 * inject markup nor name a class or a style of its own.
 */

export type AnsiColor = number | [number, number, number]

export interface AnsiStyle {
  /** 0–15 for the palette, or an RGB triple. */
  fg?: AnsiColor
  bg?: AnsiColor
  bold?: boolean
  dim?: boolean
  italic?: boolean
  underline?: boolean
  inverse?: boolean
}

export interface AnsiSegment {
  text: string
  style: AnsiStyle
}

/** At most this many styled runs; the rest of the output is one plain run. */
export const MAX_SEGMENTS = 4000

// CSI (with its parameters and final byte), OSC (to BEL or ST, or to the end
// when unterminated), any other escape, and a lone ESC at the end.
// eslint-disable-next-line no-control-regex
const ESCAPE = /\u001b(?:\[([0-?]*)[ -/]*([@-~])|\][^\u0007\u001b]*(?:\u0007|\u001b\\)?|[ -/]*[0-~]|$)/g
// C0 controls a terminal would not print, other than \t \n \r (and ESC,
// handled above), and DEL.
// eslint-disable-next-line no-control-regex
const UNPRINTED = /[\u0000-\u0008\u000b\u000c\u000e-\u001a\u001c-\u001f\u007f]/g

const clamp = (n: number) => Math.max(0, Math.min(255, Math.round(n)))

/** xterm's 256-colour table, past the 16 palette entries. */
function xterm256(n: number): AnsiColor {
  if (n < 16) return n
  if (n < 232) {
    const i = n - 16
    const level = (v: number) => (v === 0 ? 0 : 55 + v * 40)
    return [level(Math.floor(i / 36)), level(Math.floor(i / 6) % 6), level(i % 6)]
  }
  const g = 8 + (n - 232) * 10
  return [g, g, g]
}

/** `style` changed by one SGR parameter list (`1;31`, `38;5;208`, `38;2;r;g;b`). */
function applySgr(style: AnsiStyle, raw: string): AnsiStyle {
  const s: AnsiStyle = { ...style }
  const params = raw === '' ? [0] : raw.split(/[;:]/).map((p) => (p === '' ? NaN : Number(p)))
  for (let i = 0; i < params.length; i++) {
    const p = params[i]
    if (Number.isNaN(p) || p === 0) {
      for (const k of Object.keys(s)) delete s[k as keyof AnsiStyle]
    } else if (p === 1) s.bold = true
    else if (p === 2) s.dim = true
    else if (p === 3) s.italic = true
    else if (p === 4) s.underline = true
    else if (p === 7) s.inverse = true
    else if (p === 22) {
      delete s.bold
      delete s.dim
    } else if (p === 23) delete s.italic
    else if (p === 24) delete s.underline
    else if (p === 27) delete s.inverse
    else if (p >= 30 && p <= 37) s.fg = p - 30
    else if (p >= 90 && p <= 97) s.fg = p - 90 + 8
    else if (p === 39) delete s.fg
    else if (p >= 40 && p <= 47) s.bg = p - 40
    else if (p >= 100 && p <= 107) s.bg = p - 100 + 8
    else if (p === 49) delete s.bg
    else if (p === 38 || p === 48) {
      const key = p === 38 ? 'fg' : 'bg'
      const mode = params[i + 1]
      if (mode === 5 && Number.isFinite(params[i + 2])) {
        s[key] = xterm256(clamp(params[i + 2]))
        i += 2
      } else if (mode === 2) {
        // `38:2::r:g:b` carries an empty colour-space id before the triple.
        let at = i + 2
        if (Number.isNaN(params[at]) && params.length - at >= 4) at += 1
        const [r, g, b] = [params[at], params[at + 1], params[at + 2]]
        if ([r, g, b].every(Number.isFinite)) s[key] = [clamp(r), clamp(g), clamp(b)]
        i = at + 2
      } else {
        // An extended colour this parser cannot read ends the list: what
        // follows is its operands, not codes.
        break
      }
    }
  }
  return s
}

const sameStyle = (a: AnsiStyle, b: AnsiStyle) => JSON.stringify(a) === JSON.stringify(b)

/** `input` as styled runs: escapes read or dropped, overwritten lines dropped. */
export function parseAnsi(input: string): AnsiSegment[] {
  const out: AnsiSegment[] = []
  let line: AnsiSegment[] = []
  let style: AnsiStyle = {}
  let overflow = false

  const push = (text: string) => {
    if (!text) return
    const last = line[line.length - 1]
    if (last && sameStyle(last.style, style)) last.text += text
    else line.push({ text, style: { ...style } })
  }
  const endLine = (nl: boolean) => {
    for (const seg of line) {
      const last = out[out.length - 1]
      if (last && sameStyle(last.style, seg.style)) last.text += seg.text
      else out.push(seg)
    }
    line = []
    if (nl) {
      const last = out[out.length - 1]
      if (last && sameStyle(last.style, {})) last.text += '\n'
      else out.push({ text: '\n', style: {} })
    }
  }
  /** Plain text, with `\r` and `\n` applied. */
  const text = (chunk: string) => {
    const parts = chunk.replace(UNPRINTED, '').split(/(\r\n|\n|\r)/)
    for (const part of parts) {
      if (part === '\n' || part === '\r\n') endLine(true)
      else if (part === '\r') line = []
      else push(part)
    }
  }

  let at = 0
  ESCAPE.lastIndex = 0
  for (let m = ESCAPE.exec(input); m; m = ESCAPE.exec(input)) {
    text(input.slice(at, m.index))
    if (m[2] === 'm') style = applySgr(style, m[1] ?? '')
    at = m.index + m[0].length
    if (out.length + line.length >= MAX_SEGMENTS) {
      overflow = true
      break
    }
  }
  if (overflow) {
    // The rest, plain: still the output, only no longer styled.
    endLine(false)
    out.push({ text: input.slice(at).replace(ESCAPE, '').replace(UNPRINTED, ''), style: {} })
    return out.filter((s) => s.text)
  }
  text(input.slice(at))
  endLine(false)
  return out.filter((s) => s.text)
}

/** Whether `input` carries any escape at all — plain output skips the parser. */
export function hasAnsi(input: string): boolean {
  return input.includes('\u001b') || input.includes('\r')
}
