// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 G ROX EOOD
/**
 * FR-90 P1f — what an edit changes, line by line: the old text turned into
 * the new, as kept, removed and added lines, for an agent's `Edit` in its
 * transcript and in the approval a driver answers.
 *
 * The common head and tail are trimmed first: an edit is nearly always a
 * small change inside a short snippet. The middle is a longest-common-
 * subsequence table when it is small enough to hold; past that it is shown
 * as every old line removed and every new line added, which is still the
 * truth, only not the shortest one. Either side longer than
 * {@link MAX_DIFF_LINES} is not diffed at all.
 */

export type DiffOp = { kind: 'same' | 'add' | 'del'; text: string }

/** Longer sides are not diffed. */
export const MAX_DIFF_LINES = 2000
/** The largest middle (old lines × new lines) given an exact table. */
const MAX_TABLE = 250_000

function lines(s: string): string[] {
  if (s === '') return []
  const l = s.split(/\r?\n/)
  // A final newline ends the last line; it does not start another.
  if (l[l.length - 1] === '') l.pop()
  return l
}

/** The middle's operations: the exact LCS, or delete-all then add-all. */
function middle(a: string[], b: string[]): DiffOp[] {
  const n = a.length
  const m = b.length
  if (n === 0) return b.map((text) => ({ kind: 'add', text }))
  if (m === 0) return a.map((text) => ({ kind: 'del', text }))
  if (n * m > MAX_TABLE) {
    return [...a.map((text): DiffOp => ({ kind: 'del', text })), ...b.map((text): DiffOp => ({ kind: 'add', text }))]
  }
  // lcs[i][j]: the longest common subsequence of a[i..] and b[j..].
  const lcs = Array.from({ length: n + 1 }, () => new Uint32Array(m + 1))
  for (let i = n - 1; i >= 0; i--) {
    for (let j = m - 1; j >= 0; j--) {
      lcs[i][j] = a[i] === b[j] ? lcs[i + 1][j + 1] + 1 : Math.max(lcs[i + 1][j], lcs[i][j + 1])
    }
  }
  const ops: DiffOp[] = []
  let i = 0
  let j = 0
  while (i < n && j < m) {
    if (a[i] === b[j]) {
      ops.push({ kind: 'same', text: a[i] })
      i++
      j++
    } else if (lcs[i + 1][j] >= lcs[i][j + 1]) {
      ops.push({ kind: 'del', text: a[i++] })
    } else {
      ops.push({ kind: 'add', text: b[j++] })
    }
  }
  while (i < n) ops.push({ kind: 'del', text: a[i++] })
  while (j < m) ops.push({ kind: 'add', text: b[j++] })
  return ops
}

/** `before` turned into `after`, or `null` when either is too long to diff. */
export function diffLines(before: string, after: string): DiffOp[] | null {
  const a = lines(before)
  const b = lines(after)
  if (a.length > MAX_DIFF_LINES || b.length > MAX_DIFF_LINES) return null
  let head = 0
  while (head < a.length && head < b.length && a[head] === b[head]) head++
  let tail = 0
  while (tail < a.length - head && tail < b.length - head && a[a.length - 1 - tail] === b[b.length - 1 - tail]) tail++
  return [
    ...a.slice(0, head).map((text): DiffOp => ({ kind: 'same', text })),
    ...middle(a.slice(head, a.length - tail), b.slice(head, b.length - tail)),
    ...a.slice(a.length - tail).map((text): DiffOp => ({ kind: 'same', text })),
  ]
}

/** A run of kept lines folded away, in a diff shown with some context. */
export type DiffRow = DiffOp | { kind: 'skip'; count: number }

/**
 * `ops` with long runs of kept lines folded: `context` lines are kept next to
 * each change, and a run that would fold fewer than two lines is kept whole.
 */
export function withContext(ops: DiffOp[], context = 3): DiffRow[] {
  const changed = ops.map((o) => o.kind !== 'same')
  const near = ops.map((_, i) => {
    for (let d = -context; d <= context; d++) if (changed[i + d]) return true
    return false
  })
  const rows: DiffRow[] = []
  let i = 0
  while (i < ops.length) {
    if (near[i]) {
      rows.push(ops[i++])
      continue
    }
    let j = i
    while (j < ops.length && !near[j]) j++
    if (j - i < 2) rows.push(...ops.slice(i, j))
    else rows.push({ kind: 'skip', count: j - i })
    i = j
  }
  return rows
}
