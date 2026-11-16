#!/usr/bin/env node
//
// rc-path-report.cjs — FR-148 P0's instrument: what a remote-desktop session
// opened at beside what its path had already carried, and what the encoder was
// told beside what the plan wanted.
//
// WHY THIS EXISTS
//
//   Sessions to two laptops opened at a fraction of what their path had carried
//   the same week, and on one of them the encoder never followed the rate plan
//   (docs/fr/FR-148-path-measured-plan-follows.md §2). The agent opens from a
//   remembered number that stands in for a measurement, and nothing inside a
//   session refutes it. This report reads the agent's own log lines and prints,
//   per session, the opening beside what the same pair had demonstrated and the
//   plan's target beside the encoder's applied rate; per pair, the trail of
//   writes to the rate memory. Two flags count what FR-148 fixes. AC1 reads the
//   two reference failures through it, and AC6, AC7 and AC8 are judged through
//   it later. It measures. It decides nothing.
//
// TWO READS, ONE CORE
//
//   1. The FLEET read: mongosh over production `agent_logs`, with its client
//      outside the Mongo container (scripts/rc-path-report.sh, through
//      scripts/prod-mongo-read.sh; docs/production-mongo-reads.md §8). Every
//      daemon and every RC worker uploads its INFO lines there, for every org,
//      and the collection keeps 7 days.
//   2. The OFFLINE read, under Node: the NDJSON rows the fleet read prints with
//      `--rows`, or a host's rolling-log text (evidence about that host only).
//
// THE LINES IT READS (by message text, filtered on the server)
//
//   DC pump heartbeat             FFmpeg ≈2 s and VP9-444 ≈1 s windows: target_bps (the
//                                 plan), maxrate_bps (the encoder), bytes_written, size and
//                                 frame rate, encoder, avg_qp, the viewer's age, pipe_states,
//                                 the rate-apply counters, goodput_bps, prior_bps, constrained
//   DC pump starting              the pump's start, and `slow_link` (FFmpeg)
//   incoming session request      the controlling user, as its last 6 hex only
//   session carrier recorded      the carrier class (FR-102 P9c)
//   per-session ICE path detected the class's fallback on a build without the line above
//   …opening ceiling seeded…      the remembered entry a session opened from, and the
//                                 carrier half of its memory key
//   …slow-link profile engaged…   the slow-link profile and the entry that engaged it
//   FR-59 P8 …                    an opening at an entry below the nominal floor
//   …opener grace over…           the opener's measurement, the memory's second evidence
//   …measured rate remembered…    a write to the rate memory: its evidence and kept value
//
//   Only the FIELDS below leave the database, so no address, name or other line
//   does. Two leave transformed, never raw: `peer` (the memory key,
//   "<address>|<carrier>") as `peer_carrier`, the part after the bar, and
//   `controller_user_id` as `user6`, its last 6 hex. `controller_name` and every
//   `*_addr` field are never projected.
//
// THE CARRIER CLASS
//
//   From `session carrier recorded`: direct · turn · turn-local · mesh-direct ·
//   mesh-relayed/turn · mesh-relayed/derp · mesh-relayed/org · unknown. A build
//   without that line falls back to the ICE line: `relay=true` reads `turn (ice)`;
//   a non-relay pair the pump runs constrained is an overlay remote on a relay
//   tier, `mesh-relayed (pump)`; any other is `direct or mesh (ice)`.
//   What the lines cannot tell:
//   - TURN over UDP from TURN over TCP or TLS. The agent's relay candidate names
//     its relayed address, which is UDP whatever carries it to the server
//     (crates/vendored/webrtc-ice/src/agent/agent_gather.rs:829), and the
//     browser's own transport to its TURN server never reaches the agent. So
//     `turn` is either.
//   - The mesh relay's own transport, except on a session that read the memory:
//     its key carries it (`relay:derp/tcp`), printed as the key's carrier.
//   - A carrier change inside a session: the line is read once, 2 s after the
//     nomination. A run of the demonstrated rate breaks where the pump's
//     `constrained` flag flips, and only runs on the recorded class's side count.
//
// THE PAIR, AND WHAT IT DEMONSTRATED
//
//   A pair is the agent, the viewer's end and the carrier class. The lines read
//   here identify the viewer's end only by the controlling user (`incoming
//   session request`). The viewer's device and network are missing: the ICE and
//   memory lines carry them as addresses, which this report never reads. So one
//   user viewing from two networks is one pair here, and a session whose request
//   line fell outside the window belongs to the user `?`. FR-148 §9 O1 is the
//   open decision this feeds.
//
//   The DEMONSTRATED rate of a pair is the highest rate it HELD on one carrier
//   class within the read window: for every run of K consecutive heartbeat
//   windows of one pump that are all rated and all clean, the run's lowest
//   delivered rate; the pair's demonstrated rate is the highest of those.
//   - delivered = bytes_written over the gap between two heartbeats of one pump
//     (RATE_MIN_GAP_MS to RATE_MAX_GAP_MS apart): what the transport took, which
//     a window with no elevated age also delivered;
//   - clean = the viewer reported an age and it sat under AGE_SLACK_MS over its
//     floor. A window with no age is not clean;
//   - K = DEMO_K: 15 FFmpeg heartbeats (≈2 s each) or 30 VP9-444 ones (≈1 s
//     each), a run of about DEMO_RUN_MS;
//   - a run breaks at a window that is not rated or not clean, at a new pump, and
//     where `constrained` flips; a run on the side the session's class does not
//     run on is not counted.
//   It is a LOWER BOUND on what the path carries, never an estimate of its
//   capacity: today's lines cannot tell an idle screen from a full pipe (FR-148
//   P0 step 2 adds that flag). It is printed twice: over the whole read window,
//   and over the pair's sessions that ended before this one opened (AC6's
//   reading: what the pair had last demonstrated).
//
// THE PLAN AGAINST THE ENCODER, AND THE PICTURE
//
//   The plan is the heartbeat's `target_bps` (the governor's applied target). The
//   encoder is its `maxrate_bps` (`enc.current_maxrate_bps()`, FR-135 R0), which
//   older builds and every VP9-444 heartbeat lack. A move is a change of
//   `maxrate_bps` between two heartbeats of one pump. The rate-apply counters
//   (rate_moves + rebuilds + swaps, per encoder) are counted beside it as `ctr`.
//   The encoder moved 0 times only when every source read says 0.
//   The picture: avg_qp's p50 and p90 (in the codec's own scale), the viewer's
//   paint age p50 and p90, and the stalled windows: the pipe classifier's
//   `transit-stalled` windows out of those it gave a verdict (`pipe_states`,
//   cumulative per pump, less `unknown`). It judges constrained windows only,
//   so a direct session has none to count.
//
// THE MEMORY TRAIL, INFERRED
//
//   A write carries no session id. It is the pump's last act, so it goes to the
//   session of the same agent whose pump ended at most WRITE_LAG_MS before it,
//   preferring one whose memory key had the same carrier. No line says what
//   stood behind a write, so each is INFERRED from what its session logged:
//   - nothing: no evidence; the entry kept its value and its age;
//   - measured: it equals the last heartbeat's goodput_bps or the opener's
//     growth_target_bps;
//   - the prior's stand-in: it equals the last heartbeat's prior_bps in a window
//     that measured nothing, or it is the opening entry raised by a quarter n
//     times (FR-148 §2.1 step 5: 233,769 raised five times is 713,402);
//   - unexplained: nothing logged matches. The ceiling learner's stable rate and
//     the viewer's arrival rate are not logged, so a write from either reads so.
//   The kept value is checked against `rate_memory::damp` (half the way down, a
//   tenth of the way up) from the entry the session opened from, or as a first
//   write adopted outright when it opened from none.
//
// THE FLAGS
//
//   F1, opened below what the pair demonstrated: the opening (the first
//   heartbeat's target_bps, judged only when the pump's start line was read) is
//   below F1_RATIO × the pair's demonstrated rate in the window. AC6's line,
//   AC6_RATIO, is counted beside it, against the window and the before-open
//   figure alike.
//   F2, the plan moved and the encoder did not: the target's max is at least
//   F2_SPREAD × its min, the session lasted at least F2_MIN_SESSION_MS (its pump
//   start to its last heartbeat), and the encoder's applied rate moved 0 times.
//
// BOUNDED BY CONSTRUCTION
//
//   The server runs the query inside mongodb-0, whose 2 GiB an unbounded read
//   once exhausted (2026-10-26). The fleet read (`runMongosh`) is bounded the
//   way the shadow report's is:
//   - a window of at most 8 days (`agent_logs` keeps 7), read one day per query;
//   - `$match` on `source` and a `created_at` range, then `$sort` on
//     `created_at`, both served by an index the query pins with `hint`:
//     `created_at_1` for the fleet (and for `--agent` alone),
//     `tenant_id_1_agent_id_1_created_at_-1` for one agent of one `--tenant`,
//     `tenant_id_1_source_1_created_at_-1` for any other `--tenant` read. Each
//     returns the batches in `created_at` order, so there is no blocking SORT
//     stage and no in-memory sort (`--explain` prints the plan);
//   - `maxTimeMS` 240 s per query, and `allowDiskUse: false`;
//   - only streaming stages after the match: a `$project` whose `$filter` keeps
//     the lines of the messages above, `$unwind`, and a `$project` of the
//     FIELDS. mongosh folds one row at a time into per-session counters and
//     sparse histograms, and keeps only one record per memory write;
//   - the cost: every batch in the window is examined once. `agent_logs` held
//     354,193 batches across its 7 days on 2026-11-10
//     (docs/production-mongo-reads.md §5), so a 7-day run examines about 350 k
//     documents, about 50 k per daily query, all through the index: the scan of
//     one run of the shadow report over the same window. What leaves the
//     server is the matching lines' FIELDS only.
//
// RUN
//
//   node scripts/rc-path-report.cjs --self-test
//   node scripts/rc-path-report.cjs [--json] [--label HEX=NAME]... [--agent HEX] FILE...   # rows or host log text
//   node scripts/rc-path-report.cjs --to-ejson [--agent HEX] [--tenant HEX] FILE...        # a seed from host logs
//   node scripts/rc-path-report.cjs --fixtures                                             # the self-test's chains, as a seed
//   bash scripts/rc-path-report.sh --docker NAME --seed FILE                               # the rehearsal
//   bash scripts/rc-path-report.sh --mars --yes [--since ISO] [--until ISO]                # the fleet read
//
// The core below runs unchanged under Node and under mongosh, so it uses no
// Node-only API; the runners at the bottom are the only environment-specific
// code. It is `.cjs` because it is CommonJS and the repo's root package.json
// declares no module type.

'use strict';

// ---------------------------------------------------------------- constants

/** `encode::ratectl::AGE_SLACK_MS`: age over the learned floor that is an
 *  elevated LEVEL. The self-test checks it against the Rust source. */
const AGE_SLACK_MS = 70;
/** The demonstrated rate's run, about 30 s, as heartbeats of each pump. */
const DEMO_RUN_MS = 30000;
const DEMO_K = { ffmpeg: 15, vp9: 30 };
/** Two heartbeats of one pump make a rate only this far apart, as in the
 *  shadow report: closer is a replay, and further is a hole in the log. */
const RATE_MIN_GAP_MS = 500;
const RATE_MAX_GAP_MS = 10000;
/** F1: the opening is below this share of the pair's demonstrated rate. */
const F1_RATIO = 0.5;
/** AC6's line: a reconnect opens at this share of the demonstrated floor or higher. */
const AC6_RATIO = 0.85;
/** F2: the target's max is at least this multiple of its min, over a session at
 *  least this long, and the encoder's applied rate moved 0 times. */
const F2_SPREAD = 1.5;
const F2_MIN_SESSION_MS = 5 * 60 * 1000;
/** `encode::goodput::ALPHA_DOWN` / `ALPHA_UP`, which `rate_memory::damp` uses. */
const ALPHA_DOWN = 0.5;
const ALPHA_UP = 0.1;
/** `encode::prior::DECAY_NUM` / `DECAY_DEN`: the stand-in from a remembered
 *  seed grows by a quarter per step. */
const DECAY_NUM = 5;
const DECAY_DEN = 4;
/** `encode::MIN_BITRATE_BPS`: the band at which the prior stops standing in. */
const PRIOR_BAND_BPS = 1500000;
/** Quarter steps tried when a write is matched to its seed's growth; any seed
 *  of 1 bps or more reaches the band within them. */
const MAX_DECAY_STEPS = 64;
/** A memory write follows its pump's last heartbeat by at most this much. */
const WRITE_LAG_MS = 15000;

/** The lines this reads, by message text. Each phrase is in exactly one
 *  message of the agent (the self-test checks the source). */
const LINES = {
  heartbeat: /DC pump heartbeat/,
  start: /DC pump starting/,
  request: /incoming session request/,
  carrier: /session carrier recorded/,
  ice: /per-session ICE path detected/,
  seed: /rate memory: opening ceiling seeded/,
  slowLink: /slow-link profile engaged/,
  p8: /remembered slower than the nominal floor/,
  opener: /opener grace over/,
  write: /measured rate remembered for the pair/,
};
/** The same, as one pattern for the server-side `$regexMatch`. */
const MSG_PATTERN = Object.keys(LINES)
  .map((k) => LINES[k].source)
  .join('|');

/** The only fields that leave the database, besides the two transformed ones
 *  (`peer_carrier`, `user6`; see `pipelineFor`). */
const FIELDS = [
  'session_id',
  'codec_label',
  'encoder',
  'constrained',
  'target_bps',
  'target_fps',
  'width',
  'height',
  'bytes_written',
  'viewer_age_ms',
  'viewer_age_floor_ms',
  'avg_qp',
  'maxrate_bps',
  'rate_moves',
  'rebuilds',
  'swaps',
  'goodput_bps',
  'prior_bps',
  'pipe_states',
  'relay',
  'carrier',
  'rc_overlay',
  'slow_link',
  'remembered_bps',
  'max_long_edge',
  'max_fps',
  'seed_bps',
  'evidence_bps',
  'kept_bps',
  'growth_target_bps',
];
/** Identifiers and labels: emitted as text, so an all-digit value stays a string. */
const ID_FIELDS = new Set(['session_id', 'codec_label', 'encoder', 'carrier', 'peer_carrier', 'user6']);

