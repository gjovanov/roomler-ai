#!/usr/bin/env node
// says-roomlerd.cjs — FR-133 P4's CI guard: text a person reads calls the daemon
// `roomlerd` (or "the daemon" in a sentence), never an "agent".
//
// WHY THIS EXISTS
//
//   FR-133 renamed the daemon on every surface a person reads: P1 the installers,
//   P2a its notices and the companion, P2b the web app and the server's own words,
//   P2c the viewer's text, the CLI and the LocalAPI output. Each phase left a test
//   for the files it changed. This guard holds those surfaces and the ones no test
//   reads, in CI, in seconds, with no build and no install: a NEW person-facing
//   "agent" fails the PR that adds it, whichever file it lands in.
//
//   "agent" stays only where it is a wire or operating-system identity that a rename
//   would break (docs/fr/FR-133-roomlerd-not-agent.md, "Key design"). Those spellings
//   are IDENTITIES below, each with its reason; they are taken out before the word is
//   looked for. ui/src is read by its own spec (ui/src/__tests__/saysRoomlerd.spec.ts,
//   through the TypeScript and Vue parsers) and is not read twice; --self-test holds
//   that spec's IDENTITIES and this list in agreement, and the daemon tests' kept
//   spellings too.
//
// WHAT IS READ: the text a person can meet, per kind of file (readers below)
//
//   Rust        string literals outside comments, log macros (trace! ... error!, and a
//               file's own macro_rules! that calls one) and test code (#[cfg(test)]
//               and #[test] items, *_tests.rs, tests/)
//   JS          string literals outside comments and console.* calls; ${...} is code
//   HTML        text between tags; placeholder, title, aria-label, alt; inline <script>
//   Markdown    prose; code spans and fenced code as code; comments and link targets out
//   sh, ps1     quoted strings and heredocs, outside comments; a $variable is code
//   XML (.wxs)  attribute values and text, outside comments
//   Cargo.toml  description, extended-description, summary
//   JSON        productName, title, description and the like (tauri.conf.json)
//   .desktop    Name, GenericName, Comment, Keywords; a systemd unit's Description
//   README      every line
//   release-agent.yml   the description its Windows signing step stamps on the MSI
//
// THE RULE (the UI spec's): once IDENTITIES are out, the text still says `agent` or
//   `agents` as a word (`_` is a word character, so `agent_id` never does). Prose (the
//   text of a page, a README) counts as it stands. In code, a literal with no space as
//   written and no capital ('agent', "agent-logs") is an identifier, not a phrase, and
//   does not count. The Rust tests count only a literal with a space; this guard, as the
//   UI spec does, also counts a capitalised word ("Agent", "Agents:").
//
// KEEPING ONE: when the word must stay (another program's agent, an identity not
//   listed), put a comment on the line above it, or on its own line:
//
//       // AGENT-WORD-KEPT: <why it must stay>
//
//   It covers its own line and the next line that is not blank or a comment. A marker
//   with no reason fails, and so does a marker that covers nothing. A phrase in a file
//   that cannot hold a comment (JSON) or whose comments a person reads (an installed
//   README), or one that only a log line ever carries, is listed in KEPT_PHRASES below
//   with its reason.
//
// THE BASELINE is the kill switch. scripts/says-roomlerd-baseline.txt holds the hits
//   that are known and owned: a count per FILE (never a line number, so another lane's
//   edit never moves it), grouped by surface under the owner that removes them. --check
//   fails when a file holds more than its count. A PR that only REMOVES hits never
//   fails (it is told how to lock the gain in). A count moves only with
//   --update-baseline, committed with the reason, as with scripts/name-audit.sh.
//
// USAGE
//   node scripts/says-roomlerd.cjs                    # every hit, per surface; exit 0
//   node scripts/says-roomlerd.cjs --summary          # counts per surface; exit 0
//   node scripts/says-roomlerd.cjs --check            # self-test, then fail on a hit above the baseline
//   node scripts/says-roomlerd.cjs --update-baseline  # rewrite the baseline from the tree
//   node scripts/says-roomlerd.cjs --self-test        # the readers and the rule on fixtures
//   node scripts/says-roomlerd.cjs --identities       # the kept spellings, each with its reason
//   --no-baseline                                     # with the report or --check: as if none existed

'use strict'

const { execFileSync } = require('node:child_process')
const { existsSync, readFileSync, writeFileSync } = require('node:fs')
const { join } = require('node:path')

const ROOT = join(__dirname, '..')
const BASELINE = 'scripts/says-roomlerd-baseline.txt'
const SELF = 'scripts/says-roomlerd.cjs'
const MARKER = 'AGENT-WORD-KEPT'
const plural = (k, word) => `${k} ${word}${k === 1 ? '' : 's'}`
const UI_SPEC = 'ui/src/__tests__/saysRoomlerd.spec.ts'
const DAEMON_TEST = 'agents/roomlerd/src/daemon_text_tests.rs'
const NOTICE_TEST = 'agents/roomlerd/src/notice_text_tests.rs'

const WORD = /\bagents?\b/i
const CAPITALISED = /\bAgents?\b/

/**
 * The spellings that keep the word: wire and OS identities a rename would break
 * (the spec's key-design table), each with its reason. Taken out before WORD is tried.
 */
const IDENTITIES = [
  // The UI spec's five, verbatim: --self-test holds them equal to its IDENTITIES.
  { re: /\b\w*agents?_\w*/gi, why: 'snake_case names and codes: agent_id, agent_offline (wire fields)' },
  { re: /\brc:agent\.[\w.]*/gi, why: 'the rc:agent.* message types (a wire name)' },
  { re: /\/agents?\b/gi, why: 'API path segments: /api/agent/..., /tenant/{tid}/agent/{aid} (a wire name)' },
  { re: /--agents?\b/g, why: "the roomler CLI's --agent flag" },
  { re: /\bcom\.roomler\.agent\b/gi, why: "the macOS bundle id and launchd label every Mac's TCC grants key on (an OS identity)" },
  // The daemon's (daemon_text_tests.rs KEPT): --self-test holds each of those consumed here.
  { re: /\bagent-v/gi, why: 'the agent-v* release tags every deployed updater polls (D3: kept)' },
  { re: /\brole=agent\b/gi, why: 'the /ws role (a wire name)' },
  { re: /<AGENTS?>/g, why: "the placeholder clap derives from the --agent flag's field" },
  { re: /\{agents?(?::[^}]*)?\}/g, why: "a format argument's name, not a word" },
  // FR-46's retired names. scripts/name-audit.sh holds every one of them with the reason
  // it is still real, so this guard leaves them to it. The character classes keep this
  // line from being one of the names that script counts.
  { re: /\b(?:roomler[-_]agents?|Roomler[ ]Agents?)\b/g, why: "FR-46's retired names, which scripts/name-audit.sh holds (FR-133: out of scope)" },
  // The rest of the spec's key-design table.
  { re: /\bTokenType::Agent\b/g, why: "the daemon's JWT token type (a wire name)" },
  { re: /\bLaunchAgents?\b/g, why: "Apple's own word for a per-user launchd job (an OS identity); one word, so the rule never reads it as \"agent\": listed because the spec's table lists it" },
  // Found by this guard's first read of master.
  { re: /\bSoftware\\Roomler\\Agent\b/g, why: "the HKCU key the per-user MSI's components keep their KeyPath under: Windows Installer tracks each component by it, so a rename means new component GUIDs (an OS identity no page shows)" },
  { re: /\b(?:polkit )?authentication agents?\b/gi, why: "polkit's own name for the program that shows its password prompt (another program's agent)" },
  { re: /\b(?:EDR|endpoint)(?:[ -]security)? agents?\b/gi, why: "an endpoint-security (EDR) product's agent (another program's)" },
  { re: /\bUser-Agent\b/gi, why: "the HTTP header (another program's word)" },
  { re: /\bagent sessions?\b/gi, why: "FR-90's agent sessions: an AI agent's session on a device (Hive, parked), another program's agent" },
]

/**
 * Phrases that keep the word in a file that cannot hold a marker, that a test of their
 * own already lets by, or that only a log line ever carries, each with why. An entry
 * matches by file and either the exact one-line `text` or a literal's `prefix`. One
 * that `quotes` another file's text is kept only while that file still says it. An
 * entry that no longer matches anything is a fault, as a marker that covers nothing is.
 */
const KEPT_PHRASES = [
  ...[
    '{ctx}: {}: this agent did not run for ~{} s before it (frozen or suspended)',
    '{why}: this agent did not run for ~{} s of it (frozen or suspended)',
  ].map((text) => ({
    file: 'agents/roomlerd/src/signaling.rs',
    text,
    why: "AgentDidNotRun's text: the reconnect loop logs it and nothing else reads it (daemon_text_tests.rs LOG_ONLY)",
  })),
  {
    file: 'agents/roomlerd/src/tunnel/quic_peer.rs',
    prefix: 'agent quic',
    why:
      'the error-chain tags of the daemon-side QUIC endpoint: every caller writes the error to a log line ' +
      '(tunnel/acceptor.rs, tunnel/quic_setup.rs, signaling.rs) and the wire carries CloseReason::IoError, never the text',
  },
  {
    file: 'agents/roomlerd/packaging/windows/README.txt',
    text: 'agent starting',
    quotes: { file: 'agents/roomlerd/src/app.rs', text: '"agent starting"' },
    why: "the README quotes, word for word, the log line the daemon writes as it starts (app.rs); FR-133 leaves log lines as they are",
  },
]

