#!/usr/bin/env node
// autopilot — ingest FR state into cards, render the kanban board.
//
// Subcommands:
//   scan   [--repo <path>] [--docs <path>]   refresh cards from the ledger + specs + GitHub
//   board  [--docs <path>]                   regenerate BOARD.md from the cards
//   next   [--docs <path>] [--n 3]           print the next N cards by closest-to-done rank
//
// Why Node: this box has no python3 and no jq on the Git-Bash PATH (measured 2026-09-08).
// `gh --jq` covers GitHub reads; everything local is plain Node with zero dependencies.
//
// The cards are the run state. The GitHub issue and the FR spec remain authoritative for
// engineering truth — this never writes to either.

import { readFileSync, writeFileSync, readdirSync, mkdirSync, existsSync } from 'node:fs';
import { execFileSync } from 'node:child_process';
import { join, basename } from 'node:path';

const COLUMNS = [
  ['admitted', 'Admitted'],
  ['judgement', 'Needs your judgement'],
  ['in_progress', 'In progress'],
  ['pr_open', 'PR open'],
  ['field', 'Field verification'],
  ['ready', 'Ready to close'],
];
const COLUMN_IDS = COLUMNS.map(([id]) => id);

const args = process.argv.slice(2);
const cmd = args[0];
const flag = (name, dflt) => {
  const i = args.indexOf(`--${name}`);
  return i >= 0 && args[i + 1] ? args[i + 1] : dflt;
};
const REPO = flag('repo', 'C:/dev/gjovanov/roomler-ai');
const DOCS = flag('docs', 'C:/dev/gjovanov/roomler-ai-docs');
const STATE = join(DOCS, 'kanban', 'state');

// ---------------------------------------------------------------- ledger