/** The carrier line's vocabulary (`session_carrier::SessionCarrier::as_str`). */
const CARRIER_CLASS = {
  direct: 'direct',
  turn: 'turn',
  turn_local: 'turn-local',
  overlay_direct: 'mesh-direct',
  overlay_turn: 'mesh-relayed/turn',
  derp: 'mesh-relayed/derp',
  org_relay: 'mesh-relayed/org',
  unknown: 'unknown',
};

// ---------------------------------------------------------------- values

/**
 * One field value, from either source, as a plain JS value.
 * - text log:   `48000000`, `Some(6)`, `None`, `"hold"`, `false`, `Some("x")`
 * - agent_logs: numbers, booleans and strings; a `?Debug` field is the string
 *               "Some(6)" / "None", a `&str` field its text.
 * - mongosh:    an Int64 arrives as a bson Long.
 */
function value(v) {
  if (v === null || v === undefined) return null;
  if (typeof v === 'number' || typeof v === 'boolean') return v;
  if (typeof v === 'object') {
    if (typeof v.toNumber === 'function') return v.toNumber(); // bson Long
    if (typeof v.toHexString === 'function') return v.toHexString(); // ObjectId
    return null;
  }
  let s = String(v).trim();
  if (s === 'None' || s === '') return null;
  const some = /^Some\((.*)\)$/.exec(s);
  if (some) s = some[1].trim();
  if (s.length >= 2 && s[0] === '"' && s[s.length - 1] === '"') return s.slice(1, -1);
  if (s === 'true') return true;
  if (s === 'false') return false;
  if (/^-?\d+(\.\d+)?$/.test(s)) return Number(s);
  return s;
}

function num(v) {
  const x = value(v);
  return typeof x === 'number' && Number.isFinite(x) ? x : null;
}

/** An identifier or a label: quotes and `Some(…)` stripped, never read as a number. */
function text(v) {
  if (v === null || v === undefined) return null;
  if (typeof v === 'object') return typeof v.toHexString === 'function' ? v.toHexString() : null;
  let s = String(v).trim();
  if (s === 'None' || s === '') return null;
  const some = /^Some\((.*)\)$/.exec(s);
  if (some) s = some[1].trim();
  if (s.length >= 2 && s[0] === '"' && s[s.length - 1] === '"') s = s.slice(1, -1);
  return s === '' ? null : s;
}

function bool(v) {
  const x = value(v);
  return typeof x === 'boolean' ? x : null;
}

function last6(v) {
  const s = text(v);
  return s ? s.slice(-6).toLowerCase() : null;
}

/** The carrier half of a memory key ("<address>|<carrier>"), never the address. */
function keyCarrier(f) {
  if ('peer_carrier' in f) return text(f.peer_carrier);
  const s = text(f.peer);
  if (!s) return null;
  const i = s.indexOf('|');
  return i < 0 ? null : text(s.slice(i + 1));
}

function userOf(f) {
  return 'user6' in f ? last6(f.user6) : last6(f.controller_user_id);
}

/** `pipe_states`: `[unknown, clear, overproduced, transit_stalled, viewer_late]`. */
function pipeStates(v) {
  const s = text(v);
  const m = s && /^\[\s*(\d+)\s*,\s*(\d+)\s*,\s*(\d+)\s*,\s*(\d+)\s*,\s*(\d+)\s*\]$/.exec(s);
  return m ? m.slice(1).map(Number) : null;
}

/** The windows the classifier gave a verdict: every one but `unknown`, which
 *  is an unconstrained window or one with no age split (governor.rs, `PipeState::Unknown`). */
function judgedWindows(states) {
  return states[1] + states[2] + states[3] + states[4];
}

function epochMs(ts) {
  if (ts === null || ts === undefined) return null;
  if (ts instanceof Date) return ts.getTime();
  const s = String(ts);
  const ms = Date.parse(s.endsWith('Z') ? s : s + 'Z');
  return Number.isFinite(ms) ? ms : null;
}

function iso(ms) {
  return ms === null || ms === undefined ? null : new Date(ms).toISOString();
}

function bump(map, key, by) {
  map.set(key, (map.get(key) || 0) + (by || 1));
}

/** Nearest-rank percentile of a sparse histogram (value -> count). */
function percentile(hist, p) {
  let total = 0;
  for (const c of hist.values()) total += c;
  if (!total) return null;
  const rank = Math.max(1, Math.ceil(p * total));
  const keys = [...hist.keys()].sort((a, b) => a - b);
  let seen = 0;
  for (const k of keys) {
    seen += hist.get(k);
    if (seen >= rank) return k;
  }
  return keys[keys.length - 1];
}

function spanOf(hist) {
  if (!hist.size) return null;
  let min = Infinity;
  let max = -Infinity;
  let windows = 0;
  for (const [k, c] of hist) {
    if (k < min) min = k;
    if (k > max) max = k;
    windows += c;
  }
  return { min, p50: percentile(hist, 0.5), max, windows };
}

// ---------------------------------------------------------------- the agent's arithmetic

/** `rate_memory::damp`: move a remembered rate toward a measurement, fast down
 *  and slow up, in the same float arithmetic and the same truncation. */
function damp(old, measured) {
  const alpha = measured < old ? ALPHA_DOWN : ALPHA_UP;
  const next = old + alpha * (measured - old);
  return Math.trunc(Math.min(Math.max(next, 0), 4294967295));
}

/** The prior's stand-in from a remembered seed after `n` steps
 *  (`prior::RatePrior::stand_in_bps`), or null at or above the band. */
function standIn(seed, n) {
  let v = seed;
  for (let i = 0; i < n; i++) {
    if (v >= PRIOR_BAND_BPS) break;
    v = Math.floor((v * DECAY_NUM) / DECAY_DEN);
  }
  return v < PRIOR_BAND_BPS ? v : null;
}

/** How many quarter steps take `seed` to `x` under the band, or null. */
function quarterSteps(seed, x) {
  let v = seed;
  for (let n = 0; n <= MAX_DECAY_STEPS; n++) {
    if (v >= PRIOR_BAND_BPS) return null;
    if (v === x) return n;
    v = Math.floor((v * DECAY_NUM) / DECAY_DEN);
  }
  return null;
}

// ---------------------------------------------------------------- text lines

/**
 * One rolling-log line: `<ts>  <LEVEL> <message> key=value key=value …`.
 * Returns `{ ts, level, msg, fields }`, or null for anything else. A value may
 * hold spaces inside (), {}, [] or quotes, so a new field starts only at depth
 * 0, on a space followed by `identifier=` (the shadow report's tokenizer).
 */
function parseLogLine(line) {
  const m = /^(\d{4}-\d{2}-\d{2}T[0-9:.]+Z?)\s+(TRACE|DEBUG|INFO|WARN|ERROR)\s+(.*)$/.exec(line.replace(/\r$/, ''));
  if (!m) return null;
  const rest = m[3];
  const starts = [];
  let depth = 0;
  let quoted = false;
  for (let i = 0; i < rest.length; i++) {
    const ch = rest[i];
    if (quoted) {
      if (ch === '\\') i++;
      else if (ch === '"') quoted = false;
      continue;
    }
    if (ch === '"') quoted = true;
    else if (ch === '(' || ch === '{' || ch === '[') depth++;
    else if (ch === ')' || ch === '}' || ch === ']') depth = Math.max(0, depth - 1);
    else if (ch === ' ' && depth === 0) {
      const k = /^ ([a-z_][a-z0-9_.]*)=/.exec(rest.slice(i, i + 64));
      if (k) starts.push({ at: i + 1, key: k[1], valueAt: i + 1 + k[1].length + 1 });
    }
  }
  const msg = (starts.length ? rest.slice(0, starts[0].at - 1) : rest).trim();
  const fields = {};
  for (let j = 0; j < starts.length; j++) {
    const end = j + 1 < starts.length ? starts[j + 1].at - 1 : rest.length;
    fields[starts[j].key] = rest.slice(starts[j].valueAt, end);
  }
  return { ts: m[1], level: m[2], msg, fields };
}

function kindOf(msg) {
  for (const k of Object.keys(LINES)) if (LINES[k].test(msg)) return k;
  return null;
}

// ---------------------------------------------------------------- labels

/** `HEX=NAME,HEX=NAME`: an agent whose id ends with HEX prints as NAME. */
function parseLabels(spec) {
  const out = [];
  for (const part of String(spec || '').split(',')) {
    const p = part.trim();
    if (!p) continue;
    const m = /^([0-9a-fA-F]{1,24})=([A-Za-z0-9._-]{1,32})$/.exec(p);
    if (!m) throw new Error(`a label is HEX=NAME (HEX up to 24 hex digits, NAME letters, digits, ".", "_" or "-"): ${p}`);
    out.push([m[1].toLowerCase(), m[2]]);
  }
  return out;
}

function agentName(hex, labels) {
  if (!hex) return '?';
  const h = String(hex).toLowerCase();
  for (const [suffix, name] of labels || []) if (h.endsWith(suffix)) return name;
  return h.slice(-6);
}

// ---------------------------------------------------------------- the fold

function classConstrained(cls) {
  if (cls === 'direct' || cls === 'turn-local' || cls === 'mesh-direct' || cls === 'direct or mesh (ice)') return false;
  if (cls === 'turn' || cls === 'turn (ice)' || cls.indexOf('mesh-relayed') === 0) return true;
  return null;
}

function classify(s) {
  if (s.carrier) return { cls: CARRIER_CLASS[s.carrier] || s.carrier, from: 'carrier line' };
  if (s.iceRelay !== null) {
    if (s.iceRelay) return { cls: 'turn (ice)', from: 'ice line' };
    if (s.firstConstrained === true) return { cls: 'mesh-relayed (pump)', from: 'ice line and pump flag' };
    return { cls: 'direct or mesh (ice)', from: 'ice line' };
  }
  return { cls: 'unknown', from: 'none' };
}

/** The entry a session opened from, when a line shows it. */
function entryOf(s) {
  if (s.seed !== null) return s.seed;
  if (s.p8 !== null) return s.p8;
  if (s.slow && s.slow.remembered !== null) return s.slow.remembered;
  return null;
}

function judgeF1(openBps, demoBps) {
  if (!(openBps > 0) || !(demoBps > 0)) return null;
  return { below: openBps < F1_RATIO * demoBps, below85: openBps < AC6_RATIO * demoBps };
}

/** `x`: { durationMs, tmin, tmax, appliedSeen, moves, ctrSeen, ctrMoves }. */
function judgeF2(x) {
  if (x.durationMs === null || x.durationMs < F2_MIN_SESSION_MS) return null;
  if (!(x.tmin > 0) || !(x.tmax > 0)) return null;
  const haveApplied = x.appliedSeen >= 2;
  const haveCtr = x.ctrSeen >= 1;
  if (!haveApplied && !haveCtr) return null;
  const spread = x.tmax >= F2_SPREAD * x.tmin;
  const still = (!haveApplied || x.moves === 0) && (!haveCtr || x.ctrMoves === 0);
  return { spread, still, raised: spread && still };
}

/** What stood behind a memory write, inferred from its session's lines. */
function provenanceOf(w, s, entry) {
  if (w.evidence === null || w.evidence <= 0) {
    return { basis: 'nothing', rule: 'no evidence: the entry kept its value and its age' };
  }
  const x = w.evidence;
  if (!s) return { basis: 'unexplained', rule: 'no session to match it against' };
  if (s.last && s.last.goodput === x) return { basis: 'measured', rule: "equals the last heartbeat's goodput_bps" };
  if (s.opener && s.opener.growth === x) return { basis: 'measured', rule: "equals the opener's growth_target_bps" };
  if (s.last && s.last.prior === x && s.last.goodput === null) {
    return { basis: 'stand-in', rule: "equals the last heartbeat's prior_bps, in a window that measured nothing" };
  }
  if (entry !== null) {
    const n = quarterSteps(entry, x);
    if (n !== null) return { basis: 'stand-in', rule: `the opening entry raised by a quarter ${n} time${n === 1 ? '' : 's'}` };
  }
  return { basis: 'unexplained', rule: 'no logged value matches it' };
}

function writeOf(w, entry) {
  if (w.evidence === null || w.evidence <= 0) return 'nothing written';
  if (entry !== null) {
    const d = damp(entry, w.evidence);
    if (d === w.kept) return `damped from ${mbps(entry)}`;
    if (w.kept === w.evidence) return 'adopted outright, though an entry was read';
    return `kept differs from damp(entry, evidence) = ${mbps(d)}`;
  }
  if (w.kept === w.evidence) return 'adopted outright: a first write, no entry read at the open';
  return 'kept differs from the evidence, and no entry was read';
}

/** The session a write belongs to: same agent, a pump that ended at most
 *  WRITE_LAG_MS before it, the same key carrier when both are known. */
function attribute(w, rowsOfAgent) {
  const near = [];
  for (const row of rowsOfAgent) {
    let best = null;
    for (const e of row.ends) {
      if (e <= w.at && w.at - e <= WRITE_LAG_MS && (best === null || e > best)) best = e;
    }
    if (best !== null) near.push({ row, end: best });
  }
  if (!near.length) return null;
  const same = near.filter((n) => w.tag === null || n.row.s.seedTag === null || n.row.s.seedTag === w.tag);
  const pool = same.length ? same : near;
  pool.sort((a, b) => b.end - a.end || (a.row.s.sid < b.row.s.sid ? -1 : 1));
  return pool[0].row;
}

class PathReport {
  constructor(opts) {
    this.labels = (opts && opts.labels) || [];
    this.sessions = new Map();
    this.writes = [];
    this.writeKeys = new Set();
    this.lines = 0;
    this.read = 0;
    this.heartbeats = 0;
    this.replays = 0;
    this.first = null;
    this.last = null;
  }