/** Whether KEPT_PHRASES entry `k` lets the phrase `said` in `file` by. */
function keeps(k, file, said) {
  return k.file === file && (k.text !== undefined ? said === k.text : said.startsWith(k.prefix))
}

/**
 * Where a person meets the daemon's name, the phase that renamed it, the test that
 * holds it today and the owner of whatever the baseline still holds. A file belongs to
 * the first surface whose path it starts with. `scan: false` is read elsewhere.
 */
const SURFACES = [
  {
    id: 'web-app',
    phase: 'P2b',
    owner: 'FR-133 P2b',
    paths: ['ui/src/'],
    scan: false,
    test: `${UI_SPEC} (every file, through the TypeScript and Vue parsers)`,
  },
  {
    id: 'daemon',
    phase: 'P2a, P2c',
    owner: 'FR-133 follow-up',
    ac: 'AC5b',
    paths: [
      'agents/roomlerd/src/',
      'agents/roomler-cli/src/',
      'agents/roomlerdw/src/',
      'agents/roomler-cli-shim/src/',
      'crates/agent-core/src/',
      'crates/localapi/src/',
      'crates/tunnel-core/src/',
    ],
    test: `${NOTICE_TEST}, ${DAEMON_TEST} (their own file lists, and every --help)`,
  },
  {
    id: 'companion',
    phase: 'P2a',
    owner: 'FR-133 follow-up',
    ac: 'AC5',
    paths: ['agents/roomler-desktop/src/'],
    test: 'agents/roomler-desktop/src/commands.rs (mod says_roomlerd: src/front and src/*.rs)',
  },
  {
    id: 'server',
    phase: 'P2b',
    owner: 'FR-133 follow-up',
    // Every server crate whose words reach a client: a refusal the web app or the CLI
    // shows, a page the server renders. Not crates/db or crates/config (internal).
    paths: [
      'crates/api/src/',
      'crates/core/src/',
      'crates/modules/chat/src/',
      'crates/modules/conference/src/',
      'crates/modules/fleet/src/',
      'crates/modules/network/src/',
      'crates/modules/remote/src/',
      'crates/modules/saas/src/',
      'crates/remote_control/src/',
      'crates/services/src/',
    ],
    test: 'per-message asserts in fleet hub.rs and socket.rs',
  },
  {
    id: 'install-scripts',
    phase: 'P1',
    owner: 'FR-133 follow-up',
    paths: [
      'scripts/install.sh',
      'scripts/install.ps1',
      'agents/roomlerd/packaging/linux/debian/',
      'agents/roomlerd/packaging/macos/postinstall',
    ],
    test: 'none (installer-smoke.yml checks the names, not the words)',
  },
  {
    id: 'display-identity',
    phase: 'P1',
    owner: 'FR-133 follow-up',
    paths: [
      'agents/roomlerd/wix/',
      'agents/roomlerd/wix-perMachine/',
      'agents/roomlerd/Cargo.toml',
      'agents/roomlerd/packaging/',
      'agents/roomler-desktop/Cargo.toml',
      'agents/roomler-desktop/tauri.conf.json',
      'agents/roomler-desktop/packaging/',
      '.github/workflows/release-agent.yml',
    ],
    test: 'installer-smoke.yml (the MSI names only)',
  },
  {
    id: 'wizard',
    phase: 'FR-129 C',
    owner: 'FR-129 phase C',
    ac: 'AC5',
    paths: [
      'agents/roomler-setup/src/',
      'agents/roomler-setup/Cargo.toml',
      'agents/roomler-setup/tauri.conf.json',
      'crates/roomler-setup-core/src/',
    ],
    test: 'agents/roomler-setup/src/lib.rs (index.html visible text only)',
  },
  {
    id: 'docs-site',
    phase: 'P3',
    owner: 'FR-133 P3',
    paths: ['ui/docs/content/'],
    test: 'none',
  },
]

// ─── The rule ────────────────────────────────────────────────────────────────

function oneLine(s) {
  return s.replace(/\s+/g, ' ').trim()
}

function withoutIdentities(text) {
  return IDENTITIES.reduce((s, { re }) => s.replace(re, ' '), text)
}

/**
 * A phrase a person reads: it still says "agent" once the identities are out. The space
 * is looked for in the literal as written, as the Rust tests do: an identity taken out
 * leaves a space behind, and an identifier that holds one ("roomler:agent-online:{agent_id}")
 * is still an identifier. The capital is looked for once they are out, as the UI spec does.
 */
function saysAgent(text, prose) {
  const t = withoutIdentities(text)
  if (!WORD.test(t)) return false
  return prose || /\s/.test(text.trim()) || CAPITALISED.test(t)
}

// ─── The readers ─────────────────────────────────────────────────────────────
// Each returns [{ text, at, prose, skip }]: `at` is the offset of the text in the
// file, `prose` that it counts as it stands, `skip` why no person reads it (a log
// line, a console call, test code), or undefined.

const LOG_MACROS = ['trace!', 'debug!', 'info!', 'warn!', 'error!']
const TEST_ATTR = /^#\[\s*(?:cfg\s*\(\s*(?:test\s*\)|all\s*\(\s*test\b)|test\s*\]|tokio::test\b)/

