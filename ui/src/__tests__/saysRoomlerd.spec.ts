// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 G ROX EOOD
/**
 * FR-133 P2b: the daemon has one name a person reads, `roomlerd` ("the
 * daemon" in a sentence), and the machine it runs on is a "device". No text
 * the web app shows a person says "agent".
 *
 * The scan reads every source file under `ui/src` the way the build does:
 * scripts through the TypeScript parser and templates through the Vue SFC
 * parser, so a comment never counts. It reads the text a person can see:
 * template text, static attributes, and the string literals of scripts and
 * of bound expressions. It skips what no person reads: the arguments of a
 * `console.*` call, imports, comparisons, `case` labels, types and property
 * keys. A literal with no space and no capital ('agent', 'agent-logs') is an
 * identifier, not a phrase; template text counts even as one word.
 *
 * "agent" stays where it is a wire or OS identity that a rename would break,
 * and those shapes are taken out before the word is looked for (IDENTITIES).
 * The legal pages are the operator's text and are not scanned. ALLOWED holds
 * each phrase that keeps the word, with the reason. This is the shape of
 * FR-133 P4's CI guard: a new person-facing "agent" fails here, and one that
 * must stay is added to ALLOWED with its reason.
 */
import { describe, expect, it } from 'vitest'
import { readdirSync, readFileSync } from 'node:fs'
import { dirname, extname, join, relative, sep } from 'node:path'
import { fileURLToPath } from 'node:url'
import ts from 'typescript'
import { parse as parseSfc } from 'vue/compiler-sfc'

const SRC = join(dirname(fileURLToPath(import.meta.url)), '..')
const EXTENSIONS = new Set(['.ts', '.vue', '.js', '.json'])
const WORD = /\bagents?\b/i
const CAPITALISED = /\bAgents?\b/

/** Wire and OS identities that keep the word. Taken out before WORD is tried. */
const IDENTITIES: RegExp[] = [
  /\b\w*agents?_\w*/gi, // snake_case names and codes: agent_id, agent_offline
  /\brc:agent\.[\w.]*/gi, // the rc:agent.* message types
  /\/agents?\b/gi, // API path segments: /api/agent/…, /tenant/{tid}/agent/{aid}
  /--agents?\b/g, // the roomler CLI's --agent flag
  /\bcom\.roomler\.agent\b/gi, // the macOS bundle and launchd label
]

/** Static attributes whose value no person reads. */
const CODE_ATTRS = /^(class|style|id|ref|key|name|to|href|src|type|data-.*|.*icon)$/

/** Phrases that keep the word, matched by file and exact text, each with why. */
const ALLOWED: { file: string; text: string; why: string }[] = [
  ...[
    'HEVC 4:4:4: HW Rext encode on agent + HW Rext decode here',
    'H.264 4:4:4: HW High 4:4:4 encode on agent + software decode here',
    'VP9 4:4:4: HW profile-1 encode (vp9_qsv) on agent + software decode here',
    'AV1: HW encode on agent + HW decode here',
    'HEVC: HW encode on agent + HW decode here',
    'H.264-DC: HW encode on agent + HW decode here (beats the SW-encode tier)',
    'VP9 4:4:4: SW encode on agent - Sharper trades HW decode for full-chroma text',
    'VP9 4:2:0: SW encode on agent (capped ≤1920) — no HW×HW codec pair available',
    'VP9 4:2:0: SW encode on agent - this browser decodes VP9 profile 0 only',
  ].map((text) => ({
    file: 'composables/useRemoteControl.ts',
    text,
    why: "pickAutoTransport's reason: console text, only the '[rc] auto transport' console.info reads it",
  })),
]

type Hit = { file: string; line: number; text: string }

function oneLine(s: string): string {
  return s.replace(/\s+/g, ' ').trim()
}

function withoutIdentities(text: string): string {
  return IDENTITIES.reduce((s, re) => s.replace(re, ' '), text)
}

/** A phrase a person reads: it still says "agent" once identities are out. */
function saysAgent(text: string, alwaysText: boolean): boolean {
  const t = withoutIdentities(text)
  if (!WORD.test(t)) return false
  return alwaysText || /\s/.test(t.trim()) || CAPITALISED.test(t)
}