  session(agent, sid) {
    const key = (agent || '') + '/' + sid;
    let s = this.sessions.get(key);
    if (!s) {
      s = {
        agent: agent || null,
        sid,
        user: null,
        rcOverlay: null,
        carrier: null,
        iceRelay: null,
        firstConstrained: null,
        startAt: null,
        startSlow: null,
        hbFirst: null,
        hbLast: null,
        kinds: new Set(),
        encoder: null,
        codec: null,
        open: null,
        slow: null,
        seed: null,
        seedTag: null,
        p8: null,
        opener: null,
        pump: null,
        pumps: 0,
        ends: [],
        demo: { c: 0, d: 0, u: 0 },
        target: new Map(),
        applied: new Map(),
        qp: new Map(),
        age: new Map(),
        moves: 0,
        appliedSeen: 0,
        ctrMoves: 0,
        ctrSeen: 0,
        transit: 0,
        classified: 0,
        last: null,
      };
      this.sessions.set(key, s);
    }
    return s;
  }

  /** Feed one line: its message, its fields (raw values from either source),
   *  its timestamp, and the agent it came from (null for one host's text). */
  add(msg, f, ts, agent) {
    this.lines++;
    if (!msg || !f) return;
    const kind = kindOf(msg);
    if (!kind) return;
    const at = epochMs(ts);
    if (at === null) return;
    this.read++;
    if (this.first === null || at < this.first) this.first = at;
    if (this.last === null || at > this.last) this.last = at;
    if (kind === 'write') {
      const w = { agent: agent || null, at, tag: keyCarrier(f), evidence: num(f.evidence_bps), kept: num(f.kept_bps) };
      const key = [w.agent, at, w.tag, w.evidence, w.kept].join('|');
      if (!this.writeKeys.has(key)) {
        this.writeKeys.add(key);
        this.writes.push(w);
      }
      return;
    }
    const sid = text(f.session_id);
    if (!sid) return;
    const s = this.session(agent, sid);
    switch (kind) {
      case 'request': {
        const u = userOf(f);
        if (u) s.user = u;
        const o = bool(f.rc_overlay);
        if (o !== null) s.rcOverlay = o;
        break;
      }
      case 'carrier': {
        const c = text(f.carrier);
        if (c) s.carrier = c;
        break;
      }
      case 'ice': {
        const relay = bool(f.relay);
        if (relay !== null && s.iceRelay === null) s.iceRelay = relay;
        break;
      }
      case 'start': {
        if (s.startAt === null || at < s.startAt) s.startAt = at;
        const sl = bool(f.slow_link);
        if (sl !== null && s.startSlow === null) s.startSlow = sl;
        break;
      }
      case 'slowLink':
        if (!s.slow) s.slow = { remembered: num(f.remembered_bps), edge: num(f.max_long_edge), fps: num(f.max_fps) };
        break;
      case 'seed':
        if (s.seed === null) {
          s.seed = num(f.seed_bps);
          s.seedTag = keyCarrier(f);
        }
        break;
      case 'p8':
        if (s.p8 === null) s.p8 = num(f.seed_bps);
        break;
      case 'opener':
        if (!s.opener) s.opener = { growth: num(f.growth_target_bps) };
        break;
      case 'heartbeat':
        this.heartbeat(s, msg, f, at);
        break;
    }
  }

  /** Feed one NDJSON / fleet-read row: `{ a, t, m, f }`. */
  addRow(row) {
    if (!row) return;
    this.add(row.m, row.f || {}, row.t instanceof Date ? row.t.toISOString() : row.t, row.a ? text(row.a) : null);
  }

  closePump(s) {
    const p = s.pump;
    if (!p) return;
    if (p.states) {
      s.transit += p.states[3];
      s.classified += judgedWindows(p.states);
    }
    if (p.lastAt !== null) s.ends.push(p.lastAt);
  }

  heartbeat(s, msg, f, at) {
    const kind = /VP9-444/.test(msg) ? 'vp9' : 'ffmpeg';
    const bytes = num(f.bytes_written);
    let p = s.pump;
    // At or behind this pump's last heartbeat: a batch uploaded twice, or one
    // that reached the server after a later one. Never a window, and it must not
    // rewind the mark. Checked before the new-pump test, since such a batch also
    // reads a fall in bytes_written.
    if (p && p.kind === kind && p.lastAt !== null && at <= p.lastAt) {
      this.replays++;
      return;
    }
    const fresh = !p || p.kind !== kind || (bytes !== null && p.bytes !== null && bytes < p.bytes);
    if (fresh) {
      this.closePump(s);
      p = s.pump = { kind, at: null, bytes: null, lastAt: null, run: [], runC: undefined, ctr: null, maxrate: null, states: null };
      s.pumps++;
    }
    this.heartbeats++;
    s.kinds.add(kind);
    if (s.hbFirst === null) s.hbFirst = at;
    s.hbLast = at;
    p.lastAt = at;
    const c = kind === 'vp9' ? null : bool(f.constrained);
    if (s.firstConstrained === null && c !== null) s.firstConstrained = c;
    if (!s.encoder) {
      s.encoder = kind === 'vp9' ? 'vp9-444' : text(f.encoder);
      s.codec = kind === 'vp9' ? 'VP9' : text(f.codec_label);
    }
    const target = num(f.target_bps);
    const maxrate = num(f.maxrate_bps);
    if (!s.open) {
      s.open = {
        at,
        bps: target,
        width: kind === 'vp9' ? null : num(f.width),
        height: kind === 'vp9' ? null : num(f.height),
        fps: num(f.target_fps),
      };
    }
    if (target !== null && target > 0) bump(s.target, target);
    if (maxrate !== null && maxrate > 0) {
      bump(s.applied, maxrate);
      s.appliedSeen++;
      if (p.maxrate !== null && maxrate !== p.maxrate) s.moves++;
      p.maxrate = maxrate;
    }
    // Per encoder, from 0: a fall is a new encoder, whose count starts over.
    const rm = num(f.rate_moves);
    const rb = num(f.rebuilds);
    const sw = num(f.swaps);
    if (rm !== null && rb !== null && sw !== null) {
      const tot = rm + rb + sw;
      s.ctrMoves += p.ctr === null || tot < p.ctr ? tot : tot - p.ctr;
      p.ctr = tot;
      s.ctrSeen++;
    }
    const qp = num(f.avg_qp);
    if (qp !== null && qp > 0) bump(s.qp, qp);
    const age = num(f.viewer_age_ms);
    const floor = num(f.viewer_age_floor_ms);
    if (age !== null && age > 0) bump(s.age, age);
    const st = pipeStates(f.pipe_states);
    if (st) p.states = st;
    s.last = { at, goodput: num(f.goodput_bps), prior: num(f.prior_bps) };

    // The delivered rate since this pump's last rated mark, and the run.
    let rate = null;
    if (p.at !== null && p.bytes !== null && bytes !== null) {
      const gap = at - p.at;
      // Too close to the mark: no window of its own, and no break either.
      if (gap < RATE_MIN_GAP_MS) return;
      if (gap <= RATE_MAX_GAP_MS) rate = ((bytes - p.bytes) * 8000) / gap;
    }
    p.at = at;
    if (bytes !== null) p.bytes = bytes;
    const clean = age !== null && floor !== null && age - floor < AGE_SLACK_MS;
    this.extendRun(s, p, rate, clean, c);
  }

  extendRun(s, p, rate, clean, c) {
    if (rate === null || !clean) {
      p.run = [];
      p.runC = undefined;
      return;
    }
    if (p.run.length && p.runC !== c) p.run = [];
    p.runC = c;
    p.run.push(rate);
    const k = DEMO_K[p.kind];
    if (p.run.length > k) p.run.shift();
    if (p.run.length === k) {
      let low = Infinity;
      for (const r of p.run) if (r < low) low = r;
      const side = c === true ? 'c' : c === false ? 'd' : 'u';
      if (low > s.demo[side]) s.demo[side] = low;
    }
  }

  finish() {
    const labels = this.labels;
    const rows = [];
    let noCarrierLine = 0;
    let noStart = 0;
    let noHeartbeat = 0;
    const agents = new Set();
    for (const s of this.sessions.values()) {
      if (s.hbFirst === null) {
        noHeartbeat++;
        continue;
      }
      agents.add(s.agent || '');
      const k = classify(s);
      if (k.from !== 'carrier line') noCarrierLine++;
      const started = s.startAt !== null && s.startAt <= s.hbFirst;
      if (!started) noStart++;
      // The open pump's figures, without closing it: finish() may run twice.
      const p = s.pump;
      const transit = s.transit + (p && p.states ? p.states[3] : 0);
      const classified = s.classified + (p && p.states ? judgedWindows(p.states) : 0);
      const ends = s.ends.concat(p && p.lastAt !== null ? [p.lastAt] : []);
      const want = classConstrained(k.cls);
      const side = want === true ? s.demo.c : want === false ? s.demo.d : Math.max(s.demo.c, s.demo.d);
      const demo = Math.max(side, s.demo.u);
      rows.push({
        s,
        cls: k.cls,
        from: k.from,
        startMs: started ? s.startAt : s.hbFirst,
        endMs: s.hbLast,
        started,
        openJudged: started && s.open !== null && s.open.bps > 0,
        demoSession: demo > 0 ? demo : null,
        transit,
        classified,
        ends,
      });
    }
    rows.sort((a, b) => a.startMs - b.startMs || ((a.s.agent || '') < (b.s.agent || '') ? -1 : (a.s.agent || '') > (b.s.agent || '') ? 1 : a.s.sid < b.s.sid ? -1 : 1));

    // Pairs: the agent, the controlling user and the carrier class.
    const pairs = new Map();
    for (const row of rows) {
      const key = [row.s.agent || '', row.s.user || '?', row.cls].join('|');
      row.pairKey = key;
      let p = pairs.get(key);
      if (!p) {
        p = { key, agent: row.s.agent, user: row.s.user, cls: row.cls, rows: [], demo: null, demoIn: null, writes: { measured: 0, standIn: 0, nothing: 0, unexplained: 0 } };
        pairs.set(key, p);
      }
      p.rows.push(row);
      if (row.demoSession !== null && (p.demo === null || row.demoSession > p.demo)) {
        p.demo = row.demoSession;
        p.demoIn = row.s.sid;
      }
    }
    for (const p of pairs.values()) {
      for (const row of p.rows) {
        let before = null;
        for (const o of p.rows) {
          if (o !== row && o.endMs <= row.startMs && o.demoSession !== null && (before === null || o.demoSession > before)) before = o.demoSession;
        }
        row.demoBefore = before;
        row.demoWindow = p.demo;
      }
    }

    // The memory trail.
    const byAgent = new Map();
    for (const row of rows) {
      const k = row.s.agent || '';
      if (!byAgent.has(k)) byAgent.set(k, []);
      byAgent.get(k).push(row);
    }
    const trail = this.writes
      .slice()
      .sort((a, b) => a.at - b.at)
      .map((w) => {
        const row = attribute(w, byAgent.get(w.agent || '') || []);
        const s = row ? row.s : null;
        const entry = s ? entryOf(s) : null;
        const prov = provenanceOf(w, s, entry);
        if (row) {
          const pw = pairs.get(row.pairKey).writes;
          if (prov.basis === 'measured') pw.measured++;
          else if (prov.basis === 'stand-in') pw.standIn++;
          else if (prov.basis === 'nothing') pw.nothing++;
          else pw.unexplained++;
        }
        return {
          at: iso(w.at),
          agent: agentName(w.agent, labels),
          user: s ? s.user : null,
          carrier: row ? row.cls : null,
          session: s ? last6(s.sid) : null,
          keyCarrier: w.tag,
          entryBps: entry,
          evidenceBps: w.evidence,
          keptBps: w.kept,
          basis: prov.basis,
          measured: prov.basis === 'measured',
          rule: prov.rule,
          write: writeOf(w, entry),
          inferred: true,
        };
      });

    // Sessions and their flags.
    const f1 = { judged: 0, below: 0, below85: 0, judgedBefore: 0, belowBefore: 0, below85Before: 0 };
    const f2 = { judged: 0, spread: 0, raised: 0 };
    const sessions = rows.map((row) => {
      const s = row.s;
      const t = spanOf(s.target);
      const a = spanOf(s.applied);
      const one = row.openJudged ? judgeF1(s.open.bps, row.demoWindow) : null;
      const before = row.openJudged ? judgeF1(s.open.bps, row.demoBefore) : null;
      const two = judgeF2({
        durationMs: row.endMs - row.startMs,
        tmin: t ? t.min : null,
        tmax: t ? t.max : null,
        appliedSeen: s.appliedSeen,
        moves: s.moves,
        ctrSeen: s.ctrSeen,
        ctrMoves: s.ctrMoves,
      });
      if (one) {
        f1.judged++;
        if (one.below) f1.below++;
        if (one.below85) f1.below85++;
      }
      if (before) {
        f1.judgedBefore++;
        if (before.below) f1.belowBefore++;
        if (before.below85) f1.below85Before++;
      }
      if (two) {
        f2.judged++;
        if (two.spread) f2.spread++;
        if (two.raised) f2.raised++;
      }
      const pair = pairs.get(row.pairKey);
      return {
        start: iso(row.startMs),
        durationS: Math.round((row.endMs - row.startMs) / 1000),
        agent: agentName(s.agent, labels),
        session: last6(s.sid),
        user: s.user,
        carrier: row.cls,
        carrierFrom: row.from,
        keyCarrier: s.seedTag,
        rcOverlay: s.rcOverlay,
        encoder: s.encoder,
        codec: s.codec,
        pumps: s.pumps,
        opening: {
          bps: s.open ? s.open.bps : null,
          width: s.open ? s.open.width : null,
          height: s.open ? s.open.height : null,
          fps: s.open ? s.open.fps : null,
          judged: row.openJudged,
        },
        slowLink: s.slow ? { engaged: true, longEdge: s.slow.edge, fps: s.slow.fps } : { engaged: s.startSlow === null ? null : s.startSlow },
        entryBps: entryOf(s),
        demonstrated: { pairWindow: row.demoWindow, pairBefore: row.demoBefore, session: row.demoSession, pairHeldIn: last6(pair.demoIn) },
        target: t,
        applied: a,
        moves: s.appliedSeen >= 2 ? s.moves : null,
        counterMoves: s.ctrSeen ? s.ctrMoves : null,
        picture: {
          qpP50: percentile(s.qp, 0.5),
          qpP90: percentile(s.qp, 0.9),
          ageP50: percentile(s.age, 0.5),
          ageP90: percentile(s.age, 0.9),
          stalled: row.transit,
          stalledOf: row.classified,
        },
        flags: {
          f1: one ? one.below : null,
          f1At85: one ? one.below85 : null,
          f1Before: before ? before.below : null,
          f1BeforeAt85: before ? before.below85 : null,
          f2: two ? two.raised : null,
          f2Spread: two ? two.spread : null,
        },
      };
    });

    const pairsOut = [...pairs.values()]
      .sort((a, b) => (a.key < b.key ? -1 : a.key > b.key ? 1 : 0))
      .map((p) => {
        let lo = null;
        let hi = null;
        let n1 = 0;
        let n2 = 0;
        for (const row of p.rows) {
          const i = rows.indexOf(row);
          const out = sessions[i];
          if (row.openJudged) {
            lo = lo === null ? out.opening.bps : Math.min(lo, out.opening.bps);
            hi = hi === null ? out.opening.bps : Math.max(hi, out.opening.bps);
          }
          if (out.flags.f1) n1++;
          if (out.flags.f2) n2++;
        }
        return {
          agent: agentName(p.agent, labels),
          user: p.user,
          carrier: p.cls,
          sessions: p.rows.length,
          demonstrated: p.demo,
          heldIn: last6(p.demoIn),
          openings: { min: lo, max: hi },
          f1: n1,
          f2: n2,
          writes: p.writes,
        };
      });

    const verdict = (n, judged) => (!judged ? 'not judged' : n ? 'RAISED' : 'clear');
    const f1Out = Object.assign({ ratio: F1_RATIO, ac6Ratio: AC6_RATIO }, f1, { verdict: verdict(f1.below, f1.judged) });
    f1Out.line = !f1.judged
      ? 'No session had both a judged opening and a pair with a demonstrated rate.'
      : `${f1.below} of ${f1.judged} judged sessions opened below ${F1_RATIO} × their pair's demonstrated rate in the window.`;
    const f2Out = Object.assign({ spreadRatio: F2_SPREAD, minSessionS: F2_MIN_SESSION_MS / 1000 }, f2, { verdict: verdict(f2.raised, f2.judged) });
    f2Out.line = !f2.judged
      ? `No session lasted ${F2_MIN_SESSION_MS / 60000} min with a target and an encoder rate read.`
      : `${f2.raised} of ${f2.judged} judged sessions: the target spread ${F2_SPREAD} × or more and the encoder's applied rate never moved.`;

    return {
      provenance: {
        lines: this.lines,
        linesRead: this.read,
        heartbeats: this.heartbeats,
        replays: this.replays,
        sessions: rows.length,
        sessionsWithoutHeartbeat: noHeartbeat,
        sessionsWithoutCarrierLine: noCarrierLine,
        sessionsWithoutStart: noStart,
        agents: agents.size,
        writes: this.writes.length,
        first: iso(this.first),
        last: iso(this.last),
      },
      constants: {
        ageSlackMs: AGE_SLACK_MS,
        demoRunMs: DEMO_RUN_MS,
        demoK: DEMO_K,
        rateGapMs: [RATE_MIN_GAP_MS, RATE_MAX_GAP_MS],
        f1Ratio: F1_RATIO,
        ac6Ratio: AC6_RATIO,
        f2Spread: F2_SPREAD,
        f2MinSessionS: F2_MIN_SESSION_MS / 1000,
        writeLagMs: WRITE_LAG_MS,
      },
      flags: { f1: f1Out, f2: f2Out },
      pairs: pairsOut,
      sessions,
      memoryTrail: trail,
    };
  }
}

