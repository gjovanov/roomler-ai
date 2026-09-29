// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 G ROX EOOD
/**
 * FR-87 (#1776) — honest dates for the static site.
 *
 *   bun docs/dates.ts --write      (from ui/, in a clone with FULL history)
 *
 * Every date the site publishes (`<lastmod>`, `dateModified`, "Last updated")
 * must say when the CONTENT last changed. FR-60 asked `git log` at build time
 * and fell back to the build date when git had no answer — and in production
 * git never has one: `.dockerignore` drops `.git`, and CI clones one commit.
 * So every sitemap URL claimed the day of the build, while the content had
 * last changed weeks earlier (70 of 70, measured 2026-09-28). A crawler that
 * catches `lastmod` lying stops trusting it for the whole site.
 *
 * Resolution, first answer wins (`resolveDates`):
 *   1. live `git log`, but only when the clone is NOT shallow. A depth-1
 *      clone's single commit "touches" every file, so it would stamp the HEAD
 *      date on all of them: the same lie, one step removed;
 *   2. the manifest `--write` produces (`content-dates.json`), generated on
 *      the hosted-image runner, which has full history, and carried into the
 *      Docker build by `COPY ui/ .`;
 *   3. nothing. An unknown date is OMITTED, never replaced by the build date.
 *
 * A page's front-matter `updated:` overrides all three (see build.ts).
 *
 * ⚠️ `node:*` imports only. The runner generates the manifest before any
 * `bun install`, so this file must not import a package — or `site.ts`, which
 * is free today but need not stay that way.
 */
import { execFileSync } from 'node:child_process'
import { existsSync, readFileSync, writeFileSync } from 'node:fs'
import { dirname, join, relative, resolve } from 'node:path'
import { fileURLToPath } from 'node:url'

export interface ContentDates {
  /** Oldest commit that touched the file (`YYYY-MM-DD`). A rename restarts it. */
  created: string
  /** Newest commit that touched the file (`YYYY-MM-DD`) — what
   *  `git log -1 --format=%cs -- <file>` prints. */
  modified: string
}

export type DatesSource = 'git' | 'manifest' | 'none'

export interface ResolvedDates {
  source: DatesSource
  /** Keyed by repo-relative POSIX path: `ui/docs/content/start/index.md`. */
  files: Map<string, ContentDates>
  /** Why a source was passed over, for the build log. */
  note?: string
}

const HERE = dirname(fileURLToPath(import.meta.url))
export const REPO_ROOT = resolve(HERE, '..', '..')
export const MANIFEST_PATH = join(HERE, 'content-dates.json')
/** What is dated from git, repo-relative. Blog posts are not: their dates are
 *  editorial and live in front-matter, because a typo fix is not a new post. */
export const DATED_PATHS = ['ui/docs/content']
const MANIFEST_VERSION = 1
const ISO_DAY = /^\d{4}-\d{2}-\d{2}$/
const COMMIT = '__C__'

/**
 * Parses `git log --format=__C__%cs --name-only`. The log is newest first, so
 * a file's first sighting is its `modified` date and each later sighting
 * moves `created` back.
 */
export function parseGitLog(log: string): Map<string, ContentDates> {
  const files = new Map<string, ContentDates>()
  let date = ''
  for (const raw of log.split('\n')) {
    const line = raw.trim()
    if (line.startsWith(COMMIT)) {
      date = line.slice(COMMIT.length)
      continue
    }
    if (!line || !ISO_DAY.test(date)) continue
    const seen = files.get(line)
    if (seen) seen.created = date
    else files.set(line, { created: date, modified: date })
  }
  return files
}

function git(args: string[]): string {
  return execFileSync('git', args, {
    cwd: REPO_ROOT,
    encoding: 'utf8',
    maxBuffer: 64 * 1024 * 1024,
    stdio: ['ignore', 'pipe', 'ignore'],
  })
}

/** Live dates from git, or the reason there are none. */
export function gitDates(): { files?: Map<string, ContentDates>; note?: string } {
  let shallow: string
  try {
    shallow = git(['rev-parse', '--is-shallow-repository']).trim()
  } catch {
    // No git binary (the Docker UI stage), or no repository (a tarball).
    return { note: 'not a git checkout' }
  }
  // Anything but an explicit "false" — including a git too old to know the
  // flag, which echoes it back — is treated as shallow. Guessing wrong in
  // that direction costs a date; guessing wrong in the other costs the truth.
  if (shallow !== 'false') return { note: 'shallow clone' }
  // `core.quotePath=false`: otherwise a non-ASCII file name comes back
  // octal-escaped inside quotes and would never match the path build.ts asks for.
  const log = git(['-c', 'core.quotePath=false', 'log', `--format=${COMMIT}%cs`, '--name-only', '--', ...DATED_PATHS])
  return { files: parseGitLog(log) }
}

