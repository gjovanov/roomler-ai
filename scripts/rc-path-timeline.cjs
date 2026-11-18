#!/usr/bin/env node
//
// rc-path-timeline.cjs — FR-148: ONE device, ONE time window, every
// remote-desktop session in time order.
//
// WHY THIS EXISTS
//
//   rc-path-report.cjs folds a fleet into one summary row per session. A
//   field read of two laptops (FR-148 §8) needs the other view: one device's
//   day, line by line, so that a target move, a pipe state, a stall and the
//   path memory's close can be put in order. This reads FR-148's lines for one
//   agent of one org in one window and prints, per session, in time order:
//
//   | Section    | Lines read (message text, by call site)                                  |
//   |------------|--------------------------------------------------------------------------|
//   | carrier    | `session carrier recorded (FR-102 P9c)` (session_carrier.rs),            |
//   |            | `per-session ICE path detected (adaptive bitrate)` (carrier_rank.rs),    |
//   |            | `rc ICE: overlay …` (peer.rs, rc_overlay.rs),                            |
//   |            | `rc ICE: carrier rank (FR-148 P4)` and `…, the mesh link 10 s on …`      |
//   | encoder    | `ffmpeg encoder opened (…)`, `FR-148: the QSV encoder opened at QVBR…`,  |
//   |            | `ffmpeg open_as_with rejected the quality options…` (ffmpeg/encoder.rs)  |
//   | rate       | `FFmpeg DC pump heartbeat (≈2s window)`, `VP9-444 DC pump heartbeat…`    |
//   |            | (peer.rs): bytes_written and frames_sent are CUMULATIVE per pump         |
//   | delivery   | `path delivery (FR-148 P0, shadow, ≈10s)`, `path model (…)`              |
//   |            | (media_delivery.rs)                                                      |
//   | memory     | `FR-148 P1 path memory: the pair's entry at the open` / `…: what the     |
//   |            | close wrote` (media_pair_memory.rs), `FR-148 P1 rate memory: …`          |
//   |            | (encode/rate_memory.rs)                                                  |
//   | keepalive  | the heartbeat's `frames_keepalive` / `keepalive_bytes` (per window, not  |
//   |            | cumulative: media_window.rs resets them), and the capture-timing line    |
//   |            | `system-context capture: worker-side scrap frame() timing (compare to    |
//   |            | pump heartbeat …)` (system_context/capture_pump.rs), which names "pump   |
//   |            | heartbeat" in its text but carries none of the heartbeat's fields        |
//
//   The heartbeat is matched on `DC pump heartbeat` at the start of the
//   message, so the capture-timing line is never read as one (a hand query
//   that matched "pump heartbeat" anywhere printed `undefined` and NaN for it).
//   A field a line does not carry prints `-`, never 0, `undefined` or NaN.
//
// WHAT IT CANNOT TELL
//
//   - Why the target moved. The governor sets the applied target in
//     `record_applied` (agents/roomlerd/src/encode/governor.rs, `fn
//     record_applied`) and no line says why; a target move prints "reason: not
//     logged".
//   - Which session a line belongs to when the line names none: the encoder
//     lines and the rate-memory line. Such a line goes to the session of this
//     agent whose lines span its time (from OPEN_LAG_MS before the session's
//     first line to CLOSE_LAG_MS after its last), the latest-started one when
//     two do; otherwise it is counted as unattributed.
//   - Whether a session ended: a session whose last line is within
//     OPEN_AT_END_MS of the window's end is printed as open at the window's end.
//
// BOUNDED BY CONSTRUCTION (the read runs on production `agent_logs`)
//
//   - ONE agent of ONE tenant and ONE window of at most 24 h. `$match` on
//     tenant_id + agent_id + a created_at range (padded by UPLOAD_PAD_MS each
//     side, since a batch is stored after its lines), pinned with `hint` to
//     `tenant_id_1_agent_id_1_created_at_-1` (docs/data-model.md, agent_logs).
//     Equality on the first two keys, so the `$sort` on created_at is served
//     by the index: no blocking SORT stage (`--explain` prints the plan). A
//     line whose batch was stored outside the padded range is not read; the
//     report's header prints that range.
//   - Streaming stages only after the match: a `$project` whose `$filter`
//     keeps the lines of the messages above whose `ts` (a BSON Date, compared
//     as a Date, never as text) lies in the window, `$unwind`, a `$project` of
//     named fields, and `$limit` LIMIT+1. No `$group`. `allowDiskUse: false`,
//     `maxTimeMS` 240 s.
//   - A read that meets the limit prints `TRUNCATED`. Rows come in the order
//     their batches were stored, so what is missing is every batch stored
//     after the last one read, and some of its lines can be older than the
//     last time printed: narrow the window, or raise --limit.
//   - What leaves the database: named fields only (`viewers` only as a
//     number: the rank line's count, never the overlay line's list of viewer
//     addresses), plus the two path-memory lines' fields with every key that
//     could name an address, a host or a person removed on the server
//     (DENY_KEY). Ids print as their last 4 hex in the report and in --rows
//     alike (`masks`), and every printed line, like every string --rows
//     prints, is scrubbed of addresses and 24-hex ids again.
//
//   THE ESTIMATE, a ten-hour window with eight hours of sessions, FFmpeg pump:
//   heartbeats one per ≈2 s (14,400), delivery and model lines one each per
//   10 s (2,880 + 2,880), capture-timing lines one per 30 s of capture (960),
//   and a few dozen carrier, encoder and memory lines a session: about 21,200
//   rows. A projected heartbeat is about 600 bytes of JSON and the others
//   about 300: about 11 MB through `--rows`, and a report of a few thousand
//   lines (minute rows by default). The documents examined are this agent's
//   batches in the window only (about 6,000 at the upload rate of a session
//   hour, 10 a minute), through the index. DEFAULT_LIMIT (60,000) is about
//   three such days of one device; a VP9-444 pump doubles the heartbeats.
//   Split a longer window with --from/--to.
//
// RUN
//
//   node scripts/rc-path-timeline.cjs --self-test
//   node scripts/rc-path-timeline.cjs [--intervals] [--from ISO --to ISO] ROWS.ndjson   # offline
//   bash scripts/rc-path-report.sh --timeline --mars --yes --tenant HEX --agent HEX \
//        --from 2026-11-18T07:00:00Z --to 2026-11-18T18:15:00Z                         # the read
//
// The core runs unchanged under Node and under mongosh (the runners at the
// bottom are the only environment-specific code).

'use strict';

// ---------------------------------------------------------------- constants

/** A session-less line goes to a session whose first line is at most this
 *  much after it (the rate-memory line precedes the pump's start). */
const OPEN_LAG_MS = 10000;
/** … or whose last line is at most this much before it. */
const CLOSE_LAG_MS = 30000;
/** Two heartbeats of one pump make a rate only this far apart. */
const RATE_MIN_GAP_MS = 500;
const RATE_MAX_GAP_MS = 10000;
/** `encode::relay_max_bps`'s default: the plan's nominal ceiling on a relay. */
const RELAY_NOMINAL_BPS = 3000000;
/** A session whose last line is this close to the window's end may be open. */
const OPEN_AT_END_MS = 30000;
/** A batch is stored after its lines; the created_at range is padded so. */
const UPLOAD_PAD_MS = 10 * 60 * 1000;
const DEFAULT_LIMIT = 60000;
const MAX_LIMIT = 200000;
const MAX_WINDOW_MS = 24 * 3600 * 1000;
/** A window end, exactly as rc-path-report.sh's ISO pattern takes it (the
 *  self-test compares the two): a date, or a date and a time with its zone.
 *  A time without a zone is refused: JavaScript reads it in the zone of the
 *  machine that runs the read. */
const ISO_TIME = /^[0-9]{4}-(0[1-9]|1[0-2])-(0[1-9]|[12][0-9]|3[01])(T([01][0-9]|2[0-3]):[0-5][0-9](:[0-5][0-9](\.[0-9]{1,3})?)?(Z|[+-]([01][0-9]|2[0-3]):[0-5][0-9]))?$/;

/** The lines read: kind, the message's start, and the call site whose
 *  literal the self-test finds in the source. */
