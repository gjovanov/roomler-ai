#!/usr/bin/env node
//
// fr65-stall-census.cjs — FR-65's stall census: every "FFmpeg DC pump STALL" line,
// classified by what its phase breakdown says.
//
// WHY THIS EXISTS
//
//   AC7 asks that every fix carry a before and an after from the SAME instrument
//   (docs/fr/FR-65-blocking-work-on-async-runtimes.md). The census is that
//   instrument. It reads `agent_logs` in production Mongo: every daemon and every
//   RC worker uploads its lines there, for every org, with the batch's
//   `agent_version` beside them, so one read holds the build without a fix and
//   the build with it. A sweep of host logs reaches only the hosts that can be
//   reached, which is a biased sample (docs/rate-control.md).
//
// TWO READS, ONE CORE
//
//   1. The FLEET read: under mongosh inside the Mongo container, over
//      `agent_logs`. scripts/fr65-stall-census.sh carries the recipe.
//   2. The HOST read: under Node, `--from-logs FILE...`, the rolling log text of
//      a host whose files you already hold. It exists to prove read 1: the same
//      classifier over the same lines, from the two ends of the upload.
//
// THE CLASSES, ONE PER STALL LINE
//
//   reset     the window-reset artifact of builds before 0.4.192: emitted within
//             5 ms of its session's heartbeat, every phase the heartbeat zeroes
//             (capture, scale, encode, send, apply, open) at 0.0, and other at
//             >= 90 % of the pass. The pass held the heartbeat, whose reset
//             saturated those six deltas to 0. A build with the rebase reports
//             these passes under their real phase, so there it must read 0.
//   <phase>   the pass's dominant phase, from the line's `dominant` field.
//   untimed   no dominant phase and not `reset`: the time is in no named phase.
//
//   Only the FFmpeg DC pump has the watch. A host whose sessions ran another
//   pump logs no STALL lines, which reads as zero and is not health.
//
// WHAT THE FLEET READ CANNOT SEE, AND WHAT IT PRINTS ABOUT IT
//
//   - `agent_logs` keeps 7 days, the first census read 14. A window here is at
//     most 8 days, and a before and an after are read over windows of equal
//     length.
//   - The uploader can lose lines. A full channel is counted, and the next batch
//     carries `dropped=N`. A batch shed or refused is counted nowhere. So each
//     row prints `dropped`, `hb_gaps` (a session's heartbeats more than 10 s
//     apart, the uploader's losses showing as a hole) and `counted`, the sum of
//     the heartbeats' `pump_stalls`: every stall, logged or not. `counted` below
//     `stalls` means heartbeats were lost.
//   - `reset` needs the heartbeat just before a STALL, and a lost heartbeat turns
//     a `reset` line into `untimed`. Those lines still show every other mark of
//     the artifact, so they are counted as `reset_no_hb`, inside `untimed`.
//   - An uploaded line carries a millisecond stamp where the file carries
//     microseconds, so two lines 4.7 ms apart can read 5. The fleet read takes
//     <= 5 ms: an edge line is called `reset`, the class AC7 wants at 0.
//
// `--probes` is the caps-probe census of FR-65 AC6, from the same read. The daemon
// logs every probe run: the decision to probe and its outcome. A start is
// `agent starting`, a probe line within 60 s after a start is that start's probe,
// and any other is steady state.

'use strict';

const DAY = 86400000;
/** The heartbeat a `reset` line follows, in the unit its stamps carry. */
const RESET_HB_US_MS = 5;
const RESET_HB_FLEET_MS = 6;
const RESET_OTHER_SHARE = 0.9;
const HB_GAP_MS = 10000;
const HB_PERIOD_MS = 2000;
const PROBE_AFTER_START_S = 60;
/** The first build with `PassWatch::rebase` and the monotonic `stalls_total`. */
const REBASE_VERSION = '0.4.192';
/** The heartbeat a line at the window's start needs, and a batch's upload delay. */
const LEAD_MS = 5 * 60000;
const LATE_MS = 60 * 60000;
const MAX_WINDOW_DAYS = 8;

/** The six the heartbeat's reset zeroed. */
const RESET_PHASES = ['capture_ms', 'scale_ms', 'encode_ms', 'send_ms', 'apply_ms', 'open_ms'];

const STALL_RE = /DC pump STALL/;
const HB_RE = /DC pump heartbeat/;
const DROPPED_RE = /lines dropped since the last batch/;
const START_RE = /^agent starting/;
const PROBE_RE =
  /caps probe: (child reported|cache miss|cache disabled|no directory to cache in)|the probe child (hung|DIED)|could not spawn the probe child/;