/** The manifest as written: sorted keys and no timestamp, so the same history
 *  always produces the same bytes and the Docker layer cache holds. */
export function manifestJson(files: Map<string, ContentDates>): string {
  const sorted = [...files.entries()].sort(([a], [b]) => (a < b ? -1 : a > b ? 1 : 0))
  return JSON.stringify({ version: MANIFEST_VERSION, files: Object.fromEntries(sorted) }, null, 1) + '\n'
}

/** Validates and loads a manifest. A MISSING manifest is a normal state (no
 *  dates); a CORRUPT one is our own tool's output gone wrong and throws. */
export function parseManifest(text: string, where: string): Map<string, ContentDates> {
  let data: unknown
  try {
    data = JSON.parse(text)
  } catch {
    throw new Error(`${where} — not valid JSON; regenerate it with \`bun docs/dates.ts --write\``)
  }
  const obj = data as { version?: unknown; files?: unknown }
  if (obj?.version !== MANIFEST_VERSION || typeof obj.files !== 'object' || obj.files === null) {
    throw new Error(`${where} — expected { version: ${MANIFEST_VERSION}, files: {…} }`)
  }
  const files = new Map<string, ContentDates>()
  for (const [path, v] of Object.entries(obj.files as Record<string, unknown>)) {
    const d = v as Partial<ContentDates> | null
    if (!d || typeof d.created !== 'string' || typeof d.modified !== 'string' || !ISO_DAY.test(d.created) || !ISO_DAY.test(d.modified)) {
      throw new Error(`${where} — "${path}" needs YYYY-MM-DD \`created\` and \`modified\``)
    }
    files.set(path, { created: d.created, modified: d.modified })
  }
  return files
}

/** @param askGit injectable so the tests do not depend on whether THEIR clone
 *  is shallow (a dev box is not, CI is). */
export function resolveDates(manifestPath = MANIFEST_PATH, askGit = gitDates): ResolvedDates {
  const live = askGit()
  if (live.files) return { source: 'git', files: live.files }
  if (existsSync(manifestPath)) {
    const files = parseManifest(readFileSync(manifestPath, 'utf8'), relative(REPO_ROOT, manifestPath))
    return { source: 'manifest', files, note: live.note }
  }
  return { source: 'none', files: new Map(), note: `${live.note}, and no manifest` }
}

// ── CLI ─────────────────────────────────────────────────────────────────

function main(argv: string[]): number {
  if (!argv.includes('--write')) {
    console.error('usage: bun docs/dates.ts --write   (writes ui/docs/content-dates.json)')
    return 2
  }
  const live = gitDates()
  if (!live.files) {
    // Refusing is the point. A manifest written from a shallow clone would
    // carry the HEAD date for every file, and the image would ship it.
    console.error(
      `[dates] REFUSING to write a manifest: ${live.note}. ` +
        `It needs a clone with full history (actions/checkout with fetch-depth: 0).`,
    )
    return 1
  }
  // Only files that still exist: history also names every deleted page.
  const files = new Map([...live.files].filter(([p]) => existsSync(join(REPO_ROOT, p))))
  if (files.size === 0) {
    console.error(`[dates] REFUSING: git knows no files under ${DATED_PATHS.join(', ')}`)
    return 1
  }
  writeFileSync(MANIFEST_PATH, manifestJson(files))
  const newest = [...files.values()].map((d) => d.modified).sort().at(-1)
  console.log(
    `[dates] ${files.size} files -> ${relative(REPO_ROOT, MANIFEST_PATH).split('\\').join('/')} (newest change ${newest})`,
  )
  return 0
}

// Run only as a script, never on import (build.ts and the tests import this).
// Compared case-insensitively: on Windows the drive letter's case varies.
const invoked = process.argv[1] ? resolve(process.argv[1]).toLowerCase() : ''
if (invoked === fileURLToPath(import.meta.url).toLowerCase()) process.exit(main(process.argv.slice(2)))