// ---------------------------------------------------------------- render

function mbps(bps) {
  if (bps === null || bps === undefined || !Number.isFinite(bps)) return '–';
  return (bps / 1e6).toFixed(bps < 1e6 ? 3 : 2);
}

function minute(isoText) {
  return isoText ? isoText.slice(0, 16).replace('T', ' ') + 'Z' : '–';
}

function ran(s) {
  if (s === null || s === undefined) return '–';
  if (s < 120) return `${s} s`;
  if (s < 7200) return `${Math.round(s / 60)} min`;
  return `${(s / 3600).toFixed(1)} h`;
}

function span3(o) {
  return o ? `${mbps(o.min)} / ${mbps(o.p50)} / ${mbps(o.max)}` : '–';
}

function dash(v) {
  return v === null || v === undefined ? '–' : String(v);
}

function render(r) {
  const P = r.provenance;
  const L = [];
  L.push('# FR-148 path report', '');
  L.push(
    `Read ${P.first || '–'} → ${P.last || '–'}: ${P.linesRead} lines of the messages it reads, ${P.heartbeats} DC-pump heartbeats, ` +
      `${P.sessions} sessions on ${P.agents} agents, ${P.writes} memory writes. Rates in Mbit/s.`,
  );
  if (P.sessionsWithoutCarrierLine) L.push(`- ${P.sessionsWithoutCarrierLine} sessions had no carrier line: their class comes from the ICE line and the pump's flag, marked (ice) or (pump).`);
  if (P.sessionsWithoutStart) L.push(`- ${P.sessionsWithoutStart} sessions had no pump start read before their first heartbeat: their opening is not judged.`);
  if (P.sessionsWithoutHeartbeat) L.push(`- ${P.sessionsWithoutHeartbeat} sessions had lines but no DC-pump heartbeat, and are not listed.`);
  if (P.replays) L.push(`- ${P.replays} heartbeats repeated an earlier one of the same pump and were skipped.`);
  L.push('');

  const f1 = r.flags.f1;
  const f2 = r.flags.f2;
  L.push('## Flags', '');
  L.push(`**F1, opened below what the pair demonstrated: ${f1.verdict}.** ${f1.line}`, '');
  L.push(`| Yardstick | Judged | Opening < ${F1_RATIO} × (F1) | Opening < ${AC6_RATIO} × (AC6's line) |`);
  L.push('|---|---|---|---|');
  L.push(`| the pair's demonstrated rate in the read window | ${f1.judged} | ${f1.below} | ${f1.below85} |`);
  L.push(`| what the pair had demonstrated before the session opened | ${f1.judgedBefore} | ${f1.belowBefore} | ${f1.below85Before} |`);
  L.push('');
  L.push(`**F2, the plan moved and the encoder did not: ${f2.verdict}.** ${f2.line}`, '');
  L.push('| Sessions | Count |', '|---|---|');
  L.push(`| judged: at least ${F2_MIN_SESSION_MS / 60000} min, with a target and an encoder rate read | ${f2.judged} |`);
  L.push(`| the target's max at least ${F2_SPREAD} × its min | ${f2.spread} |`);
  L.push(`| … and the encoder's applied rate moved 0 times (F2) | ${f2.raised} |`);
  L.push('');

  L.push('## Pairs', '');
  L.push('| Agent | User | Carrier | Sessions | Demonstrated | Held in | Openings | F1 | F2 | Writes: measured · stand-in · nothing · unexplained |');
  L.push('|---|---|---|---|---|---|---|---|---|---|');
  for (const p of r.pairs) {
    const w = p.writes;
    L.push(
      `| ${p.agent} | ${dash(p.user)} | ${p.carrier} | ${p.sessions} | ${mbps(p.demonstrated)} | ${dash(p.heldIn)} | ` +
        `${p.openings.min === null ? '–' : `${mbps(p.openings.min)} – ${mbps(p.openings.max)}`} | ${p.f1} | ${p.f2} | ` +
        `${w.measured} · ${w.standIn} · ${w.nothing} · ${w.unexplained} |`,
    );
  }
  if (!r.pairs.length) L.push('| – | – | – | 0 | – | – | – | 0 | 0 | – |');
  L.push('');

  L.push('## Sessions', '');
  L.push(
    '| Start | Ran | Agent | Session | User | Carrier | Encoder | Opening | Slow-link | Entry | Demonstrated: window · before | ' +
      'Target min / med / max | Encoder min / med / max | Moves · ctr | QP p50 / p90 | Age p50 / p90 ms | Stalled | Flags |',
  );
  L.push('|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|');
  for (const s of r.sessions) {
    const o = s.opening;
    const size = o.width && o.height ? ` · ${o.width}×${o.height}` : '';
    const opening = `${mbps(o.bps)}${size} · ${dash(o.fps)} fps${o.judged ? '' : ' (not judged)'}`;
    const slow = s.slowLink.engaged === true ? `on (${dash(s.slowLink.longEdge)}/${dash(s.slowLink.fps)})` : s.slowLink.engaged === false ? 'off' : '?';
    const entry = s.entryBps === null ? '–' : `${mbps(s.entryBps)}${s.keyCarrier ? ` (${s.keyCarrier})` : ''}`;
    const carrier = s.carrier;
    const flags = [s.flags.f1 ? 'F1' : '', s.flags.f2 ? 'F2' : ''].filter(Boolean).join(' ') || '–';
    const pic = s.picture;
    L.push(
      `| ${minute(s.start)} | ${ran(s.durationS)} | ${s.agent} | ${s.session} | ${dash(s.user)} | ${carrier} | ${dash(s.encoder)} ${dash(s.codec)} | ` +
        `${opening} | ${slow} | ${entry} | ${mbps(s.demonstrated.pairWindow)} · ${mbps(s.demonstrated.pairBefore)} | ${span3(s.target)} | ${span3(s.applied)} | ` +
        `${dash(s.moves)} · ${dash(s.counterMoves)} | ${dash(pic.qpP50)} / ${dash(pic.qpP90)} | ${dash(pic.ageP50)} / ${dash(pic.ageP90)} | ` +
        `${pic.stalledOf ? `${pic.stalled} / ${pic.stalledOf} (${((100 * pic.stalled) / pic.stalledOf).toFixed(1)} %)` : '–'} | ${flags} |`,
    );
  }
  if (!r.sessions.length) L.push('| – | – | – | – | – | – | – | – | – | – | – | – | – | – | – | – | – | – |');
  L.push('');

  L.push('## Memory trail (every row inferred)', '');
  L.push('| When | Agent | User | Carrier | Session | Key carrier | Entry | Evidence | Kept | What stood behind it | The write |');
  L.push('|---|---|---|---|---|---|---|---|---|---|---|');
  for (const m of r.memoryTrail) {
    L.push(
      `| ${minute(m.at)} | ${m.agent} | ${dash(m.user)} | ${dash(m.carrier)} | ${dash(m.session)} | ${dash(m.keyCarrier)} | ${mbps(m.entryBps)} | ` +
        `${mbps(m.evidenceBps)} | ${mbps(m.keptBps)} | ${m.basis}: ${m.rule} | ${m.write} |`,
    );
  }
  if (!r.memoryTrail.length) L.push('| – | – | – | – | – | – | – | – | – | no write read | – |');
  L.push('');

  L.push('## How this was read', '');
  L.push(
    "- Carrier: the `session carrier recorded` line, else the ICE line and the pump's `constrained` flag, marked (ice) or (pump). " +
      '`turn` is TURN over UDP or over TCP/TLS: the agent\'s lines do not tell them apart.',
  );
  L.push(
    '- Pair: the agent, the controlling user (its last 6 hex; `?` when its request line was not read) and the carrier class. ' +
      'The viewer\'s device and network are not read, so one user on two networks is one pair (FR-148 §9 O1).',
  );
  L.push(
    `- Demonstrated: the highest lowest delivered rate over ${DEMO_K.ffmpeg} FFmpeg or ${DEMO_K.vp9} VP9-444 consecutive heartbeats ` +
      `(about ${DEMO_RUN_MS / 1000} s) of one pump, every one rated and with the viewer's age under ${AGE_SLACK_MS} ms over its floor, ` +
      "on the session's carrier side. A lower bound on what the path carries, never its capacity: an idle screen and a full pipe read the same today.",
  );
  L.push("- Opening: the first heartbeat's `target_bps`, judged only when the pump's start line was read before it.");
  L.push(
    '- Encoder: `maxrate_bps`; moves are its changes between heartbeats, and ctr is rate_moves + rebuilds + swaps. ' +
      'VP9-444 heartbeats carry neither, so F2 does not judge them.',
  );
  L.push(
    "- Stalled: the pipe classifier's transit-stalled windows of those it gave a verdict. It judges constrained windows only, so a direct session reads –.",
  );
  L.push('- Memory trail: a write carries no session id and no provenance; both are inferred from the lines around it.');
  return L.join('\n');
}

// ---------------------------------------------------------------- mongosh runner

/** The fleet read's pipeline for one day's `$match`. Streaming stages only
 *  after the index-backed `$match` + `$sort`; see the header. */
function pipelineFor(match) {
  const project = {};
  for (const k of FIELDS) project[k] = '$lines.fields.' + k;
  // The memory key leaves as its carrier half only, never the address.
  project.peer_carrier = {
    $cond: [{ $eq: [{ $type: '$lines.fields.peer' }, 'string'] }, { $arrayElemAt: [{ $split: ['$lines.fields.peer', '|'] }, 1] }, null],
  };
  // The controlling user leaves as its last 6 hex only.
  project.user6 = {
    $cond: [
      { $eq: [{ $type: '$lines.fields.controller_user_id' }, 'string'] },
      {
        $substrCP: [
          '$lines.fields.controller_user_id',
          { $max: [0, { $subtract: [{ $strLenCP: '$lines.fields.controller_user_id' }, 6] }] },
          6,
        ],
      },
      null,
    ],
  };
  return [
    { $match: match },
    { $sort: { created_at: 1 } },
    {
      $project: {
        _id: 0,
        agent_id: 1,
        lines: {
          $filter: {
            input: '$lines',
            as: 'l',
            cond: { $regexMatch: { input: { $ifNull: ['$$l.msg', ''] }, regex: MSG_PATTERN } },
          },
        },
      },
    },
    { $unwind: '$lines' },
    { $project: { a: '$agent_id', t: '$lines.ts', m: '$lines.msg', f: project } },
  ];
}

