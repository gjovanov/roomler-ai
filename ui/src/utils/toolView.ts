// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 G ROX EOOD
/**
 * FR-90 P1f — how a session's transcript shows a tool call: what it asked to
 * do, by its tool, and how much of what it returned. Used for every call and
 * for the approval a driver answers, so an `Edit` waiting for one is a diff,
 * not a JSON object with escaped newlines.
 *
 * The input is the model's JSON. A view is built only from fields of the
 * types the tool documents; anything else — a field of another type, a tool
 * this build does not know, an MCP tool — is shown as JSON, as before. Every
 * string ends up as text, never markup (`HiveToolInput`).
 */
import { diffLines, withContext, type DiffRow } from '@/utils/lineDiff'

export type TodoStatus = 'pending' | 'in_progress' | 'completed'

export interface EditChange {
  /** `null`: too long to compare line by line. */
  rows: DiffRow[] | null
  before: string
  after: string
  all: boolean
}

export type ToolView =
  | { kind: 'bash'; command: string; description?: string; background: boolean }
  | { kind: 'edit'; path: string; changes: EditChange[] }
  | { kind: 'write'; path: string; head: string[]; total: number }
  | { kind: 'read'; path: string; from?: number; to?: number }
  | { kind: 'search'; pattern: string; where?: string }
  | { kind: 'todos'; items: { text: string; status: TodoStatus }[] }
  | { kind: 'fetch'; url: string; prompt?: string }
  | { kind: 'query'; query: string }
  | { kind: 'task'; description?: string; agent?: string; prompt?: string }
  | { kind: 'json'; text: string }

/** A `Write` shows this many of its lines, then says how many more. */
export const WRITE_HEAD = 40

const TODO_STATUS: readonly TodoStatus[] = ['pending', 'in_progress', 'completed']

const isStr = (v: unknown): v is string => typeof v === 'string'
const isObj = (v: unknown): v is Record<string, unknown> => typeof v === 'object' && v !== null && !Array.isArray(v)
const text = (v: unknown) => (isStr(v) && v.trim() ? v : undefined)
const positive = (v: unknown) => (typeof v === 'number' && Number.isInteger(v) && v > 0 ? v : undefined)

function json(input: unknown): ToolView {
  let s: string | undefined
  try {
    s = JSON.stringify(input, null, 2)
  } catch {
    s = undefined
  }
  return { kind: 'json', text: s ?? String(input) }
}

function change(o: Record<string, unknown>): EditChange | null {
  if (!isStr(o.old_string) || !isStr(o.new_string)) return null
  const ops = diffLines(o.old_string, o.new_string)
  return { rows: ops && withContext(ops), before: o.old_string, after: o.new_string, all: o.replace_all === true }
}

/** What a call to `name` asked to do, by its tool when the input has that tool's shape. */
export function toolView(name: string, input: unknown): ToolView {
  if (!isObj(input)) return json(input)
  switch (name) {
    case 'Bash':
      if (isStr(input.command)) {
        return {
          kind: 'bash',
          command: input.command,
          description: text(input.description),
          background: input.run_in_background === true,
        }
      }
      break
    case 'Edit':
      if (isStr(input.file_path)) {
        const c = change(input)
        if (c) return { kind: 'edit', path: input.file_path, changes: [c] }
      }
      break
    case 'MultiEdit':
      if (isStr(input.file_path) && Array.isArray(input.edits) && input.edits.length > 0) {
        const changes = input.edits.map((e) => (isObj(e) ? change(e) : null))
        if (changes.every((c): c is EditChange => c !== null)) {
          return { kind: 'edit', path: input.file_path, changes }
        }
      }
      break
    case 'Write':
      if (isStr(input.file_path) && isStr(input.content)) {
        const lines = input.content === '' ? [] : input.content.replace(/\r?\n$/, '').split(/\r?\n/)
        return { kind: 'write', path: input.file_path, head: lines.slice(0, WRITE_HEAD), total: lines.length }
      }
      break
    case 'Read':
      if (isStr(input.file_path)) {
        // `offset` is the first line read, `limit` how many.
        const offset = positive(input.offset)
        const limit = positive(input.limit)
        const from = offset ?? (limit ? 1 : undefined)
        return { kind: 'read', path: input.file_path, from, to: limit && from ? from + limit - 1 : undefined }
      }
      break
    case 'Grep':
    case 'Glob':
      if (isStr(input.pattern)) {
        const where = [text(input.path), text(input.glob), text(input.type)].filter((w) => w !== undefined).join(' · ')
        return { kind: 'search', pattern: input.pattern, where: where || undefined }
      }
      break
    case 'TodoWrite':
      if (Array.isArray(input.todos)) {
        const items = input.todos.flatMap((t) =>
          isObj(t) && isStr(t.content)
            ? [{ text: t.content, status: TODO_STATUS.find((s) => s === t.status) ?? 'pending' }]
            : [],
        )
        if (items.length === input.todos.length) return { kind: 'todos', items }
      }
      break
    case 'WebFetch':
      if (isStr(input.url)) return { kind: 'fetch', url: input.url, prompt: text(input.prompt) }
      break
    case 'WebSearch':
      if (isStr(input.query)) return { kind: 'query', query: input.query }
      break
    case 'Task':
      if (isStr(input.description) || isStr(input.prompt)) {
        return {
          kind: 'task',
          description: text(input.description),
          agent: text(input.subagent_type),
          prompt: text(input.prompt),
        }
      }
      break
  }
  return json(input)
}

/**
 * How much of a call's result shows under it: all of it; folded behind a
 * line count (a file read, a search — long, and the model's to read); one
 * line (an edit says it was made); or nothing (the to-do list is the call
 * itself). A failure always shows in full.
 */
export type ResultMode = 'output' | 'folded' | 'line' | 'hidden'

const FOLDED = new Set(['Read', 'Grep', 'Glob', 'WebFetch', 'WebSearch', 'Task'])
const ONE_LINE = new Set(['Edit', 'MultiEdit', 'Write', 'NotebookEdit'])

export function resultMode(name: string, ok: boolean): ResultMode {
  if (!ok) return 'output'
  if (name === 'TodoWrite') return 'hidden'
  if (FOLDED.has(name)) return 'folded'
  if (ONE_LINE.has(name)) return 'line'
  return 'output'
}

/** A tool's icon in the transcript. */
export function toolIcon(name: string): string {
  switch (name) {
    case 'Bash':
      return 'mdi-console'
    case 'Edit':
    case 'MultiEdit':
      return 'mdi-file-edit-outline'
    case 'Write':
      return 'mdi-file-plus-outline'
    case 'Read':
      return 'mdi-file-eye-outline'
    case 'Grep':
    case 'Glob':
      return 'mdi-file-search-outline'
    case 'TodoWrite':
      return 'mdi-format-list-checks'
    case 'WebFetch':
      return 'mdi-web'
    case 'WebSearch':
      return 'mdi-magnify'
    case 'Task':
      return 'mdi-account-multiple-outline'
    default:
      return 'mdi-wrench'
  }
}