// | [FR-52](FR-52-cross-org-remote-access.md) | [#1100](…/issues/1100) | Title | Status prose |
// The vacated `| ~~FR-3~~ | — | *vacated* … |` row must not match: it has no spec link.
//
// The issue cell is captured SEPARATELY from the row match on purpose. A row whose issue is
// `#TBD` or `—` is a claimed number with nothing published behind it, and dropping it
// silently is how a collision hides: the first run of this scanner found exactly that —
// a local branch holding `FR-80 | #TBD` for a spec master had already given to #1532.
// An unreadable row is REPORTED, never skipped.
const LEDGER_ROW = /^\|\s*\[FR-(\d+)\]\(([^)]+)\)\s*\|\s*([^|]*?)\s*\|\s*(.*?)\s*\|\s*(.*?)\s*\|\s*$/;
const ISSUE_CELL = /\[#(\d+)\]/;

// ALWAYS read the ledger and the specs from origin/master, never the working tree. The
// ledger is the claim, and only master arbitrates it — a feature branch's copy is one
// session's opinion. Read from the tree instead and the queue changes shape depending on
// which branch the operator happens to be standing on: measured 2026-09-08, a branch
// holding `FR-80 | #TBD` hid master's real FR-80 (#1532) from the scan entirely.
function fromMaster(path) {
  try {
    return execFileSync('git', ['-C', REPO, 'show', `origin/master:${path}`], {
      encoding: 'utf8', maxBuffer: 32 * 1024 * 1024, stdio: ['ignore', 'pipe', 'ignore'],
    });
  } catch {
    return null;
  }
}

function readLedger() {
  const text = fromMaster('docs/fr/README.md')
    ?? readFileSync(join(REPO, 'docs', 'fr', 'README.md'), 'utf8');
  const rows = [], unclaimed = [];
  for (const line of text.split(/\r?\n/)) {
    const m = LEDGER_ROW.exec(line);
    if (!m) continue;
    const issueM = ISSUE_CELL.exec(m[3]);
    if (!issueM) {
      unclaimed.push({ fr: Number(m[1]), specFile: m[2], issueCell: m[3], title: m[4] });
      continue;
    }
    rows.push({
      fr: Number(m[1]),
      specFile: m[2],
      issue: Number(issueM[1]),
      title: m[4],
      ledgerStatus: m[5],
    });
  }
  return { rows, unclaimed };
}

// ---------------------------------------------------------------- spec parsing

function sectionBody(text, headingRe) {
  const lines = text.split(/\r?\n/);
  let start = -1;
  for (let i = 0; i < lines.length; i++) {
    if (/^##\s/.test(lines[i]) && headingRe.test(lines[i])) { start = i + 1; break; }
  }
  if (start < 0) return null;
  const out = [];
  for (let i = start; i < lines.length; i++) {
    if (/^##\s/.test(lines[i])) break;
    out.push(lines[i]);
  }
  return out;
}

// ACs are the spine: every one of the 79 specs uses `- [ ]` / `- [x]`, 73 have the heading,
// but only 13 use the bold `**ACn**` form — so the id is OPTIONAL and positional numbering
// is the fallback. Continuation lines (indented, or the trailing *( … )* evidence) are
// folded into the item they belong to.
const AC_ITEM = /^- \[([ xX])\]\s*(.*)$/;
const AC_BOLD_ID = /^\*\*(AC\s*\d+[a-z]?)\*\*\s*(?:—|-|:)?\s*/i;

function parseAcs(text) {
  const body = sectionBody(text, /acceptance\s+criteri/i);
  if (!body) return [];
  const acs = [];
  for (const raw of body) {
    const m = AC_ITEM.exec(raw);
    if (m) {
      let rest = m[2].trim();
      let id = null;
      const b = AC_BOLD_ID.exec(rest);
      if (b) { id = b[1].replace(/\s+/g, ''); rest = rest.slice(b[0].length); }
      acs.push({
        id: id || `AC${acs.length + 1}`,
        text: rest,
        done: m[1].toLowerCase() === 'x',
        verify: 'unclassified', // agent | operator | unclassified — see Q10 in SKILL.md
      });
    } else if (acs.length && /^\s+\S/.test(raw)) {
      acs[acs.length - 1].text += ' ' + raw.trim();
    }
  }
  // Evidence parentheticals bloat the text; keep the claim, drop the log.
  for (const ac of acs) {
    ac.text = ac.text.replace(/\s*\*\([^)]*\)\*/g, '').replace(/\s+/g, ' ').trim();
  }
  return acs;
}

// Phases are BEST EFFORT and nothing depends on them: only 41 of 79 specs have the heading,
// the tables under it have 15+ distinct header shapes, and FR-70 has prose instead of a
// table. Match header cells by NAME, never by position; a spec we cannot parse gets [].
function parsePhases(text) {
  const body = sectionBody(text, /^##\s+Phases?\b/i);
  if (!body) return [];
  const rows = body.filter((l) => /^\s*\|/.test(l));
  if (rows.length < 3) return [];
  const cells = (l) => l.replace(/^\s*\|/, '').replace(/\|\s*$/, '').split('|').map((c) => c.trim());
  const head = cells(rows[0]).map((h) => h.toLowerCase().replace(/[*`]/g, '').trim());
  const idx = (names) => head.findIndex((h) => names.some((n) => h === n || h.startsWith(n)));
  const iId = idx(['phase', 'p', '#']);
  const iWhat = idx(['what', 'scope', 'content', 'delivers', 'deliverable']);
  const iStatus = idx(['status', 'outcome', 'state']);
  if (iId < 0) return [];
  const out = [];
  for (const row of rows.slice(2)) {
    const c = cells(row);
    if (!c[iId]) continue;
    const clean = (s) => (s || '').replace(/\*\*/g, '').trim();
    out.push({
      id: clean(c[iId]),
      what: clean(c[iWhat]).slice(0, 200),
      status: clean(c[iStatus]).slice(0, 300),
    });
  }
  return out;
}

// ---------------------------------------------------------------- GitHub

function gh(argv) {
  return execFileSync('gh', argv, { encoding: 'utf8', maxBuffer: 32 * 1024 * 1024 });
}

function openIssues() {
  const raw = gh([
    'issue', 'list', '--repo', 'gjovanov/roomler-ai',
    '--state', 'open', '--limit', '300',
    '--json', 'number,title,labels,updatedAt',
  ]);
  return JSON.parse(raw);
}

function openPrs() {
  const raw = gh([
    'pr', 'list', '--repo', 'gjovanov/roomler-ai',
    '--state', 'open', '--limit', '100',
    '--json', 'number,title,headRefName,isDraft,updatedAt',
  ]);
  return JSON.parse(raw);
}

// ---------------------------------------------------------------- work in flight

// The autopilot TRACES work that is already under way; it never picks it up. A card whose
// FR has a live branch, a checked-out worktree or uncommitted changes is somebody's
// in-flight work, and a worker that started there would be racing a human over the same
// files. That is the #1144 shape — a merged, field-verified fix silently reverted, green
// CI, no conflict — and it is cheaper to observe than to recover from.
// Tunable with --fresh-days. Conservative by default: a false hands-off costs a card not
// being picked up, which the operator can see and override on the board; a false dispatch
// costs a collision with a human over the same files, which they may not see until merge.
const FRESH_DAYS = Number(flag('fresh-days', '14'));

function git(argv, cwd = REPO) {
  try {
    return execFileSync('git', ['-C', cwd, ...argv], {
      encoding: 'utf8', maxBuffer: 32 * 1024 * 1024, stdio: ['ignore', 'pipe', 'ignore'],
    });
  } catch { return ''; }
}

function branchIndex() {
  return git(['for-each-ref', '--format=%(refname:short)\t%(committerdate:unix)', 'refs/heads'])
    .split(/\r?\n/).filter(Boolean)
    .map((l) => { const [name, ts] = l.split('\t'); return { name, ts: Number(ts) }; });
}

function worktreeIndex() {
  const out = [], blocks = git(['worktree', 'list', '--porcelain']).split(/\n\n+/);
  for (const b of blocks) {
    const p = /^worktree (.+)$/m.exec(b);
    const br = /^branch refs\/heads\/(.+)$/m.exec(b);
    if (p) out.push({ path: p[1].trim(), branch: br ? br[1].trim() : null });
  }
  return out;
}

// Tracked files only: an untracked scratch file is not evidence that someone is mid-change.
const isDirty = (path) => git(['status', '--porcelain', '--untracked-files=no'], path).trim().length > 0;

function traceInFlight(card, fr, branches, worktrees) {
  const re = new RegExp(`^fr-?0*${fr}(?![0-9])`, 'i');
  const now = Date.now() / 1000;
  const found = [];
  for (const b of branches.filter((b) => re.test(b.name))) {
    const wt = worktrees.find((w) => w.branch === b.name);
    const ageDays = Math.round((now - b.ts) / 86400);
    found.push({
      branch: b.name,
      age_days: ageDays,
      worktree: wt ? wt.path : null,
      uncommitted: wt ? isDirty(wt.path) : false,
    });
  }
  card.run.in_flight = found;

  const live = found.some((f) => f.uncommitted || f.age_days <= FRESH_DAYS);
  card.run.hands_off = Boolean(card.run.adopted_pr || live);
  card.run.hands_off_reason = !card.run.hands_off ? null
    : card.run.adopted_pr ? `open PR #${card.run.adopted_pr}`
    : found.find((f) => f.uncommitted) ? `uncommitted changes in ${found.find((f) => f.uncommitted).worktree}`
    : `branch ${found.find((f) => f.age_days <= FRESH_DAYS).branch} touched ${found.find((f) => f.age_days <= FRESH_DAYS).age_days}d ago`;
  return card.run.hands_off;
}

// ---------------------------------------------------------------- cards

const cardPath = (id) => join(STATE, `${id}.json`);

function loadCard(id) {
  const p = cardPath(id);
  return existsSync(p) ? JSON.parse(readFileSync(p, 'utf8')) : null;
}

function saveCard(card) {
  mkdirSync(STATE, { recursive: true });
  card.updated_at = new Date().toISOString();
  writeFileSync(cardPath(card.id), JSON.stringify(card, null, 2) + '\n', 'utf8');
}

function allCards() {
  if (!existsSync(STATE)) return [];
  return readdirSync(STATE)
    .filter((f) => f.endsWith('.json'))
    .map((f) => JSON.parse(readFileSync(join(STATE, f), 'utf8')));
}

const freshRun = () => ({
  branch: null, worktree: null, pr: null, adopted_pr: null,
  attempts: 0, last_action: null, started_at: null, heartbeat: null,
  budget_halted: false, notes: [],
  in_flight: [], hands_off: false, hands_off_reason: null,
});

// ---------------------------------------------------------------- scan

function scan() {
  const { rows: ledger, unclaimed } = readLedger();
  const issues = openIssues();
  const prs = openPrs();
  const branches = branchIndex();
  const worktrees = worktreeIndex();
  const byNumber = new Map(issues.map((i) => [i.number, i]));

  let created = 0, updated = 0, skipped = 0;

  for (const row of ledger) {
    const issue = byNumber.get(row.issue);
    if (!issue) { skipped++; continue; } // closed issue — not our queue

    const id = `FR-${row.fr}`;
    const specRel = `docs/fr/${basename(row.specFile)}`;
    const specPath = join(REPO, specRel);
    const text = fromMaster(specRel) ?? (existsSync(specPath) ? readFileSync(specPath, 'utf8') : null);
    let acs = [], phases = [];
    if (text) { acs = parseAcs(text); phases = parsePhases(text); }

    const prev = loadCard(id);
    // A rescan must never silently un-classify an AC a human or a worker judged, and must
    // never resurrect a tick the spec has since cleared: the SPEC owns `done`, the CARD
    // owns `verify`. Match on id first, then on text, so a reworded AC keeps its judgement
    // only when its id is stable.
    if (prev) {
      const prevById = new Map(prev.acs.map((a) => [a.id, a]));
      const prevByText = new Map(prev.acs.map((a) => [a.text, a]));
      for (const ac of acs) {
        const old = prevById.get(ac.id) || prevByText.get(ac.text);
        if (old && old.verify !== 'unclassified') ac.verify = old.verify;
      }
    }

    const card = {
      id,
      kind: 'fr',
      issue: row.issue,
      title: row.title,
      spec: `docs/fr/${basename(row.specFile)}`,
      column: prev?.column ?? 'admitted',
      blocked: prev?.blocked ?? null,
      priority: prev?.priority ?? null,
      acs,
      phases,
      ledger_status: row.ledgerStatus,
      issue_updated_at: issue.updatedAt,
      labels: issue.labels.map((l) => l.name),
      run: prev?.run ?? freshRun(),
      created_at: prev?.created_at ?? new Date().toISOString(),
    };

    // Adoption (Q8): an OPEN PR whose branch names this FR is adoptable — its diff is
    // reviewable and its intent is stated. A bare branch is NEVER adopted; that is the
    // #1144 shape (a merged, field-verified fix silently reverted, green CI, no conflict).
    const slug = `fr${row.fr}`;
    const match = prs.find((p) => {
      const b = p.headRefName.toLowerCase().replace(/[^a-z0-9]/g, '');
      return b.startsWith(slug) && !/^fr\d/.test(b.slice(slug.length));
    });
    if (match && !card.run.adopted_pr) {
      card.run.adopted_pr = match.number;
      card.run.branch = match.headRefName;
      if (card.column === 'admitted') card.column = match.isDraft ? 'judgement' : 'pr_open';
      card.run.notes.push(
        `adoptable open PR #${match.number} (${match.headRefName})${match.isDraft ? ' — DRAFT, needs your call' : ''}`,
      );
    }

    // Trace, never take over. A card with work in flight is reported and left alone.
    if (traceInFlight(card, row.fr, branches, worktrees) && card.column === 'admitted') {
      card.column = 'in_progress';
    }

    saveCard(card);
    prev ? updated++ : created++;
  }

  console.log(`scan: ${created} created, ${updated} updated, ${skipped} ledger rows whose issue is closed`);

  // Two silent-drop classes, both reported loudly. A queue that quietly omits a row is
  // indistinguishable from one that has nothing to do — the same shape as a `cargo test`
  // filter matching no test.
  for (const u of unclaimed) {
    console.warn(`  ⚠️  FR-${u.fr} has no issue (${u.issueCell || 'empty'}) — ${u.specFile}`);
    console.warn('      A number claimed with nothing published behind it. Check master for a collision.');
  }
  // A duplicate AC id is a typo in the SPEC, not in the card — surface it so someone fixes
// the document; nothing here rewrites a spec.
  for (const card of allCards()) {
    const seen = new Set(), dup = new Set();
    for (const a of card.acs) (seen.has(a.id) ? dup : seen).add(a.id);
    if (dup.size) console.warn(`  ⚠️  ${card.id} labels two criteria ${[...dup].join(', ')} — fix ${card.spec}`);
  }
  const carded = new Set(ledger.filter((r) => byNumber.has(r.issue)).map((r) => r.issue));
  for (const i of issues) {
    if (/^FR-\d+/.test(i.title) && !carded.has(i.number)) {
      console.warn(`  ⚠️  open FR issue #${i.number} has no ledger row — ${i.title.slice(0, 70)}`);
      console.warn('      The row IS the claim; a spec file alone is not. Nothing will work this issue.');
    }
  }
}

// ---------------------------------------------------------------- rank + render

// Closest-to-done first (Q15): the shortest path to a shorter backlog is finishing what is
// nearly finished. `priority` is the operator override and always wins.
function rank(cards) {
  return [...cards].sort((a, b) => {
    if ((b.priority ?? 0) !== (a.priority ?? 0)) return (b.priority ?? 0) - (a.priority ?? 0);
    const fa = fraction(a), fb = fraction(b);
    if (fb !== fa) return fb - fa;
    return remaining(a).length - remaining(b).length;
  });
}
const fraction = (c) => (c.acs.length ? c.acs.filter((a) => a.done).length / c.acs.length : 0);
const remaining = (c) => c.acs.filter((a) => !a.done);
const operatorOnly = (c) => remaining(c).filter((a) => a.verify === 'operator');
const agentLeft = (c) => remaining(c).filter((a) => a.verify !== 'operator');

function bar(f) {
  const n = Math.round(f * 10);
  return '█'.repeat(n) + '░'.repeat(10 - n);
}

function board() {
  const cards = allCards();
  if (!cards.length) { console.error('no cards — run `scan` first'); process.exit(1); }
  const stamp = new Date().toISOString().replace('T', ' ').slice(0, 16) + ' UTC';
  const L = [];

  L.push('# Roomler autopilot board');
  L.push('');
  L.push(`_Generated ${stamp} by \`.claude/skills/autopilot\`. **Do not hand-edit** — every`);
  L.push('field comes from `kanban/state/*.json`, the FR specs and the GitHub issues, and a');
  L.push('hand edit is silently overwritten on the next run._');
  L.push('');

  // The payoff of Q10: one place that answers "what is waiting on ME?" across the backlog.
  const waiting = rank(cards.filter((c) => operatorOnly(c).length > 0 && agentLeft(c).length === 0));
  const draftCall = cards.filter((c) => c.column === 'judgement');
  L.push('## ⏳ Waiting on you');
  L.push('');
  if (!waiting.length && !draftCall.length) {
    L.push('_Nothing. Every open criterion is one an agent can reach._');
  } else {
    L.push('| card | issue | what only you can settle |');
    L.push('|---|---|---|');
    for (const c of waiting) {
      const items = operatorOnly(c).map((a) => `${a.id}: ${a.text}`).join('; ');
      L.push(`| **${c.id}** | [#${c.issue}](https://github.com/gjovanov/roomler-ai/issues/${c.issue}) | ${trunc(items, 220)} |`);
    }
    for (const c of draftCall) {
      const why = c.run.notes.slice(-1)[0] || 'sorted here at admission';
      L.push(`| **${c.id}** | [#${c.issue}](https://github.com/gjovanov/roomler-ai/issues/${c.issue}) | ${trunc(why, 220)} |`);
    }
  }
  L.push('');

  L.push('## Summary');
  L.push('');
  L.push('| column | cards |');
  L.push('|---|---|');
  for (const [id, label] of COLUMNS) {
    L.push(`| ${label} | ${cards.filter((c) => c.column === id).length} |`);
  }
  const unclassified = cards.reduce((n, c) => n + c.acs.filter((a) => a.verify === 'unclassified' && !a.done).length, 0);
  L.push(`| **total** | **${cards.length}** |`);
  L.push('');
  if (unclassified) {
    L.push(`> ⚠️ ${unclassified} open acceptance criteria are still \`unclassified\` — until a card's`);
    L.push('> criteria are split into agent-verifiable and operator-only, "Waiting on you" undercounts.');
    L.push('');
  }

  for (const [id, label] of COLUMNS) {
    const col = rank(cards.filter((c) => c.column === id));
    L.push(`## ${label} (${col.length})`);
    L.push('');
    if (!col.length) { L.push('_empty_'); L.push(''); continue; }
    L.push('| card | ACs | progress | phase / next | blocked |');
    L.push('|---|---|---|---|---|');
    for (const c of col) {
      const done = c.acs.filter((a) => a.done).length;
      const op = operatorOnly(c).length;
      const title = `**[${c.id}](https://github.com/gjovanov/roomler-ai/issues/${c.issue})** ${trunc(c.title, 60)}`;
      const acCell = `${done}/${c.acs.length}${op ? ` (${op} 👤)` : ''}`;
      const next = c.run.hands_off ? `🔒 ${c.run.hands_off_reason}`
        : (c.run.last_action || nextPhase(c) || '—');
      L.push(`| ${title} | ${acCell} | \`${bar(fraction(c))}\` | ${trunc(next, 70)} | ${c.blocked ? '🚧 ' + trunc(c.blocked, 60) : ''} |`);
    }
    L.push('');
  }

  L.push('---');
  L.push('');
  L.push('**Legend** — `👤` an open criterion only the operator can settle · `🚧` blocked ·');
  L.push('`🔒` work already in flight — **traced, never picked up**. A branch counts as live for');
  L.push(`${FRESH_DAYS} days after its last commit (\`--fresh-days\`); clear a card by hand if its`);
  L.push('branch is actually dead. ·');
  L.push('progress is ticked acceptance criteria, which is the spec\'s own measure, not a guess.');
  L.push('');
  L.push('An agent never closes an issue and never merges a PR. A card reaching **Ready to close**');
  L.push('means every agent-verifiable criterion is ticked *with linked field evidence* — CI green');
  L.push('is never evidence. The close is yours.');
  L.push('');

  mkdirSync(join(DOCS, 'kanban'), { recursive: true });
  writeFileSync(join(DOCS, 'kanban', 'BOARD.md'), L.join('\n'), 'utf8');
  console.log(`board: ${cards.length} cards -> ${join(DOCS, 'kanban', 'BOARD.md')}`);
}

function nextPhase(c) {
  const open = c.phases.find((p) => !/shipped|closed|done|built|verified|complete/i.test(p.status || ''));
  return open ? `${open.id} — ${open.what}` : null;
}
const trunc = (s, n) => {
  s = (s || '').replace(/\|/g, '\\|').replace(/\r?\n/g, ' ');
  return s.length > n ? s.slice(0, n - 1) + '…' : s;
};

function next() {
  const n = Number(flag('n', '3'));
  // hands_off cards are somebody else's work in flight — traced, never dispatched.
  const pool = allCards().filter((c) => !c.run.hands_off && ['admitted', 'field'].includes(c.column));
  for (const c of rank(pool).slice(0, n)) {
    const left = agentLeft(c);
    console.log(`${c.id}  #${c.issue}  ${c.acs.filter(a=>a.done).length}/${c.acs.length} ACs  [${c.column}]  ${c.title}`);
    console.log(`    agent-reachable open criteria: ${left.length}${left.length ? ' — ' + left[0].id + ': ' + trunc(left[0].text, 90) : ''}`);
  }
}

// ---------------------------------------------------------------- classify

// Apply `{"FR-71": {"AC2": "agent"}}` maps onto the cards. Judgement comes from a reader
// (a human or a classifier agent); this only records it, and refuses anything that is not
// one of the two labels — an unknown value would read as "not yet judged" downstream and
// silently shrink the "Waiting on you" list.
function classify() {
  const dir = flag('from', null);
  if (!dir) { console.error('classify: --from <dir with classify-*.json> is required'); process.exit(2); }
  const files = readdirSync(dir).filter((f) => /^classify-.*\.json$/.test(f));
  if (!files.length) { console.error(`classify: no classify-*.json in ${dir}`); process.exit(1); }

  const merged = {};
  for (const f of files) {
    const obj = JSON.parse(readFileSync(join(dir, f), 'utf8'));
    for (const [fr, acs] of Object.entries(obj)) {
      merged[fr] = { ...(merged[fr] || {}), ...acs };
    }
  }

  let applied = 0, unknownCard = [], unknownAc = [], badLabel = [];
  for (const [fr, acs] of Object.entries(merged)) {
    const card = loadCard(fr);
    if (!card) { unknownCard.push(fr); continue; }
    for (const [acId, label] of Object.entries(acs)) {
      if (label !== 'agent' && label !== 'operator') { badLabel.push(`${fr}/${acId}=${label}`); continue; }
      // EVERY match, not the first: a spec can label two criteria with the same id
      // (FR-70 has two `AC7`), and a `find` would leave the second silently unjudged —
      // which reads downstream as "not yet classified" forever.
      const hits = card.acs.filter((a) => a.id === acId);
      if (!hits.length) { unknownAc.push(`${fr}/${acId}`); continue; }
      for (const ac of hits) { ac.verify = label; applied++; }
    }
    saveCard(card);
  }

  console.log(`classify: ${applied} criteria labelled from ${files.length} file(s)`);
  if (unknownCard.length) console.warn(`  ⚠️  no such card: ${unknownCard.join(' ')}`);
  if (unknownAc.length) console.warn(`  ⚠️  no such criterion: ${unknownAc.join(' ')}`);
  if (badLabel.length) console.warn(`  ⚠️  not a label: ${badLabel.join(' ')}`);

  const left = allCards().reduce((n, c) => n + c.acs.filter((a) => a.verify === 'unclassified' && !a.done).length, 0);
  console.log(`  ${left} open criteria still unclassified`);
  place();
}

// A card whose remaining criteria are ALL operator-only has no agent work left in it —
// that is `ready`, and saying so is the point of the board. Derived, never sticky: it
// re-derives on every scan, and a card a worker has claimed is left alone.
function place() {
  let moved = 0;
  for (const card of allCards()) {
    if (card.run.started_at || card.run.hands_off || card.column === 'judgement') continue;
    const rem = remaining(card);
    const should = rem.length === 0 ? 'ready'
      : rem.every((a) => a.verify === 'operator') ? 'ready'
      : card.column === 'ready' ? 'admitted'
      : card.column;
    if (should !== card.column) { card.column = should; saveCard(card); moved++; }
  }
  if (moved) console.log(`  ${moved} card(s) re-placed by column derivation`);
}

switch (cmd) {
  case 'scan': scan(); place(); break;
  case 'board': board(); break;
  case 'next': next(); break;
  case 'classify': classify(); break;
  default:
    console.error('usage: autopilot.mjs <scan|board|next|classify --from <dir>> [--repo <p>] [--docs <p>] [--n N]');
    process.exit(2);
}