function sourceFiles(dir: string): string[] {
  return readdirSync(dir, { withFileTypes: true }).flatMap((entry) => {
    const path = join(dir, entry.name)
    if (entry.isDirectory()) return entry.name === '__tests__' ? [] : sourceFiles(path)
    if (/\.(spec|test)\.ts$/.test(entry.name)) return []
    return EXTENSIONS.has(extname(entry.name)) ? [path] : []
  })
}

const FILES = sourceFiles(SRC)
  .map((path) => relative(SRC, path).split(sep).join('/'))
  .filter((file) => !file.startsWith('views/legal/'))
  .sort()

const COMPARISONS = new Set([
  ts.SyntaxKind.EqualsEqualsEqualsToken,
  ts.SyntaxKind.ExclamationEqualsEqualsToken,
  ts.SyntaxKind.EqualsEqualsToken,
  ts.SyntaxKind.ExclamationEqualsToken,
])

/** False for a literal no person reads; the nearest deciding parent decides. */
function readByAPerson(node: ts.Node): boolean {
  let n: ts.Node = node
  let p = node.parent
  while (p) {
    if (ts.isCallExpression(p) || ts.isNewExpression(p)) {
      if (p.expression.kind === ts.SyntaxKind.ImportKeyword) return false
      return !/^console\./.test(p.expression.getText())
    }
    if (ts.isImportDeclaration(p) || ts.isExportDeclaration(p) || ts.isImportTypeNode(p)) {
      return false
    }
    if (ts.isLiteralTypeNode(p) || ts.isCaseClause(p)) return false
    if (ts.isBinaryExpression(p) && COMPARISONS.has(p.operatorToken.kind)) return false
    if ((ts.isPropertyAssignment(p) || ts.isPropertySignature(p)) && p.name === n) return false
    if (ts.isElementAccessExpression(p) && p.argumentExpression === n) return false
    if (
      ts.isArrayLiteralExpression(p) ||
      ts.isConditionalExpression(p) ||
      ts.isParenthesizedExpression(p) ||
      ts.isTemplateSpan(p) ||
      ts.isTemplateExpression(p) ||
      ts.isAsExpression(p) ||
      ts.isBinaryExpression(p)
    ) {
      n = p
      p = p.parent
      continue
    }
    return true
  }
  return true
}

function literalText(node: ts.Node): string | null {
  if (ts.isStringLiteral(node) || ts.isNoSubstitutionTemplateLiteral(node)) return node.text
  if (ts.isTemplateExpression(node)) {
    return node.head.text + node.templateSpans.map((s) => '${…}' + s.literal.text).join('')
  }
  return null
}

function scanScript(file: string, code: string, lineOffset: number, kind: ts.ScriptKind, hits: Hit[]) {
  const sf = ts.createSourceFile(file, code, ts.ScriptTarget.Latest, true, kind)
  const visit = (node: ts.Node) => {
    const text = literalText(node)
    if (text !== null) {
      if (readByAPerson(node) && saysAgent(text, false)) {
        const { line } = sf.getLineAndCharacterOfPosition(node.getStart(sf))
        hits.push({ file, line: line + 1 + lineOffset, text: oneLine(text) })
      }
      if (ts.isTemplateExpression(node)) node.templateSpans.forEach((s) => visit(s.expression))
      return
    }
    ts.forEachChild(node, visit)
  }
  visit(sf)
}

type TemplateNode = {
  type: number
  content?: unknown
  loc: { start: { line: number } }
  props?: TemplateProp[]
  children?: TemplateNode[]
  branches?: TemplateNode[]
}
type TemplateProp = {
  type: number
  name: string
  loc: { start: { line: number } }
  value?: { content: string }
  exp?: { content: string }
}

const NODE_ELEMENT = 1
const NODE_TEXT = 2
const NODE_INTERPOLATION = 5
const PROP_ATTRIBUTE = 6
const PROP_DIRECTIVE = 7