/** One fleet-read row as the offline mode reads it back: ids as text, every
 *  other field as its plain value. */
function rowOut(row) {
  const f = {};
  for (const k of Object.keys(row.f || {})) f[k] = ID_FIELDS.has(k) ? text(row.f[k]) : value(row.f[k]);
  return { a: text(row.a), t: row.t instanceof Date ? row.t.toISOString() : row.t, m: row.m, f };
}

/** The stage chain of a winning plan, outermost first, with index names. */
function planStages(plan) {
  const out = [];
  const walk = (o) => {
    if (!o || typeof o !== 'object') return;
    if (typeof o.stage === 'string') {
      out.push(o.stage + (o.indexName ? `(${o.indexName}${o.direction ? ', ' + o.direction : ''})` : ''));
    }
    for (const k of Object.keys(o)) if (k !== 'rejectedPlans') walk(o[k]);
  };
  walk(plan);
  return out;
}

/**
 * The fleet read. Runs through scripts/prod-mongo-read.sh, its client OUTSIDE
 * the Mongo container (scripts/rc-path-report.sh carries the recipe). Bounded
 * as the header says.
 */
function runMongosh() {
  const env = typeof process !== 'undefined' && process.env ? process.env : {};
  const DAY = 86400000;
  const until = env.RC_PATH_UNTIL ? new Date(env.RC_PATH_UNTIL) : new Date();
  const since = env.RC_PATH_SINCE ? new Date(env.RC_PATH_SINCE) : new Date(until.getTime() - 7 * DAY);
  if (isNaN(since.getTime()) || isNaN(until.getTime()) || since >= until) {
    throw new Error('RC_PATH_SINCE must be before RC_PATH_UNTIL (ISO 8601)');
  }
  if (until.getTime() - since.getTime() > 8 * DAY) throw new Error('the window is over 8 days; agent_logs keeps 7');
  const hex = /^[0-9a-fA-F]{24}$/;
  if (env.RC_PATH_TENANT && !hex.test(env.RC_PATH_TENANT)) throw new Error('RC_PATH_TENANT takes a 24-hex tenant id');
  const agents = String(env.RC_PATH_AGENTS || '')
    .split(',')
    .map((x) => x.trim())
    .filter(Boolean);
  for (const a of agents) if (!hex.test(a)) throw new Error('RC_PATH_AGENTS takes 24-hex agent ids, comma-separated');
  const labels = parseLabels(env.RC_PATH_LABELS || '');
  const emitRows = env.RC_PATH_EMIT === 'rows';
  const json = env.RC_PATH_JSON === '1' && !emitRows;
  const database = connectDb(env.RC_PATH_DB || 'roomler2', env);
  const coll = database.getCollection('agent_logs');
  // The read's own notes: a `#` line in row mode (the Node reader skips it), a
  // plain line before a Markdown report, and the `fleetRead` key of a JSON one,
  // so that `--json` prints JSON only.
  const log = (s) => {
    if (!json) print(emitRows ? '# ' + s : s);
  };
  const matchFor = (t0, t1) => {
    const match = { source: 'agent', created_at: { $gte: new Date(t0), $lt: new Date(t1) } };
    if (env.RC_PATH_TENANT) match.tenant_id = ObjectId(env.RC_PATH_TENANT);
    if (agents.length === 1) match.agent_id = ObjectId(agents[0]);
    else if (agents.length > 1) match.agent_id = { $in: agents.map((a) => ObjectId(a)) };
    return match;
  };
  // The index is pinned, not left to the planner: each one below returns the
  // matched batches in `created_at` order, so the `$sort` never becomes a
  // blocking SORT stage. One agent of one tenant has its own index; several
  // agents ($in) on it would need a merge, so they take the tenant's.
  const hint = !env.RC_PATH_TENANT
    ? { created_at: 1 }
    : agents.length === 1
      ? { tenant_id: 1, agent_id: 1, created_at: -1 }
      : { tenant_id: 1, source: 1, created_at: -1 };
  const options = { maxTimeMS: 240000, allowDiskUse: false, hint };
  if (env.RC_PATH_EXPLAIN === '1') {
    // queryPlanner mode: the plan is chosen, nothing is executed.
    const t1 = Math.min(since.getTime() + DAY, until.getTime());
    const ex = coll.explain('queryPlanner').aggregate(pipelineFor(matchFor(since.getTime(), t1)), options);
    const stages = planStages(ex);
    print(`plan for one day's query: ${stages.join(' <- ')}`);
    print(`blocking SORT stage: ${stages.some((x) => /^SORT\b/.test(x)) ? 'YES' : 'none'}`);
    return;
  }
  const fleetRead = { estimatedBatches: coll.estimatedDocumentCount(), since: since.toISOString(), until: until.toISOString(), days: [], lines: 0 };
  log(`agent_logs ≈ ${fleetRead.estimatedBatches} batches; reading ${fleetRead.since} → ${fleetRead.until}`);
  const report = new PathReport({ labels });
  for (let t0 = since.getTime(); t0 < until.getTime(); t0 += DAY) {
    const t1 = Math.min(t0 + DAY, until.getTime());
    const started = Date.now();
    let dayRows = 0;
    coll.aggregate(pipelineFor(matchFor(t0, t1)), options).forEach((row) => {
      dayRows++;
      if (emitRows) print(JSON.stringify(rowOut(row)));
      else report.addRow(row);
    });
    const day = { day: new Date(t0).toISOString().slice(0, 10), lines: dayRows, ms: Date.now() - started };
    fleetRead.days.push(day);
    fleetRead.lines += dayRows;
    log(`  ${day.day}: ${day.lines} lines in ${day.ms} ms`);
  }
  log(`read ${fleetRead.lines} lines`);
  if (emitRows) return;
  const r = report.finish();
  if (json) {
    r.fleetRead = fleetRead;
    print(JSON.stringify(r, null, 2));
  } else {
    print('');
    print(render(r));
  }
}

function connectDb(dbName, env) {
  if (env.RC_PATH_URI) return new Mongo(env.RC_PATH_URI).getDB(dbName);
  const u = env.MONGO_INITDB_ROOT_USERNAME;
  const p = env.MONGO_INITDB_ROOT_PASSWORD;
  if (u && p) {
    return new Mongo('mongodb://' + encodeURIComponent(u) + ':' + encodeURIComponent(p) + '@127.0.0.1:27017/?authSource=admin').getDB(dbName);
  }
  if (typeof db !== 'undefined' && db) return db.getSiblingDB(dbName);
  throw new Error('no connection: pass RC_PATH_URI, or run through scripts/prod-mongo-read.sh (it sets the credentials)');
}

// ---------------------------------------------------------------- the rehearsal seed

/**
 * Rolling-log text -> `agent_logs` batches, as the uploader would have stored
 * them (canonical EJSON, one batch per line), so the fleet read can be
 * rehearsed against a throwaway Mongo. Field types follow
 * `logs_upload::FieldVisitor`: an integer is an Int64, `true` / `false` a bool,
 * a lone quoted string its text, and every other value the string as printed.
 * Batches close at 500 lines or 5 s, the uploader's two flush rules.
 */
function ejsonField(raw) {
  const s = String(raw).trim();
  if (/^"([^"\\]|\\.)*"$/.test(s)) return s.slice(1, -1);
  if (s === 'true' || s === 'false') return s === 'true';
  if (/^-?\d+$/.test(s)) return { $numberLong: s };
  if (/^-?\d+\.\d+$/.test(s)) return { $numberDouble: s };
  return s;
}

function toEjson(parsedLines, agentHex, tenantHex, out) {
  let batch = null;
  const flush = () => {
    if (!batch || !batch.lines.length) return;
    batch.line_count = batch.lines.length;
    batch.created_at = { $date: batch.lines[batch.lines.length - 1].ts.$date };
    delete batch.startMs;
    out(JSON.stringify(batch));
    batch = null;
  };
  for (const p of parsedLines) {
    const t = new Date(p.ts.endsWith('Z') ? p.ts : p.ts + 'Z');
    if (isNaN(t.getTime())) continue;
    // A line older than the batch's last one starts a new batch: the uploader's
    // time only moves forward, and a batch is stamped with its last line.
    const behind = batch && t.getTime() < Date.parse(batch.lines[batch.lines.length - 1].ts.$date);
    if (batch && (behind || batch.lines.length >= 500 || t.getTime() - batch.startMs >= 5000)) flush();
    if (!batch) {
      batch = {
        tenant_id: { $oid: tenantHex },
        source: 'agent',
        agent_id: { $oid: agentHex },
        agent_version: 'rehearsal',
        line_count: 0,
        created_at: null,
        lines: [],
        startMs: t.getTime(),
      };
    }
    const fields = {};
    for (const k of Object.keys(p.fields)) fields[k] = ejsonField(p.fields[k]);
    batch.lines.push({ ts: { $date: t.toISOString() }, level: p.level || 'INFO', target: 'roomlerd::peer', msg: p.msg, fields });
  }
  flush();
}

// ---------------------------------------------------------------- fixtures

/**
 * The two reference chains of FR-148 §2 and a healthy session, as rolling-log
 * lines with invented ids. The values are the spec's where it gives them; every
 * id is invented, and the addresses and the name the real lines carry are
 * placeholders the self-test proves never reach the output.
 */
function hex24(head, tail) {
  return head + '0'.repeat(24 - head.length - tail.length) + tail;
}

const FX = {
  tenant: hex24('f148', 'a701'),
  corplap2: hex24('f148', 'c0a002'),
  corplap1: hex24('f148', 'c0a001'),
  healthy: hex24('f148', 'c0a003'),
  op: hex24('f148', '0ee001'),
  other: hex24('f148', '0ee002'),
  /** The CORPLAP-2 chain's memory entries and the quarter steps between them
   *  (FR-148 §2.1): each is damp(entry, entry raised n times). */
  entries: [179980, 197134, 208222, 213427, 233769, 281732],
  steps: [3, 2, 1, 3, 5, 0],
  /** The transit hiccup's anchor, and its six ×11/10 steps to 179,980. */
  anchor: 101596,
};

function fxLine(ms, msg, fields) {
  return `${new Date(ms).toISOString()}  INFO ${msg}` + fields.map(([k, v]) => ` ${k}=${v}`).join('');
}

function some(v) {
  return v === null || v === undefined ? 'None' : `Some(${v})`;
}

function anchoredStep(v) {
  return Math.floor((v * 11) / 10);
}