const SPAWN_FAIL_RE = /could not spawn the probe child/;
/** All of the above, as one pattern for the server-side `$regexMatch`. */
const MSG_PATTERN = [STALL_RE, HB_RE, DROPPED_RE, START_RE, PROBE_RE].map((r) => r.source).join('|');

/** The only fields the report reads. The fleet read projects exactly these, so
 *  nothing else (addresses, paths, server URLs) ever leaves the database. */
const FIELDS = [
  'session_id',
  'iter_ms',
  'other_ms',
  'work_ms',
  'dominant',
  ...RESET_PHASES,
  'avg_capture_ms',
  'avg_encode_ms',
  'pump_stalls',
  'dropped',
  'version',
];

// --------------------------------------------------------------- field values

/** A field as `agent_logs` holds it (a number, or a Long under mongosh) or as a
 *  log line prints it. 0 when absent: a build older than a field reads 0.0. */
function num(v) {
  if (v === null || v === undefined) return 0;
  if (typeof v === 'number') return Number.isFinite(v) ? v : 0;
  if (typeof v === 'object' && typeof v.toNumber === 'function') return v.toNumber();
  const x = Number(String(v).trim());
  return Number.isFinite(x) ? x : 0;
}

function text(v) {
  if (v === null || v === undefined) return null;
  if (typeof v === 'object') return typeof v.toHexString === 'function' ? v.toHexString() : null;
  const s = String(v).trim();
  return s.length >= 2 && s[0] === '"' && s[s.length - 1] === '"' ? s.slice(1, -1) : s;
}

function versionParts(v) {
  const m = /^(\d+)\.(\d+)\.(\d+)/.exec(text(v) || '');
  return m ? [Number(m[1]), Number(m[2]), Number(m[3])] : null;
}

/** `after` if the build carries the rebase, `before` if not, `unknown` if it
 *  does not say. */
function eraOf(version) {
  const v = versionParts(version);
  if (!v) return 'unknown';
  const bar = versionParts(REBASE_VERSION);
  for (let i = 0; i < 3; i++) {
    if (v[i] !== bar[i]) return v[i] > bar[i] ? 'after' : 'before';
  }
  return 'after';
}

// ---------------------------------------------------------------- the classifier

/** Every mark of the window-reset artifact except the one that takes a heartbeat. */
function resetShaped(f) {
  const iter = num(f.iter_ms);
  return (
    RESET_PHASES.every((k) => num(f[k]) === 0) &&
    iter > 0 &&
    num(f.other_ms) >= RESET_OTHER_SHARE * iter
  );
}

/** The class of one STALL line. `sinceHbMs` is Infinity with no heartbeat seen,
 *  `hbWindowMs` the exclusive bound in the resolution of the stamps. */