/** `code` with its comments, string literals and char literals blanked (same length, newlines kept). */
function maskRust(code) {
  const n = code.length
  const out = code.split('')
  const blank = (a, b) => {
    for (let k = a; k < b && k < n; k++) if (out[k] !== '\n') out[k] = ' '
  }
  let i = 0
  while (i < n) {
    const c = code[i]
    if (c === '/' && code[i + 1] === '/') {
      const e = code.indexOf('\n', i)
      const end = e < 0 ? n : e
      blank(i, end)
      i = end
    } else if (c === '/' && code[i + 1] === '*') {
      let d = 1
      let j = i + 2
      while (j < n && d > 0) {
        if (code.startsWith('/*', j)) (d += 1), (j += 2)
        else if (code.startsWith('*/', j)) (d -= 1), (j += 2)
        else j += 1
      }
      blank(i, j)
      i = j
    } else if (c === 'r' || c === 'b') {
      // r"..", r#".."#, br"..", b"..": a raw or byte string, unless this is the tail of a longer word.
      const m = /^b?r(#*)"/.exec(code.slice(i, i + 40))
      if (m && !/[A-Za-z0-9_]/.test(code[i - 1] ?? '')) {
        const close = '"' + m[1]
        const e = code.indexOf(close, i + m[0].length)
        const end = e < 0 ? n : e + close.length
        blank(i, end)
        i = end
      } else i += 1
    } else if (c === '"') {
      let j = i + 1
      while (j < n && code[j] !== '"') j += code[j] === '\\' ? 2 : 1
      blank(i, j + 1)
      i = j + 1
    } else if (c === "'") {
      // A char literal ('"', '{', '\'') blanks; a lifetime ('a) does not.
      if (code[i + 1] === '\\') {
        const e = code.indexOf("'", i + 3)
        const end = e < 0 ? n : e + 1
        blank(i, end)
        i = end
      } else if (code[i + 2] === "'") {
        blank(i, i + 3)
        i += 3
      } else i += 1
    } else i += 1
  }
  return out.join('')
}

const LOG_CALL = new RegExp(`(?<![A-Za-z0-9_])(?:${LOG_MACROS.map((m) => m.slice(0, -1)).join('|')})!\\s*[(\\[{]`)

/** The macros whose text a person can meet: a body that calls one is no log macro. */
const PERSON_MACROS = ['println', 'print', 'eprintln', 'eprint', 'format', 'format_args', 'write', 'writeln', 'bail', 'anyhow', 'panic', 'ensure']
const PERSON_CALL = new RegExp(`(?<![A-Za-z0-9_])(?:${PERSON_MACROS.join('|')})!\\s*[(\\[{]`)

/**
 * The names (with their `!`) of the `macro_rules!` of THIS file whose body calls a log macro, or
 * another such macro of the file, and calls no person-facing macro (PERSON_MACROS, or a local macro
 * that does): a file's own log wrapper, such as the network module's `miss_line!`. Rust's `macro_rules!` scope is textual, so the list is per file. Out of scope: a
 * `#[macro_export]` macro that another file calls, whose name this reader cannot know there.
 */
function localLogMacros(code) {
  const masked = maskRust(code)
  const defs = []
  const re = /(?<![A-Za-z0-9_])macro_rules!\s*([A-Za-z_][A-Za-z0-9_]*)\s*([({[])/g
  for (let m; (m = re.exec(masked)); ) {
    let d = 1
    let j = re.lastIndex
    while (j < masked.length && d > 0) {
      const c = masked[j]
      if (c === '(' || c === '[' || c === '{') d += 1
      else if (c === ')' || c === ']' || c === '}') d -= 1
      j += 1
    }
    defs.push({ name: `${m[1]}!`, body: masked.slice(re.lastIndex, j) })
  }
  const calls = (body, name) => new RegExp(`(?<![A-Za-z0-9_])${name}\\s*[(\\[{]`).test(body)
  // Fail closed: a macro whose body ALSO calls a person-facing macro (or a local one that does) is not
  // a log macro, even though it logs: `{ tracing::warn!($($t)+); bail!($($t)+) }` hands the same text
  // to a person, and its literal has to stay in the guard's sight.
  const person = new Set()
  for (let grew = true; grew; ) {
    grew = false
    for (const { name, body } of defs) {
      if (person.has(name)) continue
      if (PERSON_CALL.test(body) || [...person].some((o) => calls(body, o))) person.add(name), (grew = true)
    }
  }
  const names = new Set()
  for (let grew = true; grew; ) {
    grew = false
    for (const { name, body } of defs) {
      if (names.has(name) || person.has(name)) continue
      if (LOG_CALL.test(body) || [...names].some((o) => calls(body, o))) names.add(name), (grew = true)
    }
  }
  return [...names]
}

/**
 * Every string literal of Rust `code`: P2a's lexer (notice_text_tests.rs `literals`),
 * plus nested block comments, byte raw strings and test-only items.
 */
function rustLiterals(code) {
  const out = []
  const n = code.length
  let i = 0
  let depth = 0
  let logAt = null
  let logOwn = false // the log macro being read is this file's own wrapper, not one of the five
  let attr = null // an attribute being read: { depth, test }
  let test = null // a test-only item being read: { depth, body }
  const wordChar = (c) => c !== undefined && /[A-Za-z0-9_]/.test(c)
  const ownLogs = localLogMacros(code) // this file's own wrappers of the five (a literal inside one is a log line too)
  const push = (start, end) =>
    out.push({
      text: code.slice(start, end),
      at: start,
      prose: false,
      skip: test ? 'test code' : logAt !== null ? 'a log line' : undefined,
      own: !test && logAt !== null && logOwn,
    })
  while (i < n) {
    const c = code[i]
    if (c === '/' && code[i + 1] === '/') {
      const e = code.indexOf('\n', i)
      i = e < 0 ? n : e
    } else if (c === '/' && code[i + 1] === '*') {
      let d = 1
      i += 2
      while (i < n && d > 0) {
        if (code.startsWith('/*', i)) (d += 1), (i += 2)
        else if (code.startsWith('*/', i)) (d -= 1), (i += 2)
        else i += 1
      }
    } else if (c === '#' && (code[i + 1] === '[' || (code[i + 1] === '!' && code[i + 2] === '['))) {
      if (attr === null && test === null) attr = { depth, test: TEST_ATTR.test(code.slice(i, i + 40)) }
      i += 1
    } else if (c === '(' || c === '[' || c === '{') {
      if (logAt === null) {
        if (LOG_MACROS.some((m) => code.endsWith(m, i))) (logAt = depth), (logOwn = false)
        else if (ownLogs.some((m) => code.endsWith(m, i) && !wordChar(code[i - m.length - 1]))) (logAt = depth), (logOwn = true)
      }
      if (c === '{' && test && !test.body && depth === test.depth) test.body = true
      depth += 1
      i += 1
    } else if (c === ')' || c === ']' || c === '}') {
      depth = Math.max(0, depth - 1)
      if (logAt === depth) logAt = null
      if (test && (depth < test.depth || (c === '}' && test.body && depth === test.depth))) test = null
      if (attr && c === ']' && depth === attr.depth) {
        if (attr.test) test = { depth, body: false }
        attr = null
      }
      i += 1
    } else if ((c === ';' || c === ',') && test && !test.body && depth === test.depth) {
      test = null
      i += 1
    } else if (c === "'") {
      // A char literal ('"', '\'') or a lifetime ('a).
      if (code[i + 1] === '\\') {
        const e = code.indexOf("'", i + 3)
        i = e < 0 ? n : e + 1
      } else {
        const cp = code.codePointAt(i + 1)
        const w = cp !== undefined && cp > 0xffff ? 2 : 1
        i += code[i + 1 + w] === "'" ? w + 2 : 1
      }
    } else if (
      (c === 'r' || (c === 'b' && code[i + 1] === 'r')) &&
      !wordChar(code[i - 1]) &&
      /^b?r#*"/.test(code.slice(i, i + 40))
    ) {
      // A raw string: r"…", r#"…"#, br"…".
      const open = code.indexOf('"', i)
      const hashes = open - i - (c === 'b' ? 2 : 1)
      const close = '"' + '#'.repeat(hashes)
      const e = code.indexOf(close, open + 1)
      const end = e < 0 ? n : e
      push(open + 1, end)
      i = end + close.length
    } else if (c === '"') {
      const start = i + 1
      i += 1
      while (i < n && code[i] !== '"') i += code[i] === '\\' ? 2 : 1
      push(start, Math.min(i, n))
      i += 1
    } else {
      i += 1
    }
  }
  return out
}

const REGEX_BEFORE = /[(,=:[!&|?{};+\-*%<>~^]/

/**
 * Every string literal of JS `code`, outside comments, each `${…}` taken out (a
 * template's interpolation is code): the companion's `js_strings`, plus regex
 * literals stepped over and console.* arguments marked.
 */
function jsStrings(code) {
  const out = []
  const n = code.length
  let i = 0
  let depth = 0
  let consoleAt = null
  let before = ''
  while (i < n) {
    const c = code[i]
    if (c === '/' && code[i + 1] === '/') {
      const e = code.indexOf('\n', i)
      i = e < 0 ? n : e
    } else if (c === '/' && code[i + 1] === '*') {
      const e = code.indexOf('*/', i + 2)
      i = e < 0 ? n : e + 2
    } else if (c === '/' && (before === '' || REGEX_BEFORE.test(before))) {
      let j = i + 1
      let inClass = false
      while (j < n && code[j] !== '\n') {
        if (code[j] === '\\') j += 2
        else if (code[j] === '[') (inClass = true), (j += 1)
        else if (code[j] === ']') (inClass = false), (j += 1)
        else if (code[j] === '/' && !inClass) break
        else j += 1
      }
      i = j + 1
      while (i < n && /[a-z]/i.test(code[i])) i += 1
      before = '/'
    } else if (c === '"' || c === "'" || c === '`') {
      const start = i + 1
      let j = start
      let seg = start
      let shown = ''
      while (j < n && code[j] !== c) {
        if (code[j] === '\\') {
          j += 2
        } else if (c === '`' && code[j] === '$' && code[j + 1] === '{') {
          shown += code.slice(seg, j)
          let d = 1
          j += 2
          while (j < n && d > 0) {
            if (code[j] === '{') d += 1
            else if (code[j] === '}') d -= 1
            j += 1
          }
          seg = j
        } else if (c !== '`' && code[j] === '\n') {
          break
        } else {
          j += 1
        }
      }
      shown += code.slice(seg, Math.min(j, n))
      out.push({ text: shown, at: start, prose: false, skip: consoleAt !== null ? 'a console call' : undefined })
      i = j + 1
      before = c
    } else if (c === '(' || c === '[' || c === '{') {
      if (c === '(' && consoleAt === null && /\bconsole\s*\.\s*\w+\s*$/.test(code.slice(Math.max(0, i - 40), i))) {
        consoleAt = depth
      }
      depth += 1
      i += 1
      before = c
    } else if (c === ')' || c === ']' || c === '}') {
      depth = Math.max(0, depth - 1)
      if (consoleAt === depth) consoleAt = null
      i += 1
      before = c
    } else {
      if (!/\s/.test(c)) before = c
      i += 1
    }
  }
  return out
}

const HTML_SHOWN_ATTRS = ['placeholder', 'title', 'aria-label', 'alt']

/**
 * What a person meets in `html`: the text between tags, and what an element shows or
 * speaks (the companion's `html_words`). Comments and styles are not shown; an inline
 * script is read as JS.
 */
function htmlWords(html) {
  const out = []
  const n = html.length
  let i = 0
  while (i < n) {
    if (html.startsWith('<!--', i)) {
      const e = html.indexOf('-->', i + 4)
      i = e < 0 ? n : e + 3
    } else if (html[i] === '<') {
      const e = html.indexOf('>', i)
      const inside = html.slice(i + 1, e < 0 ? n : e)
      for (const name of HTML_SHOWN_ATTRS) {
        const m = new RegExp(`\\s${name}\\s*=\\s*(?:"([^"]*)"|'([^']*)')`, 'i').exec(inside)
        if (m) out.push({ text: m[1] ?? m[2], at: i + 1 + m.index, prose: true })
      }
      i = e < 0 ? n : e + 1
      const tag = (/^([A-Za-z][\w-]*)/.exec(inside) || [])[1]
      const name = tag ? tag.toLowerCase() : ''
      if (name === 'script' || name === 'style') {
        const close = html.toLowerCase().indexOf(`</${name}`, i)
        const end = close < 0 ? n : close
        if (name === 'script' && !/\ssrc\s*=/i.test(inside)) {
          for (const s of jsStrings(html.slice(i, end))) out.push({ ...s, at: s.at + i })
        }
        i = end
      }
    } else {
      const e = html.indexOf('<', i)
      const end = e < 0 ? n : e
      out.push({ text: html.slice(i, end), at: i, prose: true })
      i = end
    }
  }
  return out
}

/** The text of a Markdown page: prose as it stands; code spans, fences and front matter as code. */
function mdText(md) {
  const out = []
  const lines = md.split('\n')
  let at = 0
  let fence = null
  let front = false
  let comment = false
  lines.forEach((line, k) => {
    const start = at
    at += line.length + 1
    if (k === 0 && line.trim() === '---') {
      front = true
      return
    }
    if (front) {
      if (line.trim() === '---') front = false
      else {
        const m = /^\s*[\w.-]+\s*:\s*(.+)$/.exec(line)
        if (m) out.push({ text: m[1].replace(/^['"]|['"]$/g, ''), at: start, prose: false })
      }
      return
    }
    const f = /^\s*(`{3,}|~{3,})/.exec(line)
    if (fence) {
      if (f && f[1][0] === fence[0] && f[1].length >= fence.length) fence = null
      else out.push({ text: line, at: start, prose: false })
      return
    }
    if (f) {
      fence = f[1]
      return
    }
    let text = line
    if (comment) {
      const e = text.indexOf('-->')
      if (e < 0) return
      text = text.slice(e + 3)
      comment = false
    }
    text = text.replace(/<!--[\s\S]*?-->/g, ' ')
    const open = text.indexOf('<!--')
    if (open >= 0) {
      text = text.slice(0, open)
      comment = true
    }
    text = text.replace(/(`+)([\s\S]*?)\1/g, (_, _ticks, code) => {
      out.push({ text: code, at: start, prose: false })
      return ' '
    })
    text = text
      .replace(/\]\([^)]*\)/g, ']')
      .replace(/<https?:[^>]*>/g, ' ')
      .replace(/(:[\w-]+)\{[^}]*\}/g, '$1')
    if (text.trim()) out.push({ text, at: start, prose: true })
  })
  return out
}

/** Skip a balanced `( … )` from the `(` at `i`, stepping over quotes; the index after it. */
function skipParens(code, i) {
  let d = 0
  while (i < code.length) {
    const c = code[i]
    if (c === '\\') i += 2
    else if (c === "'" || c === '"') {
      const e = code.indexOf(c, i + 1)
      i = e < 0 ? code.length : e + 1
    } else {
      if (c === '(') d += 1
      else if (c === ')' && --d === 0) return i + 1
      i += 1
    }
  }
  return i
}

/** The text a shell script prints: quoted strings and heredocs, outside comments. */
function shStrings(code) {
  const out = []
  const n = code.length
  let i = 0
  const heredocs = []
  const unvar = (s) => s.replace(/\$\{[^}]*\}|\$\w+/g, ' ')
  while (i < n) {
    const c = code[i]
    if (c === '\n' && heredocs.length) {
      i += 1
      for (const h of heredocs) {
        while (i < n) {
          const e = code.indexOf('\n', i)
          const line = code.slice(i, e < 0 ? n : e)
          const lineAt = i
          i = e < 0 ? n : e + 1
          if ((h.strip ? line.replace(/^\t+/, '') : line) === h.word) break
          out.push({ text: h.quoted ? line : unvar(line), at: lineAt, prose: false })
        }
      }
      heredocs.length = 0
    } else if (c === '#' && (i === 0 || /[\s;|&(]/.test(code[i - 1]))) {
      const e = code.indexOf('\n', i)
      i = e < 0 ? n : e
    } else if (c === '<' && code[i + 1] === '<' && code[i + 2] !== '<') {
      const m = /^<<(-?)\s*(['"]?)([A-Za-z_]\w*)\2/.exec(code.slice(i, i + 80))
      if (m) {
        heredocs.push({ word: m[3], strip: m[1] === '-', quoted: m[2] !== '' })
        i += m[0].length
      } else i += 2
    } else if (c === "'") {
      const e = code.indexOf("'", i + 1)
      const end = e < 0 ? n : e
      out.push({ text: code.slice(i + 1, end), at: i + 1, prose: false })
      i = end + 1
    } else if (c === '"') {
      let j = i + 1
      let seg = j
      let shown = ''
      while (j < n && code[j] !== '"') {
        if (code[j] === '\\') j += 2
        else if (code[j] === '$' && code[j + 1] === '(') {
          shown += code.slice(seg, j) + ' '
          j = skipParens(code, j + 1)
          seg = j
        } else if (code[j] === '`') {
          shown += code.slice(seg, j) + ' '
          const e = code.indexOf('`', j + 1)
          j = e < 0 ? n : e + 1
          seg = j
        } else j += 1
      }
      shown += code.slice(seg, Math.min(j, n))
      out.push({ text: unvar(shown), at: i + 1, prose: false })
      i = j + 1
    } else if (c === '\\') {
      i += 2
    } else {
      i += 1
    }
  }
  return out
}

/** The text a PowerShell script prints: strings and here-strings, outside comments. */
function ps1Strings(code) {
  const out = []
  const n = code.length
  let i = 0
  const unvar = (s) => s.replace(/\$\{[^}]*\}|\$[\w:]+/g, ' ')
  while (i < n) {
    const c = code[i]
    if (c === '<' && code[i + 1] === '#') {
      const e = code.indexOf('#>', i + 2)
      i = e < 0 ? n : e + 2
    } else if (c === '#') {
      const e = code.indexOf('\n', i)
      i = e < 0 ? n : e
    } else if (c === '@' && (code[i + 1] === '"' || code[i + 1] === "'") && code[i + 2] === '\n') {
      const q = code[i + 1]
      const close = code.indexOf(`\n${q}@`, i + 2)
      const end = close < 0 ? n : close
      let at = i + 3
      for (const line of code.slice(i + 3, end).split('\n')) {
        out.push({ text: q === '"' ? unvar(line) : line, at, prose: false })
        at += line.length + 1
      }
      i = end + 3
    } else if (c === "'") {
      let j = i + 1
      while (j < n && !(code[j] === "'" && code[j + 1] !== "'")) j += code[j] === "'" ? 2 : 1
      out.push({ text: code.slice(i + 1, j).replace(/''/g, "'"), at: i + 1, prose: false })
      i = j + 1
    } else if (c === '"') {
      let j = i + 1
      let seg = j
      let shown = ''
      while (j < n && code[j] !== '"') {
        if (code[j] === '`') j += 2
        else if (code[j] === '$' && code[j + 1] === '(') {
          shown += code.slice(seg, j) + ' '
          j = skipParens(code, j + 1)
          seg = j
        } else j += 1
      }
      shown += code.slice(seg, Math.min(j, n))
      out.push({ text: unvar(shown), at: i + 1, prose: false })
      i = j + 1
    } else {
      i += 1
    }
  }
  return out
}

/** XML (a .wxs, a .plist): attribute values and text, outside comments and declarations. */
function xmlText(xml) {
  const out = []
  const n = xml.length
  let i = 0
  while (i < n) {
    if (xml.startsWith('<!--', i)) {
      const e = xml.indexOf('-->', i + 4)
      i = e < 0 ? n : e + 3
    } else if (xml.startsWith('<?', i) || xml.startsWith('<!', i)) {
      const e = xml.indexOf('>', i)
      i = e < 0 ? n : e + 1
    } else if (xml[i] === '<') {
      const e = xml.indexOf('>', i)
      const end = e < 0 ? n : e
      const re = /[\w:.-]+\s*=\s*(?:"([^"]*)"|'([^']*)')/g
      const inside = xml.slice(i, end)
      let m
      while ((m = re.exec(inside))) out.push({ text: m[1] ?? m[2], at: i + m.index, prose: false })
      i = end + 1
    } else {
      const e = xml.indexOf('<', i)
      const end = e < 0 ? n : e
      out.push({ text: xml.slice(i, end), at: i, prose: false })
      i = end
    }
  }
  return out
}

/** Keys whose value a person reads in a package's metadata. */
const SHOWN_KEYS = /^(description|extended-description|long-description|summary|productName|title|shortDescription|longDescription|tooltip)$/

/** Cargo.toml: the values of SHOWN_KEYS (`"…"`, `'…'`, `"""…"""`). */
function tomlText(toml) {
  const out = []
  const re = /^[ \t]*([\w-]+)[ \t]*=[ \t]*("""[\s\S]*?"""|"(?:[^"\\\n]|\\.)*"|'[^'\n]*')/gm
  let m
  while ((m = re.exec(toml))) {
    if (!SHOWN_KEYS.test(m[1])) continue
    const v = m[2]
    const q = v.startsWith('"""') ? 3 : 1
    out.push({ text: v.slice(q, v.length - q), at: m.index + m[0].indexOf(v) + q, prose: false })
  }
  return out
}

/** JSON: the string values of SHOWN_KEYS, at their first spot in the file. */
function jsonText(json) {
  const out = []
  let doc
  try {
    doc = JSON.parse(json)
  } catch {
    return out
  }
  const walk = (v) => {
    if (Array.isArray(v)) v.forEach(walk)
    else if (v && typeof v === 'object')
      for (const [k, x] of Object.entries(v)) {
        if (typeof x === 'string' && SHOWN_KEYS.test(k)) {
          const at = json.indexOf(JSON.stringify(x).slice(1, -1))
          out.push({ text: x, at: Math.max(0, at), prose: false })
        } else walk(x)
      }
  }
  walk(doc)
  return out
}

/** `key=value` files: the values of `keys` (a .desktop entry, a systemd unit). */
function iniText(keys) {
  return (text) => {
    const out = []
    const re = new RegExp(`^[ \\t]*(?:${keys})(?:\\[[^\\]]*\\])?[ \\t]*=(.*)$`, 'gm')
    let m
    while ((m = re.exec(text))) out.push({ text: m[1], at: m.index + m[0].indexOf('=') + 1, prose: false })
    return out
  }
}

/** A README: every line, as prose. */
function textLines(text) {
  const out = []
  let at = 0
  for (const line of text.split('\n')) {
    if (line.trim()) out.push({ text: line, at, prose: true })
    at += line.length + 1
  }
  return out
}

/** A workflow: the `description:` its Windows signing step stamps on what it signs. */
function signingText(yml) {
  const out = []
  let signing = false
  let at = 0
  for (const line of yml.split('\n')) {
    if (/^\s*-\s+name:/.test(line)) signing = false
    if (/^\s*uses:\s*\.\/\.github\/actions\/sign-/.test(line)) signing = true
    const m = signing && /^(\s*description:\s*)(['"]?)(.*?)\2\s*$/.exec(line)
    if (m) out.push({ text: m[3], at: at + m[1].length + m[2].length, prose: false })
    at += line.length + 1
  }
  return out
}

/** The reader for a file, and the comment shapes a marker may sit in, or null: not read. */
function kindOf(file) {
  const base = file.slice(file.lastIndexOf('/') + 1)
  const slash = { comment: /^(\/\/|\/\*|\*)/ }
  const hash = { comment: /^#(?![[!])/ }
  const tag = { comment: /^(<!--|-->)/ }
  if (file === '.github/workflows/release-agent.yml') return { name: 'signing', read: signingText, ...hash }
  if (file.endsWith('.rs')) return { name: 'rust', read: rustLiterals, ...slash }
  if (/\.(js|mjs|cjs)$/.test(file)) return { name: 'js', read: jsStrings, ...slash }
  if (/\.html?$/.test(file)) return { name: 'html', read: htmlWords, ...tag }
  if (file.endsWith('.md')) return { name: 'md', read: mdText, ...tag }
  if (file.endsWith('.ps1') || file.endsWith('.psm1')) return { name: 'ps1', read: ps1Strings, comment: /^(#|<#)/ }
  if (file.endsWith('.sh') || /\/debian\/(preinst|postinst|prerm|postrm|config)$/.test(file) || /\/(preinstall|postinstall)$/.test(file)) {
    return { name: 'sh', read: shStrings, ...hash }
  }
  if (/\.(wxs|wxl|wxi|plist|xml)$/.test(file)) return { name: 'xml', read: xmlText, ...tag }
  if (base === 'Cargo.toml') return { name: 'toml', read: tomlText, ...hash }
  if (file.endsWith('.json')) return { name: 'json', read: jsonText, comment: /^$/ }
  if (file.endsWith('.desktop')) return { name: 'desktop', read: iniText('Name|GenericName|Comment|Keywords'), ...hash }
  if (/\.(service|timer|path|socket)$/.test(file)) return { name: 'systemd', read: iniText('Description'), ...hash }
  if (/^README/i.test(base) || file.endsWith('.txt')) return { name: 'text', read: textLines, comment: /^$/ }
  return null
}

/** Test code by its file: `*_tests.rs`, `tests.rs`, a `tests/` or `benches/` tree. */
function isTestFile(file) {
  return /(^|\/)(tests?|[\w-]+_tests?)\.rs$/.test(file) || /(^|\/)(tests|benches|examples|fixtures|testdata)\//.test(file)
}

/** The files `#[cfg(test)] mod x;` declares in `code` (of `file`): test code too. */
function testModFiles(file, code) {
  const dir = file.slice(0, file.lastIndexOf('/'))
  const base = file.slice(file.lastIndexOf('/') + 1, -3)
  const own = /^(mod|lib|main)$/.test(base) ? dir : `${dir}/${base}`
  const out = []
  const re = /#\[cfg\(test\)\]\s*(?:#\[path\s*=\s*"([^"]+)"\]\s*)?(?:pub(?:\([^)]*\))?\s+)?mod\s+(\w+)\s*;/g
  let m
  while ((m = re.exec(code))) {
    if (m[1]) out.push(`${dir}/${m[1]}`)
    else out.push(`${own}/${m[2]}.rs`, `${own}/${m[2]}/mod.rs`)
  }
  return out
}

// ─── Reading the tree ────────────────────────────────────────────────────────

function lineStarts(text) {
  const starts = [0]
  for (let i = 0; i < text.length; i++) if (text[i] === '\n') starts.push(i + 1)
  return starts
}

function lineOf(starts, at) {
  let lo = 0
  let hi = starts.length - 1
  while (lo < hi) {
    const mid = (lo + hi + 1) >> 1
    if (starts[mid] <= at) lo = mid
    else hi = mid - 1
  }
  return lo + 1
}

/**
 * The hits and the marker faults of one file's `text`: each hit { file, line, text },
 * each fault { file, line, why }.
 */
function scanText(file, text, kind) {
  const starts = lineStarts(text)
  const lines = text.split('\n')
  const hits = []
  const wrapped = [] // lines of a literal that says "agent" and is skipped only as a log line of the file's own macro
  for (const item of kind.read(text)) {
    if (item.skip && item.own && saysAgent(item.text, item.prose)) wrapped.push(lineOf(starts, item.at))
    if (item.skip || !saysAgent(item.text, item.prose)) continue
    const said = oneLine(item.text)
    if (KEPT_PHRASES.some((k) => keeps(k, file, said))) continue
    hits.push({ file, line: lineOf(starts, item.at), text: said })
  }
  // Markers: each covers its own line and the next line that is neither blank nor a comment.
  const faults = []
  const markers = []
  lines.forEach((line, k) => {
    const at = line.indexOf(MARKER)
    if (at < 0) return
    const m = /^:\s*(.*)$/.exec(line.slice(at + MARKER.length))
    const why = m ? m[1].replace(/\s*(-->|\*\/|#>)\s*$/, '').trim() : ''
    if (!/\S+\s+\S+/.test(why)) {
      faults.push({ file, line: k + 1, why: `${MARKER} needs a reason after its colon ("${MARKER}: <why it must stay>")` })
      return
    }
    let next = k + 1
    while (next < lines.length && (!lines[next].trim() || kind.comment.test(lines[next].trim()))) next += 1
    markers.push({ line: k + 1, covers: [k + 1, next + 1], used: false })
  })
  const kept = hits.filter((h) => {
    const m = markers.find((mk) => mk.covers.includes(h.line))
    if (m) m.used = true
    return !m
  })
  // A marker on a log line of the file's own macro is true: it says the wording stays for a person
  // who reads the log, and the line is no longer a hit only because the macro is read as a log.
  for (const m of markers) if (wrapped.some((l) => m.covers.includes(l))) m.used = true
  for (const m of markers.filter((mk) => !mk.used)) {
    faults.push({ file, line: m.line, why: `${MARKER} covers no "agent" (its line and the next): remove the marker` })
  }
  return { hits: kept, faults }
}

function gitFiles() {
  const out = execFileSync('git', ['ls-files', '-z'], { cwd: ROOT, maxBuffer: 256 * 1024 * 1024 })
  // Mid-merge, ls-files lists a conflicted file once per stage (up to 3): read it once.
  return [...new Set(out.toString('utf8').split('\0').filter(Boolean))]
}

function surfaceOf(file) {
  return SURFACES.find((s) => s.paths.some((p) => (p.endsWith('/') ? file.startsWith(p) : file === p)))
}

/** Every hit and fault in the tree, and the files read per surface. */
function scanTree() {
  const files = gitFiles()
  const read = new Map(SURFACES.map((s) => [s.id, 0]))
  const testFiles = new Set()
  const texts = []
  for (const file of files) {
    const surface = surfaceOf(file)
    if (!surface || surface.scan === false) continue
    const kind = kindOf(file)
    if (!kind) continue
    let text
    try {
      text = readFileSync(join(ROOT, file), 'utf8').replace(/^﻿/, '').replace(/\r\n/g, '\n')
    } catch {
      continue
    }
    read.set(surface.id, read.get(surface.id) + 1)
    if (kind.name === 'rust') for (const t of testModFiles(file, text)) testFiles.add(t)
    texts.push({ file, text, kind, surface })
  }
  const hits = []
  const faults = []
  for (const { file, text, kind, surface } of texts) {
    if (kind.name === 'rust' && (isTestFile(file) || testFiles.has(file) || /#!\[cfg\(test\)\]/.test(text))) continue
    if (!/agent/i.test(text)) continue
    const r = scanText(file, text, kind)
    for (const h of r.hits) hits.push({ ...h, surface: surface.id })
    faults.push(...r.faults)
  }
  for (const k of KEPT_PHRASES) {
    const t = texts.find((x) => x.file === k.file)
    const there = t && t.kind.read(t.text).some((item) => keeps(k, k.file, oneLine(item.text)))
    const name = k.text !== undefined ? `"${k.text}"` : `the literals that start "${k.prefix}"`
    if (!there) {
      faults.push({ file: k.file, line: 0, why: `KEPT_PHRASES lists ${name}, which is no longer there: remove it from ${SELF}` })
    } else if (k.quotes) {
      let quoted = ''
      try {
        quoted = readFileSync(join(ROOT, k.quotes.file), 'utf8')
      } catch {}
      if (!quoted.includes(k.quotes.text)) {
        faults.push({
          file: k.file,
          line: 0,
          why: `this file quotes ${k.quotes.text} from ${k.quotes.file}, which no longer says it: quote what it says now, and update the KEPT_PHRASES entry in ${SELF}`,
        })
      }
    }
  }
  return { hits, faults, read }
}

// ─── The baseline ────────────────────────────────────────────────────────────

/** A pointer to a hit for the baseline's reader: its words around the first "agent". */
function hint(text) {
  const t = oneLine(withoutIdentities(text))
  const m = WORD.exec(t)
  const from = Math.max(0, (m ? m.index : 0) - 50)
  const s = t.slice(from, from + 110)
  return (from > 0 ? '…' : '') + s + (from + 110 < t.length ? '…' : '')
}

function readBaseline() {
  const counts = new Map()
  const hints = new Map()
  if (!existsSync(join(ROOT, BASELINE))) return null
  let last = null
  for (const line of readFileSync(join(ROOT, BASELINE), 'utf8').replace(/\r\n/g, '\n').split('\n')) {
    const row = /^(\d+)\t(\S.*)$/.exec(line)
    if (row) {
      last = row[2]
      counts.set(last, Number(row[1]))
      hints.set(last, [])
      continue
    }
    const h = /^#\t"(.*)"$/.exec(line)
    if (h && last) hints.get(last).push(h[1])
  }
  return { counts, hints }
}

function writeBaseline(hits) {
  const byFile = new Map()
  for (const h of hits) {
    if (!byFile.has(h.file)) byFile.set(h.file, [])
    byFile.get(h.file).push(h)
  }
  const out = [
    '# FR-133 P4 — the baseline of scripts/says-roomlerd.cjs: the person-facing "agent"s that are',
    '# known and owned, as a count per FILE (never a line number), grouped by surface under the owner',
    '# that removes them. `--check` fails when a file holds more than its count; a PR that only removes',
    '# them never fails. A count moves only with `node scripts/says-roomlerd.cjs --update-baseline`,',
    '# committed with the reason. The quoted lines under a row are the words around each hit when it',
    '# was written (identities left out): a pointer for the owner, not part of the check.',
    '#',
  ]
  for (const s of SURFACES.filter((x) => x.scan !== false)) {
    const files = [...byFile.keys()].filter((f) => surfaceOf(f) === s).sort()
    const n = files.reduce((a, f) => a + byFile.get(f).length, 0)
    out.push(`## ${s.id} · owner ${s.owner} · ${plural(files.length, 'file')} · ${plural(n, 'hit')}`)
    for (const f of files) {
      const list = byFile.get(f).sort((a, b) => a.line - b.line)
      out.push(`${list.length}\t${f}`)
      for (const h of list) out.push(`#\t"${hint(h.text).replace(/"/g, "'")}"`)
    }
    out.push('#')
  }
  writeFileSync(join(ROOT, BASELINE), out.join('\n') + '\n')
}

// ─── Output ──────────────────────────────────────────────────────────────────

const FIXES = [
  'Fix each one, either way:',
  '  1. Reword it: `roomlerd`, "the daemon", or "the device" (the machine it runs on).',
  `  2. If the word must stay (a wire or OS identity, another program's agent), mark it in a`,
  "     comment on the line above (or at the end of its own line), in the file's comment syntax:",
  `         // ${MARKER}: <why it must stay>        (# in sh, ps1, TOML; <!-- --> in HTML, XML, Markdown)`,
  `     A phrase in JSON, in an installed README or in an error only a log line carries goes in`,
  `     KEPT_PHRASES in ${SELF} instead, with its reason.`,
  `The kept spellings, each with its reason: node ${SELF} --identities. Every hit: node ${SELF}.`,
  'Why: FR-133 (docs/fr/FR-133-roomlerd-not-agent.md): the daemon has one name a person reads.',
]

function annotate(file, line, msg) {
  if (process.env.GITHUB_ACTIONS === 'true') {
    console.log(`::error file=${file}${line ? `,line=${line}` : ''}::${msg.replace(/\r?\n/g, ' ')}`)
  }
}

function summaryRows(hits, read, baseline) {
  return SURFACES.map((s) => {
    if (s.scan === false) return { s, files: '-', hits: '-', base: '-', note: `held by ${s.test}` }
    const mine = hits.filter((h) => h.surface === s.id)
    const base = baseline
      ? [...baseline.counts].filter(([f]) => surfaceOf(f) === s).reduce((a, [, c]) => a + c, 0)
      : 0
    return { s, files: String(read.get(s.id)), hits: String(mine.length), base: baseline ? String(base) : '-', note: `test: ${s.test}` }
  })
}

function printSummary(hits, read, baseline) {
  const rows = summaryRows(hits, read, baseline)
  console.log('surface            phase      files  hits  baseline  owner')
  for (const r of rows) {
    console.log(
      `${r.s.id.padEnd(18)} ${r.s.phase.padEnd(10)} ${r.files.padStart(5)} ${r.hits.padStart(5)} ${r.base.padStart(9)}  ${r.s.owner}${r.s.ac ? ` (${r.s.ac})` : ''}`,
    )
  }
}

function report(hits, faults, read) {
  for (const s of SURFACES) {
    const mine = hits.filter((h) => h.surface === s.id)
    console.log(`\n== ${s.id} (${s.phase}; owner ${s.owner}${s.ac ? `; ${s.ac}` : ''}) ${s.scan === false ? `not read here: held by ${s.test}` : `${plural(read.get(s.id), 'file')} read, ${plural(mine.length, 'hit')}`}`)
    for (const h of mine) console.log(`${h.file}:${h.line}: "${h.text.length > 200 ? h.text.slice(0, 200) + '…' : h.text}"`)
  }
  if (faults.length) {
    console.log('\n== marker and list faults')
    for (const f of faults) console.log(`${f.file}:${f.line}: ${f.why}`)
  }
}

function check(hits, faults, read, baseline) {
  const byFile = new Map()
  for (const h of hits) {
    if (!byFile.has(h.file)) byFile.set(h.file, [])
    byFile.get(h.file).push(h)
  }
  const over = []
  const under = []
  for (const [file, list] of byFile) {
    const allowed = baseline ? baseline.counts.get(file) || 0 : 0
    if (list.length > allowed) over.push({ file, list, allowed })
  }
  if (baseline) {
    for (const [file, count] of baseline.counts) {
      const now = (byFile.get(file) || []).length
      if (now < count) under.push({ file, count, now })
    }
  }
  console.log('')
  printSummary(hits, read, baseline)
  console.log('')
  if (under.length) {
    console.log(`${under.length} baselined file(s) now hold fewer — good. To lock that in, run`)
    console.log(`  node ${SELF} --update-baseline   and commit the baseline:`)
    for (const u of under) console.log(`  ${u.file}: ${u.count} -> ${u.now}`)
    console.log('')
  }
  if (!over.length && !faults.length) {
    console.log(`says-roomlerd: OK — no new person-facing "agent" (baseline: ${BASELINE})`)
    return 0
  }
  if (over.length) {
    const n = over.reduce((a, o) => a + o.list.length - o.allowed, 0)
    console.log(`says-roomlerd: FAIL — ${n} new person-facing "agent"(s). A person reads \`roomlerd\` (or "the daemon"):\n`)
    for (const o of over) {
      const known = baseline ? baseline.hints.get(o.file) || [] : []
      const fresh = o.list.filter((h) => !known.includes(hint(h.text).replace(/"/g, "'")))
      const shown = fresh.length ? fresh : o.list
      if (o.allowed) {
        console.log(`  ${o.file}: holds ${o.list.length}, its baseline ${o.allowed}${fresh.length ? '; not in the baseline:' : '; one of these is new:'}`)
      }
      for (const h of shown) {
        console.log(`  ${h.file}:${h.line}: "${h.text}"`)
        annotate(h.file, h.line, `Person-facing text says "agent" (FR-133): say roomlerd, "the daemon" or "the device", or mark it with a ${MARKER}: <why> comment. See: node ${SELF} --check`)
      }
    }
    console.log('')
    for (const l of FIXES) console.log(l)
  }
  if (faults.length) {
    console.log(`\nsays-roomlerd: FAIL — ${faults.length} marker or list fault(s):`)
    for (const f of faults) {
      console.log(`  ${f.file}:${f.line}: ${f.why}`)
      annotate(f.file, f.line, f.why)
    }
  }
  return 1
}

// ─── The self-test ───────────────────────────────────────────────────────────

/** In-memory files with the hits each must give (file:line "text"), one per reader. */
const FIXTURES = [
  {
    file: 'agents/roomlerd/src/fixture.rs',
    text: [
      '// the agent, in a comment',
      'fn f() {',
      '    tracing::warn!("the agent is late {}", n);',
      '    bail!("the agent could not start");',
      '    let id = "agent";',
      '    let path = "/api/agent/{id}/ws";',
      '    let tag = format!("agent-v{}", v);',
      '    eprintln!("Agent: {}", id);',
      '    // AGENT-WORD-KEPT: OpenSSH names its own key agent so',
      '    let ssh = "the SSH agent socket";',
      '}',
      '#[cfg(test)]',
      'mod tests {',
      '    const T: &str = "a test agent phrase";',
      '}',
      'const AFTER: &str = "after the tests, an agent again";',
    ].join('\n'),
    want: [
      'agents/roomlerd/src/fixture.rs:4 "the agent could not start"',
      'agents/roomlerd/src/fixture.rs:8 "Agent: {}"',
      'agents/roomlerd/src/fixture.rs:16 "after the tests, an agent again"',
    ],
    faults: 0,
  },
  {
    file: 'agents/roomler-desktop/src/front/fixture.js',
    text: [
      "// 'the agent' in a comment",
      "const kind = 'agent'",
      "console.warn('the agent went away')",
      'const re = /["\']agent/g',
      'el.textContent = `${n} agents are online`',
      "const ok = 'two words'",
    ].join('\n'),
    want: ['agents/roomler-desktop/src/front/fixture.js:5 "agents are online"'],
    faults: 0,
  },
  {
    file: 'agents/roomler-desktop/src/front/fixture.html',
    text: [
      '<!-- the agent, in a comment -->',
      '<p title="Restart the agent">Install the',
      '  agent first</p>',
      "<script>var s = 'agent'; show('the agent is up')</script>",
      '<input placeholder="device id">',
    ].join('\n'),
    want: [
      'agents/roomler-desktop/src/front/fixture.html:2 "Restart the agent"',
      'agents/roomler-desktop/src/front/fixture.html:2 "Install the agent first"',
      'agents/roomler-desktop/src/front/fixture.html:4 "the agent is up"',
    ],
    faults: 0,
  },
  {
    file: 'ui/docs/content/fixture.md',
    text: [
      '---',
      'title: Install the Agent',
      '---',
      '# The agent',
      '<!-- the agent, in a comment -->',
      'Run `roomlerd enroll --agent x` and see [the docs](/agent/install).',
      '```bash',
      'curl https://roomler.ai/api/agent/installer/linux | sh',
      'echo "restart the agent"',
      '```',
      '<!-- AGENT-WORD-KEPT -->',
      'An agent here.',
    ].join('\n'),
    want: [
      'ui/docs/content/fixture.md:2 "Install the Agent"',
      'ui/docs/content/fixture.md:4 "# The agent"',
      'ui/docs/content/fixture.md:9 "echo "restart the agent""',
      'ui/docs/content/fixture.md:12 "An agent here."',
    ],
    faults: 1,
  },
  {
    file: 'scripts/install.sh',
    text: [
      '#!/bin/sh',
      '# install the agent (a comment)',
      'echo "Installing the agent..."',
      'echo "agent_id=$agent_id" "$agent"',
      'cat <<EOF',
      'The agent is installed.',
      'EOF',
      "echo 'done'  # AGENT-WORD-KEPT: a marker with nothing to cover",
    ].join('\n'),
    want: ['scripts/install.sh:3 "Installing the agent..."', 'scripts/install.sh:6 "The agent is installed."'],
    faults: 1,
  },
  {
    file: 'scripts/install.ps1',
    text: [
      '<# the agent, in a block comment #>',
      '# the agent, in a comment',
      'Write-Host "Stopping the agent $($svc.Name)"',
      "Write-Host 'The agent''s log'",
      '$x = @"',
      'Agent ready',
      '"@',
    ].join('\n'),
    want: ['scripts/install.ps1:3 "Stopping the agent"', "scripts/install.ps1:4 \"The agent's log\"", 'scripts/install.ps1:6 "Agent ready"'],
    faults: 0,
  },
  {
    file: 'agents/roomlerd/wix/fixture.wxs',
    // The split string keeps this line from holding a retired name, which scripts/name-audit.sh would count.
    text: ['<!-- the Agent -->', '<Package Name="Roomler" Manufacturer="Roomler">', '  <Shortcut Name="Roomler' + ' Agent Console" Description="Opens the agent" />', '</Package>'].join('\n'),
    want: ['agents/roomlerd/wix/fixture.wxs:3 "Opens the agent"'],
    faults: 0,
  },
  {
    file: 'agents/roomlerd/Cargo.toml',
    text: ['[package]', 'name = "roomlerd"', 'description = "The Roomler agent"', 'keywords = ["agent"]'].join('\n'),
    want: ['agents/roomlerd/Cargo.toml:3 "The Roomler agent"'],
    faults: 0,
  },
  {
    file: 'agents/roomler-desktop/tauri.conf.json',
    text: JSON.stringify({ productName: 'Roomler', app: { windows: [{ title: 'Agent settings', label: 'agent' }] } }, null, 2),
    want: ['agents/roomler-desktop/tauri.conf.json:6 "Agent settings"'],
    faults: 0,
  },
  {
    file: 'agents/roomler-desktop/packaging/linux/fixture.desktop',
    text: ['[Desktop Entry]', 'Name=Roomler', 'Comment=Talks to the agent', 'Exec=roomler-desktop --agent'].join('\n'),
    want: ['agents/roomler-desktop/packaging/linux/fixture.desktop:3 "Talks to the agent"'],
    faults: 0,
  },
  {
    file: 'agents/roomlerd/packaging/linux/fixture.service',
    text: ['[Unit]', 'Description=Roomler agent', 'After=network.target'].join('\n'),
    want: ['agents/roomlerd/packaging/linux/fixture.service:2 "Roomler agent"'],
    faults: 0,
  },
  {
    file: 'agents/roomlerd/packaging/windows/README.txt',
    text: ['Roomler', 'Uninstall the agent from Apps.', 'See com.roomler.agent'].join('\n'),
    want: ['agents/roomlerd/packaging/windows/README.txt:2 "Uninstall the agent from Apps."'],
    faults: 0,
  },
  {
    file: '.github/workflows/release-agent.yml',
    text: [
      '      - name: Sign',
      '        uses: ./.github/actions/sign-windows',
      '        with:',
      "          description:              'Roomler agent installer'",
      '      - name: Other',
      "        description: 'not a signing step agent'",
    ].join('\n'),
    want: ['.github/workflows/release-agent.yml:4 "Roomler agent installer"'],
    faults: 0,
  },
]

function selfTest({ quiet = false } = {}) {
  const results = []
  const ok = (name, pass, detail = '') => results.push({ name, pass, detail })
  const eq = (a, b) => JSON.stringify(a) === JSON.stringify(b)

  // The rule, on the shapes it has to get right.
  ok('a person-facing "agent" counts', saysAgent('the agent stopped', false) && saysAgent('Agents:', false))
  ok('an identifier does not', !saysAgent('agent', false) && !saysAgent('agent-logs', false) && !saysAgent('agent_id', true))
  ok('prose counts as one word too', saysAgent('agent', true))
  ok('identities are out', !saysAgent('see /api/agent/x and rc:agent.hello', false) && !saysAgent('com.roomler.agent here', false))
  ok('agents, any case', saysAgent('the AGENTS are up', false) && !saysAgent('useragent here', false))
  ok("the spec's wire and OS names are identities", !saysAgent('want TokenType::Agent here', false) && !saysAgent('~/Library/LaunchAgents/com.roomler.agent.plist', true) && saysAgent('the Agent token', false))
  ok("the per-user MSI's registry key is an identity", !saysAgent("Key='Software\\Roomler\\Agent' Name='installed_exe'", false) && saysAgent('Software\\Roomler\\Agent restarts the agent', false))
  ok("polkit's authentication agent is another program's", !saysAgent('no polkit authentication agent in this session', false) && !saysAgent('Authentication Agent', false) && saysAgent('the agent asks polkit', false))
  ok('an identifier that holds an identity is still an identifier', !saysAgent('roomler:agent-online:{agent_id_hex}', false) && saysAgent('agent-online: {agent_id_hex}', false))
  ok("FR-90's agent session is an AI agent's", !saysAgent("an agent session's ticket, and FR-90 is parked", false) && saysAgent("this device's agent cannot hold an agent session", false))
  ok("the HTTP header is another program's word", !saysAgent('counted by the User-Agent header', false) && !saysAgent('user-agent', true))
  ok("an EDR product's agent is another program's", !saysAgent('no process for an endpoint agent to terminate', true) && !saysAgent('EDR agents', true) && saysAgent('the endpoint runs the agent', true))
  const quic = KEPT_PHRASES.find((k) => k.prefix)
  ok('a kept prefix lets its own file by and no other', keeps(quic, quic.file, `${quic.prefix}: local_addr`) && !keeps(quic, 'agents/roomlerd/src/app.rs', `${quic.prefix}: local_addr`) && !keeps(quic, quic.file, `the ${quic.prefix}`))

  // P2a's lexer case (notice_text_tests.rs `the_literal_lexer_reads_rust`), byte for byte.
  const p2a = String.raw`
        let a = '"'; let b = '\''; fn f<'a>(x: &'a str) {}
        // "a comment"
        /* "a block" */
        tracing::warn!(n = 1, "a log line {}", format!("inner"));
        bail!("an error, {e}");
        let r = r#"raw "quoted""#;
    `
  const got = rustLiterals(p2a)
  ok(
    "the Rust reader reads P2a's case as P2a's lexer does",
    eq(got.map((g) => g.text), ['a log line {}', 'inner', 'an error, {e}', 'raw "quoted"']) &&
      eq(got.map((g) => g.skip === 'a log line'), [true, true, false, false]),
    JSON.stringify(got.map((g) => [g.text, g.skip])),
  )
  // The companion's readers' cases (commands.rs `the_readers_read_what_a_person_sees`).
  const js = "// 'a comment'\nconst a = `${x} is the agent`; f('two words', \"x\");"
  ok("the JS reader reads the companion's case", eq(jsStrings(js).map((s) => s.text), [' is the agent', 'two words', 'x']), JSON.stringify(jsStrings(js).map((s) => s.text)))
  const html = '<!-- agent --><p title="a b">Hi <b>there</b></p><script>var s = \'agent\';</script><input placeholder="24-hex id">'
  const words = htmlWords(html).filter((w) => w.prose && w.text.trim()).map((w) => w.text)
  ok("the HTML reader reads the companion's case", eq(words, ['a b', 'Hi ', 'there', '24-hex id']), JSON.stringify(words))
  const rust = 'let c = \'"\'; fn f<\'a>() {} warn!("a log"); bail!("an error");'
  ok("the Rust reader reads the companion's case", eq(rustLiterals(rust).map((g) => [g.text, g.skip === 'a log line']), [['a log', true], ['an error', false]]))

  // A file's own log macro (FR-133 P4 follow-up): a `macro_rules!` whose body calls one of the five
  // tracing macros is a log macro in that file, so a literal handed to it is a log line, as in
  // overlay.rs's `miss_line!`. One that wraps no log macro is not, and its literals still count.
  const wrapped = String.raw`
        macro_rules! miss_line {
            ($cond:expr, $($line:tt)+) => {
                if $cond { tracing::debug!($($line)+); } else { tracing::trace!($($line)+); }
            };
        }
        macro_rules! shout { ($($t:tt)+) => { miss_line!(true, $($t)+) }; }
        fn f() {
            miss_line!(true, id = 1, "an agent line");
            shout!("another agent line");
            bail!("an agent error");
        }
    `
  const gotWrapped = rustLiterals(wrapped)
  ok(
    "a literal inside a file's own log macro is a log line, one macro away too",
    eq(gotWrapped.map((g) => [g.text, g.skip === 'a log line']), [['an agent line', true], ['another agent line', true], ['an agent error', false]]),
    JSON.stringify(gotWrapped.map((g) => [g.text, g.skip])),
  )
  const plain = String.raw`
        macro_rules! say { ($($t:tt)+) => { println!($($t)+) }; }
        macro_rules! oops { ($($t:tt)+) => { eprintln!($($t)+) }; }
        macro_rules! fail { ($($t:tt)+) => { bail!($($t)+) }; }
        macro_rules! fmt { ($($t:tt)+) => { format!($($t)+) }; }
        macro_rules! quiet { ($($t:tt)+) => { /* info!("a comment") */ { let _s = "warn!(x)"; format!($($t)+) } }; }
        macro_rules! both { ($($t:tt)+) => {{ tracing::warn!($($t)+); bail!($($t)+) }}; }
        macro_rules! via_both { ($($t:tt)+) => { both!($($t)+) }; }
        macro_rules! also { ($($t:tt)+) => {{ tracing::info!($($t)+); let _ = write!(out, $($t)+); }}; }
        fn f() {
            say!("the agent said"); oops!("the agent erred"); fail!("the agent failed"); fmt!("the agent formatted"); quiet!("the agent hid");
            both!("the agent both"); via_both!("the agent via both"); also!("the agent also");
        }
    `
  const gotPlain = rustLiterals(plain).filter((g) => /^the agent/.test(g.text))
  ok(
    'a local macro that wraps no log macro, or one that also calls a person-facing macro (or one that does), does not skip its literals',
    gotPlain.length === 8 && gotPlain.every((g) => g.skip === undefined),
    JSON.stringify(gotPlain.map((g) => [g.text, g.skip])),
  )

  // A marker on a line of the file's own log macro is true (overlay.rs keeps one there), while one on
  // a plain log line, or on an own-macro line that no longer says "agent", still covers nothing.
  const own = 'macro_rules! hint { ($($t:tt)+) => { tracing::debug!($($t)+) }; }\n'
  const scanRs = (body) => scanText('agents/roomlerd/src/x.rs', own + body, kindOf('agents/roomlerd/src/x.rs'))
  const markedOwn = scanRs('fn f() {\n    // AGENT-WORD-KEPT: a log message; reads of the log count this wording\n    hint!(a = 1, "an agent line");\n}\n')
  const plainLog = scanRs('fn f() {\n    // AGENT-WORD-KEPT: a log message; reads of the log count this wording\n    tracing::debug!(a = 1, "an agent line");\n}\n')
  const reworded = scanRs('fn f() {\n    // AGENT-WORD-KEPT: a log message; reads of the log count this wording\n    hint!(a = 1, "a daemon line");\n}\n')
  ok(
    "a marker on the file's own log macro holds, and one on a plain log line or a reworded one is still a fault",
    markedOwn.hits.length === 0 && markedOwn.faults.length === 0 && plainLog.faults.length === 1 && reworded.faults.length === 1,
    JSON.stringify([markedOwn, plainLog, reworded]),
  )

  // Each reader on a fixture file: a person-facing "agent" FAILS; an identity, a comment,
  // a log line, test code and a marked occurrence with a reason pass; a marker with no
  // reason FAILS, and so does a marker that covers nothing.
  for (const fx of FIXTURES) {
    const kind = kindOf(fx.file)
    const r = scanText(fx.file, fx.text, kind)
    const saw = r.hits.map((h) => `${h.file}:${h.line} "${h.text}"`)
    const pass = eq(saw, fx.want) && r.faults.length === fx.faults
    ok(`the ${kind.name} reader: ${fx.want.length} planted hit(s) FAIL, the rest pass${fx.faults ? `, ${fx.faults} bad marker(s) FAIL` : ''}`, pass, pass ? `planted: ${saw[0]} -> FAIL, as it must` : `got ${JSON.stringify(saw)} faults ${JSON.stringify(r.faults)}`)
  }

  // Agreement: the UI spec's IDENTITIES, verbatim; the daemon tests' kept spellings, consumed.
  const mine = new Set(IDENTITIES.map(({ re }) => `/${re.source}/${re.flags}`))
  const ui = readFileSync(join(ROOT, UI_SPEC), 'utf8')
  const block = /const IDENTITIES[^=]*=\s*\[([\s\S]*?)\n\]/.exec(ui)
  const theirs = block ? [...block[1].matchAll(/^\s*(\/.+?\/[a-z]*),/gm)].map((m) => m[1]) : []
  const missing = theirs.filter((r) => !mine.has(r))
  const todo = (list, sep, what) => (list.length ? `${list.join(sep)} -- ${what}` : '')
  ok(`the UI spec's ${theirs.length} IDENTITIES are this guard's, verbatim`, theirs.length > 0 && !missing.length, todo(missing, ' ', `add each to IDENTITIES in ${SELF}, with its reason`))
  const daemon = readFileSync(join(ROOT, DAEMON_TEST), 'utf8').replace(/\r\n/g, '\n')
  const kept = /const KEPT:[^=]*=\s*\[([\s\S]*?)\n\];/.exec(daemon)
  const froms = kept ? [...kept[1].matchAll(/\(\s*"([^"]+)"\s*,\s*"[^"]*"\s*\)/g)].map((m) => m[1]) : []
  const leaks = froms.filter((f) => WORD.test(withoutIdentities(f)))
  ok(`the daemon test's ${froms.length} KEPT spellings are kept here too`, froms.length > 0 && !leaks.length, todo(leaks, ' ', `keep each in ${SELF} too (IDENTITIES), with its reason`))
  const logOnly = /const LOG_ONLY:[^=]*=\s*\[([\s\S]*?)\n\];/.exec(daemon)
  const sentences = logOnly ? rustLiterals(logOnly[1]).map((l) => l.text).filter((t) => t.includes(' ')) : []
  const unlisted = sentences.filter((t) => !KEPT_PHRASES.some((k) => k.text === t))
  ok(`the daemon test's ${sentences.length} LOG_ONLY sentences are KEPT_PHRASES here`, sentences.length > 0 && !unlisted.length, todo(unlisted, ' | ', `list each in KEPT_PHRASES in ${SELF}, with its reason`))
  const notice = readFileSync(join(ROOT, NOTICE_TEST), 'utf8')
  const replaced = [...notice.matchAll(/\.replace\("([^"]+)",\s*"[^"]*"\)/g)].map((m) => m[1])
  const noticeLeaks = replaced.filter((f) => WORD.test(withoutIdentities(f)))
  ok(`the notice test's ${replaced.length} kept spellings are kept here too`, replaced.length > 0 && !noticeLeaks.length, todo(noticeLeaks, ' ', `keep each in ${SELF} too (IDENTITIES), with its reason`))

  const failed = results.filter((r) => !r.pass)
  if (!quiet || failed.length) {
    for (const r of results) console.log(`self-test ${r.pass ? 'ok    ' : 'FAILED'} ${r.name}${r.detail ? `  [${r.detail}]` : ''}`)
  }
  console.log(`says-roomlerd: self-test ${results.length - failed.length}/${results.length} ok`)
  return failed.length ? 1 : 0
}

// ─── Main ────────────────────────────────────────────────────────────────────

function main(argv) {
  const mode = argv.find((a) => a.startsWith('--') && a !== '--no-baseline') || '--report'
  const noBaseline = argv.includes('--no-baseline')
  if (mode === '--self-test') return selfTest()
  if (mode === '--identities') {
    for (const { re, why } of IDENTITIES) console.log(`/${re.source}/${re.flags}\t${why}`)
    for (const k of KEPT_PHRASES) console.log(`${k.file}: ${k.text !== undefined ? `"${k.text}"` : `"${k.prefix}…"`}\t${k.why}`)
    return 0
  }
  if (!['--report', '--summary', '--check', '--update-baseline'].includes(mode)) {
    console.error(`usage: node ${SELF} [--summary | --check | --update-baseline | --self-test | --identities] [--no-baseline]`)
    return 2
  }
  if (mode === '--check' && selfTest({ quiet: true })) {
    console.log('says-roomlerd: the self-test failed (the FAILED lines above say what and how), so the guard cannot be trusted until it passes')
    return 1
  }
  const { hits, faults, read } = scanTree()
  const baseline = noBaseline ? null : readBaseline()
  if (mode === '--update-baseline') {
    if (faults.length) {
      for (const f of faults) console.log(`${f.file}:${f.line}: ${f.why}`)
      console.log('says-roomlerd: fix the faults above first; the baseline holds hits, not faults')
      return 1
    }
    writeBaseline(hits)
    console.log(`says-roomlerd: wrote ${BASELINE}: ${hits.length} hits in ${new Set(hits.map((h) => h.file)).size} files`)
    printSummary(hits, read, readBaseline())
    return 0
  }
  if (mode === '--summary') {
    printSummary(hits, read, baseline)
    if (faults.length) console.log(`${faults.length} marker or list fault(s): run --check`)
    return 0
  }
  if (mode === '--check') return check(hits, faults, read, baseline)
  report(hits, faults, read)
  console.log('')
  printSummary(hits, read, baseline)
  return 0
}

process.exitCode = main(process.argv.slice(2))