/** One session's lines in time order. See `fixtures` for the shape of `o`. */
function fxSession(o) {
  const L = [];
  const t = o.at;
  const q = (v) => `"${v}"`;
  L.push(
    fxLine(t, 'incoming session request — running consent broker', [
      ['session_id', o.sid],
      ['controller_user_id', o.user],
      ['controller_name', 'NAME-SECRET'],
      ['permissions', 'Permissions(VIEW | INPUT)'],
      ['chosen_codec', 'av1'],
      ['rc_overlay', String(!!o.overlay)],
    ]),
  );
  L.push(
    fxLine(t + 400, 'per-session ICE path detected (adaptive bitrate)', [
      ['session_id', o.sid],
      ['relay', String(!!o.relay)],
      ['local_typ', o.relay ? 'Relay' : 'Host'],
      ['local_proto', 'Udp'],
      ['local_addr', 'ADDR-SECRET-L:50000'],
      ['remote_typ', 'Host'],
      ['remote_proto', 'Udp'],
      ['remote_addr', 'ADDR-SECRET-R:50001'],
    ]),
  );
  const key = o.seed || o.write ? `"ADDR-SECRET-P${o.tag ? '|' + o.tag : ''}"` : null;
  if (o.seed) {
    L.push(
      fxLine(t + 600, "FR-35 rate memory: opening ceiling seeded from the pair's remembered stable rate", [
        ['session_id', o.sid],
        ['codec_label', q(o.codec)],
        ['peer', key],
        ['seed_bps', String(o.seed)],
        ['hi_bps', '20000000'],
      ]),
    );
  }
  if (o.slow) {
    L.push(
      fxLine(t + 700, 'FR-59 P5 slow-link profile engaged — the pair is remembered as slow, opening with fewer pixels and fewer frames', [
        ['session_id', o.sid],
        ['codec_label', q(o.codec)],
        ['remembered_bps', some(o.seed)],
        ['max_long_edge', '1280'],
        ['max_fps', '15'],
        ['target_fps', String(o.fps)],
      ]),
    );
  }
  if (o.start !== false) {
    L.push(
      o.vp9
        ? fxLine(t + 800, 'VP9-444 DC pump starting', [
            ['session_id', o.sid],
            ['codec_label', q(o.codec)],
            ['target_fps', String(o.fps)],
          ])
        : fxLine(t + 800, 'FFmpeg DC pump starting', [
            ['session_id', o.sid],
            ['codec_label', q(o.codec)],
            ['target_fps', String(o.fps)],
            ['slow_link', String(!!o.slow)],
          ]),
    );
  }
  if (o.slow) {
    L.push(
      fxLine(
        t + 900,
        'FR-59 P8 — the pair is remembered slower than the nominal floor: opening at the remembered rate, floor relief seeded from it, refine lift held while the profile is on',
        [
          ['session_id', o.sid],
          ['codec_label', q(o.codec)],
          ['seed_bps', String(o.seed)],
        ],
      ),
    );
  }
  if (o.carrier) {
    L.push(
      fxLine(t + 2600, 'session carrier recorded (FR-102 P9c)', [
        ['session_id', o.sid],
        ['carrier', q(o.carrier)],
        ['local_typ', 'Host'],
        ['local_addr', 'ADDR-SECRET-L'],
        ['remote_typ', 'Host'],
        ['remote_addr', 'ADDR-SECRET-R'],
        ['overlay', 'None'],
      ]),
    );
  }
  let ts = t + 3000;
  let bytes = 0;
  const cum = [0, 0, 0, 0, 0];
  // The encoder's applied rate, and its moves: `rate_moves` counts each change
  // unless a window gives its own count.
  let applied = null;
  let moved = 0;
  o.wins.forEach((w, i) => {
    const gap = w.gap || (o.vp9 ? 1000 : 2000);
    if (i > 0) {
      ts += gap;
      bytes += Math.round((w.rate * gap) / 8000);
    }
    if (w.restart) bytes = w.restart;
    const maxrate = w.maxrate === null ? null : w.maxrate === undefined ? w.target : w.maxrate;
    if (maxrate !== null) {
      if (applied !== null && maxrate !== applied) moved++;
      applied = maxrate;
    }
    const stalled = w.stalled || 0;
    if (w.c === false || (w.c === undefined && !o.constrained)) cum[0] += 2;
    else {
      cum[3] += stalled;
      cum[1] += 2 - stalled;
    }
    if (o.vp9) {
      // The VP9-444 pump's heartbeat: no size, no encoder, no constrained flag,
      // no applied rate and no rate-apply counters.
      L.push(
        fxLine(ts, 'VP9-444 DC pump heartbeat (≈1s window)', [
          ['session_id', o.sid],
          ['target_bps', String(w.target)],
          ['bytes_written', String(bytes)],
          ['avg_qp', some(w.qp)],
          ['goodput_bps', some(w.goodput)],
          ['viewer_age_ms', some(w.age)],
          ['viewer_age_floor_ms', some(w.floor)],
          ['pipe_states', `[${cum.join(', ')}]`],
          ['prior_bps', some(w.prior)],
          ['window_ms', some(gap)],
        ]),
      );
      return;
    }
    L.push(
      fxLine(ts, 'FFmpeg DC pump heartbeat (≈2s window)', [
        ['session_id', o.sid],
        ['codec_label', q(o.codec)],
        ['target_fps', String(o.fps)],
        ['constrained', String(w.c === undefined ? !!o.constrained : w.c)],
        ['target_bps', String(w.target)],
        ['width', String(o.w)],
        ['height', String(o.h)],
        ['encoder', q(o.enc)],
        ['frames_encoded', '30'],
        ['bytes_written', String(bytes)],
        ['send_wait_max_ms', '0.4'],
        ['dc_ordered', 'Some(true)'],
        ['avg_qp', some(w.qp)],
        ['goodput_bps', some(w.goodput)],
        ['goodput_samples', '(0, 0)'],
        ['viewer_age_ms', some(w.age)],
        ['viewer_age_floor_ms', some(w.floor)],
        ['age_split', 'Some(AgeSplit { sender_ms: Some(0.1), transit_ms: 3, viewer_ms: 3 })'],
        ['pipe_state', 'Some("clear")'],
        ['pipe_states', `[${cum.join(', ')}]`],
        ['prior_bps', some(w.prior)],
        ['rate_moves', String(w.rm === undefined ? moved : w.rm)],
        ['rebuilds', '0'],
        ['swaps', String(w.sw || 0)],
        ['plan_divergences', 'PlanDivergences { budget: 0, hard_budget: 0 }'],
        ['window_ms', some(gap)],
        ['maxrate_bps', some(maxrate)],
      ]),
    );
    if (o.opener && i === 2) {
      L.push(
        fxLine(ts + 100, "FR-35 opener grace over — the opening burst sized the pair memory's next step", [
          ['session_id', o.sid],
          ['codec_label', q(o.codec)],
          ['opener_bytes', '120000'],
          ['opener_measured_bps', some(o.opener.measured)],
          ['growth_target_bps', String(o.opener.growth)],
        ]),
      );
    }
  });
  if (o.write) {
    L.push(
      fxLine(ts + (o.write.lag === undefined ? 300 : o.write.lag), "FR-79 V4 rate memory: the carrier's measured rate remembered for the pair", [
        ['peer', key],
        ['evidence_bps', some(o.write.evidence)],
        ['kept_bps', String(o.write.kept)],
      ]),
    );
  }
  return L;
}

/** `[agentHex, line]` pairs for the two chains and the healthy session. */
function fixtures() {
  const out = [];
  const push = (agent, lines) => {
    for (const l of lines) out.push([agent, l]);
  };
  const at = (s) => Date.parse(s);
  const n = (count, f) => Array.from({ length: count }, (_, i) => f(i));
  const derp = { carrier: 'derp', tag: 'relay:derp/tcp', relay: false, overlay: true, constrained: true, enc: 'av1_nvenc', codec: 'AV1' };

  // a. CORPLAP-2. The pair's measured entry is read and the session ends with no
  // evidence, so nothing renews it ...
  push(
    FX.corplap2,
    fxSession(
      Object.assign({}, derp, {
        sid: hex24('f148', 'a20001'),
        user: FX.op,
        at: at('2026-11-10T12:35:05Z'),
        seed: 3743114,
        w: 1920,
        h: 1200,
        fps: 30,
        opener: { growth: 0, measured: null },
        wins: n(25, (i) => ({ target: 3181646, rate: 3600000 + Math.round(((((i * 7919) % 13) - 6) / 6) * 200000), age: 60, floor: 40, qp: 30 })),
        write: { evidence: null, kept: 3743114 },
      }),
    ),
  );
  // ... it ages out: the next session reads no entry, a transit hiccup anchors the
  // prior near 100 kbit/s, and the decayed placeholder is its only evidence ...
  let prior = FX.anchor;
  const anchored = [prior];
  for (let i = 0; i < 6; i++) anchored.push((prior = anchoredStep(prior)));
  push(
    FX.corplap2,
    fxSession(
      Object.assign({}, derp, {
        sid: hex24('f148', 'a20002'),
        user: FX.op,
        at: at('2026-11-11T09:57:13Z'),
        seed: null,
        w: 1920,
        h: 1200,
        fps: 30,
        opener: { growth: 0, measured: null },
        wins: n(90, (i) => {
          if (i < 4) return { target: 1500000, rate: 1200000, age: 60, floor: 40, qp: 60 };
          if (i === 4) return { target: 1000000, rate: 100000, age: 900, floor: 40, qp: 90, goodput: FX.anchor, prior: FX.anchor, stalled: 1 };
          const p = anchored[Math.min(6, Math.floor((i - 5) / 5))];
          return { target: p, rate: Math.round((p * 0.9) / 4) * 4, age: 60, floor: 40, qp: 200, prior: p };
        }),
        write: { evidence: anchored[6], kept: anchored[6] },
      }),
    ),
  );
  // ... and every session after it opens from the placeholder, at 1280×800 and 15 fps,
  // and writes the placeholder back, damped.
  const dates = ['2026-11-12T13:24:05Z', '2026-11-12T22:13:24Z', '2026-11-13T17:36:09Z', '2026-11-14T20:35:21Z', '2026-11-16T17:55:20Z', '2026-11-16T18:30:02Z'];
  const counts = [30, 24, 36, 40, 72, 24];
  FX.entries.forEach((entry, k) => {
    const steps = FX.steps[k];
    const evidence = standIn(entry, steps);
    // The fourth session's last step lands after its last heartbeat, so only the
    // arithmetic can name it.
    const shown = k === 3 ? steps - 1 : steps;
    push(
      FX.corplap2,
      fxSession(
        Object.assign({}, derp, {
          sid: hex24('f148', 'a2010' + k),
          user: FX.op,
          at: at(dates[k]),
          seed: entry,
          slow: true,
          w: 1280,
          h: 800,
          fps: 15,
          opener: { growth: 0, measured: null },
          wins: n(counts[k], (i) => {
            const p = standIn(entry, Math.min(shown, Math.floor(i / 5)));
            return {
              target: p,
              rate: Math.round((p * (0.55 + 0.04 * ((i * 7) % 10))) / 4) * 4,
              age: 55 + (i % 16),
              floor: 40,
              qp: 150 + ((i * 29) % 91),
              prior: p,
              stalled: (k === 4 && i % 9 === 4) || (k === 1 && i % 4 === 1) ? 1 : 0,
            };
          }),
          write: { evidence, kept: damp(entry, evidence) },
        }),
      ),
    );
  });
  // The same laptop that morning over TURN: another carrier, so another pair.
  push(
    FX.corplap2,
    fxSession({
      sid: hex24('f148', 'a20003'),
      user: FX.op,
      at: at('2026-11-16T07:45:30Z'),
      carrier: 'turn',
      relay: true,
      overlay: true,
      constrained: true,
      enc: 'av1_nvenc',
      codec: 'AV1',
      w: 1920,
      h: 1200,
      fps: 30,
      wins: n(31, (i) => ({ target: 3250000, rate: 900000 + ((i * 17) % 5) * 50000, age: 55, floor: 40, qp: 14 })),
    }),
  );

  // b. CORPLAP-1: hevc_qsv over TURN, the target climbs 2.55 → 7.45 Mbit/s over six
  // minutes and the encoder's rate stays where it opened, with 0 moves.
  push(
    FX.corplap1,
    fxSession({
      sid: hex24('f148', 'b10001'),
      user: FX.op,
      at: at('2026-11-16T12:56:30Z'),
      carrier: 'turn',
      relay: true,
      overlay: false,
      constrained: true,
      enc: 'hevc_qsv',
      codec: 'HEVC',
      seed: 3000000,
      w: 1920,
      h: 1200,
      fps: 30,
      opener: { growth: 0, measured: null },
      wins: n(181, (i) => ({
        target: 2550000 + 245000 * Math.min(20, Math.floor(i / 9)),
        maxrate: 2550000,
        rate: 1500000 + ((i * 53) % 7) * 50000,
        age: 56 + (i % 8),
        floor: 40,
        qp: 28,
        stalled: i === 90 ? 1 : 0,
      })),
      write: { evidence: null, kept: 3000000 },
    }),
  );

  // c. A healthy direct session: the plan moves and the encoder follows it.
  push(
    FX.healthy,
    fxSession({
      sid: hex24('f148', 'c30001'),
      user: FX.other,
      at: at('2026-11-14T10:00:00Z'),
      carrier: 'direct',
      relay: false,
      overlay: false,
      constrained: false,
      enc: 'av1_nvenc',
      codec: 'AV1',
      w: 2560,
      h: 1600,
      fps: 60,
      wins: n(181, (i) => ({
        target: i < 60 ? 8000000 : i < 120 ? 12000000 : 10000000,
        rm: i < 60 ? 0 : i < 120 ? 1 : 2,
        rate: 6000000 + ((i * 31) % 10) * 300000,
        age: 20,
        floor: 15,
        qp: 40,
      })),
    }),
  );
  return out;
}

/** The fixtures as `agent_logs` batches, canonical EJSON, one string each: every
 *  agent's lines in time order, as its uploader would have sent them. */
function fixtureSeed() {
  const byAgent = new Map();
  for (const [agent, l] of fixtures()) {
    if (!byAgent.has(agent)) byAgent.set(agent, []);
    byAgent.get(agent).push(parseLogLine(l));
  }
  const out = [];
  for (const [agent, lines] of byAgent) {
    const ordered = lines
      .map((p, i) => ({ p, i }))
      .sort((a, b) => Date.parse(a.p.ts) - Date.parse(b.p.ts) || a.i - b.i)
      .map((x) => x.p);
    toEjson(ordered, agent, FX.tenant, (s) => out.push(s));
  }
  return out;
}

// ---------------------------------------------------------------- self-test

/** What `pipelineFor` lets out of the database for one line whose stored
 *  fields `stored` hold the driver's types: the FIELDS as they are, `peer` as
 *  its carrier half (missing when it has no bar, null when it is no string) and
 *  `controller_user_id` as its last 6 characters. */
function serverProject(stored) {
  const f = {};
  for (const k of FIELDS) if (k in stored) f[k] = stored[k];
  const peer = stored.peer;
  if (typeof peer === 'string') {
    const parts = peer.split('|');
    if (parts.length > 1) f.peer_carrier = parts[1];
  } else f.peer_carrier = null;
  const user = stored.controller_user_id;
  f.user6 = typeof user === 'string' ? user.slice(Math.max(0, user.length - 6)) : null;
  return f;
}

/** A rolling-log line's fields as the uploader stores them and the driver
 *  returns them: an integer an Int64 (a bson Long in mongosh), a quoted value
 *  its text, `true` / `false` a bool, anything else the string as printed. */
function storedLikeTheUploader(fields) {
  const out = {};
  for (const k of Object.keys(fields)) {
    const s = String(fields[k]).trim();
    if (/^"([^"\\]|\\.)*"$/.test(s)) out[k] = s.slice(1, -1);
    else if (s === 'true' || s === 'false') out[k] = s === 'true';
    else if (/^-?\d+$/.test(s)) {
      const x = Number(s);
      out[k] = { toNumber: () => x };
    } else if (/^-?\d+\.\d+$/.test(s)) out[k] = Number(s);
    else out[k] = s;
  }
  return out;
}

function projectLikeTheServer(fields) {
  return serverProject(storedLikeTheUploader(fields));
}

/** One canonical-EJSON value as the driver returns it. */
function fromEjson(v) {
  if (v && typeof v === 'object' && !Array.isArray(v)) {
    if ('$numberLong' in v) {
      const x = Number(v.$numberLong);
      return { toNumber: () => x };
    }
    if ('$numberDouble' in v) return Number(v.$numberDouble);
    if ('$date' in v) return new Date(v.$date);
    if ('$oid' in v) {
      const h = v.$oid;
      return { toHexString: () => h };
    }
  }
  return v;
}