function classify(f, sinceHbMs, hbWindowMs) {
  if (resetShaped(f) && sinceHbMs < hbWindowMs) return 'reset';
  const dom = /^Some\(\("([a-z_]+)"/.exec(String(f.dominant || ''));
  return dom ? dom[1] : 'untimed';
}

// --------------------------------------------------------------- the fold

function newBucket() {
  return {
    heartbeats: 0,
    stalls: 0,
    counted: 0,
    dropped: 0,
    hbGaps: 0,
    hbGapMs: 0,
    resetNoHb: 0,
    first: null,
    last: null,
    sessions: new Set(),
    cls: {},
  };
}

function noteTime(b, t) {
  if (b.first === null || t < b.first) b.first = t;
  if (b.last === null || t > b.last) b.last = t;
}

function tally(b, cls, iter) {
  const c = b.cls[cls] || (b.cls[cls] = { n: 0, max: 0, sum: 0 });
  c.n++;
  c.sum += iter;
  if (iter > c.max) c.max = iter;
}

class Census {
  constructor(opts) {
    this.since = (opts && opts.since) || -Infinity;
    this.until = (opts && opts.until) || Infinity;
    this.hbWindowMs = (opts && opts.hbWindowMs) || RESET_HB_FLEET_MS;
    this.onRow = (opts && opts.onRow) || null;
    this.agents = new Map();
    this.lastHb = new Map();
    this.probes = new Map();
    this.lines = 0;
  }

  /** agent -> version -> bucket. */
  bucket(agent, version) {
    if (!this.agents.has(agent)) this.agents.set(agent, new Map());
    const byVersion = this.agents.get(agent);
    const v = text(version) || '?';
    if (!byVersion.has(v)) byVersion.set(v, newBucket());
    return byVersion.get(v);
  }

  probe(agent) {
    if (!this.probes.has(agent)) {
      this.probes.set(agent, { starts: [], lines: [], fails: 0, batches: 0, first: null, last: null });
    }
    return this.probes.get(agent);
  }

  /** One line, in the order its process wrote it. A line outside the window
   *  still moves the fold: the heartbeat before a window's first STALL does. */
  add(agent, version, t, msg, f) {
    const fields = f || {};
    const inWindow = t >= this.since && t < this.until;
    this.lines++;
    if (HB_RE.test(msg)) return this.heartbeat(agent, version, t, fields, inWindow);
    if (STALL_RE.test(msg)) return inWindow ? this.stall(agent, version, t, fields) : undefined;
    if (!inWindow) return undefined;
    if (DROPPED_RE.test(msg)) {
      this.bucket(agent, version).dropped += num(fields.dropped);
    } else if (START_RE.test(msg)) {
      this.probe(agent).starts.push(t);
    } else if (PROBE_RE.test(msg)) {
      const p = this.probe(agent);
      p.lines.push(t);
      if (SPAWN_FAIL_RE.test(msg)) p.fails++;
    }
    return undefined;
  }

  heartbeat(agent, version, t, f, inWindow) {
    const sid = text(f.session_id) || '?';
    const key = agent + '/' + sid;
    const prev = this.lastHb.get(key);
    this.lastHb.set(key, {
      t,
      open_ms: num(f.open_ms),
      avg_capture_ms: num(f.avg_capture_ms),
      avg_encode_ms: num(f.avg_encode_ms),
    });
    if (!inWindow) return;
    const b = this.bucket(agent, version);
    b.heartbeats++;
    b.counted += num(f.pump_stalls);
    b.sessions.add(sid);
    noteTime(b, t);
    if (prev && t - prev.t > HB_GAP_MS) {
      b.hbGaps++;
      b.hbGapMs += t - prev.t - HB_PERIOD_MS;
    }
  }

  stall(agent, version, t, f) {
    const sid = text(f.session_id) || '?';
    const hb = this.lastHb.get(agent + '/' + sid);
    const sinceHb = hb ? t - hb.t : Infinity;
    const cls = classify(f, sinceHb, this.hbWindowMs);
    const iter = num(f.iter_ms);
    const b = this.bucket(agent, version);
    b.stalls++;
    b.sessions.add(sid);
    noteTime(b, t);
    tally(b, cls, iter);
    if (cls === 'untimed' && resetShaped(f)) b.resetNoHb++;
    if (this.onRow) {
      const row = {
        k: 'stall',
        a: agent,
        v: text(version),
        t: new Date(Math.floor(t)).toISOString(),
        cls,
        s: sid,
        iter_ms: iter,
        other_ms: num(f.other_ms),
        dominant: text(f.dominant),
        since_hb_ms: sinceHb === Infinity ? null : Math.round(sinceHb * 1000) / 1000,
      };
      if (cls === 'reset') {
        row.hb = { open_ms: hb.open_ms, avg_capture_ms: hb.avg_capture_ms, avg_encode_ms: hb.avg_encode_ms };
      }
      this.onRow(row);
    }
  }

  /** The caps-probe census, per agent. */
  probeRows() {
    const out = [];
    for (const [agent, p] of this.probes) {
      const starts = p.starts.slice().sort((a, b) => a - b);
      let atStart = 0;
      const steady = [];
      for (const t of p.lines) {
        const near = starts.some((s) => t >= s && t - s <= PROBE_AFTER_START_S * 1000);
        if (near) atStart++;
        else steady.push(new Date(t).toISOString().slice(0, 19));
      }
      out.push({
        agent,
        starts: starts.length,
        probeLines: p.lines.length,
        atStart,
        steady,
        fails: p.fails,
        batches: p.batches,
        first: p.first,
        last: p.last,
      });
    }
    return out;
  }

  finish() {
    const agents = [];
    const eras = { before: newBucket(), after: newBucket(), unknown: newBucket() };
    for (const [agent, byVersion] of this.agents) {
      const versions = [];
      for (const [version, b] of byVersion) {
        versions.push({ version, era: eraOf(version), b });
        const e = eras[eraOf(version)];
        e.heartbeats += b.heartbeats;
        e.stalls += b.stalls;
        e.counted += b.counted;
        e.dropped += b.dropped;
        e.hbGaps += b.hbGaps;
        e.hbGapMs += b.hbGapMs;
        e.resetNoHb += b.resetNoHb;
        for (const s of b.sessions) e.sessions.add(agent + '/' + s);
        if (b.first !== null) noteTime(e, b.first);
        if (b.last !== null) noteTime(e, b.last);
        for (const k of Object.keys(b.cls)) {
          const from = b.cls[k];
          const to = e.cls[k] || (e.cls[k] = { n: 0, max: 0, sum: 0 });
          to.n += from.n;
          to.sum += from.sum;
          if (from.max > to.max) to.max = from.max;
        }
      }
      versions.sort((x, y) => (x.b.first || 0) - (y.b.first || 0));
      agents.push({ agent, versions });
    }
    agents.sort((x, y) => (x.agent < y.agent ? -1 : 1));
    return { agents, eras, probes: this.probeRows(), lines: this.lines };
  }
}

// ---------------------------------------------------------------- rendering

function pad(s, n) {
  return String(s).padStart(n);
}

function iso(t) {
  return t === null || t === undefined ? '-' : new Date(t).toISOString().slice(0, 19);
}

function bucketLine(b) {
  // Heartbeats are session time, so the rate compares windows of unequal length.
  const rate = ((b.stalls / Math.max(1, b.heartbeats)) * 1000).toFixed(1);
  return (
    `heartbeats=${b.heartbeats} stalls=${b.stalls} (${rate} per 1000 heartbeats) counted=${b.counted} ` +
    `sessions=${b.sessions.size} dropped=${b.dropped} hb_gaps=${b.hbGaps} (~${Math.round(b.hbGapMs / 1000)} s) ` +
    `reset_no_hb=${b.resetNoHb} span ${iso(b.first)} .. ${iso(b.last)}`
  );
}

function classLines(b) {
  return Object.keys(b.cls)
    .sort()
    .map((k) => {
      const c = b.cls[k];
      return `    ${k.padEnd(10)} n=${pad(c.n, 4)}  iter_ms max=${pad(c.max.toFixed(1), 9)}  mean=${pad((c.sum / Math.max(1, c.n)).toFixed(1), 8)}`;
    });
}

function render(r, labels) {
  const out = [];
  for (const { agent, versions } of r.agents) {
    if (!versions.some((v) => v.b.stalls || v.b.heartbeats)) continue;
    const l = labels && labels[agent];
    out.push(`=== ${l ? l + ' ' : ''}${agent}`);
    for (const { version, era, b } of versions) {
      out.push(`  ${version} (${era}): ${bucketLine(b)}`);
      out.push(...classLines(b));
    }
  }
  out.push(`=== every agent, by build (${REBASE_VERSION} carries the rebase)`);
  for (const era of ['before', 'after', 'unknown']) {
    const b = r.eras[era];
    if (!b.stalls && !b.heartbeats) continue;
    out.push(`  ${era}: ${bucketLine(b)}`);
    out.push(...classLines(b));
  }
  return out.join('\n');
}

function renderProbes(r, labels) {
  const out = [];
  for (const p of r.probes) {
    const hours = p.first === null ? 0 : (p.last - p.first) / 3600000;
    const l = labels && labels[p.agent];
    out.push(
      `${l ? l + ' ' : ''}${p.agent}: window ${iso(p.first)} .. ${iso(p.last)} (${hours.toFixed(0)} h) starts=${p.starts} ` +
        `probe_lines=${p.probeLines} within_60s_of_a_start=${p.atStart} steady_state=${p.steady.length} spawn_failures=${p.fails}`,
    );
    if (p.steady.length) out.push('  steady-state probe lines: ' + p.steady.join(' '));
  }
  return out.join('\n');
}

// ---------------------------------------------------------------- mongosh runner

/**
 * The fleet read. Runs inside the Mongo container (the recipe in
 * scripts/fr65-stall-census.sh) and prints the report. Bounded by construction,
 * because an unbounded read inside that container once OOM-killed production
 * Mongo (2026-10-26):
 *   - a window of at most 8 days, read one day per query, each with `maxTimeMS`
 *     and `allowDiskUse: false`;
 *   - only streaming stages after the index-backed `$match` + `$sort`, so the
 *     server holds no result set and mongosh holds one row at a time;
 *   - only the FIELDS above are projected, and the fold keeps counters, each
 *     session's last heartbeat and the stamps of the probe lines;
 *   - credentials come from the container's own environment, never argv.
 */
function runMongosh() {
  const env = typeof process !== 'undefined' && process.env ? process.env : {};
  const now = Date.now();
  const until = env.FR65_UNTIL ? new Date(env.FR65_UNTIL).getTime() : now;
  const since = env.FR65_SINCE ? new Date(env.FR65_SINCE).getTime() : until - 7 * DAY;
  if (isNaN(since) || isNaN(until) || since >= until) {
    throw new Error('FR65_SINCE must be before FR65_UNTIL (ISO 8601)');
  }
  if (until - since > MAX_WINDOW_DAYS * DAY) {
    throw new Error('the window is over 8 days; agent_logs keeps 7');
  }
  const showRows = env.FR65_ROWS === '1';
  const showProbes = env.FR65_PROBES === '1';
  const database = connectDb(env.FR65_DB || 'roomler2', env);
  const coll = database.getCollection('agent_logs');
  print(`agent_logs ≈ ${coll.estimatedDocumentCount()} batches; reading ${new Date(since).toISOString()} → ${new Date(until).toISOString()}`);

  const census = new Census({ since, until, onRow: showRows ? (row) => print(JSON.stringify(row)) : null });
  const project = {};
  for (const k of FIELDS) project[k] = '$lines.fields.' + k;
  const agentIds = (env.FR65_AGENTS || '').split(',').filter(Boolean).map((h) => ObjectId(h));
  const scanEnd = Math.min(until + LATE_MS, now + 60000);
  for (let t0 = since - LEAD_MS; t0 < scanEnd; t0 += DAY) {
    const t1 = Math.min(t0 + DAY, scanEnd);
    const match = { source: 'agent', created_at: { $gte: new Date(t0), $lt: new Date(t1) } };
    if (env.FR65_TENANT) match.tenant_id = ObjectId(env.FR65_TENANT);
    if (agentIds.length) match.agent_id = { $in: agentIds };
    const pipeline = [
      { $match: match },
      { $sort: { created_at: 1 } },
      {
        $project: {
          _id: 0,
          a: '$agent_id',
          v: '$agent_version',
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
      { $project: { a: 1, v: 1, t: '$lines.ts', m: '$lines.msg', f: project } },
    ];
    const started = Date.now();
    let dayRows = 0;
    coll.aggregate(pipeline, { maxTimeMS: 240000, allowDiskUse: false }).forEach((row) => {
      dayRows++;
      census.add(text(row.a), row.v, new Date(row.t).getTime(), row.m, row.f);
    });
    print(`  ${new Date(t0).toISOString().slice(0, 16)}: ${dayRows} lines in ${Date.now() - started} ms`);
    if (showProbes) {
      coll
        .aggregate(
          [
            { $match: match },
            { $group: { _id: '$agent_id', n: { $sum: 1 }, first: { $min: '$created_at' }, last: { $max: '$created_at' } } },
          ],
          { maxTimeMS: 120000, allowDiskUse: false },
        )
        .forEach((g) => {
          const p = census.probe(text(g._id));
          p.batches += g.n;
          const first = new Date(g.first).getTime();
          const last = new Date(g.last).getTime();
          if (p.first === null || first < p.first) p.first = first;
          if (p.last === null || last > p.last) p.last = last;
        });
    }
  }

  const r = census.finish();
  const labels = agentLabels(database, r);
  print(showProbes ? renderProbes(r, labels) : render(r, labels));
  print(`read ${r.lines} lines`);
}

/** Names for the report, from the one small collection that holds them. */
function agentLabels(database, r) {
  const ids = r.agents.map((x) => x.agent).concat(r.probes.map((x) => x.agent));
  const labels = {};
  database
    .getCollection('agents')
    .find({ _id: { $in: ids.map((h) => ObjectId(h)) } }, { name: 1, display_name: 1, os: 1 })
    .limit(2000)
    .forEach((a) => {
      labels[text(a._id)] = `${a.display_name || a.name} (${a.os})`;
    });
  return labels;
}

function connectDb(dbName, env) {
  if (env.FR65_URI) return new Mongo(env.FR65_URI).getDB(dbName);
  const u = env.MONGO_INITDB_ROOT_USERNAME;
  const p = env.MONGO_INITDB_ROOT_PASSWORD;
  if (u && p) {
    return new Mongo(
      'mongodb://' + encodeURIComponent(u) + ':' + encodeURIComponent(p) + '@127.0.0.1:27017/?authSource=admin',
    ).getDB(dbName);
  }
  throw new Error('no connection: pass FR65_URI, or run inside the Mongo container (its env has the credentials)');
}

// ---------------------------------------------------------------- host read

/** A file's stamp, to the microsecond: `Date` keeps only milliseconds, and the
 *  5 ms edge of `reset` is read to the stamp's own resolution here. */
function textStampMs(ts) {
  const m = /^(.*?)(\.\d+)?Z?$/.exec(ts);
  return Date.parse(m[1] + 'Z') + (m[2] ? Number('0' + m[2]) * 1000 : 0);
}

function nodeMain(argv) {
  const files = [];
  const opts = { rows: false, probes: false, json: false, since: null, until: null };
  for (let i = 0; i < argv.length; i++) {
    const a = argv[i];
    if (a === '--self-test') return process.exit(selfTest());
    else if (a === '--rows') opts.rows = true;
    else if (a === '--probes') opts.probes = true;
    else if (a === '--since') opts.since = Date.parse(argv[++i]);
    else if (a === '--until') opts.until = Date.parse(argv[++i]);
    else if (a === '--from-logs') continue;
    else if (a === '-h' || a === '--help') {
      console.log(
        'usage: fr65-stall-census.cjs --from-logs [--rows] [--probes] [--since ISO] [--until ISO] FILE...\n' +
          '       fr65-stall-census.cjs --self-test\n' +
          'The fleet read: scripts/fr65-stall-census.sh.',
      );
      return process.exit(0);
    } else files.push(a);
  }
  if (!files.length) {
    console.error('fr65-stall-census: no input. Pass log files, or --help for the fleet read.');
    return process.exit(1);
  }
  const fs = require('fs');
  const { parseLogLine } = require('./rc-shadow-report.cjs');
  const census = new Census({
    hbWindowMs: RESET_HB_US_MS,
    since: opts.since || -Infinity,
    until: opts.until || Infinity,
    onRow: opts.rows ? (row) => console.log(JSON.stringify(row)) : null,
  });
  let version = null;
  for (const file of files) {
    for (const line of fs.readFileSync(file, 'utf8').split('\n')) {
      const p = parseLogLine(line);
      if (!p) continue;
      if (START_RE.test(p.msg) && p.fields.version) version = text(p.fields.version);
      census.add('host', version, textStampMs(p.ts), p.msg, p.fields);
    }
  }
  const r = census.finish();
  console.log(opts.probes ? renderProbes(r) : render(r));
  return undefined;
}

// ---------------------------------------------------------------- self-test

/** Real line shapes: a dev box's own logs, 2026-10-30 and 2026-11-02, ids replaced. */
const FIX = {
  open:
    '2026-11-02T02:34:01.931858Z  WARN FFmpeg DC pump STALL — one pass exceeded the budget session_id=6ae7f69dd85355b55cf55d73 codec_label="AV1" constrained=false iter_ms=536.661 capture_ms=8.753 scale_ms=0.0 encode_ms=20.588 send_ms=0.017 apply_ms=1.507 open_ms=503.567 pace_ms=0.0 stats_ms=0.0 ctrl_ms=0.0 swap_ms=0.003 gate_ms=0.005 dc_lock_ms=0.001 enc_cmd_ms=0.0 close_ms=0.0 recapture_ms=0.0 poll_ms=0.0 heartbeat_ms=0.0 other_ms=2.22 work_ms=527.908 dominant=Some(("open", 503567)) target_bps=31104000',
  resetLine: (ts) =>
    `${ts}  WARN FFmpeg DC pump STALL — one pass exceeded the budget session_id=aaaaaaaaaaaaaaaaaaaaaaaa codec_label="HEVC" constrained=false iter_ms=3586.0 capture_ms=0.0 scale_ms=0.0 encode_ms=0.0 send_ms=0.0 apply_ms=0.0 open_ms=0.0 pace_ms=0.0 stats_ms=0.1 ctrl_ms=0.0 swap_ms=0.0 gate_ms=0.0 other_ms=3585.9 work_ms=3586.0 dominant=None target_bps=9000000`,
  heartbeat: (ts) =>
    `${ts}  INFO FFmpeg DC pump heartbeat (≈2s window) session_id=aaaaaaaaaaaaaaaaaaaaaaaa avg_capture_ms=4.1 avg_encode_ms=11.2 open_ms=3529.0 pump_stalls=1`,
};

function selfTest() {
  const assert = require('assert');
  const { parseLogLine } = require('./rc-shadow-report.cjs');
  let n = 0;
  const ok = (cond, name, got) => {
    n++;
    assert.ok(cond, name + (got === undefined ? '' : ` — got ${JSON.stringify(got)}`));
  };
  const feed = (census, line) => {
    const p = parseLogLine(line);
    census.add('h', '0.4.191', textStampMs(p.ts), p.msg, p.fields);
  };
  const classesOf = (census) => census.finish().agents[0].versions[0].b.cls;
  const textCensus = () => new Census({ hbWindowMs: RESET_HB_US_MS });

  // The field reads, as both ends of the upload hold them.
  ok(num({ toNumber: () => 7 }) === 7, 'a Long reads as its value');
  ok(num('12.5') === 12.5 && num(undefined) === 0, 'a text number, and an absent field is 0');
  ok(text('"0.4.197"') === '0.4.197', 'a quoted value loses its quotes');
  ok(text({ toHexString: () => 'ab' }) === 'ab', 'an ObjectId reads as hex');

  // Build eras.
  ok(eraOf('0.4.191') === 'before' && eraOf('0.4.192') === 'after' && eraOf('0.4.212') === 'after', 'the rebase rides 0.4.192');
  ok(eraOf('0.3.999') === 'before' && eraOf('0.5.0') === 'after', 'each part compares numerically');
  ok(eraOf(undefined) === 'unknown', 'no version is not a before');

  // A named phase is the answer, and it survives the heartbeat before it.
  let c = textCensus();
  feed(c, FIX.open);
  ok(classesOf(c).open.n === 1, 'dominant open is `open`', classesOf(c));
  ok(Math.abs(classesOf(c).open.max - 536.661) < 1e-9, 'the class keeps iter_ms');

  // The artifact: within 5 ms of the session's heartbeat, every reset phase 0.0.
  c = textCensus();
  feed(c, FIX.heartbeat('2026-10-20T10:00:00.000000Z'));
  feed(c, FIX.resetLine('2026-10-20T10:00:00.004700Z'));
  ok(classesOf(c).reset.n === 1, 'the reset shape 4.7 ms after a heartbeat is `reset`', classesOf(c));

  // Negative controls: each of the rule's marks is load-bearing.
  c = textCensus();
  feed(c, FIX.heartbeat('2026-10-20T10:00:00.000000Z'));
  feed(c, FIX.resetLine('2026-10-20T10:00:00.005000Z'));
  ok(classesOf(c).untimed.n === 1 && !classesOf(c).reset, 'exactly 5 ms is not `reset` on a file stamp', classesOf(c));
  ok(c.finish().agents[0].versions[0].b.resetNoHb === 1, 'a reset shape with no heartbeat near is `reset_no_hb`');
  c = textCensus();
  feed(c, FIX.resetLine('2026-10-20T10:00:00.001000Z'));
  ok(classesOf(c).untimed.n === 1, 'no heartbeat seen at all is `untimed`', classesOf(c));
  c = textCensus();
  feed(c, FIX.heartbeat('2026-10-20T10:00:00.000000Z').replace('aaaaaaaaaaaaaaaaaaaaaaaa', 'bbbbbbbbbbbbbbbbbbbbbbbb'));
  feed(c, FIX.resetLine('2026-10-20T10:00:00.001000Z'));
  ok(classesOf(c).untimed.n === 1, "another session's heartbeat does not count", classesOf(c));
  c = textCensus();
  feed(c, FIX.heartbeat('2026-10-20T10:00:00.000000Z'));
  feed(c, FIX.resetLine('2026-10-20T10:00:00.001000Z').replace('other_ms=3585.9', 'other_ms=3000.0'));
  ok(classesOf(c).untimed.n === 1, 'other below 90 % of the pass is not `reset`', classesOf(c));
  c = textCensus();
  feed(c, FIX.heartbeat('2026-10-20T10:00:00.000000Z'));
  feed(c, FIX.resetLine('2026-10-20T10:00:00.001000Z').replace('open_ms=0.0', 'open_ms=0.5'));
  ok(classesOf(c).untimed.n === 1, 'one reset phase off zero is not `reset`', classesOf(c));

  // The fleet read: `agent_logs` numbers, a Debug string, millisecond stamps.
  const row = { session_id: 's1', iter_ms: 3586, other_ms: 3585.9, dominant: 'None', capture_ms: 0 };
  c = new Census();
  c.add('a', '0.4.191', 1000, 'FFmpeg DC pump heartbeat (≈2s window)', { session_id: 's1', pump_stalls: { toNumber: () => 1 } });
  c.add('a', '0.4.191', 1005, 'FFmpeg DC pump STALL — one pass exceeded the budget', row);
  ok(classesOf(c).reset.n === 1, 'a fleet read takes 5 ms at millisecond stamps', classesOf(c));
  ok(c.finish().agents[0].versions[0].b.counted === 1, 'pump_stalls sums as a Long');
  c = new Census();
  c.add('a', '0.4.191', 1000, 'FFmpeg DC pump heartbeat (≈2s window)', { session_id: 's1' });
  c.add('a', '0.4.191', 1006, 'FFmpeg DC pump STALL — one pass exceeded the budget', row);
  ok(classesOf(c).untimed.n === 1, 'and refuses 6', classesOf(c));
  c = new Census();
  c.add('a', '0.4.192', 1, 'FFmpeg DC pump STALL — one pass exceeded the budget', {
    session_id: 's1',
    iter_ms: 900,
    other_ms: 1,
    dominant: 'Some(("poll", 899000))',
  });
  ok(classesOf(c).poll.n === 1, 'the Debug string of `dominant` names a new phase', classesOf(c));

  // What the read says about its own coverage.
  c = new Census();
  c.add('a', '0.4.197', 0, 'VP9-444 DC pump heartbeat (≈1s window)', { session_id: 's1' });
  c.add('a', '0.4.197', 2000, 'FFmpeg DC pump heartbeat (≈2s window)', { session_id: 's1' });
  c.add('a', '0.4.197', 14000, 'FFmpeg DC pump heartbeat (≈2s window)', { session_id: 's1' });
  c.add('a', '0.4.197', 14100, 'log upload channel was full: lines dropped since the last batch', { dropped: 7 });
  const b = c.finish().agents[0].versions[0].b;
  ok(b.heartbeats === 3 && b.hbGaps === 1 && b.hbGapMs === 10000, 'a hole in the heartbeats shows', b);
  ok(b.dropped === 7, 'the uploader\'s count shows', b);

  // A window: the heartbeat before it moves the fold, and is not counted.
  c = new Census({ since: 1000, until: 2000 });
  c.add('a', '0.4.191', 998, 'FFmpeg DC pump heartbeat (≈2s window)', { session_id: 's1' });
  c.add('a', '0.4.191', 1001, 'FFmpeg DC pump STALL — one pass exceeded the budget', row);
  c.add('a', '0.4.191', 2000, 'FFmpeg DC pump STALL — one pass exceeded the budget', row);
  const w = c.finish().agents[0].versions[0].b;
  ok(w.stalls === 1 && w.heartbeats === 0 && w.cls.reset.n === 1, 'the window bounds the count, not the fold', w);

  // The caps-probe census, with the steady-state line as its control.
  c = new Census();
  c.add('a', '0.4.197', 100000, 'agent starting', { version: '0.4.197' });
  c.add('a', '0.4.197', 130000, 'caps probe: child reported', {});
  c.add('a', '0.4.197', 160000, 'caps probe: cache miss — probing', {});
  c.add('a', '0.4.197', 161000, 'caps probe: cache miss — probing', {});
  c.add('a', '0.4.197', 99000, 'caps probe: could not spawn the probe child — no HW advertisement', {});
  const [pr] = c.finish().probes;
  ok(pr.starts === 1 && pr.probeLines === 4, 'starts and probe lines count', pr);
  ok(pr.atStart === 2 && pr.steady.length === 2 && pr.fails === 1, 'within 60 s of a start, and not before it', pr);
  ok(!PROBE_RE.test('caps probe: 4:4:4 phase reported'), 'a phase line is not a run');

  // The server-side filter is the union of the lines the fold reads.
  const re = new RegExp(MSG_PATTERN);
  for (const m of [
    'FFmpeg DC pump STALL — one pass exceeded the budget',
    'FFmpeg DC pump heartbeat (≈2s window)',
    'VP9-444 DC pump heartbeat (≈1s window)',
    'agent starting',
    'caps probe: the probe child hung — the daemon is unaffected',
    'log upload channel was full: lines dropped since the last batch',
  ]) {
    ok(re.test(m), 'the server keeps: ' + m);
  }
  ok(!re.test('not agent starting anything'), 'a start is anchored');

  console.log(`fr65-stall-census self-test: ${n} checks passed`);
  return 0;
}

// ---------------------------------------------------------------- dispatch

if (typeof module !== 'undefined' && module.exports) {
  module.exports = { Census, classify, eraOf, render, textStampMs };
}
if (typeof Mongo === 'function' && typeof print === 'function') {
  runMongosh();
} else if (typeof require !== 'undefined' && typeof module !== 'undefined' && require.main === module) {
  nodeMain(process.argv.slice(2));
}