const KINDS = [
  { k: 'heartbeat', re: /^(FFmpeg|VP9-444) DC pump heartbeat/, src: ['peer.rs', 'FFmpeg DC pump heartbeat (≈2s window)', 'VP9-444 DC pump heartbeat (≈1s window)'] },
  { k: 'carrier', re: /^session carrier recorded/, src: ['session_carrier.rs', 'session carrier recorded (FR-102 P9c)'] },
  { k: 'ice', re: /^per-session ICE path detected/, src: ['carrier_rank.rs', 'per-session ICE path detected (adaptive bitrate)'] },
  { k: 'rank', re: /^rc ICE: carrier rank/, src: ['rc_overlay.rs', 'rc ICE: carrier rank (FR-148 P4)', 'rc ICE: carrier rank, the mesh link 10 s on (FR-148 P4)'] },
  { k: 'overlay', re: /^rc ICE: overlay /, src: ['peer.rs', 'rc ICE: overlay {}'] },
  { k: 'qsv', re: /^FR-148: the QSV encoder opened at QVBR/, src: ['encode/ffmpeg/encoder.rs', 'FR-148: the QSV encoder opened at QVBR, its target under its cap'] },
  { k: 'encOpen', re: /^ffmpeg encoder opened \(/, src: ['encode/ffmpeg/encoder.rs', 'ffmpeg encoder opened (constant-quality + maxrate cap)'] },
  { k: 'rejected', re: /^ffmpeg open_as_with rejected the quality options/, src: ['encode/ffmpeg/encoder.rs', 'ffmpeg open_as_with rejected the quality options'] },
  { k: 'delivery', re: /^path delivery \(FR-148 P0/, src: ['media_delivery.rs', 'path delivery (FR-148 P0, shadow, ≈10s)'] },
  { k: 'model', re: /^path model \(FR-148 P0/, src: ['media_delivery.rs', 'path model (FR-148 P0, shadow, ≈10s)'] },
  { k: 'memOpen', re: /^FR-148 P1 path memory: the pair's entry at the open/, src: ['media_pair_memory.rs', "FR-148 P1 path memory: the pair's entry at the open"] },
  { k: 'memClose', re: /^FR-148 P1 path memory: what the close wrote/, src: ['media_pair_memory.rs', 'FR-148 P1 path memory: what the close wrote'] },
  { k: 'rateMem', re: /^FR-148 P1 rate memory/, src: ['encode/rate_memory.rs', "FR-148 P1 rate memory: the pair's entry has no measured provenance"] },
  { k: 'capTiming', re: /^system-context capture: worker-side scrap frame\(\) timing/, src: ['system_context/capture_pump.rs', 'system-context capture: worker-side scrap frame() timing'] },
];
const MSG_PATTERN = KINDS.map((d) => d.re.source).join('|');
/** The two lines whose fields all leave (minus DENY_KEY), so a new field or
 *  value prints as it is. */
const MEM_PATTERN = '^FR-148 P1 path memory: ';
/** A key that could carry an address, a host, a person or a raw key. */
const DENY_KEY = /addr|peer|user|name|host|^ip$|_ip$|mesh_blocks|candidate|email/i;
/** The fields that carry an id, by kind. An id prints as its last 4 hex. */
const ID_KEYS = new Map([
  ['session_id', 'session'],
  ['session', 'session'],
  ['key_id', 'key'],
]);
/** Fields that leave only as a number. The rank line's `viewers` is a count
 *  (rc_overlay.rs `log_rank`); the overlay-auto line's is its list of viewer
 *  addresses, which stays in the database. */
const NUMBER_ONLY = ['viewers'];

/** Named fields that leave the database, by the line that carries them. */
const FIELDS = [
  // ids: printed as their last 4 hex
  'session_id',
  'session',
  // heartbeat (peer.rs)
  'codec_label',
  'encoder',
  'constrained',
  'target_bps',
  'target_fps',
  'width',
  'height',
  'frames_sent',
  'bytes_written',
  'maxrate_bps',
  'pipe_state',
  'pipe_measured',
  'readings_refuted',
  'link_unoffered',
  'frames_keepalive',
  'keepalive_bytes',
  'window_ms',
  // carrier + ICE (session_carrier.rs, carrier_rank.rs)
  'carrier',
  'local_typ',
  'remote_typ',
  'relay',
  'local_proto',
  'remote_proto',
  'waited_ms',
  'opening',
  // carrier rank + overlay (rc_overlay.rs, peer.rs)
  'carrier_rank',
  'link',
  'link_at_offer',
  'upgrading',
  'stalled',
  'viewers', // a number only (NUMBER_ONLY)
  'viewers_from',
  'ranked_last',
  'read_ms',
  'overlay_blocks',
  // encoder (ffmpeg/encoder.rs)
  'rate_control',
  'fallback',
  'bit_rate_bps',
  'fps',
  'cq',
  'input',
  'open_err',
  // delivery + model (media_delivery.rs)
  'span_ms',
  'delivery_samples',
  'backlogged_bps',
  'app_limited_bps',
  'stalled_ms',
  'viewer_rx_bps',
  'model_capacity_bps',
  'model_floor_bps',
  // rate memory (encode/rate_memory.rs) — its `peer` never leaves
  'assumed_bps',
  'age_s',
  // capture timing (system_context/capture_pump.rs)
  'worker_avg_capture_ms',
  'calls',
];

/** The close line's sample counts, both generations (0.4.225 carries
 *  `samples_not_counted` only; #4029 adds the rest). FR-148 O7. */
const O7_COUNTED = ['samples_counted', 'samples_counted_overproduced', 'samples_counted_clear'];
const O7_ASIDE = ['samples_unread', 'samples_viewer_late', 'samples_under_plan', 'samples_not_counted'];

// ---------------------------------------------------------------- values

/** One stored value as a plain JS value: numbers, booleans, `Some(x)`, `None`,
 *  quoted text, a bson Long. */
function value(v) {
  if (v === null || v === undefined) return null;
  if (typeof v === 'number') return Number.isFinite(v) ? v : null;
  if (typeof v === 'boolean') return v;
  if (typeof v === 'object') {
    if (typeof v.toNumber === 'function') return v.toNumber();
    if (typeof v.toHexString === 'function') return v.toHexString();
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

/** A value as text: `Some(…)` and quotes stripped, never parsed as a number,
 *  so an all-digit id keeps every digit. */
function text(v) {
  if (v === null || v === undefined) return null;
  if (typeof v === 'number') return Number.isFinite(v) ? String(v) : null;
  if (typeof v === 'object') {
    if (typeof v.toHexString === 'function') return v.toHexString();
    if (typeof v.toNumber === 'function') return String(v.toNumber());
    return null;
  }
  let s = String(v).trim();
  if (s === 'None' || s === '') return null;
  const some = /^Some\((.*)\)$/.exec(s);
  if (some) s = some[1].trim();
  if (s.length >= 2 && s[0] === '"' && s[s.length - 1] === '"') s = s.slice(1, -1);
  return s === '' ? null : s;
}

/** `<bps>/<basis>/<age>ms` (media_delivery.rs `Shown`), or null. */
function shown(v) {
  const s = text(v);
  if (!s) return null;
  const m = /^(\d+)\/([^/]+)(?:\/(\d+)ms)?$/.exec(s);
  return m ? { bps: Number(m[1]), basis: m[2], ageMs: m[3] === undefined ? null : Number(m[3]) } : { raw: s };
}

function epochMs(t) {
  if (t === null || t === undefined) return null;
  if (t instanceof Date) return t.getTime();
  if (typeof t === 'number') return t;
  if (typeof t === 'object' && t.$date !== undefined) return epochMs(t.$date);
  const ms = Date.parse(String(t));
  return Number.isFinite(ms) ? ms : null;
}

function kindOf(msg) {
  for (const d of KINDS) if (d.re.test(msg)) return d.k;
  return null;
}

// ---------------------------------------------------------------- privacy

/** An id as its last 4 hex; an id already masked (`…`) as it is. */
function mask(id) {
  const s = text(id);
  if (!s) return '-';
  return s[0] === '…' ? s : '…' + s.slice(-4).toLowerCase();
}

/**
 * The masks of one read, by kind (ID_KEYS). An id prints as `mask` gives,
 * and an id whose last 4 hex the read already gave to a different id of its
 * kind gets `~2` after them (then `~3`, …), so two sessions never fold into
 * one. The read's order decides, so one read's rows mask alike in the report
 * and in --rows.
 */
function masks() {
  const kinds = {};
  return (kind, id) => {
    const s = text(id);
    if (s === null || s[0] === '…') return s;
    const k = kinds[kind] || (kinds[kind] = { of: new Map(), n: new Map() });
    if (!k.of.has(s)) {
      const base = mask(s);
      const n = (k.n.get(base) || 0) + 1;
      k.n.set(base, n);
      k.of.set(s, n === 1 ? base : `${base}~${n}`);
    }
    return k.of.get(s);
  };
}

/** Every printed line: no 24-hex id, no IPv4 or IPv6 address. */
function scrub(s) {
  return String(s)
    .replace(/\b[0-9a-fA-F]{24}\b/g, (x) => '…' + x.slice(-4).toLowerCase())
    .replace(/\b\d{1,3}(\.\d{1,3}){3}(:\d+)?(\/\d+)?\b/g, '<ip>')
    .replace(/(?:[0-9a-fA-F]{0,4}:){2,7}[0-9a-fA-F]{0,4}(?:\/\d+)?/g, (x) => (/[a-fA-F]|::/.test(x) ? '<ip>' : x));
}

// ---------------------------------------------------------------- render helpers

function dash(v) {
  return v === null || v === undefined || (typeof v === 'number' && !Number.isFinite(v)) ? '-' : String(v);
}

function mbps(bps) {
  return bps === null || bps === undefined || !Number.isFinite(bps) ? '-' : (bps / 1e6).toFixed(2) + ' Mbit/s';
}

function clock(ms) {
  return new Date(ms).toISOString().slice(11, 23);
}

function pct(a, b) {
  return b > 0 ? ((100 * a) / b).toFixed(1) + '%' : '-';
}

function secs(ms) {
  return ms === null || ms === undefined ? '-' : (ms / 1000).toFixed(0) + ' s';
}

/** A line's fields as `k=v`, ids masked, in the order stored. */
function kv(f, keys) {
  const out = [];
  for (const k of keys || Object.keys(f || {})) {
    if (!(k in (f || {})) || DENY_KEY.test(k)) continue;
    const v = ID_KEYS.has(k) ? mask(f[k]) : dash(text(f[k]));
    out.push(`${k}=${v}`);
  }
  return out.join(' ');
}

// ---------------------------------------------------------------- the fold

function newSession(sid, t) {
  return { sid, first: t, last: t, recs: [] };
}

/** The session a line without a session id belongs to, or null. */
function attribute(list, t) {
  let best = null;
  for (const s of list) if (s.first - OPEN_LAG_MS <= t && t <= s.last + CLOSE_LAG_MS) best = s;
  return best;
}

function rateOf(ivs) {
  let bytes = 0;
  let frames = 0;
  let ms = 0;
  for (const iv of ivs) {
    bytes += iv.bytes;
    frames += iv.frames;
    ms += iv.t1 - iv.t0;
  }
  return ms > 0 ? { bps: (bytes * 8000) / ms, fps: (frames * 1000) / ms, ms } : null;
}

/**
 * Rows ({t, m, f, fm}) of ONE agent -> sessions with their timelines and
 * summaries, and the device's summary. Rows may come in any order.
 */
function fold(rows, opts) {
  opts = opts || {};
  const recs = [];
  rows.forEach((r, i) => {
    const m = String((r && r.m) || '');
    const kind = kindOf(m);
    const t = epochMs(r && r.t);
    if (!kind || t === null) return;
    const f = Object.assign({}, r.f || {}, r.fm || {});
    recs.push({ i, t, kind, m, f, mem: !!r.fm });
  });
  recs.sort((a, b) => a.t - b.t || a.i - b.i);
  const sessions = new Map();
  for (const rec of recs) {
    rec.sid = text(rec.f.session_id) || text(rec.f.session);
    if (!rec.sid) continue;
    let s = sessions.get(rec.sid);
    if (!s) sessions.set(rec.sid, (s = newSession(rec.sid, rec.t)));
    s.first = Math.min(s.first, rec.t);
    s.last = Math.max(s.last, rec.t);
  }
  const list = Array.from(sessions.values()).sort((a, b) => a.first - b.first);
  const unattributed = {};
  for (const rec of recs) {
    const s = rec.sid ? sessions.get(rec.sid) : attribute(list, rec.t);
    if (s) s.recs.push(rec);
    else unattributed[rec.kind] = (unattributed[rec.kind] || 0) + 1;
  }
  const to = epochMs(opts.to);
  for (const s of list) finishSession(s, to);
  return { sessions: list, device: deviceOf(list, unattributed), truncated: !!opts.truncated, rows: rows.length, opts };
}

function finishSession(s, windowEnd) {
  const ev = [];
  const push = (t, what, line) => ev.push({ t, what, line });
  const hb = { n: 0, noCounters: 0, resets: 0, holes: 0, ivs: [], prev: null, firstT: null, lastT: null };
  let lastTarget = null;
  let targetMax = null;
  let targetMoves = 0;
  let lastPipeKey = null;
  const ka = { frames: 0, bytes: 0, carried: 0, missing: 0 };
  const cap = { judged: 0, at: 0, nominal: 0 };
  const enc = { opens: 0, qsv: 0, rejected: 0 };
  const del = { windows: 0, stalled: 0, longestMs: 0, stalledMs: 0 };
  const model = { n: 0, lastRx: null, maxRx: null, lastCap: null };
  let lastModelKey = null;
  const mem = { open: [], close: [], rate: [] };
  let carrier = null;
  let carrierAt = null;
  let rankOn = false;
  let rankSeen = false;
  let capTiming = 0;
  let openTarget = null;
  let pad = null;
  const pipes = {};
  for (const r of s.recs) {
    const f = r.f;
    switch (r.kind) {
      case 'heartbeat': {
        hb.n++;
        const bw = num(f.bytes_written);
        const fs = num(f.frames_sent);
        const target = num(f.target_bps);
        const maxrate = num(f.maxrate_bps);
        const fps = num(f.target_fps);
        if (openTarget === null && target !== null) openTarget = target;
        // AC13: what a keepalive cost before FR-148 P2, `maxrate ÷ fps ÷ 8`.
        if (maxrate !== null && fps) pad = maxrate / fps / 8;
        // The pipe classifier's verdict on the window this heartbeat closes.
        const ps = text(f.pipe_state);
        if (ps !== null) pipes[ps] = (pipes[ps] || 0) + 1;
        if (bw === null || fs === null) hb.noCounters++;
        else {
          if (hb.firstT === null) hb.firstT = r.t;
          hb.lastT = r.t;
          const p = hb.prev;
          if (p && (bw < p.bw || fs < p.fs)) {
            hb.resets++;
            push(r.t, 'reset', `counters reset (bytes_written ${p.bw} -> ${bw}): a new pump or a reopen; the interval across it is not rated`);
          } else if (p) {
            const gap = r.t - p.t;
            if (gap >= RATE_MIN_GAP_MS && gap <= RATE_MAX_GAP_MS) {
              const iv = { t0: p.t, t1: r.t, bytes: bw - p.bw, frames: fs - p.fs, target, maxrate, pipe: text(f.pipe_state) };
              hb.ivs.push(iv);
              if (target !== null && maxrate !== null) {
                cap.judged += gap;
                if (target === maxrate) cap.at += gap;
              }
              if (target !== null && target >= RELAY_NOMINAL_BPS) cap.nominal += gap;
            } else hb.holes++;
          }
          hb.prev = { t: r.t, bw, fs };
        }
        if (target !== null) {
          if (lastTarget !== null && target !== lastTarget) {
            targetMoves++;
            push(r.t, 'target', `target ${lastTarget} -> ${target} bps (reason: not logged)`);
          }
          lastTarget = target;
          targetMax = targetMax === null ? target : Math.max(targetMax, target);
        }
        const pipeKey = [text(f.pipe_state), num(f.readings_refuted), num(f.link_unoffered)].join('|');
        if (pipeKey !== lastPipeKey) {
          lastPipeKey = pipeKey;
          push(
            r.t,
            'pipe',
            `pipe_state=${dash(text(f.pipe_state))} pipe_measured=${dash(text(f.pipe_measured))} readings_refuted=${dash(num(f.readings_refuted))} link_unoffered=${dash(num(f.link_unoffered))}`,
          );
        }
        const kf = num(f.frames_keepalive);
        const kb = num(f.keepalive_bytes);
        if (kf === null && kb === null) ka.missing++;
        else {
          ka.carried++;
          ka.frames += kf || 0;
          ka.bytes += kb || 0;
        }
        break;
      }
      case 'carrier':
        carrier = text(f.carrier);
        if (carrierAt === null) carrierAt = r.t - s.first;
        push(r.t, 'carrier', kv(f, ['carrier', 'local_typ', 'remote_typ']));
        break;
      case 'ice':
        push(r.t, 'ice', kv(f, ['relay', 'local_typ', 'local_proto', 'remote_typ', 'remote_proto', 'waited_ms', 'opening']));
        break;
      case 'rank': {
        rankSeen = true;
        const cr = text(f.carrier_rank);
        if (cr !== null && !/^(off|0|false|none)$/i.test(cr)) rankOn = true;
        push(r.t, 'rank', kv(f, ['carrier_rank', 'link', 'link_at_offer', 'upgrading', 'stalled', 'viewers', 'viewers_from', 'ranked_last']));
        break;
      }
      case 'overlay':
        push(r.t, 'overlay', [r.m.slice(0, 120), kv(f, ['overlay_blocks'])].filter(Boolean).join(' '));
        break;
      case 'qsv':
        enc.qsv++;
        push(r.t, 'encoder', `QVBR open: ${kv(f, ['encoder', 'rate_control', 'fallback', 'bit_rate_bps', 'maxrate_bps'])}`);
        break;
      case 'encOpen':
        enc.opens++;
        // The message's own rest, "(constant-quality + maxrate cap)", has its parentheses.
        push(r.t, 'encoder', `${enc.opens > 1 ? 'REOPEN' : 'open'}: ${kv(f, ['encoder', 'width', 'height', 'fps', 'cq', 'maxrate_bps', 'input'])} ${r.m.replace(/^ffmpeg encoder opened /, '')}`);
        break;
      case 'rejected':
        enc.rejected++;
        push(r.t, 'encoder', `quality options rejected, retried with defaults: ${kv(f, ['encoder', 'open_err'])}`);
        break;
      case 'delivery': {
        del.windows++;
        const st = num(f.stalled_ms);
        if (st !== null && st > 0) {
          del.stalled++;
          del.stalledMs += st;
          del.longestMs = Math.max(del.longestMs, st);
          push(r.t, 'stall', kv(f, ['span_ms', 'delivery_samples', 'backlogged_bps', 'app_limited_bps', 'stalled_ms']));
        }
        break;
      }
      case 'model': {
        model.n++;
        const rx = shown(f.viewer_rx_bps);
        const mc = shown(f.model_capacity_bps);
        if (rx) model.lastRx = rx;
        if (rx && rx.bps !== undefined) model.maxRx = model.maxRx === null ? rx.bps : Math.max(model.maxRx, rx.bps);
        model.lastCap = mc;
        const key = [rx ? rx.basis || rx.raw : null, mc ? mc.basis || mc.raw : null].join('|');
        if (key !== lastModelKey) {
          lastModelKey = key;
          push(r.t, 'model', `viewer_rx_bps=${dash(text(f.viewer_rx_bps))} model_capacity_bps=${dash(text(f.model_capacity_bps))} model_floor_bps=${dash(text(f.model_floor_bps))}`);
        }
        break;
      }
      case 'memOpen':
        mem.open.push(f);
        push(r.t, 'memory', `open: ${kv(f)}`);
        break;
      case 'memClose':
        mem.close.push(f);
        push(r.t, 'memory', `close: ${kv(f)}`);
        break;
      case 'rateMem':
        mem.rate.push(f);
        push(r.t, 'memory', `rate memory, assumed: ${kv(f, ['assumed_bps', 'age_s'])}`);
        break;
      case 'capTiming':
        capTiming++;
        break;
    }
  }
  const whole = rateOf(hb.ivs);
  const after30 = hb.firstT === null ? null : rateOf(hb.ivs.filter((iv) => iv.t0 >= hb.firstT + 30000));
  const last60 = hb.lastT === null ? null : rateOf(hb.ivs.filter((iv) => iv.t0 >= hb.lastT - 60000));
  s.ev = ev;
  s.summary = {
    carrier,
    carrierAt,
    openTarget,
    pad,
    pipes,
    rankOn,
    rankSeen,
    heartbeats: hb.n,
    heartbeatsWithoutCounters: hb.noCounters,
    resets: hb.resets,
    holes: hb.holes,
    whole,
    after30,
    last60,
    targetMoves,
    targetMax,
    cap,
    ka,
    enc,
    del,
    model,
    mem,
    capTiming,
    openAtEnd: windowEnd !== null && windowEnd !== undefined && s.last >= windowEnd - OPEN_AT_END_MS,
  };
  s.ivs = hb.ivs;
}

function deviceOf(list, unattributed) {
  const d = {
    sessions: list.length,
    byCarrier: {},
    rankOn: 0,
    rankOnTurn: 0,
    windows: 0,
    stalledWindows: 0,
    longestStallMs: 0,
    closes: 0,
    closesBy: {},
    o7: {},
    // Capture-timing lines outside every session count here too.
    capTiming: unattributed.capTiming || 0,
    pipes: {},
    unattributed,
  };
  for (const k of O7_COUNTED.concat(O7_ASIDE)) d.o7[k] = { sum: 0, carried: 0, missing: 0 };
  for (const s of list) {
    const x = s.summary;
    for (const k of Object.keys(x.pipes)) d.pipes[k] = (d.pipes[k] || 0) + x.pipes[k];
    const c = x.carrier || 'unknown (no carrier line)';
    d.byCarrier[c] = (d.byCarrier[c] || 0) + 1;
    if (x.rankOn) {
      d.rankOn++;
      if (x.carrier === 'turn') d.rankOnTurn++;
    }
    d.windows += x.del.windows;
    d.stalledWindows += x.del.stalled;
    d.longestStallMs = Math.max(d.longestStallMs, x.del.longestMs);
    d.capTiming += x.capTiming;
    for (const f of x.mem.close) {
      d.closes++;
      const key = `wrote=${dash(text(f.wrote))} why=${dash(text(f.why))}`;
      d.closesBy[key] = (d.closesBy[key] || 0) + 1;
      for (const k of O7_COUNTED.concat(O7_ASIDE)) {
        const v = num(f[k]);
        if (v === null) d.o7[k].missing++;
        else {
          d.o7[k].sum += v;
          d.o7[k].carried++;
        }
      }
    }
  }
  return d;
}

// ---------------------------------------------------------------- render

/** Heartbeat windows by the pipe classifier's verdict; the stalled share is
 *  `transit-stalled` out of the windows it gave a verdict (not `unknown`). */
function pipeCells(pipes) {
  const keys = Object.keys(pipes);
  const judged = keys.filter((k) => k !== 'unknown').reduce((a, k) => a + pipes[k], 0);
  const stalled = pipes['transit-stalled'] || 0;
  return { list: keys.map((k) => `${k} ${pipes[k]}`).join(', ') || '-', share: `${stalled} of ${judged} (${pct(stalled, judged)})` };
}

function minuteRows(ivs) {
  const out = [];
  let cur = null;
  for (const iv of ivs) {
    const m = Math.floor(iv.t1 / 60000);
    if (!cur || cur.m !== m) {
      cur = { m, ivs: [], tmin: null, tmax: null, pipes: {} };
      out.push(cur);
    }
    cur.ivs.push(iv);
    if (iv.target !== null) {
      cur.tmin = cur.tmin === null ? iv.target : Math.min(cur.tmin, iv.target);
      cur.tmax = cur.tmax === null ? iv.target : Math.max(cur.tmax, iv.target);
    }
    if (iv.pipe) cur.pipes[iv.pipe] = (cur.pipes[iv.pipe] || 0) + 1;
  }
  return out.map((c) => {
    const r = rateOf(c.ivs);
    const pipes = Object.keys(c.pipes)
      .map((k) => `${k}×${c.pipes[k]}`)
      .join(',');
    // Dated at the end of its first interval, so it sorts among the minute's events.
    return { t: c.ivs[0].t1, line: `sent ${mbps(r && r.bps)} ${r ? r.fps.toFixed(1) : '-'} fps, target ${dash(c.tmin)}..${dash(c.tmax)} bps, pipe ${pipes || '-'}` };
  });
}

function render(r) {
  const o = r.opts || {};
  const L = [];
  const trunc = `TRUNCATED: the read stopped at ${o.limit || '?'} rows. Rows come in the order their batches were stored, so every batch stored after the last one read is missing, and some of its lines can be older than the last time printed. Narrow the window with --from/--to, or raise --limit (at most ${MAX_LIMIT}).`;
  // The range of storage times the $match read: the window the database was
  // read for (never a label given offline), padded by UPLOAD_PAD_MS.
  const stored = (t, d) => {
    const ms = epochMs(t);
    return ms === null ? '-' : new Date(ms + d).toISOString();
  };
  const rd = o.read || {};
  const pad = UPLOAD_PAD_MS / 60000;
  L.push(`# FR-148 path timeline: agent ${mask(o.agent)}, ${o.from ? new Date(epochMs(o.from)).toISOString() : '-'} -> ${o.to ? new Date(epochMs(o.to)).toISOString() : '-'}`);
  L.push(`batches read: stored ${stored(rd.from, -UPLOAD_PAD_MS)} -> ${stored(rd.to, UPLOAD_PAD_MS)} (the window ± ${pad} min). A line whose batch was stored outside that range, uploaded over ${pad} min late or stamped by a clock over ${pad} min fast, is not in this report.`);
  L.push(`rows read: ${r.rows}${r.truncated ? ' (TRUNCATED)' : ''}`);
  if (r.truncated) L.push(trunc);
  r.sessions.forEach((s, n) => {
    const x = s.summary;
    L.push('');
    L.push(`## session ${n + 1}: ${mask(s.sid)}, ${clock(s.first)} -> ${clock(s.last)}${x.openAtEnd ? ' (open at the window end)' : ''}, carrier ${dash(x.carrier)}`);
    L.push('');
    const rows = s.ev.slice();
    if (o.intervals) for (const iv of s.ivs) rows.push({ t: iv.t1, what: 'rate', line: `sent ${mbps((iv.bytes * 8000) / (iv.t1 - iv.t0))} ${((iv.frames * 1000) / (iv.t1 - iv.t0)).toFixed(1)} fps target ${dash(iv.target)} maxrate ${dash(iv.maxrate)} pipe ${dash(iv.pipe)}` });
    else for (const m of minuteRows(s.ivs)) rows.push({ t: m.t, what: 'minute', line: m.line });
    rows.sort((a, b) => a.t - b.t);
    for (const e of rows) L.push(`  ${clock(e.t)}  ${e.what.padEnd(8)} ${e.line}`.trimEnd());
    L.push('');
    L.push('| Session summary | |');
    L.push('|---|---|');
    L.push(`| start / end / duration | ${clock(s.first)} / ${clock(s.last)} / ${secs(s.last - s.first)} |`);
    L.push(`| carrier recorded, from the session's first line | ${x.carrierAt === null ? '-' : '+' + (x.carrierAt / 1000).toFixed(1) + ' s'} |`);
    L.push(`| opened at (the first heartbeat's target) | ${dash(x.openTarget)} |`);
    const pc = pipeCells(x.pipes);
    L.push(`| heartbeat windows by pipe state / transit-stalled share | ${pc.list} / ${pc.share} |`);
    L.push(`| sent: whole / after the first 30 s / last 60 s | ${mbps(x.whole && x.whole.bps)} / ${mbps(x.after30 && x.after30.bps)} / ${mbps(x.last60 && x.last60.bps)} |`);
    L.push(`| frame rate, whole | ${x.whole ? x.whole.fps.toFixed(1) + ' fps' : '-'} |`);
    L.push(`| target: highest / moves / time at the encoder's cap (target = maxrate_bps) / time at >= 3 Mbit/s | ${dash(x.targetMax)} / ${x.targetMoves} / ${pct(x.cap.at, x.cap.judged)} / ${pct(x.cap.nominal, x.whole ? x.whole.ms : 0)} |`);
    L.push(`| heartbeats / without counters / counter resets / holes | ${x.heartbeats} / ${x.heartbeatsWithoutCounters} / ${x.resets} / ${x.holes} |`);
    L.push(`| encoder: opens / QVBR opens / rejected quality options | ${x.enc.opens} / ${x.enc.qsv} / ${x.enc.rejected} |`);
    L.push(`| delivery: windows / stalled / share / longest stall | ${x.del.windows} / ${x.del.stalled} / ${pct(x.del.stalled, x.del.windows)} / ${x.del.windows ? x.del.longestMs + ' ms' : '-'} |`);
    const rx = x.model.lastRx;
    const mc = x.model.lastCap;
    L.push(`| path model: lines / last viewer_rx (basis, age) / highest viewer_rx / last model_capacity (basis) | ${x.model.n} / ${rx ? `${dash(rx.bps)} (${dash(rx.basis || rx.raw)}, ${dash(rx.ageMs)} ms)` : '-'} / ${dash(x.model.maxRx)} / ${mc ? `${dash(mc.bps)} (${dash(mc.basis || mc.raw)})` : '-'} |`);
    L.push(`| path memory: opens / closes / rate-memory lines | ${x.mem.open.length} / ${x.mem.close.length} / ${x.mem.rate.length} |`);
    L.push(`| keepalive: heartbeats carrying it / frames / bytes | ${x.ka.carried} of ${x.heartbeats} / ${x.ka.carried ? x.ka.frames : '-'} / ${x.ka.carried ? x.ka.bytes : '-'} |`);
    L.push(`| keepalive: bytes per frame / maxrate ÷ fps ÷ 8 at the last heartbeat | ${x.ka.frames ? Math.round(x.ka.bytes / x.ka.frames) : '-'} / ${x.pad === null ? '-' : Math.round(x.pad)} |`);
    L.push(`| capture-timing lines (no byte count in the code) | ${x.capTiming} |`);
    L.push(`| carrier rank: line seen / on | ${x.rankSeen ? 'yes' : 'no'} / ${x.rankOn ? 'yes' : 'no'} |`);
  });
  const d = r.device;
  L.push('');
  L.push('## device and window');
  L.push('');
  L.push('| Device summary | |');
  L.push('|---|---|');
  L.push(`| sessions | ${d.sessions} |`);
  L.push(`| by carrier | ${Object.keys(d.byCarrier).map((k) => `${k} ${d.byCarrier[k]}`).join(', ') || '-'} |`);
  L.push(`| carrier rank on, rode carrier="turn" (AC22) | ${d.rankOnTurn} of ${d.rankOn} |`);
  if (o.carrierRecord) {
    const cr = o.carrierRecord;
    const keys = Object.keys(cr).sort();
    L.push(`| by carrier, the server's record (remote_sessions created in the window) | ${keys.map((k) => `${k} ${cr[k]}`).join(', ') || 'none'} |`);
  }
  L.push(`| delivery windows stalled, all sessions | ${d.stalledWindows} of ${d.windows} (${pct(d.stalledWindows, d.windows)}), longest ${d.windows ? d.longestStallMs + ' ms' : '-'} |`);
  L.push(`| heartbeat windows transit-stalled, all sessions (AC22) | ${pipeCells(d.pipes).share} |`);
  L.push(`| path-memory closes by wrote / why | ${Object.keys(d.closesBy).map((k) => `${k}: ${d.closesBy[k]}`).join('; ') || '-'} |`);
  for (const k of O7_COUNTED.concat(O7_ASIDE)) {
    const v = d.o7[k];
    L.push(`| O7 ${k}: sum over the closes carrying it (closes without) | ${v.carried ? v.sum : '-'} (${v.missing}) |`);
  }
  L.push(`| capture-timing lines | ${d.capTiming} |`);
  const un = Object.keys(d.unattributed);
  L.push(`| lines in no session | ${un.map((k) => `${k} ${d.unattributed[k]}`).join(', ') || '0'} |`);
  if (r.truncated) {
    L.push('');
    L.push(trunc);
  }
  return L.map(scrub).join('\n');
}

// ---------------------------------------------------------------- the query

/**
 * The read for one agent and one window. `tenant` and `agent` are ObjectIds
 * (strings in the self-test); `from` and `to` are Dates.
 */
function timelinePipeline(o) {
  const project = {};
  for (const k of FIELDS) {
    const v = '$lines.fields.' + k;
    project[k] = NUMBER_ONLY.includes(k) ? { $cond: [{ $isNumber: v }, v, '$$REMOVE'] } : v;
  }
  return [
    {
      $match: {
        tenant_id: o.tenant,
        agent_id: o.agent,
        created_at: { $gte: new Date(o.from.getTime() - UPLOAD_PAD_MS), $lt: new Date(o.to.getTime() + UPLOAD_PAD_MS) },
      },
    },
    { $sort: { created_at: 1 } },
    {
      $project: {
        _id: 0,
        lines: {
          $filter: {
            input: '$lines',
            as: 'l',
            cond: {
              $and: [
                { $regexMatch: { input: { $ifNull: ['$$l.msg', ''] }, regex: MSG_PATTERN } },
                { $gte: ['$$l.ts', o.from] },
                { $lt: ['$$l.ts', o.to] },
              ],
            },
          },
        },
      },
    },
    { $unwind: '$lines' },
    {
      $project: {
        t: '$lines.ts',
        m: '$lines.msg',
        f: project,
        fm: {
          $cond: [
            { $regexMatch: { input: '$lines.msg', regex: MEM_PATTERN } },
            {
              $arrayToObject: {
                $filter: {
                  input: { $objectToArray: { $ifNull: ['$lines.fields', { $literal: {} }] } },
                  as: 'kv',
                  cond: { $not: [{ $regexMatch: { input: '$$kv.k', regex: DENY_KEY.source, options: 'i' } }] },
                },
              },
            },
            '$$REMOVE',
          ],
        },
      },
    },
    { $limit: o.limit + 1 },
  ];
}

const QUERY_OPTIONS = { maxTimeMS: 240000, allowDiskUse: false, hint: { tenant_id: 1, agent_id: 1, created_at: -1 } };

/**
 * AC22's cheap cross-check: the server's own record of each session's carrier
 * (`remote_sessions.carrier`, FR-102 P9c; absent is unknown, never direct),
 * counted over this agent's sessions created in the window. A device's day is
 * tens of rows, so one `$group` over them is small; the index is
 * `agent_id_1_created_at_-1` (crates/modules/remote/src/lib.rs). The rank is
 * not in that record: "rank on" comes from the agent's lines only.
 */
function carrierPipeline(o) {
  return [
    { $match: { agent_id: o.agent, tenant_id: o.tenant, created_at: { $gte: o.from, $lt: o.to } } },
    { $group: { _id: { $ifNull: ['$carrier', 'absent'] }, n: { $sum: 1 } } },
  ];
}

const CARRIER_OPTIONS = { maxTimeMS: 30000, allowDiskUse: false, hint: { agent_id: 1, created_at: -1 } };

/**
 * One row as `--rows` prints it and as the direct mode folds it, so that the
 * offline re-read renders byte for byte as the direct read: ids masked
 * through `ids` (one `masks()` per read), every key DENY_KEY names left out,
 * every string scrubbed as a printed line is.
 */
function rowOut(row, ids) {
  const idOf = ids || masks();
  const plain = (o) => {
    if (!o) return undefined;
    const f = {};
    for (const k of Object.keys(o)) {
      if (DENY_KEY.test(k)) continue;
      let v = o[k];
      if (v !== null && typeof v === 'object' && typeof v.toNumber === 'function') v = v.toNumber();
      if (ID_KEYS.has(k)) v = idOf(ID_KEYS.get(k), v);
      else if (typeof v === 'string') v = scrub(v);
      f[k] = v;
    }
    return f;
  };
  const out = { t: row.t instanceof Date ? row.t.toISOString() : row.t, m: typeof row.m === 'string' ? scrub(row.m) : row.m, f: plain(row.f) };
  if (row.fm) out.fm = plain(row.fm);
  return out;
}

/** The `--rows` text's first lines: the read's window, which the offline
 *  reader takes (the agent as its last 4 hex, and the limit), and the
 *  server's record. */
function rowsHead(s, carrierRecord) {
  return [
    `# window: ${JSON.stringify({ from: s.from.toISOString(), to: s.to.toISOString(), agent4: s.agent.slice(-4), limit: s.limit })}`,
    `# carrier record: ${JSON.stringify(carrierRecord)}`,
  ];
}

/** The `--rows` text's last line, after `n` rows came back for `limit`. */
function rowsTail(n, limit) {
  const cut = ` TRUNCATED at ${limit}: every batch stored after the last one read is missing, and some of its lines can be older than the last row's time; narrow --from/--to or raise --limit`;
  return `# rows: ${Math.min(n, limit)}${n > limit ? cut : ''}`;
}

/** What the direct read renders with. */
function readOpts(s, truncated, carrierRecord) {
  return { truncated, limit: s.limit, agent: s.agent, from: s.from, to: s.to, carrierRecord, read: { from: s.from, to: s.to } };
}

/** A `--rows` text back as rows, with what its comment lines say. */
function parseRows(textIn) {
  const out = { rows: [], truncated: false, carrierRecord: null, window: null };
  for (const line of String(textIn).split('\n')) {
    if (/^# .*TRUNCATED/.test(line)) out.truncated = true;
    const cr = /^# carrier record: (\{.*\})\s*$/.exec(line);
    if (cr) {
      try {
        out.carrierRecord = JSON.parse(cr[1]);
      } catch (_) {
        /* not a record */
      }
    }
    const win = /^# window: (\{.*\})\s*$/.exec(line);
    if (win && !out.window) {
      try {
        out.window = JSON.parse(win[1]);
      } catch (_) {
        /* not a window */
      }
    }
    if (line[0] !== '{') continue;
    try {
      out.rows.push(JSON.parse(line));
    } catch (_) {
      /* not a row */
    }
  }
  return out;
}

/**
 * `--rows` texts rendered as the direct read renders them. A window given
 * here (`cli`) wins over the file's as the report's label; the range read is
 * always the file's.
 */
function renderRows(texts, cli) {
  const o = cli || {};
  const rows = [];
  let truncated = false;
  let carrierRecord = null;
  let w = null;
  for (const t of texts) {
    const p = parseRows(t);
    for (const x of p.rows) rows.push(x);
    truncated = truncated || p.truncated;
    if (p.carrierRecord) carrierRecord = p.carrierRecord;
    if (!w && p.window) w = p.window;
  }
  w = w || {};
  const read = w.from && w.to ? { from: w.from, to: w.to } : null;
  return render(fold(rows, { truncated, intervals: !!o.intervals, agent: o.agent || w.agent4, from: o.from || w.from, to: o.to || w.to, carrierRecord, limit: w.limit, read }));
}

function planStages(plan) {
  const out = [];
  const walk = (o) => {
    if (!o || typeof o !== 'object') return;
    if (typeof o.stage === 'string') out.push(o.stage + (o.indexName ? `(${o.indexName})` : ''));
    for (const k of Object.keys(o)) if (k !== 'rejectedPlans') walk(o[k]);
  };
  walk(plan);
  return out;
}

/** The window and the limit from the environment the wrapper sets. */
function readSettings(env) {
  const hex = /^[0-9a-fA-F]{24}$/;
  if (!hex.test(env.RC_PATH_TENANT || '')) throw new Error('the timeline reads one org: --tenant HEX');
  const agents = String(env.RC_PATH_AGENTS || '')
    .split(',')
    .filter(Boolean);
  if (agents.length !== 1 || !hex.test(agents[0])) throw new Error('the timeline reads ONE agent: --agent HEX, once');
  const since = env.RC_PATH_SINCE || '';
  const until = env.RC_PATH_UNTIL || '';
  if (!ISO_TIME.test(since) || !ISO_TIME.test(until)) throw new Error('--from and --to take YYYY-MM-DD, or YYYY-MM-DDTHH:MM[:SS[.sss]] with Z or +HH:MM');
  const from = new Date(since);
  const to = new Date(until);
  if (isNaN(from.getTime()) || isNaN(to.getTime()) || from >= to) throw new Error('--from must be before --to (ISO 8601)');
  if (to.getTime() - from.getTime() > MAX_WINDOW_MS) throw new Error('the window is over 24 h: split it with --from/--to');
  const limit = env.RC_PATH_LIMIT ? Number(env.RC_PATH_LIMIT) : DEFAULT_LIMIT;
  if (!Number.isInteger(limit) || limit < 1 || limit > MAX_LIMIT) throw new Error(`--limit takes 1 to ${MAX_LIMIT}`);
  return { tenant: env.RC_PATH_TENANT, agent: agents[0], from, to, limit };
}

function connectDb(dbName, env) {
  if (env.RC_PATH_URI) return new Mongo(env.RC_PATH_URI).getDB(dbName);
  const u = env.MONGO_INITDB_ROOT_USERNAME;
  const p = env.MONGO_INITDB_ROOT_PASSWORD;
  if (u && p) return new Mongo('mongodb://' + encodeURIComponent(u) + ':' + encodeURIComponent(p) + '@127.0.0.1:27017/?authSource=admin').getDB(dbName);
  if (typeof db !== 'undefined' && db) return db.getSiblingDB(dbName);
  throw new Error('no connection: run through scripts/rc-path-report.sh --timeline');
}

function runMongosh() {
  const env = typeof process !== 'undefined' && process.env ? process.env : {};
  const s = readSettings(env);
  const emitRows = env.RC_PATH_EMIT === 'rows';
  const database = connectDb(env.RC_PATH_DB || 'roomler2', env);
  const coll = database.getCollection('agent_logs');
  const ids = { tenant: ObjectId(s.tenant), agent: ObjectId(s.agent), from: s.from, to: s.to, limit: s.limit };
  const pipeline = timelinePipeline(ids);
  const sessions = database.getCollection('remote_sessions');
  if (env.RC_PATH_EXPLAIN === '1') {
    const stages = planStages(coll.explain('queryPlanner').aggregate(pipeline, QUERY_OPTIONS));
    print(`plan: ${stages.join(' <- ')}`);
    print(`blocking SORT stage: ${stages.some((x) => /^SORT\b/.test(x)) ? 'YES' : 'none'}`);
    print(`carrier record plan: ${planStages(sessions.explain('queryPlanner').aggregate(carrierPipeline(ids), CARRIER_OPTIONS)).join(' <- ')}`);
    return;
  }
  // The server's record first: tens of rows, and it never depends on the
  // agent's lines having been uploaded.
  const carrierRecord = {};
  sessions.aggregate(carrierPipeline(ids), CARRIER_OPTIONS).forEach((g) => {
    carrierRecord[String(g._id)] = g.n;
  });
  // The offline reader takes the window from here, so a session still open
  // at its end is told apart there too.
  if (emitRows) for (const l of rowsHead(s, carrierRecord)) print(l);
  const rows = [];
  const idMasks = masks();
  let n = 0;
  coll.aggregate(pipeline, QUERY_OPTIONS).forEach((row) => {
    n++;
    if (n > s.limit) return;
    // Both modes take each row as --rows prints it, masked in the read's order.
    const out = rowOut(row, idMasks);
    if (emitRows) print(JSON.stringify(out));
    else rows.push(out);
  });
  const truncated = n > s.limit;
  if (emitRows) {
    print(rowsTail(n, s.limit));
    return;
  }
  print(render(fold(rows, readOpts(s, truncated, carrierRecord))));
}

// ---------------------------------------------------------------- self-test

const FX = { tenant: 'f148000000000000000000a1', agent: 'f1480000000000000000a018', s1: 'f14800000000000000005e51', s2: 'f14800000000000000005e52' };

function at(sec) {
  return new Date(Date.UTC(2026, 10, 18, 7, 0, 0) + sec * 1000).toISOString();
}

function hbRow(sec, sid, o) {
  return {
    t: at(sec),
    m: 'FFmpeg DC pump heartbeat (≈2s window)',
    f: Object.assign(
      {
        session_id: sid,
        codec_label: 'HEVC',
        encoder: 'hevc_qsv',
        constrained: true,
        target_bps: 3000000,
        target_fps: 30,
        width: 1920,
        height: 1080,
        maxrate_bps: 'Some(3000000)',
        pipe_state: 'Some("clear")',
        pipe_measured: 'None',
        readings_refuted: 0,
        link_unoffered: 0,
        frames_keepalive: 'Some(1)',
        keepalive_bytes: 'Some(900)',
      },
      o,
    ),
  };
}

function selfTest() {
  let fails = 0;
  let cases = 0;
  const check = (name, ok, detail) => {
    cases++;
    if (!ok) {
      fails++;
      console.log(`FAIL ${name}${detail ? ': ' + detail : ''}`);
    }
  };
  // 1. Every message literal is in the agent's source, at the file named.
  try {
    const fs = require('fs');
    const path = require('path');
    const src = path.join(__dirname, '..', 'agents', 'roomlerd', 'src');
    for (const d of KINDS) {
      const body = fs.readFileSync(path.join(src, d.src[0]), 'utf8');
      for (const lit of d.src.slice(1)) {
        check(`source: ${d.k} literal in ${d.src[0]}`, body.includes(lit), lit);
        check(`source: ${d.k} pattern matches its literal`, d.re.test(lit), lit);
      }
    }
    const pm = fs.readFileSync(path.join(src, 'media_pair_memory.rs'), 'utf8');
    for (const k of O7_COUNTED.concat(O7_ASIDE, ['taken', 'taken_why', 'wrote', 'why'])) check(`source: close/open field ${k}`, pm.includes(`"${k}"`));
  } catch (e) {
    check('source files readable', false, e.message);
  }
  // 2. The fixtures: two sessions back to back, the second open at the end.
  const rows = [];
  rows.push({ t: at(0), m: 'rc ICE: carrier rank (FR-148 P4)', f: { session: FX.s1, carrier_rank: 'prio', link: 'relay', upgrading: false, stalled: false, viewers: 1, viewers_from: 'relay-urls', ranked_last: 'host,srflx,relay' } });
  rows.push({ t: at(0.1), m: 'rc ICE: overlay allowed (device policy rc_overlay=on)', f: { session: FX.s1, overlay_blocks: 1 } });
  rows.push({ t: at(1), m: 'per-session ICE path detected (adaptive bitrate)', f: { session_id: FX.s1, relay: true, local_typ: 'Relay', local_proto: 'Udp', remote_typ: 'Relay', remote_proto: 'Udp', waited_ms: 1200, opening: 'moved' } });
  rows.push({ t: at(1.2), m: "FR-148 P1 rate memory: the pair's entry has no measured provenance, so it is read as assumed", f: { assumed_bps: 3500000, age_s: 160000 } });
  rows.push({ t: at(2), m: 'session carrier recorded (FR-102 P9c)', f: { session_id: FX.s1, carrier: 'turn', local_typ: 'Relay', remote_typ: 'Srflx' } });
  rows.push({
    t: at(2.5),
    m: "FR-148 P1 path memory: the pair's entry at the open",
    f: { session_id: FX.s1 },
    fm: { session_id: FX.s1, codec_label: 'HEVC', mode: 'shadow', key_id: 'f148f148a001', key_carrier: 'turn', key_net: 'srflx', entry: 'none', model_trusted: false, nominal_bps: 3000000, taken: 'today', taken_why: 'shadow_mode' },
  });
  rows.push({ t: at(2.6), m: 'ffmpeg open_as_with rejected the quality options — retrying with encoder defaults', f: { encoder: 'hevc_nvenc', open_err: 'Operation not permitted' } });
  rows.push({ t: at(3), m: 'FR-148: the QSV encoder opened at QVBR, its target under its cap', f: { encoder: 'hevc_qsv', rate_control: 'qvbr', fallback: false, bit_rate_bps: 2700000, maxrate_bps: 3000000 } });
  rows.push({ t: at(3), m: 'ffmpeg encoder opened (constant-quality + maxrate cap)', f: { encoder: 'hevc_qsv', width: 1920, height: 1080, fps: 30, cq: 22, maxrate_bps: 3000000, input: 'cpu' } });
  // Heartbeats every 2 s from 4 s to 100 s: 250 kB/s cumulative = 2 Mbit/s, 25 fps.
  for (let sec = 4, i = 0; sec <= 100; sec += 2, i++) {
    const o = { bytes_written: 500000 * i, frames_sent: 50 * i };
    if (sec >= 40) o.target_bps = 2000000;
    if (sec >= 40) o.maxrate_bps = 'Some(2000000)';
    if (sec >= 60 && sec < 70) o.pipe_state = 'Some("transit-stalled")';
    if (sec === 50) {
      // the reopen: counters start again
      o.bytes_written = 1000;
      o.frames_sent = 1;
    }
    if (sec > 50) {
      o.bytes_written = 1000 + 500000 * (i - 23);
      o.frames_sent = 1 + 50 * (i - 23);
    }
    rows.push(hbRow(sec, FX.s1, o));
  }
  rows.push({ t: at(50), m: 'ffmpeg encoder opened (constant-quality + maxrate cap)', f: { encoder: 'hevc_qsv', width: 1920, height: 1080, fps: 30, cq: 22, maxrate_bps: 2000000, input: 'cpu' } });
  // A heartbeat whose media window is off: no keepalive fields, never NaN.
  rows.push(hbRow(102, FX.s1, { bytes_written: 1000 + 500000 * 26, frames_sent: 1 + 50 * 26, target_bps: 2000000, maxrate_bps: 'Some(2000000)', frames_keepalive: 'None', keepalive_bytes: 'None' }));
  rows.push({ t: at(30), m: 'system-context capture: worker-side scrap frame() timing (compare to pump heartbeat avg_capture_ms — the diff is the per-frame async round-trip)', f: { worker_avg_capture_ms: 4.9, calls: 300 } });
  rows.push({ t: at(20), m: 'path delivery (FR-148 P0, shadow, ≈10s)', f: { session_id: FX.s1, span_ms: 10000, delivery_samples: '(5, 0, 0)', backlogged_bps: 'Some((1, 2, 3))', app_limited_bps: 'None', stalled_ms: 0 } });
  rows.push({ t: at(30), m: 'path delivery (FR-148 P0, shadow, ≈10s)', f: { session_id: FX.s1, span_ms: 10000, delivery_samples: '(3, 0, 2)', backlogged_bps: 'None', app_limited_bps: 'None', stalled_ms: 4000 } });
  rows.push({ t: at(40), m: 'path delivery (FR-148 P0, shadow, ≈10s)', f: { session_id: FX.s1, span_ms: 10000, delivery_samples: '(4, 0, 1)', backlogged_bps: 'None', app_limited_bps: 'None', stalled_ms: 1500 } });
  rows.push({ t: at(20), m: 'path model (FR-148 P0, shadow, ≈10s)', f: { session_id: FX.s1, viewer_rx_bps: '33000/app-limited/200ms', model_capacity_bps: 'None', model_floor_bps: '1000000/app-limited/110000ms' } });
  rows.push({ t: at(30), m: 'path model (FR-148 P0, shadow, ≈10s)', f: { session_id: FX.s1, viewer_rx_bps: '2500000/backlogged/150ms', model_capacity_bps: '2600000/pushed-back/90ms', model_floor_bps: 'None' } });
  // The close, the NEW generation, with an unknown field and an unknown why.
  rows.push({
    t: at(103),
    m: 'FR-148 P1 path memory: what the close wrote',
    f: { session_id: FX.s1 },
    fm: { session_id: FX.s1, key_id: 'f148f148a001', key_carrier: 'turn', key_net: 'srflx', wrote: 'floor', why: 'a_reason_from_the_future', floor_bps: 'Some(300000)', samples_counted: 7, samples_counted_overproduced: 2, samples_counted_clear: 5, samples_not_counted: 1, samples_unread: 0, samples_viewer_late: 3, samples_under_plan: 4, record_ended: true, frozen: false, brand_new_field: 'x42' },
  });
  // Session 2, back to back, with the OLD (0.4.225) close; open at the window end.
  rows.push({ t: at(104), m: 'rc ICE: carrier rank (FR-148 P4)', f: { session: FX.s2, carrier_rank: 'prio', link: 'direct' } });
  rows.push({ t: at(105), m: 'session carrier recorded (FR-102 P9c)', f: { session_id: FX.s2, carrier: 'direct', local_typ: 'Host', remote_typ: 'Srflx' } });
  for (let sec = 106, i = 0; sec <= 200; sec += 2, i++) rows.push(hbRow(sec, FX.s2, { bytes_written: 1000000 * i, frames_sent: 60 * i, target_bps: 6000000, maxrate_bps: 'Some(6000000)' }));
  rows.push({ t: at(150), m: 'FR-148 P1 path memory: what the close wrote', f: { session_id: FX.s2 }, fm: { session_id: FX.s2, key_id: 'f148f148a002', wrote: 'nothing', why: 'not_constrained', samples_not_counted: 2, record_ended: false, frozen: false } });
  // Out of order: reverse the whole set.
  rows.reverse();
  const r = fold(rows, { to: at(210), from: at(0), agent: FX.agent, limit: 1000 });
  const s1 = r.sessions[0];
  const s2 = r.sessions[1];
  check('two sessions, in order', r.sessions.length === 2 && s1.sid === FX.s1 && s2.sid === FX.s2, String(r.sessions.length));
  check('carrier per session', s1.summary.carrier === 'turn' && s2.summary.carrier === 'direct');
  check('rank on, rode turn: 1 of 2', r.device.rankOn === 2 && r.device.rankOnTurn === 1);
  check('timeline in time order', s1.ev.every((e, i) => i === 0 || s1.ev[i - 1].t <= e.t));
  check('the reset is seen, not rated', s1.summary.resets === 1, String(s1.summary.resets));
  const w = s1.summary.whole;
  check('cumulative counters -> 2 Mbit/s', w && Math.abs(w.bps - 2000000) < 1, w && String(w.bps));
  check('cumulative counters -> 25 fps', w && Math.abs(w.fps - 25) < 0.01, w && String(w.fps));
  check('after 30 s and last 60 s rated', s1.summary.after30 && s1.summary.last60 && Math.abs(s1.summary.last60.bps - 2000000) < 1);
  check('session 2 at 4 Mbit/s', s2.summary.whole && Math.abs(s2.summary.whole.bps - 4000000) < 1);
  check('a target move with its reason unlogged', s1.ev.some((e) => e.what === 'target' && /3000000 -> 2000000 bps \(reason: not logged\)/.test(e.line)));
  check('highest target', s1.summary.targetMax === 3000000 && s2.summary.targetMax === 6000000);
  check('time at the cap: all of it', s1.summary.cap.judged > 0 && s1.summary.cap.at === s1.summary.cap.judged);
  check('pipe state changes in order', s1.ev.filter((e) => e.what === 'pipe').length === 3);
  check('the reopen printed as REOPEN', s1.ev.some((e) => e.what === 'encoder' && /^REOPEN/.test(e.line)) && s1.summary.enc.opens === 2);
  check('QVBR and the rejected options', s1.summary.enc.qsv === 1 && s1.summary.enc.rejected === 1);
  check('the rate-memory line goes to its session by time', s1.summary.mem.rate.length === 1);
  check('delivery windows, stalls, longest', s1.summary.del.windows === 3 && s1.summary.del.stalled === 2 && s1.summary.del.longestMs === 4000);
  check('the model line: last viewer_rx with basis and age', s1.summary.model.lastRx.bps === 2500000 && s1.summary.model.lastRx.basis === 'backlogged' && s1.summary.model.lastRx.ageMs === 150);
  check('keepalive: summed where carried, the rest counted', s1.summary.ka.carried === 49 && s1.summary.ka.missing === 1 && s1.summary.ka.bytes === 49 * 900);
  check('capture-timing line counted, not a heartbeat', s1.summary.capTiming === 1 && s1.summary.heartbeats === 50);
  check('session 2 open at the window end', s2.summary.openAtEnd === true && s1.summary.openAtEnd === false);
  check('opened at the first target; carrier at +2 s', s1.summary.openTarget === 3000000 && s1.summary.carrierAt === 2000);
  check('pipe windows: 5 transit-stalled of 50', s1.summary.pipes['transit-stalled'] === 5 && s1.summary.pipes.clear === 45, JSON.stringify(s1.summary.pipes));
  check('AC22 share over the device: 5 of 98', /heartbeat windows transit-stalled, all sessions \(AC22\) \| 5 of 98 \(5\.1%\)/.test(render(r)));
  check('AC13: 900 bytes a keepalive beside maxrate ÷ fps ÷ 8', s1.summary.ka.bytes / s1.summary.ka.frames === 900 && Math.round(s1.summary.pad) === 8333);
  check('O7: new close summed, old close counted as without', r.device.o7.samples_viewer_late.sum === 3 && r.device.o7.samples_viewer_late.missing === 1 && r.device.o7.samples_not_counted.sum === 3 && r.device.o7.samples_not_counted.missing === 0);
  check('closes by wrote/why, unknown why kept', r.device.closesBy['wrote=floor why=a_reason_from_the_future'] === 1 && r.device.closesBy['wrote=nothing why=not_constrained'] === 1);
  const out = render(r);
  check('unknown field printed as it is', /brand_new_field=x42/.test(out));
  check('taken and taken_why printed', /taken=today/.test(out) && /taken_why=shadow_mode/.test(out));
  check('no undefined or NaN anywhere', !/undefined|NaN/.test(out), (out.match(/.{0,40}(undefined|NaN).{0,40}/) || [''])[0]);
  check('an absent close field prints -', /O7 samples_unread: sum over the closes carrying it \(closes without\) \| 0 \(1\)/.test(out));
  const digits = '123456789012345678901234';
  const lone = fold(
    [
      { t: at(0), m: 'session carrier recorded (FR-102 P9c)', f: { session_id: digits, carrier: 'direct' } },
      { t: at(500), m: 'system-context capture: worker-side scrap frame() timing (compare to pump heartbeat avg_capture_ms)', f: { worker_avg_capture_ms: 5, calls: 300 } },
    ],
    { to: at(600) },
  );
  check('an all-digit session id keeps every digit', lone.sessions.length === 1 && lone.sessions[0].sid === digits, lone.sessions[0] && lone.sessions[0].sid);
  check('a capture-timing line in no session counts in the device total', lone.device.capTiming === 1 && lone.device.unattributed.capTiming === 1);
  // 3. Masking: a full id and an address in a field print neither.
  const leak = fold(
    [
      { t: at(0), m: 'session carrier recorded (FR-102 P9c)', f: { session_id: FX.s1, carrier: 'turn', local_typ: 'Relay' } },
      { t: at(1), m: 'FR-148 P1 path memory: what the close wrote', f: { session_id: FX.s1 }, fm: { session_id: FX.s1, peer: '198.51.100.7|turn', note: 'via 198.51.100.7:3478 and 2001:db8::7', wrote: 'floor', why: 'x' } },
    ],
    { to: at(60), from: at(0), agent: FX.agent, limit: 10 },
  );
  const lo = render(leak);
  check('masking: no full id', !lo.includes(FX.s1) && !lo.includes(FX.agent) && lo.includes('…5e51'));
  check('masking: no address', !/198\.51\.100\.7|2001:db8/.test(lo) && !/peer=/.test(lo), lo);
  check('masking: clock times survive the scrub', scrub('07:00:01.000') === '07:00:01.000');
  // 4. TRUNCATED, at the top and the bottom.
  const tr = render(fold(rows.slice(0, 5), { truncated: true, limit: 5, to: at(210), from: at(0), agent: FX.agent }));
  check('TRUNCATED marker printed twice', (tr.match(/TRUNCATED/g) || []).length >= 3);
  // 5. The query's shape.
  const p = timelinePipeline({ tenant: FX.tenant, agent: FX.agent, from: new Date(at(0)), to: new Date(at(3600)), limit: 7 });
  const stages = p.map((x) => Object.keys(x)[0]);
  check('pipeline stages', stages.join(',') === '$match,$sort,$project,$unwind,$project,$limit', stages.join(','));
  check('no $group, limit + 1', !JSON.stringify(p).includes('$group') && p[p.length - 1].$limit === 8);
  check('match on tenant, agent, created_at', p[0].$match.tenant_id === FX.tenant && p[0].$match.agent_id === FX.agent && p[0].$match.created_at.$gte instanceof Date);
  const cond = p[2].$project.lines.$filter.cond.$and;
  check('ts compared as a Date', cond[1].$gte[1] instanceof Date && cond[2].$lt[1] instanceof Date);
  check('options: hint, maxTimeMS, no disk', QUERY_OPTIONS.allowDiskUse === false && QUERY_OPTIONS.maxTimeMS > 0 && QUERY_OPTIONS.hint.agent_id === 1);
  const cp = carrierPipeline({ tenant: FX.tenant, agent: FX.agent, from: new Date(at(0)), to: new Date(at(3600)) });
  check('carrier record: $match then one $group', cp.map((x) => Object.keys(x)[0]).join(',') === '$match,$group' && cp[0].$match.agent_id === FX.agent && cp[0].$match.created_at.$lt instanceof Date);
  check('carrier record options: its index, no disk', CARRIER_OPTIONS.hint.agent_id === 1 && CARRIER_OPTIONS.hint.created_at === -1 && CARRIER_OPTIONS.allowDiskUse === false && CARRIER_OPTIONS.maxTimeMS > 0);
  check('carrier record printed', /the server's record \(remote_sessions created in the window\) \| absent 1, turn 2 \|/.test(render(fold(rows, { to: at(210), carrierRecord: { turn: 2, absent: 1 } }))));
  const ro = rowOut({ t: new Date(at(5)), m: 'FR-148 P1 path memory: what the close wrote', f: { session_id: FX.s1, peer: '198.51.100.7|turn' }, fm: { local_addr: '198.51.100.7:1', wrote: 'floor' } });
  check('rowOut: ts as ISO, no address key in f or fm', ro.t === at(5) && !('peer' in ro.f) && !('local_addr' in ro.fm) && ro.fm.wrote === 'floor');
  check('the capture-timing line is no heartbeat', kindOf('system-context capture: worker-side scrap frame() timing (compare to pump heartbeat avg_capture_ms)') === 'capTiming');
  check('no address key in the named fields', FIELDS.every((k) => !DENY_KEY.test(k)));
  let threw = 0;
  for (const bad of [{}, { RC_PATH_TENANT: FX.tenant, RC_PATH_AGENTS: FX.agent + ',' + FX.agent, RC_PATH_SINCE: at(0), RC_PATH_UNTIL: at(10) }, { RC_PATH_TENANT: FX.tenant, RC_PATH_AGENTS: FX.agent, RC_PATH_SINCE: at(0), RC_PATH_UNTIL: at(90000) }]) {
    try {
      readSettings(bad);
    } catch (_) {
      threw++;
    }
  }
  check('settings refuse no org, two agents, a window over 24 h', threw === 3);
  const fv = p[4].$project.f;
  check(
    'viewers leaves only as a number, every other named field as stored',
    JSON.stringify(fv.viewers) === JSON.stringify({ $cond: [{ $isNumber: '$lines.fields.viewers' }, '$lines.fields.viewers', '$$REMOVE'] }) && FIELDS.every((k) => k === 'viewers' || fv[k] === '$lines.fields.' + k),
    JSON.stringify(fv.viewers),
  );
  // 6. --rows: one read's rows, masked as the report masks them. A third
  //    session whose id ends as session 1's does, its key as session 1's key,
  //    and the overlay-auto line's addresses as an older read sent them.
  const s3 = 'f14800000000000000015e51';
  const more = [
    { t: at(0.05), m: 'rc ICE: overlay auto (FR-128) — the session may ride the mesh', f: { session: FX.s1, viewers: '[192.0.2.10, 2001:db8::10]' } },
    { t: at(300), m: 'session carrier recorded (FR-102 P9c)', f: { session_id: s3, carrier: 'turn', local_typ: 'Relay', remote_typ: 'Relay' } },
    {
      t: at(301),
      m: "FR-148 P1 path memory: the pair's entry at the open",
      f: { session_id: s3 },
      fm: { session_id: s3, key_id: 'f148f149a001', key_carrier: 'turn', entry: 'none', taken: 'today', taken_why: 'shadow_mode', note: 'via 198.51.100.7:3478 and 2001:db8::7' },
    },
    hbRow(302, s3, { bytes_written: 0, frames_sent: 0 }),
    hbRow(304, s3, { bytes_written: 500000, frames_sent: 50 }),
  ];
  const readIn = rows.concat(more);
  const set = { tenant: FX.tenant, agent: FX.agent, from: new Date(at(0)), to: new Date(at(400)), limit: readIn.length };
  const ids = masks();
  const printed = readIn.map((x) => rowOut(x, ids));
  const rowsText = (lim) =>
    rowsHead(Object.assign({}, set, { limit: lim }), { turn: 2 })
      .concat(
        printed.slice(0, lim).map((x) => JSON.stringify(x)),
        [rowsTail(Math.min(printed.length, lim + 1), lim)],
      )
      .join('\n');
  const full = rowsText(readIn.length);
  const body = full
    .split('\n')
    .filter((l) => l[0] === '{')
    .join('\n');
  const v6 = (s) => (s.match(/(?:[0-9a-fA-F]{0,4}:){2,7}[0-9a-fA-F]{0,4}/g) || []).filter((x) => /[a-fA-F]|::/.test(x));
  check('--rows: no 24-hex run, no raw key id', !/[0-9a-fA-F]{24}/.test(full) && !/f148f148a00[12]|f148f149a001/.test(full), (full.match(/.{0,30}[0-9a-fA-F]{24}.{0,10}/) || [''])[0]);
  check('--rows: no dotted or colon address', !/\b\d{1,3}(\.\d{1,3}){3}\b/.test(body) && v6(body).length === 0 && !/192\.0\.2\.|198\.51\.100\.|2001:db8/i.test(full), JSON.stringify(v6(body)));
  check('--rows: ids and key ids as the report masks them', /"session":"…5e51"/.test(body) && /"session_id":"…5e51~2"/.test(body) && /"key_id":"…a001"/.test(body) && /"key_id":"…a001~2"/.test(body));
  const direct = render(fold(printed, readOpts(set, false, { turn: 2 })));
  const offline = renderRows([full], {});
  const firstDiff = (a, b) => {
    const x = a.split('\n');
    const y = b.split('\n');
    const i = x.findIndex((l, j) => l !== y[j]);
    return i < 0 ? `lengths ${x.length} / ${y.length}` : `line ${i + 1}: ${x[i]} | ${y[i]}`;
  };
  check('the masked --rows, read back offline, renders byte for byte as the direct read', offline === direct && !/[0-9a-fA-F]{24}/.test(full), firstDiff(direct, offline));
  const sids = fold(printed, {}).sessions.map((x) => x.sid);
  check('two ids with one last 4 hex stay two sessions (…5e51, …5e51~2)', sids.join(',') === '…5e51,…5e52,…5e51~2' && /^## session 3: …5e51~2, /m.test(direct) && /^## session 3: …5e51~2, /m.test(offline), sids.join(','));
  const cut = 40;
  const directCut = render(fold(printed.slice(0, cut), readOpts(Object.assign({}, set, { limit: cut }), true, { turn: 2 })));
  check('a TRUNCATED --rows read back offline renders as the direct read, its limit too', renderRows([rowsText(cut)], {}) === directCut && /stopped at 40 rows/.test(directCut), firstDiff(directCut, renderRows([rowsText(cut)], {})));
  check('mask: an id already masked prints as it is', mask('…5e51~2') === '…5e51~2' && mask(FX.s1) === '…5e51' && mask(null) === '-');
  check('the header prints the range of storage times read', /^batches read: stored 2026-11-18T06:50:00\.000Z -> 2026-11-18T07:16:40\.000Z \(the window ± 10 min\)\. /m.test(direct), direct.split('\n')[1]);
  check(
    'TRUNCATED says the cut follows the order batches were stored',
    /^TRUNCATED: the read stopped at 40 rows\. Rows come in the order their batches were stored, so every batch stored after the last one read is missing/m.test(directCut) &&
      /^# rows: 40 TRUNCATED at 40: every batch stored after the last one read is missing/m.test(rowsText(cut)),
  );
  check('encoder rows: the message keeps its one pair of parentheses', /  encoder  open: encoder=hevc_qsv .* \(constant-quality \+ maxrate cap\)$/m.test(direct) && !/\(\(/.test(direct));
  check('no printed line ends in a space', !/ $/m.test(direct), (direct.match(/^.* $/m) || [''])[0]);
  const takes = (from, to, limit) => {
    try {
      readSettings({ RC_PATH_TENANT: FX.tenant, RC_PATH_AGENTS: FX.agent, RC_PATH_SINCE: from, RC_PATH_UNTIL: to, RC_PATH_LIMIT: limit });
      return true;
    } catch (_) {
      return false;
    }
  };
  check(
    'the window takes ISO_TIME only: a date, a time with Z or an offset; never a zone-less time',
    takes('2026-11-18', '2026-11-18T08:00Z') &&
      takes('2026-11-18T07:00:00.250+01:00', '2026-11-18T08:00:00Z') &&
      !takes('2026-11-18T07:00:00', '2026-11-18T08:00:00Z') &&
      !takes('2026-11-18T07:00:00Z', 'Nov 18 2026 09:00') &&
      !takes('2026-11-18 07:00:00Z', '2026-11-18T08:00:00Z') &&
      !takes('2026-11-18t07:00:00z', '2026-11-18T08:00:00Z'),
  );
  // 7. The wrapper checks its arguments before it looks at the target.
  try {
    const sh = require('fs').readFileSync(require('path').join(__dirname, 'rc-path-report.sh'), 'utf8');
    const shl = sh.split('\n');
    const lineOf = (re) => shl.findIndex((l) => re.test(l));
    const target = lineOf(/^\[ -n "\$TARGET" \] \|\| die/);
    const before = (i) => i >= 0 && target > i && lineOf(/^case "\$TARGET" in$/) > target;
    const twice = (flag) => before(shl.findIndex((l) => /\|\| die "[^"]*given twice/.test(l) && l.includes(flag)));
    check('the wrapper refuses --from/--since, --to/--until, --tenant, --agent or --limit given twice, before the target', ['--from', '--to', '--tenant', '--agent', '--limit'].every(twice));
    const iso = /^ISO='([^']+)'$/m.exec(sh);
    check('the wrapper checks both window values with ISO_TIME, before the target', iso && iso[1] === ISO_TIME.source && before(lineOf(/--since\|--from\)/)) && shl.filter((l) => /=~ \$ISO \]\]/.test(l)).length === 2, iso && iso[1]);
    const lm = /^LIMIT_MAX=(\d+)$/m.exec(sh);
    const said = MAX_LIMIT.toLocaleString('en-US');
    check(
      '--limit has one bound: the wrapper and the read agree on it, and the help states it once',
      lm && Number(lm[1]) === MAX_LIMIT && shl.filter((l) => /^#/.test(l) && l.includes(said)).length === 1 && takes(at(0), at(10), String(MAX_LIMIT)) && !takes(at(0), at(10), String(MAX_LIMIT + 1)),
      lm && lm[1],
    );
    check('every value bound for the remote command line is checked plain, before the target', before(lineOf(/\*\[!A-Za-z0-9\._:\+=-\]\*\)/)));
  } catch (e) {
    check('the wrapper is readable beside this script', false, e.message);
  }
  console.log(`rc-path-timeline self-test: ${cases - fails}/${cases} passed`);
  return fails ? 1 : 0;
}

// ---------------------------------------------------------------- Node runner

function nodeMain(argv) {
  const opts = { files: [], intervals: false, agent: null, from: null, to: null };
  for (let i = 0; i < argv.length; i++) {
    const a = argv[i];
    if (a === '--self-test') return process.exit(selfTest());
    if (a === '--intervals') opts.intervals = true;
    else if (a === '--agent') opts.agent = argv[++i];
    else if (a === '--from') opts.from = argv[++i];
    else if (a === '--to') opts.to = argv[++i];
    else if (a === '-h' || a === '--help') {
      console.log('usage: rc-path-timeline.cjs [--intervals] [--agent HEX] [--from ISO --to ISO] ROWS.ndjson...   (rows from rc-path-report.sh --timeline --rows)\n       rc-path-timeline.cjs --self-test');
      return process.exit(0);
    } else opts.files.push(a);
  }
  if (!opts.files.length) {
    console.error('rc-path-timeline: no input (--help)');
    return process.exit(1);
  }
  const fs = require('fs');
  // The command line wins over the file as the report's label.
  console.log(renderRows(opts.files.map((file) => fs.readFileSync(file === '-' ? 0 : file, 'utf8')), opts));
}

// ---------------------------------------------------------------- dispatch

if (typeof module !== 'undefined' && module.exports) {
  module.exports = {
    fold,
    render,
    timelinePipeline,
    carrierPipeline,
    QUERY_OPTIONS,
    CARRIER_OPTIONS,
    KINDS,
    FIELDS,
    MSG_PATTERN,
    ISO_TIME,
    MAX_LIMIT,
    scrub,
    mask,
    masks,
    value,
    readSettings,
    readOpts,
    rowOut,
    rowsHead,
    rowsTail,
    parseRows,
    renderRows,
    selfTest,
  };
}
if (typeof Mongo === 'function' && typeof print === 'function') {
  runMongosh();
} else if (typeof require !== 'undefined' && typeof module !== 'undefined' && require.main === module) {
  nodeMain(process.argv.slice(2));
}
