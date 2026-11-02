#!/usr/bin/env node
//
// rc-shadow-report.cjs — FR-63 AC2's instrument: has the shadow rate controller
// agreed with the live governor closely enough to be flipped live?
//
// WHY THIS EXISTS
//
//   FR-63 B1 (agent-v0.4.188 and later) runs `encode::ratectl::Controller` in
//   SHADOW beside the live `RateGovernor`. It drives nothing; it logs what it
//   WOULD have decided in six `shadow_*` fields on both DC-pump heartbeats. B2
//   may flip it live only on the flip criterion of
//   docs/fr/FR-63-one-delay-based-rate-controller.md:
//
//       >= 20 constrained + >= 20 direct sessions, and shadow <= live in
//       >= 90 % of the elevated-age windows (and all four B0 fixtures green).
//
//   This script MEASURES that criterion. It does not decide it: whether the bar
//   is the right one, and whether to flip, are the operator's calls.
//
// TWO READS, ONE CORE
//
//   1. The FLEET read, the one AC2 is judged by. Run under mongosh inside the
//      Mongo container, over `agent_logs`: every daemon and every RC worker
//      uploads its INFO lines there (7-day TTL), across every org. That is the
//      only unbiased population. An exec sweep of host logs reaches only
//      exec-enabled, online hosts of one org, and the criterion is a fleet-wide
//      PROPORTION. `GET …/agent/{id}/logs` cannot serve it either: it takes
//      only `limit` (<= 500 recent batches per agent, roughly 40 minutes of a
//      session). scripts/rc-shadow-report.sh carries the recipe.
//
//   2. The HOST read, under Node: rolling-log text (a host's `roomlerd.log.*`,
//      `roomler logs` output, `roomler exec` output) or the NDJSON rows the
//      fleet read prints with RC_SHADOW_EMIT=rows. It is evidence about the
//      hosts it read, never a fleet proportion.
//
// WHAT A WINDOW IS, AND WHICH ONES COUNT
//
//   One DC-pump heartbeat (FFmpeg ≈2 s, VP9-444 ≈1 s) is one window.
//   - no `shadow_state` field          -> a build without B1 (counted, ignored)
//   - `shadow_state="off"`             -> ROOMLERD_RATECTL=off (counted, ignored)
//   - `shadow_target_bps=None`         -> the shadow has not opened yet
//   - `target_bps=0`                   -> nothing applied yet
//   - otherwise it is compared: shadow <= live is AGREE (the safe direction).
//   ELEVATED = viewer_age_ms - viewer_age_floor_ms >= 70 ms, the controller's own
//   level threshold (ratectl::AGE_SLACK_MS); CLEAN when below it; NO-AGE when
//   the viewer sent no age (an old viewer, or no window yet).
//
//   The path class comes from the heartbeat's `constrained` flag (the FFmpeg
//   pump logs it every window). The VP9-444 heartbeat does not carry it, so a
//   VP9 session takes its class from the session's "per-session ICE path
//   detected" line (`relay=`) when that line was read, and is `unknown`
//   otherwise. The ICE line's `relay` misses one case the pump's flag catches
//   (an overlay remote on a relay tier), so ICE-classed windows are reported
//   apart.
//
// WHAT THE CRITERION CANNOT SEE, AND WHAT IS PRINTED FOR IT
//
//   Agreement is `shadow <= live`, so a shadow pinned at its floor scores 100 %:
//   the criterion has no power against asking for too little. The disagreement
//   cannot say either, because `target_bps` is a cap the encoder is usually far
//   under, so -96 % reads the same on a resting desktop that lost nothing and on
//   a scroll spending 14 times the shadow's target.
//
//   So the report also compares the shadow with what the encoder actually SPENT:
//   `bytes_written` over the gap between two heartbeats of one pump. Spending over
//   the shadow across a whole window proves it would have capped real spending.
//   Under it proves less: a burst inside the window can still have been capped.
//   The heartbeat has no maxrate field, so the spend bounds the encoder's demand
//   from below and nothing bounds it from above. No bar is applied: the flip
//   criterion is unchanged, and whether it gains a clause is the operator's call.
//
// RUN
//
//   node scripts/rc-shadow-report.cjs --self-test
//   node scripts/rc-shadow-report.cjs [--json] [--min-windows N] FILE...
//   scripts/rc-shadow-report.sh --mars --yes [--since ISO] [--until ISO]   # the fleet read
//
// The core below runs unchanged under Node and under mongosh, so it uses no
// Node-only API; the two runners at the bottom are the only environment-specific
// code. It is `.cjs` because it is CommonJS and the repo's root package.json
// declares no module type.

'use strict';

// ---------------------------------------------------------------- constants

/** `encode::ratectl::AGE_SLACK_MS` — age over the learned floor that is an
 *  elevated LEVEL. The self-test checks this against the Rust source. */
const AGE_SLACK_MS = 70;
/** `encode::ratectl::CONSTRAINED_FLOOR_BPS` — the controller's floor on a
 *  constrained path. A constrained shadow target below it is a defect. */
const CONSTRAINED_FLOOR_BPS = 200000;
/** The flip criterion, as the spec states it. */
const FLIP_MIN_SESSIONS = 20;
const FLIP_MIN_AGREEMENT = 0.9;

/** The lines this reads. Message text, not target: a heartbeat's message is
 *  stable across both pumps, and the fleet read filters on it server-side. */
const HEARTBEAT_RE = /DC pump heartbeat/;
const ICE_RE = /per-session ICE path detected/;
/** The same, as one pattern for the server-side `$regexMatch`. */
const MSG_PATTERN = 'DC pump heartbeat|per-session ICE path detected';

/** The only fields the report reads. The fleet read projects exactly these,
 *  so nothing else (addresses, names) ever leaves the database. */
const FIELDS = [
  'session_id',
  'constrained',
  'relay',
  'encoder',
  'target_bps',
  'bytes_written',
  'viewer_age_ms',
  'viewer_age_floor_ms',
  'shadow_target_bps',
  'shadow_state',
  'shadow_measured_bps',
  'shadow_disagree_pct',
  'shadow_pauses',
  'shadow_reason',
];

const CLASSES = ['constrained', 'direct', 'unknown'];

/** What the encoder spent, in bps: each row's lower edge is inclusive. */
const SPENT_EDGES_BPS = [500000, 1000000, 2000000, 5000000, 10000000, 20000000, 50000000];
const SPENT_LABELS = ['< 0.5', '0.5 – 1', '1 – 2', '2 – 5', '5 – 10', '10 – 20', '20 – 50', '≥ 50'];
/** spent ÷ the shadow's target: bin 0 is covered, each upper edge is inclusive. */
const OVER_EDGES = [1, 1.25, 2, 4, 8];
const OVER_LABELS = ['covered', '≤ ×1.25', '≤ ×2', '≤ ×4', '≤ ×8', '> ×8'];
/** A pump's opening. The law climbs ×1.08 per 2-s window, about 47 rises to a
 *  ~100 Mbps ceiling (FR-63 § "Day 0"), so 120 s holds the whole climb. */
const OPENING_MS = 120000;
/** Two heartbeats of one pump make a rate only this far apart. Nominally 1-2 s:
 *  closer is a replay, and further is a hole in the log, whose average says
 *  nothing about any one window. */
const RATE_MIN_GAP_MS = 500;
const RATE_MAX_GAP_MS = 10000;

// ---------------------------------------------------------------- values

/**
 * One field value, from either source, as a plain JS value.
 * - text log:   `48000000`, `Some(6)`, `None`, `"hold"`, `false`, `Some("x")`
 * - agent_logs: numbers (u64/i64/f64), booleans, and strings — a `?Debug`
 *               field is the string "Some(6)" / "None", a `&str` field its text.
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

function str(v) {
  const x = value(v);
  return x === null ? null : String(x);
}

/** An identifier or a label: quotes and `Some(…)` stripped, never read as a
 *  number (an all-digit ObjectId must not become a float). */
function text(v) {
  if (v === null || v === undefined) return null;
  if (typeof v === 'object') return typeof v.toHexString === 'function' ? v.toHexString() : null;
  let s = String(v).trim();
  if (s === 'None' || s === '') return null;
  const some = /^Some\((.*)\)$/.exec(s);
  if (some) s = some[1].trim();
  if (s.length >= 2 && s[0] === '"' && s[s.length - 1] === '"') s = s.slice(1, -1);
  return s;
}

function bool(v) {
  const x = value(v);
  return typeof x === 'boolean' ? x : null;
}

// ---------------------------------------------------------------- text lines

/**
 * One rolling-log line: `<ts>  <LEVEL> <message> key=value key=value …`.
 * Returns `{ ts, msg, fields }`, or null for anything else (a `roomler logs`
 * header, an exec banner, a blank line). Values keep their raw text; `value()`
 * reads them. A value may hold spaces inside (), {}, [] or quotes —
 * `age_split=Some(AgeSplit { sender_ms: … })` — so a new field starts only at
 * depth 0, on a space followed by `identifier=`.
 */