function scanVue(file: string, src: string, hits: Hit[]) {
  const { descriptor } = parseSfc(src, { filename: file })
  for (const block of [descriptor.script, descriptor.scriptSetup]) {
    if (block) scanScript(file, block.content, block.loc.start.line - 1, ts.ScriptKind.TS, hits)
  }
  const expression = (exp: string | undefined, line: number) => {
    if (exp && WORD.test(exp)) scanScript(file, `(${exp}\n)`, line - 1, ts.ScriptKind.TS, hits)
  }
  const walk = (node: TemplateNode) => {
    if (node.type === NODE_TEXT) {
      const text = String(node.content)
      if (saysAgent(text, true)) hits.push({ file, line: node.loc.start.line, text: oneLine(text) })
    } else if (node.type === NODE_INTERPOLATION) {
      expression((node.content as { content?: string } | undefined)?.content, node.loc.start.line)
    } else if (node.type === NODE_ELEMENT) {
      for (const prop of node.props ?? []) {
        if (prop.type === PROP_ATTRIBUTE) {
          const value = prop.value?.content
          if (value && !CODE_ATTRS.test(prop.name) && saysAgent(value, false)) {
            hits.push({ file, line: prop.loc.start.line, text: oneLine(value) })
          }
        } else if (prop.type === PROP_DIRECTIVE) {
          expression(prop.exp?.content, prop.loc.start.line)
        }
      }
    }
    node.children?.forEach(walk)
    node.branches?.forEach(walk)
  }
  if (descriptor.template?.ast) walk(descriptor.template.ast as unknown as TemplateNode)
}

function scanJson(file: string, src: string, hits: Hit[]) {
  const visit = (value: unknown) => {
    if (typeof value === 'string') {
      if (saysAgent(value, false)) hits.push({ file, line: 0, text: oneLine(value) })
    } else if (value && typeof value === 'object') {
      Object.values(value).forEach(visit)
    }
  }
  visit(JSON.parse(src))
}

/** Every phrase in `ui/src` a person reads that says "agent", ALLOWED included. */
function scan(): Hit[] {
  const hits: Hit[] = []
  for (const file of FILES) {
    const src = readFileSync(join(SRC, file), 'utf8')
    if (!WORD.test(src)) continue
    if (file.endsWith('.vue')) scanVue(file, src, hits)
    else if (file.endsWith('.json')) scanJson(file, src, hits)
    else scanScript(file, src, 0, file.endsWith('.js') ? ts.ScriptKind.JS : ts.ScriptKind.TS, hits)
  }
  return hits
}

const isAllowed = (hit: Hit) => ALLOWED.some((a) => a.file === hit.file && a.text === hit.text)

describe('FR-133: the web app says roomlerd, never agent', () => {
  const hits = scan()

  it('no text a person reads says "agent"', () => {
    const said = hits.filter((hit) => !isAllowed(hit)).map((h) => `${h.file}:${h.line}  ${h.text}`)
    expect(said).toEqual([])
  })

  it('every ALLOWED phrase is still in the source', () => {
    const stale = ALLOWED.filter((a) => !hits.some((h) => h.file === a.file && h.text === a.text))
    expect(stale).toEqual([])
  })

  it('reads a phrase and skips identities, comments and console lines', () => {
    const probe: Hit[] = []
    scanScript(
      'probe.ts',
      [
        '// the agent in a comment never counts',
        "console.warn('the agent went away')",
        "const kind = x === 'agent' ? 1 : 2",
        "const url = `/api/agent/${id}/logs`",
        "const cmd = 'roomler forward --agent <id>'",
        "const id = 'com.roomler.agent'",
        "const code = 'agent_offline'",
        "notify('The agent is offline')",
      ].join('\n'),
      0,
      ts.ScriptKind.TS,
      probe,
    )
    expect(probe).toEqual([{ file: 'probe.ts', line: 8, text: 'The agent is offline' }])
  })

  it('the subnet-routes page names a node kind, never prints the wire value', () => {
    // It printed `n.kind` raw, so a person read `agent` / `tunnel_client`.
    const src = readFileSync(join(SRC, 'components/admin/OverlaySubnetRoutesSection.vue'), 'utf8')
    expect(src).not.toMatch(/\{\{\s*n\.kind\s*\}\}/)
  })
})