function selfTest() {
  let failed = 0;
  let n = 0;
  const ok = (cond, name, detail) => {
    n++;
    if (cond) console.log(`ok ${n} - ${name}`);
    else {
      failed++;
      console.log(`not ok ${n} - ${name}${detail !== undefined ? ' — ' + JSON.stringify(detail) : ''}`);
    }
  };
  const runPairs = (pairs, opts) => {
    const r = new PathReport(opts);
    for (const [agent, l] of pairs) {
      const p = parseLogLine(l);
      if (p) r.add(p.msg, p.fields, p.ts, agent);
    }
    return r.finish();
  };
  const sessionOf = (r, sid6) => r.sessions.find((s) => s.session === sid6);

  // --- the arithmetic the trail is inferred from, on FR-148 §2.1's own numbers
  ok(damp(233769, 713402) === 281732, 'damp: 233,769 toward 713,402 keeps 281,732 (§2.1 step 5)', damp(233769, 713402));
  ok(standIn(233769, 5) === 713402 && quarterSteps(233769, 713402) === 5, '233,769 raised by a quarter five times is 713,402');
  const chain = FX.entries.slice(0, 5).map((e, k) => damp(e, standIn(e, FX.steps[k])));
  ok(JSON.stringify(chain) === JSON.stringify(FX.entries.slice(1)), 'the stand-in, damped, walks the entry 179,980 → 281,732 exactly as the field did', chain);
  let a = FX.anchor;
  for (let i = 0; i < 6; i++) a = anchoredStep(a);
  ok(a === 179980, 'the hiccup anchor, decayed ×11/10 six times, is the 179,980 the memory adopted', a);
  ok(quarterSteps(281732, 281732) === 0 && quarterSteps(233769, 713403) === null, 'zero steps is the entry itself, and a near miss is no match');
  ok(quarterSteps(1500000, 1500000) === null && standIn(1400000, 1) === null, 'at or above the band the prior stands in for nothing');
  ok(damp(3743114, 179980) === 1961547, 'damp: a lower reading moves the entry half the way down', damp(3743114, 179980));

  // --- the tokenizer and the values
  const hbLine = fixtures().find(([, l]) => /DC pump heartbeat/.test(l))[1];
  const hb = parseLogLine(hbLine);
  ok(hb && hb.msg === 'FFmpeg DC pump heartbeat (≈2s window)', 'the message is everything before the first field', hb && hb.msg);
  ok(hb && hb.fields.age_split === 'Some(AgeSplit { sender_ms: Some(0.1), transit_ms: 3, viewer_ms: 3 })', 'a struct value keeps its inner spaces');
  ok(hb && pipeStates(hb.fields.pipe_states) !== null, 'pipe_states reads as five counts', hb && hb.fields.pipe_states);
  ok(value('Some(6)') === 6 && value('None') === null && text('"derp"') === 'derp' && value({ toNumber: () => 7 }) === 7, 'Debug, quoted and Long values read');
  ok(text('000001') === '000001' && last6({ toHexString: () => 'f148000000000000000ee001' }) === '0ee001', 'an id stays text, never a number');
  ok(keyCarrier({ peer: '"ADDR-SECRET|relay:derp/tcp"' }) === 'relay:derp/tcp' && keyCarrier({ peer: '"ADDR-SECRET"' }) === null, 'the key carrier is the part after the bar, never the address');

  // --- the reference chains
  const fx = fixtures();
  const labels = [
    ['c0a002', 'CORPLAP-2'],
    ['c0a001', 'CORPLAP-1'],
  ];
  const r = runPairs(fx, { labels });
  const derpPair = r.pairs.find((p) => p.agent === 'CORPLAP-2' && p.carrier === 'mesh-relayed/derp');
  // The demonstrated rate, brute force over the first session's own windows.
  const s0 = fx.filter(([ag, l]) => ag === FX.corplap2 && l.indexOf(hex24('f148', 'a20001')) >= 0 && /DC pump heartbeat/.test(l)).map(([, l]) => parseLogLine(l));
  const rates = [];
  for (let i = 1; i < s0.length; i++) {
    rates.push(((Number(s0[i].fields.bytes_written) - Number(s0[i - 1].fields.bytes_written)) * 8000) / (Date.parse(s0[i].ts) - Date.parse(s0[i - 1].ts)));
  }
  let brute = 0;
  for (let i = 0; i + DEMO_K.ffmpeg <= rates.length; i++) brute = Math.max(brute, Math.min(...rates.slice(i, i + DEMO_K.ffmpeg)));
  ok(derpPair && derpPair.sessions === 8, 'CORPLAP-2 on the mesh relay is one pair of eight sessions', derpPair);
  ok(derpPair && derpPair.demonstrated === brute && brute > 3000000, 'the pair demonstrated over 3 Mbit/s that week, the best 15-window run held', [derpPair && derpPair.demonstrated, brute]);
  const late = FX.entries.map((_, k) => sessionOf(r, 'a2010' + k));
  ok(late.every((s, k) => s && s.opening.bps === FX.entries[k] && s.opening.bps < 300000), 'the six sessions open at the remembered 0.18 to 0.28 Mbit/s', late.map((s) => s && s.opening.bps));
  ok(late.every((s) => s.opening.width === 1280 && s.opening.height === 800 && s.opening.fps === 15 && s.slowLink.engaged === true), 'at 1280×800 and 15 fps, with the slow-link profile engaged');
  ok(late.every((s, k) => s.entryBps === FX.entries[k] && s.keyCarrier === 'relay:derp/tcp' && s.carrier === 'mesh-relayed/derp'), 'each from its remembered entry, on the mesh relay (DERP)');
  ok(late.every((s) => s.flags.f1 === true && s.flags.f1At85 === true && s.flags.f1Before === true), 'F1 on every one of them, on the window and on what the pair had shown before');
  const sB = sessionOf(r, 'a20002');
  const sA = sessionOf(r, 'a20001');
  ok(sB && sB.flags.f1 === true && sB.opening.bps === 1500000, 'the session after the entry aged out opened at the nominal rate, below half the pair: F1', sB && sB.flags);
  ok(sA && sA.flags.f1 === false && sA.flags.f1Before === null, 'the session that read the measured entry is no F1, and had nothing before it', sA && sA.flags);
  ok(r.flags.f1.verdict === 'RAISED' && r.flags.f1.below === 7 && r.flags.f1.judged === 11 && r.flags.f1.below85 === 7, 'F1 RAISED: 7 of 11, and 7 under the 0.85 line', r.flags.f1);
  ok(r.flags.f1.judgedBefore === 7 && r.flags.f1.belowBefore === 7, 'the before-open yardstick judges the 7 sessions whose pair had an earlier one', r.flags.f1);
  const trailA = r.memoryTrail.filter((m) => m.agent === 'CORPLAP-2');
  ok(trailA.length === 8 && trailA.every((m) => m.measured === false && m.inferred === true), 'all eight CORPLAP-2 memory writes are marked not measured, and inferred', trailA.map((m) => m.basis));
  ok(trailA[0].basis === 'nothing' && trailA[0].write === 'nothing written' && trailA[0].keptBps === 3743114, 'the measured entry is read and nothing renews it', trailA[0]);
  ok(trailA[1].basis === 'stand-in' && /prior_bps/.test(trailA[1].rule) && /first write/.test(trailA[1].write) && trailA[1].keptBps === 179980, '179,980: the prior\'s placeholder, adopted outright as a first write', trailA[1]);
  ok(
    trailA.slice(2).every((m, k) => m.basis === 'stand-in' && m.entryBps === FX.entries[k] && m.write === `damped from ${mbps(FX.entries[k])}` && m.keptBps === damp(FX.entries[k], standIn(FX.entries[k], FX.steps[k]))),
    'each later write is the stand-in, damped from the entry it opened from',
    trailA.slice(2).map((m) => [m.entryBps, m.evidenceBps, m.keptBps, m.write]),
  );
  ok(trailA[5].rule === 'the opening entry raised by a quarter 3 times', 'a write past the last heartbeat is named by the arithmetic alone', trailA[5].rule);
  ok(trailA.slice(2).every((m) => m.session !== null && m.carrier === 'mesh-relayed/derp'), 'every write lands on its own session', trailA.map((m) => m.session));
  const turnMorning = sessionOf(r, 'a20003');
  ok(turnMorning && turnMorning.carrier === 'turn' && turnMorning.flags.f1 === false, 'the morning over TURN is another pair, and no F1', turnMorning && [turnMorning.carrier, turnMorning.flags]);
  const q = sessionOf(r, 'b10001');
  ok(q && q.target.min === 2550000 && q.target.max === 7450000, 'CORPLAP-1: the target climbs 2.55 → 7.45 Mbit/s', q && q.target);
  ok(q && q.applied.min === 2550000 && q.applied.max === 2550000 && q.moves === 0 && q.counterMoves === 0, 'and the encoder stays at 2.55 Mbit/s with 0 moves', q && [q.applied, q.moves, q.counterMoves]);
  ok(q && q.flags.f2 === true && q.flags.f1 === false && q.durationS >= 300, 'F2 on it, and no F1', q && [q.flags, q.durationS]);
  ok(r.flags.f2.verdict === 'RAISED' && r.flags.f2.raised === 1 && r.flags.f2.judged === 2 && r.flags.f2.spread === 2, 'F2 RAISED: 1 of 2 judged', r.flags.f2);
  const h = sessionOf(r, 'c30001');
  ok(h && h.flags.f1 === false && h.flags.f2 === false && h.flags.f2Spread === true && h.moves === 2, 'the healthy session moves 1.5× and its encoder follows: neither flag', h && [h.flags, h.moves]);
  ok(h && h.carrier === 'direct' && h.picture.stalledOf === 0 && h.picture.ageP50 === 20, 'a direct session has no window the classifier judged, and its age reads', h && h.picture);
  ok(late[4].picture.stalled === 8 && late[4].picture.stalledOf === 144, 'stalled windows count from pipe_states', late[4].picture);
  ok(late[0].picture.qpP90 >= 184 && late[0].picture.ageP50 >= 55 && late[0].picture.ageP90 <= 70, 'the picture: QP p90 and age read', late[0].picture);

  // --- the demonstrated rule's edges, one pump at a time
  const edgeAgent = hex24('f148', 'c0a0e0');
  const edge = (sid, wins, extra) =>
    fxSession(
      Object.assign(
        { sid: hex24('f148', sid), user: FX.other, at: Date.parse('2026-11-15T10:00:00Z'), carrier: 'turn', relay: true, constrained: true, enc: 'av1_nvenc', codec: 'AV1', w: 1920, h: 1080, fps: 30, wins },
        extra || {},
      ),
    ).map((l) => [edgeAgent, l]);
  const flat = (count, f) => Array.from({ length: count }, (_, i) => Object.assign({ target: 4000000, rate: 4000000, age: 50, floor: 40 }, f ? f(i) : {}));
  const demoOf = (rr) => (rr.sessions[0] ? rr.sessions[0].demonstrated.session : 'none');
  ok(demoOf(runPairs(edge('e00001', flat(15)))) === null, '14 rated windows are no run');
  ok(demoOf(runPairs(edge('e00002', flat(16)))) === 4000000, '15 rated, clean windows hold their lowest rate');
  ok(demoOf(runPairs(edge('e00003', flat(20, (i) => (i === 10 ? { age: 300 } : {}))))) === null, 'an elevated window breaks the run');
  ok(demoOf(runPairs(edge('e00004', flat(20, (i) => (i === 10 ? { age: null } : {}))))) === null, 'a window with no age breaks the run');
  ok(demoOf(runPairs(edge('e00005', flat(20, (i) => (i === 10 ? { gap: 12000 } : {}))))) === null, 'a hole in the log breaks the run');
  ok(demoOf(runPairs(edge('e00006', flat(20, (i) => (i === 10 ? { restart: 10 } : {}))))) === null, 'a new pump breaks the run');
  ok(demoOf(runPairs(edge('e00007', flat(20, (i) => (i === 10 ? { c: false } : {}))))) === null, 'a flip of constrained breaks the run');
  ok(demoOf(runPairs(edge('e00008', flat(20, () => ({ c: false }))))) === null, 'a run on the side the class does not run on is not counted');
  ok(demoOf(runPairs(edge('e00009', flat(20, (i) => ({ rate: i === 12 ? 1000000 : 4000000 }))))) === 1000000, 'a run holds its lowest window');
  ok(demoOf(runPairs(edge('e0000a', flat(20, (i) => (i === 10 ? { gap: 300 } : {}))))) === 4000000, 'a heartbeat too close to the last neither rates nor breaks');
  // A batch uploaded twice, or one that reached the server after a later one,
  // reads a fall in bytes_written. It is a replay, never a new pump.
  const once = edge('e0000b', flat(20));
  const beats = once.filter(([, l]) => /DC pump heartbeat/.test(l));
  const rOnce = runPairs(once);
  const rLate = runPairs(once.concat(beats.slice(12, 16), beats.slice(2, 5)));
  ok(
    rLate.sessions[0].pumps === 1 && rLate.provenance.replays === 7 && JSON.stringify(rLate.sessions) === JSON.stringify(rOnce.sessions),
    'heartbeats repeated, or late, are replays: one pump and the same session',
    [rLate.sessions[0].pumps, rLate.provenance.replays],
  );
  // The VP9-444 pump beats about once a second, so its run is 30 windows, and it
  // logs no applied rate and no rate-apply counter, so F2 cannot judge it.
  const vp9 = (sid, wins) => edge(sid, wins, { vp9: true, codec: 'VP9' });
  ok(demoOf(runPairs(vp9('e0000c', flat(30)))) === null && demoOf(runPairs(vp9('e0000d', flat(31)))) === 4000000, 'a VP9-444 run is 30 windows of about 1 s');
  const v9 = runPairs(vp9('e0000e', flat(361, (i) => ({ target: i < 180 ? 2000000 : 4000000 })))).sessions[0];
  ok(
    v9.encoder === 'vp9-444' && v9.durationS >= 300 && v9.target.max === 2 * v9.target.min && v9.flags.f2 === null && v9.moves === null && v9.opening.width === null,
    'a VP9-444 session reads its plan, and F2 does not judge it',
    v9 && [v9.encoder, v9.durationS, v9.flags, v9.moves],
  );

  // --- the flags' edges
  const pairRun = (openings) =>
    runPairs(
      edge('e10001', flat(16)).concat(
        ...openings.map((o, i) => edge('e1000' + (i + 2), flat(3, () => ({ target: o })), { at: Date.parse('2026-11-15T11:00:00Z') + i * 60000 })),
      ),
    );
  const pr = pairRun([2000000, 1999999, 3400000, 3399999]);
  const fl = (sid) => sessionOf(pr, sid).flags;
  ok(fl('e10002').f1 === false && fl('e10003').f1 === true, 'F1 is strictly below half: 2,000,000 against 4,000,000 is no flag, 1,999,999 is');
  ok(fl('e10004').f1At85 === false && fl('e10005').f1At85 === true, 'the 0.85 line is strict too');
  ok(judgeF2({ durationMs: 300000, tmin: 2e6, tmax: 3e6, appliedSeen: 2, moves: 0, ctrSeen: 0, ctrMoves: 0 }).raised === true, 'F2 at exactly 5 min and exactly 1.5× is raised');
  ok(judgeF2({ durationMs: 299999, tmin: 2e6, tmax: 3e6, appliedSeen: 2, moves: 0, ctrSeen: 0, ctrMoves: 0 }) === null, 'under 5 min is not judged');
  ok(judgeF2({ durationMs: 300000, tmin: 2e6, tmax: 2999999, appliedSeen: 2, moves: 0, ctrSeen: 0, ctrMoves: 0 }).raised === false, 'a target spread under 1.5× is no flag');
  ok(judgeF2({ durationMs: 300000, tmin: 2e6, tmax: 3e6, appliedSeen: 9, moves: 0, ctrSeen: 9, ctrMoves: 1 }).raised === false, 'one counted move is a move');
  ok(judgeF2({ durationMs: 300000, tmin: 2e6, tmax: 3e6, appliedSeen: 0, moves: 0, ctrSeen: 3, ctrMoves: 0 }).raised === true, 'with no maxrate_bps the counters judge alone');
  ok(judgeF2({ durationMs: 300000, tmin: 2e6, tmax: 3e6, appliedSeen: 1, moves: 0, ctrSeen: 0, ctrMoves: 0 }) === null, 'with no encoder rate read, F2 does not judge');
  const unstarted = runPairs(edge('e20001', flat(16), { start: false }));
  ok(unstarted.sessions[0].opening.judged === false && unstarted.flags.f1.judged === 0, 'a session without its start line read is not judged for F1');

  // --- the trail's other bases, and attribution
  const measured = runPairs(edge('e30001', flat(16, (i) => (i === 15 ? { goodput: 2400000 } : {})), { tag: null, write: { evidence: 2400000, kept: 2400000 } }));
  ok(measured.memoryTrail[0].basis === 'measured' && /goodput/.test(measured.memoryTrail[0].rule), 'a write equal to the last goodput is measured', measured.memoryTrail[0]);
  const opener = runPairs(edge('e30002', flat(16), { opener: { growth: 1800000, measured: 1800000 }, write: { evidence: 1800000, kept: 1800000 } }));
  ok(opener.memoryTrail[0].basis === 'measured' && /opener/.test(opener.memoryTrail[0].rule), "a write equal to the opener's growth target is measured", opener.memoryTrail[0]);
  const odd = runPairs(edge('e30003', flat(16), { write: { evidence: 1234567, kept: 1234567 } }));
  ok(odd.memoryTrail[0].basis === 'unexplained', 'a write nothing logged matches is unexplained', odd.memoryTrail[0]);
  const lateWrite = runPairs(edge('e30004', flat(16), { write: { evidence: 1234567, kept: 1234567, lag: WRITE_LAG_MS + 1000 } }));
  ok(lateWrite.memoryTrail[0].session === null && lateWrite.memoryTrail[0].basis === 'unexplained', 'a write too long after any pump end belongs to no session', lateWrite.memoryTrail[0]);
  const twoUsers = runPairs(edge('e40001', flat(16)).concat(edge('e40002', flat(16), { user: FX.op, at: Date.parse('2026-11-15T12:00:00Z') })));
  ok(twoUsers.pairs.length === 2, 'two users on one agent and carrier are two pairs', twoUsers.pairs.map((p) => p.user));

  // --- what leaves: no address, no name, no full id
  const md = render(r);
  const json = JSON.stringify(r);
  ok(!/SECRET/.test(md) && !/SECRET/.test(json), 'no address and no name reaches the report');
  ok(!/[0-9a-f]{24}/i.test(md) && !/[0-9a-f]{24}/i.test(json), 'no full id reaches the report');
  ok(/CORPLAP-2/.test(md) && /CORPLAP-1/.test(md) && !/c0a002/.test(md), 'a label replaces the agent id');
  ok(/\*\*F1, opened below what the pair demonstrated: RAISED\.\*\*/.test(md) && /\*\*F2, the plan moved and the encoder did not: RAISED\.\*\*/.test(md), 'both verdict lines render', md.split('\n').filter((l) => /\*\*F/.test(l)));
  ok(/\| the pair's demonstrated rate in the read window \| 11 \| 7 \| 7 \|/.test(md), 'the F1 table carries the 0.5 and the 0.85 lines');
  const quiet = render(runPairs(fx.filter(([ag]) => ag === FX.healthy)));
  ok(/F1, opened below what the pair demonstrated: clear/.test(quiet) && /F2, the plan moved and the encoder did not: clear/.test(quiet), 'a healthy fleet reads clear on both');

  // --- the fleet read's trap: a field the fold reads and the projection drops
  // reads as missing there and nowhere else. Push every fixture line through
  // the server's projection with the driver's types, then through the row the
  // fleet read prints, and the report must not change.
  const viaRow = new PathReport({ labels });
  const viaPrinted = new PathReport({ labels });
  let leaked = false;
  for (const [agent, l] of fx) {
    const p = parseLogLine(l);
    const row = { a: { toHexString: () => agent }, t: new Date(p.ts), m: p.msg, f: projectLikeTheServer(p.fields) };
    viaRow.addRow(row);
    const printed = JSON.stringify(rowOut(row));
    if (/SECRET/.test(printed) || printed.indexOf(FX.op) >= 0) leaked = true;
    viaPrinted.addRow(JSON.parse(printed));
  }
  ok(JSON.stringify(viaRow.finish()) === json, 'every field the fold reads survives the projection');
  ok(JSON.stringify(viaPrinted.finish()) === json, 'the printed rows read back to the same report');
  ok(!leaked, 'a printed row carries no address, no name and no full user id');

  // --- the rehearsal's seed, read the way the fleet read reads it: batches in
  // created_at order, the message filter, the projection, and ids and dates as
  // the driver returns them. A batch stamped out of order once gave a session a
  // second pump here.
  const msgRe = new RegExp(MSG_PATTERN);
  const seedBatches = fixtureSeed().map((s) => JSON.parse(s));
  const viaSeed = new PathReport({ labels });
  seedBatches
    .map((b, i) => ({ b, i }))
    .sort((x, y) => Date.parse(x.b.created_at.$date) - Date.parse(y.b.created_at.$date) || x.i - y.i)
    .forEach(({ b }) => {
      for (const line of b.lines) {
        if (!msgRe.test(line.msg)) continue;
        const stored = {};
        for (const k of Object.keys(line.fields)) stored[k] = fromEjson(line.fields[k]);
        viaSeed.addRow({ a: fromEjson(b.agent_id), t: fromEjson(line.ts), m: line.msg, f: serverProject(stored) });
      }
    });
  ok(JSON.stringify(viaSeed.finish()) === json, 'the rehearsal seed, in created_at order, reads back to the same report');
  ok(
    seedBatches.every((b) => b.lines.every((l, i) => i === 0 || Date.parse(l.ts.$date) >= Date.parse(b.lines[i - 1].ts.$date))),
    'no seed batch runs backwards in time',
  );
  const backwards = [];
  toEjson([parseLogLine(fxLine(10000, 'x', [])), parseLogLine(fxLine(5000, 'x', []))], FX.corplap1, FX.tenant, (s) => backwards.push(s));
  ok(backwards.length === 2, 'a line older than its batch starts a new batch', backwards.length);

  // --- the agent still says what this reads (a drift check, when run in a checkout)
  try {
    const fs = require('fs');
    const path = require('path');
    const src = (p) => fs.readFileSync(path.join(__dirname, '..', 'agents', 'roomlerd', 'src', ...p.split('/')), 'utf8');
    const ratectl = src('encode/ratectl.rs');
    const goodput = src('encode/goodput.rs');
    const priorRs = src('encode/prior.rs');
    const modRs = src('encode/mod.rs');
    const agentSrc = [src('peer.rs'), src('signaling.rs'), src('session_carrier.rs')].join('\n');
    const m1 = /pub const AGE_SLACK_MS: u16 = (\d+);/.exec(ratectl);
    ok(m1 && Number(m1[1]) === AGE_SLACK_MS, 'AGE_SLACK_MS matches ratectl.rs', m1 && m1[1]);
    const up = /const ALPHA_UP: f64 = ([\d.]+);/.exec(goodput);
    const down = /const ALPHA_DOWN: f64 = ([\d.]+);/.exec(goodput);
    ok(up && down && Number(up[1]) === ALPHA_UP && Number(down[1]) === ALPHA_DOWN, 'ALPHA_UP and ALPHA_DOWN match goodput.rs', [up && up[1], down && down[1]]);
    const dn = /pub const DECAY_NUM: u64 = (\d+);/.exec(priorRs);
    const dd = /pub const DECAY_DEN: u64 = (\d+);/.exec(priorRs);
    ok(dn && dd && Number(dn[1]) === DECAY_NUM && Number(dd[1]) === DECAY_DEN, 'DECAY_NUM / DECAY_DEN match prior.rs', [dn && dn[1], dd && dd[1]]);
    const band = /pub const MIN_BITRATE_BPS: u32 = ([\d_]+);/.exec(modRs);
    ok(band && Number(band[1].replace(/_/g, '')) === PRIOR_BAND_BPS, 'PRIOR_BAND_BPS matches MIN_BITRATE_BPS in mod.rs', band && band[1]);
    const missing = Object.keys(LINES).filter((k) => !LINES[k].test(agentSrc.replace(/\\\r?\n\s*/g, '')));
    ok(missing.length === 0, "every message this reads is still in the agent's source", missing);
    const absent = FIELDS.concat(['peer', 'controller_user_id']).filter((k) => !new RegExp('\\b' + k + '\\b').test(agentSrc));
    ok(absent.length === 0, "every field this reads is still in the agent's source", absent);
  } catch (e) {
    console.log(`# skipped the drift checks against the agent's source: ${e.message}`);
  }

  console.log(failed ? `# ${failed} of ${n} FAILED` : `# all ${n} passed`);
  return failed ? 1 : 0;
}

// ---------------------------------------------------------------- Node runner

function nodeMain(argv) {
  const opts = { json: false, files: [], toEjson: false, labels: [] };
  let agentHex = null;
  let tenantHex = FX.tenant;
  for (let i = 0; i < argv.length; i++) {
    const a = argv[i];
    if (a === '--self-test') return process.exit(selfTest());
    if (a === '--fixtures') {
      // The self-test's chains as `agent_logs` batches: the rehearsal's seed.
      for (const batch of fixtureSeed()) process.stdout.write(batch + '\n');
      return;
    }
    if (a === '--json') opts.json = true;
    else if (a === '--to-ejson') opts.toEjson = true;
    else if (a === '--agent') agentHex = argv[++i];
    else if (a === '--tenant') tenantHex = argv[++i];
    else if (a === '--label') {
      try {
        opts.labels.push(...parseLabels(argv[++i]));
      } catch (e) {
        console.error(`rc-path-report: ${e.message}`);
        return process.exit(1);
      }
    } else if (a === '-h' || a === '--help') {
      console.log(
        'usage: rc-path-report.cjs [--json] [--label HEX=NAME]... [--agent HEX] FILE...   (rows from --rows, or host log text)\n' +
          '       rc-path-report.cjs --to-ejson [--agent HEX] [--tenant HEX] FILE...      (a rehearsal seed from host logs)\n' +
          '       rc-path-report.cjs --fixtures                                         (the self-test chains, as a seed)\n' +
          '       rc-path-report.cjs --self-test',
      );
      return process.exit(0);
    } else opts.files.push(a);
  }
  for (const h of [agentHex, tenantHex]) {
    if (h !== null && !/^[0-9a-fA-F]{24}$/.test(h)) {
      console.error('rc-path-report: --agent and --tenant take a 24-hex id');
      return process.exit(1);
    }
  }
  if (!opts.files.length) {
    console.error('rc-path-report: no input. Pass row files, log files, or - for stdin (--help).');
    return process.exit(1);
  }
  const fs = require('fs');
  const readline = require('readline');
  const report = new PathReport(opts);
  const seedLines = [];
  let i = 0;
  const next = () => {
    if (i >= opts.files.length) {
      if (opts.toEjson) {
        toEjson(seedLines, agentHex || hex24('f148', 'a01'), tenantHex, (s) => process.stdout.write(s + '\n'));
        return;
      }
      const r = report.finish();
      console.log(opts.json ? JSON.stringify(r, null, 2) : render(r));
      return;
    }
    const file = opts.files[i++];
    const input = file === '-' ? process.stdin : fs.createReadStream(file, { encoding: 'utf8' });
    const rl = readline.createInterface({ input, crlfDelay: Infinity });
    rl.on('line', (line) => {
      if (line[0] === '{') {
        try {
          const row = JSON.parse(line);
          if (row && row.m) report.addRow(row);
        } catch (_) {
          /* not a row */
        }
        return;
      }
      const p = parseLogLine(line);
      if (!p) return;
      if (opts.toEjson) seedLines.push(p);
      else report.add(p.msg, p.fields, p.ts, agentHex);
    });
    rl.on('close', next);
  };
  next();
}

// ---------------------------------------------------------------- dispatch

if (typeof module !== 'undefined' && module.exports) {
  module.exports = { PathReport, parseLogLine, value, render, damp, standIn, quarterSteps, judgeF1, judgeF2, fixtures, FIELDS, MSG_PATTERN };
}
if (typeof Mongo === 'function' && typeof print === 'function') {
  runMongosh();
} else if (typeof require !== 'undefined' && typeof module !== 'undefined' && require.main === module) {
  nodeMain(process.argv.slice(2));
}