function parseLogLine(line) {
  const m = /^(\d{4}-\d{2}-\d{2}T[0-9:.]+Z?)\s+(TRACE|DEBUG|INFO|WARN|ERROR)\s+(.*)$/.exec(
    line.replace(/\r$/, ''),
  );
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

// ---------------------------------------------------------------- the fold

function newBucket() {
  return {
    windows: 0,
    compared: 0,
    agree: 0,
    elevated: 0,
    elevatedAgree: 0,
    clean: 0,
    cleanAgree: 0,
    noAge: 0,
    noShadow: 0,
    noLive: 0,
    neg: 0,
    zero: 0,
    pos: 0,
    belowFloor: 0,
    pauseRisesOnDirect: 0,
    viaIce: 0,
    noRate: 0,
    cleanOver: 0,
    controlNear: 0,
    controlOf: 0,
    hist: { elevated: new Map(), clean: new Map(), noAge: new Map() },
    // One sparse cube per age class, keyed phase|spent bin|over bin|encoder|state.
    // Every table the report prints is a marginal of it.
    spend: { elevated: new Map(), clean: new Map(), noAge: new Map() },
    states: new Map(),
    reasons: new Map(),
  };
}

function bump(map, key, by) {
  map.set(key, (map.get(key) || 0) + (by === undefined ? 1 : by));
}

function mergeBucket(into, from) {
  for (const k of Object.keys(from)) {
    if (typeof from[k] === 'number') into[k] += from[k];
  }
  for (const h of ['elevated', 'clean', 'noAge']) {
    for (const [k, v] of from.hist[h]) bump(into.hist[h], k, v);
    for (const [k, v] of from.spend[h]) bump(into.spend[h], k, v);
  }
  for (const [k, v] of from.states) bump(into.states, k, v);
  for (const [k, v] of from.reasons) bump(into.reasons, k, v);
}

function epochMs(ts) {
  if (ts === null || ts === undefined) return null;
  const s = String(ts);
  const ms = Date.parse(s.endsWith('Z') ? s : s + 'Z');
  return Number.isFinite(ms) ? ms : null;
}

function spentBin(bps) {
  const i = SPENT_EDGES_BPS.findIndex((edge) => bps < edge);
  return i < 0 ? SPENT_EDGES_BPS.length : i;
}

function overBin(ratio) {
  const i = OVER_EDGES.findIndex((edge) => ratio <= edge);
  return i < 0 ? OVER_EDGES.length : i;
}

/**
 * What one pump sent since its last heartbeat. `bytes_written` is the send task's
 * running total of whole frames the transport took, so a rate is the difference
 * over the two heartbeats' own gap. Null when there is no honest rate.
 *
 * `cover` is the higher of the shadow's targets at the two ends, because a shadow
 * that cut mid-window held the higher one for part of it. That can only UNDER-count
 * a bite, which is the side to err on: the reading exists to prove one.
 */
function pumpSpend(s, kind, f, ts, shadow) {
  const bytes = num(f.bytes_written);
  const at = epochMs(ts);
  if (bytes === null || at === null) return null;
  const p = s.pump;
  // Behind the mark: a replayed batch, never a window, and it must not rewind it.
  if (p && at <= p.at) return null;
  // A counter that falls is a new pump (a new codec, a reopened channel), with a
  // new governor, so its shadow opens again too.
  if (!p || p.kind !== kind || bytes < p.bytes) {
    s.pump = { kind, at, bytes, shadow, openedAt: at };
    return null;
  }
  const gap = at - p.at;
  if (gap < RATE_MIN_GAP_MS) return null;
  const spent =
    gap > RATE_MAX_GAP_MS
      ? null
      : {
          bps: ((bytes - p.bytes) * 8000) / gap,
          cover: Math.max(shadow || 0, p.shadow || 0),
          opening: at - p.openedAt < OPENING_MS,
        };
  p.at = at;
  p.bytes = bytes;
  p.shadow = shadow;
  return spent;
}

function noteSpend(b, judged, spent, encoder, state, measured) {
  if (!spent) {
    b.noRate++;
    return;
  }
  const over = overBin(spent.bps / spent.cover);
  bump(b.spend[judged], [spent.opening ? 'opening' : 'later', spentBin(spent.bps), over, encoder, state].join('|'));
  if (judged === 'clean' && over > 0) b.cleanOver++;
  // The positive control: the sender's own count against what the viewer says
  // arrived, two readings that share nothing. See the render.
  if (measured) {
    b.controlOf++;
    if (spent.bps * 2 >= measured && spent.bps <= measured * 2) b.controlNear++;
  }
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

class ShadowReport {
  constructor(opts) {
    this.minWindows = Math.max(1, Number((opts && opts.minWindows) || 1));
    this.sessions = new Map();
    this.agents = new Set();
    this.lines = 0;
    this.heartbeats = 0;
    this.preB1 = 0;
    this.off = 0;
    this.first = null;
    this.last = null;
  }

  session(agent, sid) {
    const key = (agent || '') + '/' + sid;
    let s = this.sessions.get(key);
    if (!s) {
      s = { iceRelay: null, cls: {}, lastPauses: null, kinds: new Set(), pump: null };
      this.sessions.set(key, s);
    }
    return s;
  }

  /** Feed one line: its message, its fields (raw values from either source),
   *  its timestamp, and the agent it came from (null for a single host's log). */
  add(msg, f, ts, agent) {
    this.lines++;
    if (!msg || !f) return;
    if (ICE_RE.test(msg)) {
      const sid = text(f.session_id);
      const relay = bool(f.relay);
      if (sid && relay !== null) this.session(agent, sid).iceRelay = relay;
      return;
    }
    if (!HEARTBEAT_RE.test(msg)) return;
    const sid = text(f.session_id);
    if (!sid) return;
    this.heartbeats++;
    if (agent) this.agents.add(agent);
    if (ts) {
      const t = String(ts);
      if (!this.first || t < this.first) this.first = t;
      if (!this.last || t > this.last) this.last = t;
    }
    const s = this.session(agent, sid);
    if (!('shadow_state' in f)) {
      this.preB1++;
      return;
    }
    const state = text(f.shadow_state);
    if (state === 'off') {
      this.off++;
      return;
    }
    const kind = /FFmpeg/.test(msg) ? 'ffmpeg' : 'vp9';
    s.kinds.add(kind);
    const c = bool(f.constrained);
    const cls = c === true ? 'constrained' : c === false ? 'direct' : 'unknown';
    const b = s.cls[cls] || (s.cls[cls] = newBucket());
    b.windows++;
    bump(b.states, state || '?');
    bump(b.reasons, text(f.shadow_reason) || '?');

    // The "never pauses on direct" invariant. `shadow_pauses` is the session's
    // running count, so a rise seen in a DIRECT window is a pause ordered there.
    const pauses = num(f.shadow_pauses);
    if (pauses !== null) {
      if (cls === 'direct' && s.lastPauses !== null && pauses > s.lastPauses) b.pauseRisesOnDirect++;
      s.lastPauses = pauses;
    }

    const live = num(f.target_bps);
    const shadow = num(f.shadow_target_bps);
    // Before the two returns: a window that is not compared still moves the
    // pump's mark, or the next rate would average over two windows.
    const spent = pumpSpend(s, kind, f, ts, shadow);
    if (shadow === null) {
      b.noShadow++;
      return;
    }
    if (!live) {
      b.noLive++;
      return;
    }
    if (shadow < CONSTRAINED_FLOOR_BPS) b.belowFloor++;
    b.compared++;
    const agree = shadow <= live;
    if (agree) b.agree++;
    // Mirrors `RateGovernor::shadow_disagree_pct` (an i64 division, truncated).
    let pct = num(f.shadow_disagree_pct);
    if (pct === null) pct = Math.trunc(((shadow - live) * 100) / live);
    if (pct < 0) b.neg++;
    else if (pct === 0) b.zero++;
    else b.pos++;
    const age = num(f.viewer_age_ms);
    const floor = num(f.viewer_age_floor_ms);
    let judged;
    if (age === null || floor === null) {
      b.noAge++;
      bump(b.hist.noAge, pct);
      judged = 'noAge';
    } else if (age - floor >= AGE_SLACK_MS) {
      b.elevated++;
      if (agree) b.elevatedAgree++;
      bump(b.hist.elevated, pct);
      judged = 'elevated';
    } else {
      b.clean++;
      if (agree) b.cleanAgree++;
      bump(b.hist.clean, pct);
      judged = 'clean';
    }
    // The VP9-444 heartbeat has no `encoder` field: it is always libvpx.
    const encoder = kind === 'vp9' ? 'vp9-444' : text(f.encoder) || '?';
    noteSpend(b, judged, spent, encoder, state || '?', num(f.shadow_measured_bps));
  }

  /** Feed one NDJSON / fleet-read row: `{ a, t, m, f }`. */
  addRow(row) {
    if (!row) return;
    this.add(row.m, row.f || {}, row.t instanceof Date ? row.t.toISOString() : row.t, row.a ? text(row.a) : null);
  }

  finish() {
    const classes = {};
    for (const c of CLASSES) {
      classes[c] = { sessions: 0, viaIce: 0, over: 0, windows: 0, bucket: newBucket() };
    }
    let sessionsWithShadow = 0;
    const ffmpegSessions = { n: 0 };
    const vp9Sessions = { n: 0 };
    for (const s of this.sessions.values()) {
      // A VP9 window has no class of its own: give it the session's ICE class.
      const unknown = s.cls.unknown;
      if (unknown && s.iceRelay !== null) {
        const into = s.iceRelay ? 'constrained' : 'direct';
        const target = s.cls[into] || (s.cls[into] = newBucket());
        mergeBucket(target, unknown);
        target.viaIce += unknown.windows;
        delete s.cls.unknown;
      }
      let any = false;
      for (const c of CLASSES) {
        const b = s.cls[c];
        if (!b || !b.windows) continue;
        any = true;
        mergeBucket(classes[c].bucket, b);
        classes[c].windows += b.windows;
        if (b.windows >= this.minWindows) {
          classes[c].sessions++;
          if (b.viaIce) classes[c].viaIce++;
          if (b.cleanOver) classes[c].over++;
        }
      }
      if (any) sessionsWithShadow++;
      if (s.kinds.has('ffmpeg')) ffmpegSessions.n++;
      if (s.kinds.has('vp9')) vp9Sessions.n++;
    }

    const out = {
      provenance: {
        lines: this.lines,
        heartbeats: this.heartbeats,
        windowsWithoutShadow: this.preB1,
        windowsShadowOff: this.off,
        sessions: this.sessions.size,
        sessionsWithShadow,
        ffmpegSessions: ffmpegSessions.n,
        vp9Sessions: vp9Sessions.n,
        agents: this.agents.size,
        first: this.first,
        last: this.last,
        minWindows: this.minWindows,
      },
      classes: {},
    };
    const pooled = newBucket();
    for (const c of CLASSES) {
      const k = classes[c];
      const b = k.bucket;
      if (c !== 'unknown') mergeBucket(pooled, b);
      out.classes[c] = summarise(b, { sessions: k.sessions, sessionsViaIce: k.viaIce, sessionsOver: k.over });
    }
    out.pooled = summarise(pooled, {});

    // The criterion, clause by clause, as the spec words it: sessions per
    // class; the agreement over ALL elevated windows (pooled). The per-class
    // agreement is printed beside it, so a class that falls short while the
    // pool passes is visible — that is the operator's call, not this script's.
    const clauses = [];
    for (const c of ['constrained', 'direct']) {
      const n = out.classes[c].sessions;
      clauses.push({
        clause: `${c} sessions >= ${FLIP_MIN_SESSIONS}`,
        value: n,
        status: n >= FLIP_MIN_SESSIONS ? 'met' : 'not met',
      });
    }
    const e = out.pooled.elevated;
    clauses.push({
      clause: `shadow <= live in >= ${FLIP_MIN_AGREEMENT * 100} % of elevated-age windows (pooled)`,
      value: e ? `${out.pooled.elevatedAgree}/${e} = ${pctText(out.pooled.elevatedAgree / e)}` : '0 elevated windows',
      status: !e ? 'no evidence' : out.pooled.elevatedAgree / e >= FLIP_MIN_AGREEMENT ? 'met' : 'not met',
    });
    clauses.push({
      clause: 'all four B0 fixtures green',
      value: 'cargo test -p roomlerd --lib encode::sim (CI)',
      status: 'not checked here',
    });
    const notMet = clauses.some((x) => x.status === 'not met');
    const noEvidence = clauses.some((x) => x.status === 'no evidence');
    out.criterion = {
      clauses,
      verdict: notMet ? 'NOT MET' : noEvidence ? 'INCONCLUSIVE' : 'MET',
      classesBelowBar: ['constrained', 'direct'].filter(
        (c) => out.classes[c].elevated && out.classes[c].elevatedAgree / out.classes[c].elevated < FLIP_MIN_AGREEMENT,
      ),
    };

    // B1's invariants, read off the field instead of a test.
    out.invariants = {
      pauseRisesOnDirect: out.classes.direct.pauseRisesOnDirect,
      constrainedBelowFloor: out.classes.constrained.belowFloor,
    };
    return out;
  }
}

function summarise(b, extra) {
  const o = Object.assign({}, extra, {
    windows: b.windows,
    compared: b.compared,
    agree: b.agree,
    elevated: b.elevated,
    elevatedAgree: b.elevatedAgree,
    clean: b.clean,
    cleanAgree: b.cleanAgree,
    noAge: b.noAge,
    noShadow: b.noShadow,
    noLive: b.noLive,
    sign: { neg: b.neg, zero: b.zero, pos: b.pos },
    disagreePct: {
      elevated: { p50: percentile(b.hist.elevated, 0.5), p90: percentile(b.hist.elevated, 0.9) },
      clean: { p50: percentile(b.hist.clean, 0.5), p90: percentile(b.hist.clean, 0.9) },
    },
    pauseRisesOnDirect: b.pauseRisesOnDirect,
    belowFloor: b.belowFloor,
    viaIceWindows: b.viaIce,
    states: Object.fromEntries([...b.states].sort((x, y) => y[1] - x[1])),
    reasons: Object.fromEntries([...b.reasons].sort((x, y) => y[1] - x[1])),
    noRate: b.noRate,
    control: { near: b.controlNear, of: b.controlOf },
    spend: {
      clean: spendSummary(b.spend.clean),
      noAge: spendSummary(b.spend.noAge),
      elevated: spendSummary(b.spend.elevated),
    },
  });
  return o;
}

/** One age class's cube: its totals, the rows the report prints, and the cube
 *  itself for `--json`, so a later cut needs no second read of the database. */
function spendSummary(cube) {
  const o = {
    rated: 0,
    over: 0,
    opening: { rated: 0, over: 0 },
    later: { rated: 0, over: 0 },
    bins: SPENT_LABELS.map((spent) => ({
      spent,
      windows: 0,
      over: 0,
      over25: 0,
      over2x: 0,
      over4x: 0,
      later: { windows: 0, over: 0 },
      atOrAbove: { windows: 0, over: 0 },
    })),
    encoders: {},
    overStates: {},
    cells: [],
  };
  // Sorted, so the same windows print the same JSON whichever read fed them.
  for (const key of [...cube.keys()].sort()) {
    const n = cube.get(key);
    const [phase, spentAt, overAt, encoder, state] = key.split('|');
    const at = Number(overAt);
    const row = o.bins[Number(spentAt)];
    const enc = o.encoders[encoder] || (o.encoders[encoder] = { rated: 0, over: 0 });
    o.rated += n;
    o[phase].rated += n;
    row.windows += n;
    enc.rated += n;
    if (phase === 'later') row.later.windows += n;
    if (at > 0) {
      o.over += n;
      o[phase].over += n;
      row.over += n;
      enc.over += n;
      o.overStates[state] = (o.overStates[state] || 0) + n;
      if (phase === 'later') row.later.over += n;
    }
    if (at > 1) row.over25 += n;
    if (at > 2) row.over2x += n;
    if (at > 3) row.over4x += n;
    o.cells.push({ phase, spent: SPENT_LABELS[Number(spentAt)], over: OVER_LABELS[at], encoder, state, windows: n });
  }
  let windows = 0;
  let over = 0;
  for (let i = o.bins.length - 1; i >= 0; i--) {
    windows += o.bins[i].windows;
    over += o.bins[i].over;
    o.bins[i].atOrAbove = { windows, over };
  }
  return o;
}

function pctText(x) {
  return (Math.round(x * 1000) / 10).toFixed(1) + ' %';
}

function fmtPct(v) {
  return v === null || v === undefined ? '—' : (v > 0 ? '+' : '') + v + ' %';
}

/** The report as Markdown, ready to paste into the FR issue. Counts only: no
 *  agent id, session id, name or address is ever printed. */
function render(r) {
  const p = r.provenance;
  const L = [];
  L.push('## FR-63 shadow report');
  L.push('');
  L.push(
    `${p.heartbeats} heartbeat windows from ${p.sessions} sessions` +
      (p.agents ? ` on ${p.agents} agents` : '') +
      (p.first ? `, ${p.first} → ${p.last}` : '') +
      `. ${p.sessionsWithShadow} sessions carry the shadow; ${p.windowsWithoutShadow} windows came from builds without it` +
      (p.windowsShadowOff ? `, ${p.windowsShadowOff} had it switched off` : '') +
      `. A session counts toward a class with >= ${p.minWindows} window(s) in it.`,
  );
  L.push('');
  L.push('| class | sessions | windows | compared | elevated | shadow ≤ live, elevated | shadow ≤ live, clean | disagree p50 / p90, elevated | disagree p50 / p90, clean | sign − / 0 / + |');
  L.push('|---|---|---|---|---|---|---|---|---|---|');
  for (const c of [...CLASSES, 'pooled']) {
    const x = c === 'pooled' ? r.pooled : r.classes[c];
    if (c === 'unknown' && !x.windows) continue;
    const label = c === 'pooled' ? '**pooled** (constrained + direct)' : c + (x.viaIceWindows ? ` (${x.viaIceWindows} windows classed by the ICE line)` : '');
    L.push(
      `| ${label} | ${c === 'pooled' ? '—' : x.sessions} | ${x.windows} | ${x.compared} | ${x.elevated} | ` +
        `${x.elevated ? `${x.elevatedAgree}/${x.elevated} = ${pctText(x.elevatedAgree / x.elevated)}` : '—'} | ` +
        `${x.clean ? `${x.cleanAgree}/${x.clean} = ${pctText(x.cleanAgree / x.clean)}` : '—'} | ` +
        `${fmtPct(x.disagreePct.elevated.p50)} / ${fmtPct(x.disagreePct.elevated.p90)} | ` +
        `${fmtPct(x.disagreePct.clean.p50)} / ${fmtPct(x.disagreePct.clean.p90)} | ` +
        `${x.sign.neg} / ${x.sign.zero} / ${x.sign.pos} |`,
    );
  }
  L.push('');
  L.push('**The flip criterion (FR-63 AC2)**');
  L.push('');
  L.push('| clause | value | status |');
  L.push('|---|---|---|');
  for (const c of r.criterion.clauses) L.push(`| ${c.clause} | ${c.value} | ${c.status} |`);
  L.push('');
  L.push(`**Verdict: ${r.criterion.verdict}** (the measurement; the flip is the operator's call).`);
  // A MET is the line a reader stops at, and it can only ever speak to asking for
  // too much. Said on every run, so the read does not depend on knowing that.
  L.push('');
  L.push('This verdict can fail a shadow that asks for too much, never one that asks for too little: read it with "What the encoder spent" below.');
  if (r.criterion.classesBelowBar.length) {
    L.push('');
    L.push(`⚠️ Below the bar on its own: ${r.criterion.classesBelowBar.join(', ')}.`);
  }
  L.push('');
  L.push('**B1 invariants, read in the field**');
  L.push('');
  if (!p.sessionsWithShadow) {
    L.push('- no evidence: no window in this input carries the shadow (a build before agent-v0.4.188, `ROOMLERD_RATECTL=off`, or no RC session)');
    return L.join('\n');
  }
  L.push(`- pauses ordered in a DIRECT window ("never on direct"): ${r.invariants.pauseRisesOnDirect}`);
  L.push(`- constrained windows with the shadow below its 200 kbps floor: ${r.invariants.constrainedBelowFloor}`);
  for (const c of ['constrained', 'direct']) {
    const x = r.classes[c];
    if (!x.windows) continue;
    const st = Object.entries(x.states).map(([k, v]) => `${k} ${v}`).join(', ');
    L.push(`- ${c} shadow states: ${st}`);
  }
  // The criterion judges elevated windows only, so a shadow that asks for far
  // less on a CLEAN path passes it (the day-0 read found exactly that on direct
  // sessions). Said plainly, with no threshold: what bar it needs is the
  // operator's call.
  L.push('');
  L.push('**Clean windows (the criterion does not judge these)**');
  L.push('');
  for (const c of ['constrained', 'direct']) {
    const x = r.classes[c];
    if (!x.clean) continue;
    L.push(
      `- ${c}: the shadow asked for no more than live in ${x.cleanAgree} of ${x.clean} clean windows; ` +
        `disagreement p50 ${fmtPct(x.disagreePct.clean.p50)}, p90 ${fmtPct(x.disagreePct.clean.p90)}`,
    );
  }
  renderSpend(r, L);
  return L.join('\n');
}

function share(over, of) {
  return of ? `${over}/${of} = ${pctText(over / of)}` : '—';
}

/** What the encoder spent against what the shadow would have allowed. Counts
 *  only, and no bar: each row carries the share at its spend or more, so any gate
 *  can be read off this one report, and which gate is the operator's call. */
function renderSpend(r, L) {
  L.push('');
  L.push('**What the encoder spent, against the shadow (clean windows; the criterion does not judge these)**');
  L.push('');
  const present = CLASSES.filter((c) => r.classes[c].spend.clean.rated || r.classes[c].spend.noAge.rated);
  if (!present.length) {
    L.push('- no evidence: no window has a spend rate (a build without `bytes_written`, or sessions of one window)');
    return;
  }
  L.push(
    '`spent` is `Δbytes_written` between two heartbeats of one pump, against the higher of the shadow\'s targets at the two ends. ' +
      'Over it across a whole window proves the shadow would have capped real spending, and a shadow pinned at its floor meets the criterion above and shows only here. ' +
      'Under it proves less: a burst inside the window can still have been capped.',
  );
  for (const c of present) {
    const x = r.classes[c];
    const k = x.spend.clean;
    L.push('');
    L.push(
      `${c}: ${k.rated} clean windows with a rate, ${k.over} over the shadow, in ${x.sessionsOver} of ${x.sessions} sessions. ` +
        `First 120 s of a pump: ${k.opening.over} of ${k.opening.rated}; later: ${k.later.over} of ${k.later.rated}.`,
    );
    L.push('');
    L.push('| spent, Mbps | clean windows | spent > shadow | by > 25 % | by > ×2 | by > ×4 | over, after the first 120 s | share over, at this spend or more |');
    L.push('|---|---|---|---|---|---|---|---|');
    for (const b of k.bins) {
      if (!b.windows) continue;
      L.push(
        `| ${b.spent} | ${b.windows} | ${b.over} | ${b.over25} | ${b.over2x} | ${b.over4x} | ` +
          `${b.later.over} of ${b.later.windows} | ${share(b.atOrAbove.over, b.atOrAbove.windows)} |`,
      );
    }
    L.push('');
    const encoders = Object.entries(k.encoders).map(([name, v]) => `${name} ${v.over} of ${v.rated}`);
    if (encoders.length) L.push(`- over the shadow, by encoder: ${encoders.join(', ')}`);
    const states = Object.entries(k.overStates).map(([name, n]) => `${name} ${n}`);
    if (states.length) L.push(`- the shadow's state when it was over: ${states.join(', ')}`);
    const noAge = x.spend.noAge;
    if (noAge.rated) L.push(`- no viewer age, so not judged clean: ${noAge.rated} windows with a rate, ${noAge.over} over the shadow`);
    L.push(`- no rate (a pump's first window, a hole in the log): ${x.noRate} of ${x.compared} compared windows`);
    if (x.control.of) L.push(`- positive control, the sender's rate within ×2 of \`shadow_measured_bps\`: ${share(x.control.near, x.control.of)}`);
  }
}

// ---------------------------------------------------------------- mongosh runner

/**
 * The fleet read. Runs inside the Mongo container (the mars recipe in
 * scripts/rc-shadow-report.sh) and prints the report. Bounded by
 * construction, because an unbounded read inside that container once
 * OOM-killed production Mongo (2026-10-26):
 *   - a time window of at most 8 days (`agent_logs` keeps 7), read one day per
 *     query, each with `maxTimeMS` and `allowDiskUse: false`;
 *   - only streaming stages after the index-backed `$match` + `$sort`, so the
 *     server holds no result set and mongosh holds one row at a time;
 *   - only the FIELDS above are projected; the fold keeps counters and sparse
 *     histograms, never the rows;
 *   - credentials come from the container's own environment, never argv.
 */
function runMongosh() {
  const env = typeof process !== 'undefined' && process.env ? process.env : {};
  const DAY = 86400000;
  const until = env.RC_SHADOW_UNTIL ? new Date(env.RC_SHADOW_UNTIL) : new Date();
  const since = env.RC_SHADOW_SINCE ? new Date(env.RC_SHADOW_SINCE) : new Date(until.getTime() - 7 * DAY);
  if (isNaN(since.getTime()) || isNaN(until.getTime()) || since >= until) {
    throw new Error('RC_SHADOW_SINCE must be before RC_SHADOW_UNTIL (ISO 8601)');
  }
  if (until.getTime() - since.getTime() > 8 * DAY) {
    throw new Error('the window is over 8 days; agent_logs keeps 7');
  }
  const emitRows = env.RC_SHADOW_EMIT === 'rows';
  const dbName = env.RC_SHADOW_DB || 'roomler2';
  const database = connectDb(dbName, env);
  const coll = database.getCollection('agent_logs');
  // In row mode a note is a `#` line, which the Node reader skips.
  const log = (s) => print(emitRows ? '# ' + s : s);
  log(`agent_logs ≈ ${coll.estimatedDocumentCount()} batches; reading ${since.toISOString()} → ${until.toISOString()}`);

  const project = {};
  for (const k of FIELDS) project[k] = '$lines.fields.' + k;
  const report = new ShadowReport({ minWindows: Number(env.RC_SHADOW_MIN_WINDOWS || 1) });
  let rows = 0;
  for (let t0 = since.getTime(); t0 < until.getTime(); t0 += DAY) {
    const t1 = Math.min(t0 + DAY, until.getTime());
    const match = { source: 'agent', created_at: { $gte: new Date(t0), $lt: new Date(t1) } };
    if (env.RC_SHADOW_TENANT) match.tenant_id = ObjectId(env.RC_SHADOW_TENANT);
    const pipeline = [
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
    const started = Date.now();
    let dayRows = 0;
    coll.aggregate(pipeline, { maxTimeMS: 240000, allowDiskUse: false }).forEach((row) => {
      dayRows++;
      if (emitRows) {
        const f = {};
        for (const k of Object.keys(row.f || {})) f[k] = value(row.f[k]);
        print(JSON.stringify({ a: text(row.a), t: row.t instanceof Date ? row.t.toISOString() : row.t, m: row.m, f }));
      } else {
        report.addRow(row);
      }
    });
    rows += dayRows;
    log(`  ${new Date(t0).toISOString().slice(0, 10)}: ${dayRows} lines in ${Date.now() - started} ms`);
  }
  if (!emitRows) {
    const r = report.finish();
    print(env.RC_SHADOW_JSON === '1' ? JSON.stringify(r, null, 2) : render(r));
  }
  log(`read ${rows} lines`);
}

function connectDb(dbName, env) {
  if (env.RC_SHADOW_URI) return new Mongo(env.RC_SHADOW_URI).getDB(dbName);
  const u = env.MONGO_INITDB_ROOT_USERNAME;
  const p = env.MONGO_INITDB_ROOT_PASSWORD;
  if (u && p) {
    return new Mongo(
      'mongodb://' + encodeURIComponent(u) + ':' + encodeURIComponent(p) + '@127.0.0.1:27017/?authSource=admin',
    ).getDB(dbName);
  }
  if (typeof db !== 'undefined' && db) return db.getSiblingDB(dbName);
  throw new Error('no connection: pass RC_SHADOW_URI, or run inside the Mongo container (its env has the credentials)');
}

// ---------------------------------------------------------------- self-test

/** Real line shapes (2026-10-24/29 from a dev box's own logs; addresses and
 *  ids replaced). The B1 lines add the six fields exactly as the heartbeat
 *  writes them: Debug for the Options, quoted for the &str fields. */
const FIX = {
  preB1:
    '2026-10-29T00:06:14.904250Z  INFO FFmpeg DC pump heartbeat (≈2s window) session_id=aaaaaaaaaaaaaaaaaaaaaa01 codec_label="AV1" target_fps=60 constrained=false target_bps=48000000 width=2560 height=1600 encoder="av1_nvenc" viewer_age_ms=Some(6) viewer_age_floor_ms=Some(1) viewer_age_implausible=0 age_split=Some(AgeSplit { sender_ms: Some(0.107), transit_ms: 3, viewer_ms: 3 }) pipe_state=None pipe_states=[0, 0, 0, 0, 0] plan_divergences=PlanDivergences { budget: 0, hard_budget: 0, lag_bound: 0, gate: 0, fps: 0, dims: 0, bitrate: 0 } plan_epoch=3 pump_stalls=0',
  ice: (sid, relay) =>
    `2026-10-30T16:00:00.000000Z  INFO per-session ICE path detected (adaptive bitrate) session_id=${sid} relay=${relay} local_typ=Host local_proto=Udp local_addr=192.0.2.10:52131 remote_typ=Host remote_proto=Udp remote_addr=198.51.100.20:57224`,
  // `o.s` is seconds past 16:00:00, so a session can run past the opening.
  at: (s) =>
    `2026-10-30T16:${String(Math.floor((s || 0) / 60)).padStart(2, '0')}:${String((s || 0) % 60).padStart(2, '0')}.000000Z`,
  ff: (sid, o) =>
    `${FIX.at(o.s)}  INFO FFmpeg DC pump heartbeat (≈2s window) session_id=${sid} codec_label="HEVC" target_fps=30 constrained=${o.c} target_bps=${o.live} encoder="${o.enc || 'hevc_qsv'}"${o.bytes === undefined ? '' : ` frames_sent=7 bytes_written=${o.bytes} send_errors=0`} viewer_age_ms=${o.age === null ? 'None' : `Some(${o.age})`} viewer_age_floor_ms=${o.floor === null ? 'None' : `Some(${o.floor})`} age_split=Some(AgeSplit { sender_ms: Some(0.1), transit_ms: 3, viewer_ms: 3 }) plan_divergences=PlanDivergences { budget: 0, hard_budget: 0 } shadow_target_bps=${o.shadow === null ? 'None' : `Some(${o.shadow})`} shadow_state="${o.state || 'hold'}" shadow_measured_bps=${o.measured === undefined ? 'Some(900000)' : o.measured === null ? 'None' : `Some(${o.measured})`} shadow_disagree_pct=${o.shadow === null || !o.live ? 'None' : `Some(${Math.trunc(((o.shadow - o.live) * 100) / o.live)})`} shadow_pauses=${o.pauses || 0} shadow_reason="${o.reason || 'held'}" pump_stalls=0`,
  vp9: (sid, o) =>
    `${FIX.at(o.s)}  INFO VP9-444 DC pump heartbeat (≈1s window) session_id=${sid} target_fps=30 cpu_used=6${o.bytes === undefined ? '' : ` frames_sent=7 bytes_written=${o.bytes} send_errors=0`} paced_fps=None target_bps=${o.live} avg_qp=Some(200) viewer_age_ms=Some(${o.age}) viewer_age_floor_ms=Some(${o.floor}) pipe_state=Some("unknown") link_stats=(0, 0, 0) shadow_target_bps=Some(${o.shadow}) shadow_state="normal" shadow_measured_bps=None shadow_disagree_pct=Some(${Math.trunc(((o.shadow - o.live) * 100) / o.live)}) shadow_pauses=0 shadow_reason="climbing"`,
};

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
  const run = (lines, opts) => {
    const r = new ShadowReport(opts);
    for (const l of lines) {
      const p = parseLogLine(l);
      if (p) r.add(p.msg, p.fields, p.ts, null);
    }
    return r.finish();
  };

  // --- the tokenizer, on the shapes that defeat a naive split
  const pre = parseLogLine(FIX.preB1);
  ok(pre && pre.msg === 'FFmpeg DC pump heartbeat (≈2s window)', 'message is everything before the first field', pre && pre.msg);
  ok(pre && pre.fields.age_split === 'Some(AgeSplit { sender_ms: Some(0.107), transit_ms: 3, viewer_ms: 3 })', 'a struct value keeps its inner spaces');
  ok(pre && pre.fields.plan_epoch === '3' && pre.fields.pump_stalls === '0', 'fields after a braced value still split');
  ok(value('Some(6)') === 6 && value('None') === null && value('"hold"') === 'hold' && value('Some("unknown")') === 'unknown', 'Debug and quoted values read');
  ok(value({ toNumber: () => 48000000 }) === 48000000, 'a bson Long reads as a number');
  ok(parseLogLine('C:\\ProgramData\\roomler\\…\\roomlerd.log.2026-10-30  (5812562 bytes total)') === null, 'a `roomler logs` header is not a line');

  // --- fail-first: a build without B1 has no shadow, and the report says so
  const r0 = run([FIX.preB1, FIX.preB1]);
  ok(r0.provenance.windowsWithoutShadow === 2 && r0.provenance.sessionsWithShadow === 0, 'pre-B1 windows are counted, never compared', r0.provenance);
  ok(r0.criterion.verdict === 'NOT MET', 'no shadow, no flip', r0.criterion.verdict);

  // --- one constrained session: an elevated window that agrees, one that does not, a clean one, no-age
  const c1 = 'cccccccccccccccccccccc01';
  const r1 = run([
    FIX.ff(c1, { s: 1, c: true, live: 1500000, shadow: 600000, age: 400, floor: 40 }), // elevated, agree
    FIX.ff(c1, { s: 3, c: true, live: 1500000, shadow: 1800000, age: 300, floor: 40 }), // elevated, disagree
    FIX.ff(c1, { s: 5, c: true, live: 1500000, shadow: 1400000, age: 60, floor: 40 }), // clean (20 < 70), agree
    FIX.ff(c1, { s: 7, c: true, live: 1500000, shadow: 1500000, age: null, floor: null }), // no age
    FIX.ff(c1, { s: 9, c: true, live: 1500000, shadow: null, age: 60, floor: 40 }), // not opened
    FIX.ff(c1, { s: 11, c: true, live: 0, shadow: 300000, age: 60, floor: 40 }), // nothing applied
  ]);
  const k1 = r1.classes.constrained;
  ok(k1.sessions === 1 && k1.windows === 6 && k1.compared === 4, 'windows vs compared', k1);
  ok(k1.elevated === 2 && k1.elevatedAgree === 1 && k1.clean === 1 && k1.cleanAgree === 1 && k1.noAge === 1, 'elevated / clean / no-age split at 70 ms', k1);
  ok(k1.noShadow === 1 && k1.noLive === 1, 'an unopened shadow and a zero live rate are not compared');
  ok(k1.sign.neg === 2 && k1.sign.zero === 1 && k1.sign.pos === 1, 'the sign of (shadow − live)', k1.sign);
  ok(k1.disagreePct.elevated.p50 === -60 && k1.disagreePct.elevated.p90 === 20, 'percentiles are nearest-rank over the elevated windows', k1.disagreePct);
  // The criterion ignores clean windows; the report must still say what the
  // shadow did there (the day-0 read: -96 % on every clean direct window).
  const out1 = render(r1);
  ok(
    /- constrained: the shadow asked for no more than live in 1 of 1 clean windows; disagreement p50 -6 %, p90 -6 %/.test(out1),
    'the render states the clean windows the criterion does not judge',
    out1.split('\n').filter((l) => /clean windows/.test(l)),
  );

  // --- the boundary: exactly 70 ms over the floor IS elevated
  const r2 = run([FIX.ff(c1, { s: 1, c: true, live: 1000000, shadow: 900000, age: 110, floor: 40 })]);
  ok(r2.classes.constrained.elevated === 1, 'age − floor == 70 counts as elevated (>=)');
  const r3 = run([FIX.ff(c1, { s: 1, c: true, live: 1000000, shadow: 900000, age: 109, floor: 40 })]);
  ok(r3.classes.constrained.clean === 1, 'age − floor == 69 is clean');
  // "shadow <= live": asking for exactly the live rate under strain agrees.
  const r3b = run([FIX.ff(c1, { s: 1, c: true, live: 1000000, shadow: 1000000, age: 500, floor: 40 })]);
  ok(r3b.classes.constrained.elevatedAgree === 1, 'shadow == live in an elevated window agrees (<=)', r3b.classes.constrained);

  // --- VP9 windows take the session's ICE class; without it they stay unknown
  const v1 = 'dddddddddddddddddddddd01';
  const v2 = 'dddddddddddddddddddddd02';
  const r4 = run([
    FIX.ice(v1, 'true'),
    FIX.vp9(v1, { s: 1, live: 3000000, shadow: 2000000, age: 500, floor: 30 }),
    FIX.vp9(v2, { s: 2, live: 3000000, shadow: 2000000, age: 500, floor: 30 }),
  ]);
  ok(r4.classes.constrained.sessions === 1 && r4.classes.constrained.sessionsViaIce === 1, 'a VP9 session with relay=true is constrained, via the ICE line', r4.classes.constrained);
  ok(r4.classes.unknown.sessions === 1 && r4.classes.direct.sessions === 0, 'a VP9 session with no ICE line is unknown, never direct', r4.classes.unknown);
  ok(r4.pooled.elevated === 1, 'unknown windows stay out of the pooled agreement');

  // --- the criterion at its edges: 20 + 20 sessions, and 90 % is inclusive
  const edge = (constrainedSessions, agreeing) => {
    const out = [];
    for (let i = 0; i < constrainedSessions; i++) {
      const shadow = i < agreeing ? 800000 : 1200000;
      out.push(FIX.ff('c' + String(i).padStart(23, '0'), { s: 1, c: true, live: 1000000, shadow, age: 500, floor: 50 }));
    }
    for (let i = 0; i < 20; i++) {
      out.push(FIX.ff('e' + String(i).padStart(23, '0'), { s: 1, c: false, live: 9000000, shadow: 8000000, age: 20, floor: 10 }));
    }
    return run(out);
  };
  const r5 = edge(20, 18);
  ok(r5.classes.constrained.sessions === 20 && r5.classes.direct.sessions === 20, '20 + 20 sessions');
  ok(r5.pooled.elevated === 20 && r5.pooled.elevatedAgree === 18 && r5.criterion.verdict === 'MET', '18/20 = 90.0 % meets the bar (>=)', r5.criterion);
  const r5b = edge(20, 17);
  ok(r5b.criterion.verdict === 'NOT MET', '17/20 = 85 % does not', r5b.criterion.clauses);
  const r5c = edge(19, 19);
  ok(r5c.criterion.verdict === 'NOT MET' && r5c.criterion.clauses[0].status === 'not met', '19 constrained sessions do not', r5c.criterion.clauses);

  // --- red run: shadow above live in most elevated windows fails the bar
  const red = [];
  for (let i = 0; i < 20; i++) {
    red.push(FIX.ff('f' + String(i).padStart(23, '0'), { s: 1, c: true, live: 1000000, shadow: 1500000, age: 600, floor: 50 }));
    red.push(FIX.ff('g' + String(i).padStart(23, '0'), { s: 1, c: false, live: 9000000, shadow: 8000000, age: 200, floor: 10 }));
  }
  const r6 = run(red);
  ok(r6.criterion.verdict === 'NOT MET' && r6.criterion.classesBelowBar.includes('constrained'), 'a shadow asking for MORE under strain fails the bar', r6.criterion);

  // --- the green run: every clause the logs can meet is met
  const green = [];
  for (let i = 0; i < 20; i++) {
    green.push(FIX.ff('h' + String(i).padStart(23, '0'), { s: 1, c: true, live: 1000000, shadow: 800000, age: 600, floor: 50 }));
    green.push(FIX.ff('j' + String(i).padStart(23, '0'), { s: 1, c: false, live: 9000000, shadow: 8000000, age: 200, floor: 10 }));
  }
  const r7 = run(green);
  ok(r7.criterion.verdict === 'MET', 'sessions and agreement met', r7.criterion);

  // --- no elevated windows anywhere: inconclusive, never met
  const calm = [];
  for (let i = 0; i < 20; i++) {
    calm.push(FIX.ff('k' + String(i).padStart(23, '0'), { s: 1, c: true, live: 1000000, shadow: 800000, age: 60, floor: 50 }));
    calm.push(FIX.ff('m' + String(i).padStart(23, '0'), { s: 1, c: false, live: 9000000, shadow: 8000000, age: 20, floor: 10 }));
  }
  ok(run(calm).criterion.verdict === 'INCONCLUSIVE', 'zero elevated windows is no evidence, not a pass');

  // --- --min-windows excludes short sessions from the session count only
  const r8 = run([FIX.ff(c1, { s: 1, c: true, live: 1000000, shadow: 800000, age: 600, floor: 50 })], { minWindows: 2 });
  ok(r8.classes.constrained.sessions === 0 && r8.classes.constrained.windows === 1, '--min-windows gates the session count, not the windows');

  // --- invariants: a pause ordered on a direct window is reported
  const p1 = 'pppppppppppppppppppppp01';
  const r9 = run([
    FIX.ff(p1, { s: 1, c: true, live: 1000000, shadow: 800000, age: 900, floor: 50, pauses: 1 }),
    FIX.ff(p1, { s: 3, c: false, live: 9000000, shadow: 8000000, age: 20, floor: 10, pauses: 1 }), // carried over: fine
    FIX.ff(p1, { s: 5, c: false, live: 9000000, shadow: 8000000, age: 20, floor: 10, pauses: 2 }), // a rise on direct
  ]);
  ok(r9.invariants.pauseRisesOnDirect === 1, 'a pause counted in a direct window is flagged; one carried over is not', r9.invariants);
  const r10 = run([FIX.ff(c1, { s: 1, c: true, live: 1000000, shadow: 150000, age: 600, floor: 50 })]);
  ok(r10.invariants.constrainedBelowFloor === 1, 'a constrained shadow under 200 kbps is flagged');

  // --- an agent_logs row reads exactly like the text line it came from
  const t = parseLogLine(FIX.ff(c1, { s: 1, c: true, live: 1500000, shadow: 600000, age: 400, floor: 40 }));
  const row = {
    a: 'aaaaaaaaaaaaaaaaaaaaaaaa',
    t: t.ts,
    m: t.msg,
    f: {
      session_id: c1,
      constrained: true,
      target_bps: { toNumber: () => 1500000 },
      viewer_age_ms: 'Some(400)',
      viewer_age_floor_ms: 'Some(40)',
      shadow_target_bps: 'Some(600000)',
      shadow_state: 'hold',
      shadow_measured_bps: 'Some(900000)',
      shadow_disagree_pct: 'Some(-60)',
      shadow_pauses: 0,
      shadow_reason: 'held',
    },
  };
  const ra = new ShadowReport();
  ra.addRow(row);
  const rt = new ShadowReport();
  rt.add(t.msg, t.fields, t.ts, 'aaaaaaaaaaaaaaaaaaaaaaaa');
  ok(JSON.stringify(ra.finish().classes) === JSON.stringify(rt.finish().classes), 'agent_logs row == text line');

  // --- what the encoder spent. `bytes_written` is the pump's running total, so a
  // rate is the difference between two heartbeats of one pump over their own gap.
  const mbps = (m, secs) => Math.round((m * 1e6 * secs) / 8);
  // Heartbeats every 2 s from `o.from` (default 1), spending `o.spend` Mbps, the
  // running total starting at `o.start` (default 1000).
  const spending = (sid, n, o) => {
    const out = [];
    let bytes = o.start === undefined ? 1000 : o.start;
    const from = o.from === undefined ? 1 : o.from;
    for (let i = 0; i < n; i++) {
      out.push(FIX.ff(sid, Object.assign({ s: from + 2 * i, c: false, live: 89760000, shadow: 2457600, age: 60, floor: 50, bytes }, o)));
      bytes += mbps(o.spend, 2);
    }
    return out;
  };
  const s1 = 'ssssssssssssssssssssss01';
  const s2 = 'ssssssssssssssssssssss02';

  const clean = (r, c) => r.classes[c || 'direct'].spend.clean;
  const k0 = run(spending(s1, 3, { spend: 8 }));
  ok(clean(k0).rated === 2 && k0.classes.direct.noRate === 1, "a pump's first window has no rate, the next ones do", k0.classes.direct.spend);
  ok(clean(k0).over === 2 && k0.classes.direct.sessionsOver === 1, 'spending 8 Mbps against a 2.46 M shadow is over it', clean(k0));

  // The reason the column exists. These two sessions print the SAME disagreement
  // and the same criterion, and only one of them would have lost anything.
  const atRest = run(spending(s1, 5, { spend: 0.05 }));
  const working = run(spending(s1, 5, { spend: 20 }));
  ok(
    JSON.stringify(atRest.classes.direct.disagreePct) === JSON.stringify(working.classes.direct.disagreePct),
    'the disagreement cannot tell a resting encoder from a working one',
    [atRest.classes.direct.disagreePct, working.classes.direct.disagreePct],
  );
  ok(clean(atRest).rated === 4 && clean(atRest).over === 0, 'at rest the same shadow caps nothing', clean(atRest));
  ok(clean(working).rated === 4 && clean(working).over === 4, 'working, the same shadow caps every window', clean(working));

  // The blind spot, measured: pinned at its floor with the encoder working, the
  // shadow still MEETS the criterion. The criterion is the operator's, so this
  // pins that the new reading does not move it.
  const pinned = [];
  for (let i = 0; i < 20; i++) {
    for (const [cls, sid, live, floor] of [
      [true, 'q' + String(i).padStart(23, '0'), 2550000, 200000],
      [false, 'r' + String(i).padStart(23, '0'), 89760000, 2457600],
    ]) {
      pinned.push(FIX.ff(sid, { s: 1, c: cls, live, shadow: floor, age: 600, floor: 50, bytes: 1000 }));
      pinned.push(FIX.ff(sid, { s: 3, c: cls, live, shadow: floor, age: 60, floor: 50, bytes: 1000 + mbps(20, 2) }));
      pinned.push(FIX.ff(sid, { s: 5, c: cls, live, shadow: floor, age: 60, floor: 50, bytes: 1000 + mbps(40, 2) }));
    }
  }
  const pin = run(pinned);
  ok(pin.criterion.verdict === 'MET', 'a shadow pinned at its floor MEETS the criterion', pin.criterion);
  ok(
    clean(pin, 'constrained').over === 40 && clean(pin, 'direct').over === 40 && pin.classes.direct.sessionsOver === 20,
    'and every clean window it would have capped says so',
    [clean(pin, 'constrained'), clean(pin, 'direct')],
  );

  // A bite is proved only when the spend beat the higher of the window's two
  // targets: a shadow that cut mid-window held the higher one for part of it.
  const cut = [
    FIX.ff(s1, { s: 1, c: false, live: 89760000, shadow: 8000000, age: 60, floor: 50, bytes: 1000 }),
    FIX.ff(s1, { s: 3, c: false, live: 89760000, shadow: 4000000, age: 60, floor: 50, bytes: 1000 + mbps(6, 2) }),
    FIX.ff(s1, { s: 5, c: false, live: 89760000, shadow: 4000000, age: 60, floor: 50, bytes: 1000 + mbps(12, 2) }),
  ];
  const k1b = run(cut);
  ok(clean(k1b).rated === 2 && clean(k1b).over === 1, 'the spend is judged against the higher of the two ends of its window', clean(k1b));
  const rose = run([
    FIX.ff(s1, { s: 1, c: false, live: 89760000, shadow: 2000000, age: 60, floor: 50, bytes: 1000 }),
    FIX.ff(s1, { s: 3, c: false, live: 89760000, shadow: 4000000, age: 60, floor: 50, bytes: 1000 + mbps(3, 2) }),
  ]);
  ok(clean(rose).rated === 1 && clean(rose).over === 0, 'whichever end is the higher one', clean(rose));

  // Exactly the target is covered; a byte more is not.
  const edge2 = (extra) =>
    run([
      FIX.ff(s1, { s: 1, c: false, live: 89760000, shadow: 4000000, age: 60, floor: 50, bytes: 0 }),
      FIX.ff(s1, { s: 3, c: false, live: 89760000, shadow: 4000000, age: 60, floor: 50, bytes: 1000000 + extra }),
    ]);
  ok(clean(edge2(0)).rated === 1 && clean(edge2(0)).over === 0, 'spending exactly the target is covered', clean(edge2(0)));
  ok(clean(edge2(1)).over === 1, 'one byte more is over it', clean(edge2(1)));

  // Bins: the spend's lower edge is inclusive, the ratio's upper edge is.
  const single = (spendMbps, shadow) =>
    run([
      FIX.ff(s1, { s: 1, c: false, live: 89760000, shadow, age: 60, floor: 50, bytes: 0 }),
      FIX.ff(s1, { s: 3, c: false, live: 89760000, shadow, age: 60, floor: 50, bytes: mbps(spendMbps, 2) }),
    ]);
  const bin = (r) => clean(r).bins.filter((x) => x.windows)[0];
  ok(bin(single(2, 100000000)).spent === '2 – 5', 'exactly 2 Mbps falls in the 2 – 5 row', clean(single(2, 100000000)).bins);
  ok(bin(single(1.99, 100000000)).spent === '1 – 2', 'just under falls in the 1 – 2 row');
  ok(bin(single(60, 100000000)).spent === '≥ 50', 'the top row is open');
  const by = (r) => [bin(r).over, bin(r).over25, bin(r).over2x, bin(r).over4x].join();
  ok(by(single(5, 4000000)) === '1,0,0,0', 'exactly 25 % over counts as over, not as over by more than 25 %', by(single(5, 4000000)));
  ok(by(single(5.2, 4000000)) === '1,1,0,0', 'more than 25 % over', by(single(5.2, 4000000)));
  ok(by(single(8.2, 4000000)) === '1,1,1,0', 'more than twice', by(single(8.2, 4000000)));
  ok(by(single(16.2, 4000000)) === '1,1,1,1', 'more than four times', by(single(16.2, 4000000)));

  // Each row carries the share at its spend or more, so any gate can be read off
  // the one report without this script choosing one.
  const mix = run([...spending(s1, 5, { spend: 0.2 }), ...spending(s2, 3, { spend: 8, from: 41 })]);
  const rows = clean(mix).bins.filter((x) => x.windows);
  ok(
    rows.length === 2 && rows[0].atOrAbove.windows === 6 && rows[0].atOrAbove.over === 2 && rows[1].atOrAbove.windows === 2 && rows[1].atOrAbove.over === 2,
    'each row carries the share at its spend or more',
    rows,
  );

  // A pump restarted (a new codec, a reopened channel) counts from zero.
  const rst = [
    FIX.ff(s1, { s: 1, c: false, live: 89760000, shadow: 2457600, age: 60, floor: 50, bytes: 50000000 }),
    FIX.ff(s1, { s: 3, c: false, live: 89760000, shadow: 2457600, age: 60, floor: 50, bytes: 60000000 }),
    FIX.ff(s1, { s: 5, c: false, live: 89760000, shadow: 2457600, age: 60, floor: 50, bytes: 1000 }),
    FIX.ff(s1, { s: 7, c: false, live: 89760000, shadow: 2457600, age: 60, floor: 50, bytes: 1000 + mbps(8, 2) }),
  ];
  const k2 = run(rst);
  ok(clean(k2).rated === 2 && k2.classes.direct.noRate === 2 && clean(k2).over === 2, 'a counter that falls is a new pump, never a negative rate', clean(k2));

  // A hole in the log is not a low rate, and a replayed batch is not a window.
  const gap = [
    FIX.ff(s1, { s: 1, c: false, live: 89760000, shadow: 2457600, age: 60, floor: 50, bytes: 1000 }),
    FIX.ff(s1, { s: 31, c: false, live: 89760000, shadow: 2457600, age: 60, floor: 50, bytes: 1000 + mbps(8, 30) }),
    FIX.ff(s1, { s: 33, c: false, live: 89760000, shadow: 2457600, age: 60, floor: 50, bytes: 1000 + mbps(8, 32) }),
  ];
  const k3 = run(gap);
  ok(clean(k3).rated === 1 && k3.classes.direct.noRate === 2, 'a gap over 10 s has no rate; the window after it does', clean(k3));
  const replay = [
    FIX.ff(s1, { s: 1, c: false, live: 89760000, shadow: 2457600, age: 60, floor: 50, bytes: 1000 }),
    FIX.ff(s1, { s: 3, c: false, live: 89760000, shadow: 2457600, age: 60, floor: 50, bytes: 1000 + mbps(8, 2) }),
    FIX.ff(s1, { s: 1, c: false, live: 89760000, shadow: 2457600, age: 60, floor: 50, bytes: 1000 }),
    FIX.ff(s1, { s: 5, c: false, live: 89760000, shadow: 2457600, age: 60, floor: 50, bytes: 1000 + mbps(8, 2) + mbps(30, 2) }),
  ];
  const k4 = run(replay);
  // Rewound to the replayed line, the last rate would average to 19 Mbps over 4 s.
  ok(
    clean(k4).rated === 2 && k4.classes.direct.noRate === 2 && clean(k4).bins[6].windows === 1,
    'a replayed line neither rates nor rewinds the pump',
    clean(k4).bins.filter((x) => x.windows),
  );
  // Hard on the mark's heels: no window is that short, and the mark stays put.
  const near = new ShadowReport();
  const nearLines = [
    [FIX.ff(s1, { s: 1, c: false, live: 89760000, shadow: 2457600, age: 60, floor: 50, bytes: 1000 }), null],
    [FIX.ff(s1, { s: 1, c: false, live: 89760000, shadow: 2457600, age: 60, floor: 50, bytes: 1000 + mbps(8, 2) }), '2026-10-30T16:00:01.200Z'],
    [FIX.ff(s1, { s: 3, c: false, live: 89760000, shadow: 2457600, age: 60, floor: 50, bytes: 1000 + mbps(8, 2) }), null],
  ];
  for (const [l, ts] of nearLines) {
    const p = parseLogLine(l);
    near.add(p.msg, p.fields, ts || p.ts, null);
  }
  const k4b = near.finish();
  ok(clean(k4b).rated === 1 && bin(k4b).spent === '5 – 10', 'two heartbeats under 500 ms apart make no rate, and the next one spans from the mark', clean(k4b).bins.filter((x) => x.windows));
  // A session that changed pump: the VP9 total is not a continuation of the FFmpeg one.
  const swap = run([
    FIX.ice(s1, 'false'),
    FIX.ff(s1, { s: 1, c: false, live: 89760000, shadow: 2457600, age: 60, floor: 50, bytes: 1000 }),
    FIX.vp9(s1, { s: 3, live: 89760000, shadow: 2457600, age: 60, floor: 50, bytes: 1000 + mbps(8, 2) }),
    FIX.vp9(s1, { s: 4, live: 89760000, shadow: 2457600, age: 60, floor: 50, bytes: 1000 + mbps(8, 2) + mbps(1, 1) }),
  ]);
  ok(clean(swap).rated === 1 && bin(swap).spent === '1 – 2', 'a different pump starts its own count', clean(swap).bins.filter((x) => x.windows));

  // The opening: B2's question is whether the harm ends when the climb does.
  const long = run(spending(s1, 70, { spend: 8 }));
  ok(clean(long).opening.rated === 59 && clean(long).later.rated === 10, 'the first 120 s of a pump are its opening', [clean(long).opening, clean(long).later]);
  ok(bin(long).later.windows === 10 && bin(long).later.over === 10 && bin(long).windows === 69, 'each row says how much of it came after the opening', bin(long));
  const restarted = run([
    ...spending(s1, 70, { spend: 8, start: 900000000 }),
    ...spending(s1, 3, { spend: 8, from: 200 }),
  ]);
  ok(clean(restarted).opening.rated === 61, 'a restarted pump opens again', clean(restarted).opening);

  // Elevated and no-age windows are not clean; a window not compared still moves the mark.
  const ages = run([
    FIX.ff(s1, { s: 1, c: false, live: 89760000, shadow: 2457600, age: 60, floor: 50, bytes: 1000 }),
    FIX.ff(s1, { s: 3, c: false, live: 89760000, shadow: 2457600, age: 600, floor: 50, bytes: 1000 + mbps(8, 2) }),
    FIX.ff(s1, { s: 5, c: false, live: 89760000, shadow: 2457600, age: null, floor: null, bytes: 1000 + mbps(8, 4) }),
    FIX.ff(s1, { s: 7, c: false, live: 89760000, shadow: null, age: 60, floor: 50, bytes: 1000 + mbps(8, 6) }),
    FIX.ff(s1, { s: 9, c: false, live: 89760000, shadow: 2457600, age: 60, floor: 50, bytes: 1000 + mbps(16, 6) }),
  ]);
  const ka = ages.classes.direct.spend;
  ok(ka.elevated.over === 1 && ka.noAge.over === 1 && ka.clean.rated === 1, 'elevated, no-age and clean windows are read apart', ka);
  ok(bin(ages).spent === '20 – 50', 'an uncompared window still moves the mark: the next rate spans 2 s, not 4', bin(ages));
  const onlyElevated = run([
    FIX.ff(s1, { s: 1, c: false, live: 89760000, shadow: 2457600, age: 600, floor: 50, bytes: 1000 }),
    FIX.ff(s1, { s: 3, c: false, live: 89760000, shadow: 2457600, age: 600, floor: 50, bytes: 1000 + mbps(8, 2) }),
  ]);
  ok(onlyElevated.classes.direct.sessionsOver === 0, 'over the shadow while the age is elevated is the safe direction, not a session over it', onlyElevated.classes.direct);

  // Who was over, and in what state.
  const who = run([
    ...spending(s1, 3, { spend: 8, enc: 'av1_nvenc', state: 'startup' }),
    ...spending(s2, 3, { spend: 0.1, enc: 'hevc_qsv', state: 'hold', from: 41 }),
  ]);
  ok(
    clean(who).encoders.av1_nvenc.over === 2 && clean(who).encoders.av1_nvenc.rated === 2 && clean(who).encoders.hevc_qsv.over === 0 && clean(who).encoders.hevc_qsv.rated === 2,
    'clean windows and those over the shadow, per encoder',
    clean(who).encoders,
  );
  ok(clean(who).overStates.startup === 2 && !clean(who).overStates.hold, "the shadow's state when it was over", clean(who).overStates);

  // The positive control: the sender's rate against the viewer's own arrival rate.
  const ctl = run([
    FIX.ff(s1, { s: 1, c: false, live: 89760000, shadow: 2457600, age: 60, floor: 50, bytes: 1000, measured: 900000 }),
    FIX.ff(s1, { s: 3, c: false, live: 89760000, shadow: 2457600, age: 60, floor: 50, bytes: 1000 + mbps(1, 2), measured: 900000 }),
    FIX.ff(s1, { s: 5, c: false, live: 89760000, shadow: 2457600, age: 60, floor: 50, bytes: 1000 + mbps(9, 2), measured: 900000 }),
    FIX.ff(s1, { s: 7, c: false, live: 89760000, shadow: 2457600, age: 60, floor: 50, bytes: 1000 + mbps(10, 2), measured: null }),
  ]);
  ok(ctl.classes.direct.control.near === 1 && ctl.classes.direct.control.of === 2, 'the control counts a rate within ×2 of the viewer arrival, and skips a window with none', ctl.classes.direct.control);
  const ctlLow = run([
    FIX.ff(s1, { s: 1, c: false, live: 89760000, shadow: 2457600, age: 60, floor: 50, bytes: 1000, measured: 900000 }),
    FIX.ff(s1, { s: 3, c: false, live: 89760000, shadow: 2457600, age: 60, floor: 50, bytes: 1000 + mbps(0.6, 2), measured: 900000 }),
    FIX.ff(s1, { s: 5, c: false, live: 89760000, shadow: 2457600, age: 60, floor: 50, bytes: 1000 + mbps(0.6, 2) + mbps(0.4, 2), measured: 900000 }),
  ]);
  ok(ctlLow.classes.direct.control.near === 1 && ctlLow.classes.direct.control.of === 2, 'the ×2 is on both sides', ctlLow.classes.direct.control);

  // A VP9 window takes its class from the ICE line, spend included.
  const v3 = 'dddddddddddddddddddddd03';
  const vr = run([
    FIX.ice(v3, 'true'),
    FIX.vp9(v3, { s: 1, live: 3000000, shadow: 300000, age: 60, floor: 50, bytes: 1000 }),
    FIX.vp9(v3, { s: 2, live: 3000000, shadow: 300000, age: 60, floor: 50, bytes: 1000 + mbps(1, 1) }),
  ]);
  ok(
    clean(vr, 'constrained').rated === 1 && clean(vr, 'constrained').over === 1 && clean(vr, 'constrained').encoders['vp9-444'].over === 1 && vr.classes.constrained.sessionsOver === 1 && vr.classes.unknown.windows === 0,
    'a VP9 window spends under its ICE class, as vp9-444',
    vr.classes.constrained.spend,
  );
  ok(clean(vr, 'constrained').rated === vr.pooled.spend.clean.rated, 'the pool carries the spend');

  // The fleet read's trap: a field the fold reads and $project drops reads as
  // "no rate" there and nowhere else. Push each line through FIELDS, with the
  // driver's types, and the report must not change.
  const lines = [...spending(s1, 4, { spend: 8, enc: 'av1_nvenc' }), ...spending(s2, 3, { spend: 0.1, from: 41 })];
  const viaText = new ShadowReport();
  const viaRow = new ShadowReport();
  for (const l of lines) {
    const p = parseLogLine(l);
    viaText.add(p.msg, p.fields, p.ts, 'aaaaaaaaaaaaaaaaaaaaaaaa');
    const f = {};
    for (const k of FIELDS) {
      if (!(k in p.fields)) continue;
      const v = value(p.fields[k]);
      f[k] = typeof v === 'number' && Number.isInteger(v) && /^\d+$/.test(p.fields[k]) ? { toNumber: () => v } : p.fields[k].replace(/^"(.*)"$/, '$1');
    }
    viaRow.addRow({ a: { toHexString: () => 'aaaaaaaaaaaaaaaaaaaaaaaa' }, t: new Date(p.ts), m: p.msg, f });
  }
  const textOut = viaText.finish();
  const rowOut = viaRow.finish();
  ok(rowOut.classes.direct.spend.clean.rated === 5, 'the fleet read rates what it projects', rowOut.classes.direct.spend.clean);
  ok(JSON.stringify(rowOut.classes) === JSON.stringify(textOut.classes), 'every field the fold reads survives the projection', [rowOut.classes.direct.spend, textOut.classes.direct.spend]);
  ok(/^2026-10-30T/.test(rowOut.provenance.first), "a driver's date reads as ISO", rowOut.provenance.first);

  // The render states it, counts only, and says so when it has nothing to say.
  const md = render(working);
  ok(/spent > shadow/.test(md) && /\| 20 – 50 \| 4 \| 4 \| 4 \| 4 \| 4 \|/.test(md), 'the render carries the spend table', md.split('\n').filter((l) => /\|/.test(l)));
  ok(!md.includes(s1), 'no session id is printed');
  ok(/\*\*Verdict: MET\*\*[^]*never one that asks for too little/.test(render(pin)), 'a MET says what it cannot see', render(pin).split('\n').filter((l) => /Verdict|too little/.test(l)));
  const none = render(run([FIX.ff(s1, { s: 1, c: false, live: 89760000, shadow: 2457600, age: 60, floor: 50 }), FIX.ff(s1, { s: 3, c: false, live: 89760000, shadow: 2457600, age: 60, floor: 50 })]));
  ok(/no window has a spend rate/.test(none), 'no rate is no evidence, not a clean bill', none.split('\n').slice(-4));

  // --- the constants still mirror the controller (a drift check, when run in a checkout)
  try {
    const fs = require('fs');
    const path = require('path');
    const src = fs.readFileSync(path.join(__dirname, '..', 'agents', 'roomlerd', 'src', 'encode', 'ratectl.rs'), 'utf8');
    const slack = /pub const AGE_SLACK_MS: u16 = (\d+);/.exec(src);
    const floor = /pub const CONSTRAINED_FLOOR_BPS: u32 = ([\d_]+);/.exec(src);
    ok(slack && Number(slack[1]) === AGE_SLACK_MS, 'AGE_SLACK_MS matches ratectl.rs', slack && slack[1]);
    ok(floor && Number(floor[1].replace(/_/g, '')) === CONSTRAINED_FLOOR_BPS, 'CONSTRAINED_FLOOR_BPS matches ratectl.rs', floor && floor[1]);
  } catch (e) {
    console.log(`# skipped the ratectl.rs drift check: ${e.message}`);
  }

  console.log(failed ? `# ${failed} of ${n} FAILED` : `# all ${n} passed`);
  return failed ? 1 : 0;
}

// ---------------------------------------------------------------- the rehearsal seed

/**
 * Rolling-log text -> `agent_logs` batches, as the uploader would have stored
 * them (canonical EJSON, one batch per line), so the fleet read can be
 * rehearsed against a throwaway Mongo holding REAL log lines
 * (`rc-shadow-report.sh --docker NAME --seed FILE`). Field types follow
 * `logs_upload::FieldVisitor`: an integer is an Int64 (`record_u64`), `true` /
 * `false` a bool, a lone quoted string its text (`record_str`), and anything
 * else — every `?Debug` value — the string as printed. Batches close at 500
 * lines or 5 s, the uploader's two flush rules.
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
    if (batch && (batch.lines.length >= 500 || t.getTime() - batch.startMs >= 5000)) flush();
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

// ---------------------------------------------------------------- Node runner

function nodeMain(argv) {
  const opts = { json: false, minWindows: 1, files: [], toEjson: false };
  let agentHex = 'f63000000000000000000a01';
  let tenantHex = 'f63000000000000000000701';
  for (let i = 0; i < argv.length; i++) {
    const a = argv[i];
    if (a === '--self-test') return process.exit(selfTest());
    else if (a === '--json') opts.json = true;
    else if (a === '--min-windows') opts.minWindows = Number(argv[++i]);
    else if (a === '--to-ejson') opts.toEjson = true;
    else if (a === '--agent') agentHex = argv[++i];
    else if (a === '--tenant') tenantHex = argv[++i];
    else if (a === '-h' || a === '--help') {
      console.log(
        'usage: rc-shadow-report.cjs [--json] [--min-windows N] FILE...\n' +
          '       rc-shadow-report.cjs --to-ejson [--agent HEX] [--tenant HEX] FILE...   (a rehearsal seed)\n' +
          '       rc-shadow-report.cjs --self-test',
      );
      return process.exit(0);
    } else opts.files.push(a);
  }
  if (!opts.files.length) {
    console.error('rc-shadow-report: no input. Pass log files, NDJSON rows, or - for stdin (--help).');
    return process.exit(1);
  }
  const fs = require('fs');
  const readline = require('readline');
  const report = new ShadowReport(opts);
  const seedLines = [];
  let i = 0;
  const next = () => {
    if (i >= opts.files.length) {
      if (opts.toEjson) {
        toEjson(seedLines, agentHex, tenantHex, (s) => process.stdout.write(s + '\n'));
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
      else report.add(p.msg, p.fields, p.ts, null);
    });
    rl.on('close', next);
  };
  next();
}

// ---------------------------------------------------------------- dispatch

if (typeof module !== 'undefined' && module.exports) {
  module.exports = { ShadowReport, parseLogLine, value, render, AGE_SLACK_MS, CONSTRAINED_FLOOR_BPS };
}
if (typeof Mongo === 'function' && typeof print === 'function') {
  runMongosh();
} else if (typeof require !== 'undefined' && typeof module !== 'undefined' && require.main === module) {
  nodeMain(process.argv.slice(2));
}
