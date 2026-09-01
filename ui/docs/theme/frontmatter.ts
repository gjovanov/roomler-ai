// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 G ROX EOOD
/**
 * FR-59 (#1165) — a deliberately tiny YAML-subset frontmatter parser.
 *
 * Adding `gray-matter` + `js-yaml` to ship a docs site would pull a YAML
 * engine into the repo for ten keys, none of which needs anchors, flow
 * mappings, multi-document streams or type coercion. What IS supported:
 *
 *     key: scalar                 (quoted or bare)
 *     key: [a, b, c]              (inline list)
 *     key:                        (block list)
 *       - a
 *       - b
 *     key: true | false           (booleans)
 *     key: 12                     (integers)
 *     # comment
 *
 * Anything else is a build ERROR rather than a silent misparse — a
 * frontmatter key that quietly reads as the string "[object Object]"
 * would ship a broken <title> to production, so the parser refuses what
 * it does not understand instead of guessing.
 */

export type FrontmatterValue = string | number | boolean | string[]
export type Frontmatter = Record<string, FrontmatterValue>

export interface ParsedFile {
  data: Frontmatter
  /** Markdown body with the frontmatter block removed. Line numbers in
   *  errors refer to the ORIGINAL file, so the offset is kept. */
  body: string
  /** 1-based line on which the body starts in the original file. */
  bodyStartLine: number
}

const DELIM = /^---\s*$/

class FrontmatterError extends Error {}

function stripQuotes(raw: string): string {
  const s = raw.trim()
  if (s.length >= 2) {
    const first = s[0]
    const last = s[s.length - 1]
    if ((first === '"' && last === '"') || (first === "'" && last === "'")) {
      return s.slice(1, -1)
    }
  }
  return s
}

function coerce(raw: string): FrontmatterValue {
  const s = raw.trim()
  if (s === 'true') return true
  if (s === 'false') return false
  // Integers only. A bare `1.2` stays a string on purpose: version-like
  // values are the common case here and 0.40 must not become 0.4.
  if (/^-?\d+$/.test(s)) return Number(s)
  return stripQuotes(s)
}

function parseInlineList(raw: string): string[] {
  const inner = raw.trim().slice(1, -1).trim()
  if (inner === '') return []
  return inner.split(',').map((p) => stripQuotes(p))
}

/**
 * Split a file into frontmatter + body. A file with no leading `---`
 * block is an error: every page in this site needs title/description/tags
 * (enforced later, in the build), and a page that silently has none would
 * be indexed with a search-engine-invented snippet.
 */
export function parseFrontmatter(source: string, filePath: string): ParsedFile {
  // Tolerate a UTF-8 BOM and CRLF — this repo is checked out with CRLF on
  // Windows, and an unhandled \r turns every value into "value\r".
  const text = source.replace(/^﻿/, '')
  const lines = text.split(/\r?\n/)

  if (!DELIM.test(lines[0] ?? '')) {
    throw new FrontmatterError(`${filePath}:1 — file must open with a \`---\` frontmatter block`)
  }

  let end = -1
  for (let i = 1; i < lines.length; i++) {
    if (DELIM.test(lines[i]!)) {
      end = i
      break
    }
  }
  if (end === -1) {
    throw new FrontmatterError(`${filePath}:1 — frontmatter block is never closed with \`---\``)
  }

  const data: Frontmatter = {}
  let pendingListKey: string | null = null

  for (let i = 1; i < end; i++) {
    const line = lines[i]!
    const lineNo = i + 1
    if (line.trim() === '' || line.trimStart().startsWith('#')) continue

    // Block-list continuation: `  - value`
    const listItem = /^\s+-\s+(.*)$/.exec(line)
    if (listItem) {
      if (!pendingListKey) {
        throw new FrontmatterError(`${filePath}:${lineNo} — list item with no key above it`)
      }
      ;(data[pendingListKey] as string[]).push(stripQuotes(listItem[1]!))
      continue
    }

    const kv = /^([A-Za-z_][A-Za-z0-9_-]*):\s*(.*)$/.exec(line)
    if (!kv) {
      throw new FrontmatterError(
        `${filePath}:${lineNo} — cannot parse \`${line.trim()}\`. ` +
          `Supported: \`key: value\`, \`key: [a, b]\`, or \`key:\` followed by \`  - item\` lines.`,
      )
    }

    const key = kv[1]!
    const rest = kv[2]!.trim()

    if (rest === '') {
      // Opens a block list. Stays an empty array if nothing follows, which
      // is meaningful: `tags:` with no items is "explicitly none", not
      // "absent" — the Some([]) vs None distinction this codebase keeps
      // relearning the hard way.
      data[key] = []
      pendingListKey = key
      continue
    }

    pendingListKey = null
    data[key] = rest.startsWith('[') && rest.endsWith(']') ? parseInlineList(rest) : coerce(rest)
  }

  return {
    data,
    body: lines.slice(end + 1).join('\n'),
    bodyStartLine: end + 2,
  }
}

/** Typed accessors that fail loudly. The build calls these, so a missing
 *  or wrong-typed key stops the build instead of rendering `undefined`. */
export function requireString(fm: Frontmatter, key: string, filePath: string): string {
  const v = fm[key]
  if (typeof v !== 'string' || v.trim() === '') {
    throw new FrontmatterError(`${filePath} — frontmatter \`${key}\` is required and must be a non-empty string`)
  }
  return v.trim()
}

export function requireStringArray(fm: Frontmatter, key: string, filePath: string): string[] {
  const v = fm[key]
  if (!Array.isArray(v) || v.length === 0) {
    throw new FrontmatterError(`${filePath} — frontmatter \`${key}\` is required and must be a non-empty list`)
  }
  return v.map((s) => String(s).trim()).filter(Boolean)
}

export function optionalString(fm: Frontmatter, key: string): string | undefined {
  const v = fm[key]
  return typeof v === 'string' && v.trim() !== '' ? v.trim() : undefined
}

export function optionalNumber(fm: Frontmatter, key: string): number | undefined {
  const v = fm[key]
  return typeof v === 'number' ? v : undefined
}

export function optionalBoolean(fm: Frontmatter, key: string): boolean | undefined {
  const v = fm[key]
  return typeof v === 'boolean' ? v : undefined
}

export { FrontmatterError }
