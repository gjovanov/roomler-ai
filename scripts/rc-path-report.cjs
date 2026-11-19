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
//   writes to the rate memory. Since FR-148 P0 step 2 it also prints what the
//   path carried while bytes waited (the delivery samples), a change of path
//   inside a session, whether the picture ever left the slow-link profile, and
//   whether the entry at the open was measured and was the pair's. Three flags
//   count what FR-148 fixes. AC1 reads the two reference failures through it,
//   and AC6, AC7, AC8 and AC14 are judged through it later. It measures. It
//   decides nothing.
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
//   session carrier recorded      the carrier class (FR-102 P9c), and since FR-148 P0 what
//                                 carries the agent's own TURN allocation (`relay_transport`)
//   per-session ICE path detected the class's fallback on a build without the line above
//   …opening ceiling seeded…      the remembered entry a session opened from, and the
//                                 carrier half of its memory key
//   …slow-link profile engaged…   the slow-link profile and the entry that engaged it
//   FR-59 P8 …                    an opening at an entry below the nominal floor
//   …opener grace over…           the opener's measurement, the memory's second evidence
//   …measured rate remembered…    a write to the rate memory: its evidence and kept value
//   path delivery (FR-148 P0…)    ≈10 s of the transport's delivery samples, from both DC
//                                 pumps (#3968): the samples by class, the backlogged rates
//                                 (lowest, mean, highest), the highest application-limited
//                                 rate, the stalled time; and on a newer agent the samples
//                                 queued (`queued`) and their standing over the session's
//                                 least acknowledgement delay (`standing_ms`)
//   …no measured provenance…      FR-148 P1 (#3969): an entry an open passed over as assumed,
//                                 its rate and age, and the carrier half of its memory key
//
//   Only the FIELDS below leave the database, so no address, name or other line
//   does. Two leave transformed, never raw: `peer` (the memory key,
//   "<address>|<carrier>") as `peer_carrier`, the part after the bar, on every
//   line that carries it, the FR-148 P1 line included; and `controller_user_id`
//   as `user6`, its last 6 hex. `controller_name`, every `*_addr` field and the
//   delivery line's `acked_bytes` are never projected.
//
// THE CARRIER CLASS
//
//   From `session carrier recorded`: direct · turn · turn-local · mesh-direct ·
//   mesh-relayed/turn · mesh-relayed/derp · mesh-relayed/org · unknown. A build
//   without that line falls back to the ICE line: `relay=true` reads `turn (ice)`;
//   a non-relay pair the pump runs constrained is an overlay remote on a relay
//   tier, `mesh-relayed (pump)`; any other is `direct or mesh (ice)`.
//   THE RELAY TRANSPORT, from the same line on a newer agent (FR-148 P0): what
//   carries the agent's own TURN allocation for the selected pair, from the TURN
//   URL it allocated over: `udp`, `tcp` or `tls`; `none` where the agent's
//   selected candidate is no relay; `unknown` where the agent could not say. A
//   session whose carrier line lacks the field (an older agent), or that has no
//   carrier line, counts as `no line`.
//   What the lines cannot tell:
//   - The transport of a relay the agent did not allocate: the browser's own
//     transport to its TURN server never reaches the agent. So on a `turn`
//     session whose relay is the browser's, `relay_transport` reads `none`, and
//     the browser's leg may be TURN over UDP, TCP or TLS.
//   - The mesh relay's own transport, except on a session that read the memory:
//     its key carries it (`relay:derp/tcp`), printed as the key's carrier.
//   - A carrier change inside a session: the line is written once a peer
//     connection, 2 s after the nomination, so it names the opening carrier. A
//     change shows in the pump's flag instead (THE PATH below). Should the line
//     repeat, the last one still names the class, as before.
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
//   capacity: a still screen holds it low. It is printed twice: over the whole
//   read window, and over the pair's sessions that ended before this one opened
//   (AC6's reading: what the pair had last demonstrated).
//
// WHAT THE PATH CARRIED, AND A CHANGE OF PATH
//
//   The `path delivery` line folds about five samples of ≈2 s each: BACKLOGGED
//   (bytes waited the whole sample and some were acknowledged), APPLICATION-
//   LIMITED (the queue ran empty) and STALLED (bytes waited, none acknowledged).
//   Per session: the samples by class; the backlogged rates' lowest and highest
//   single sample and their mean over all their bytes (each line's mean weighed
//   by its backlogged time, which a line gives exactly when none of its samples
//   is application-limited and otherwise is shared out by sample count); the
//   highest application-limited rate; the stalled time. A session with no such
//   line reads `–`, never 0. Per pair, BACKLOGGED MAX: the highest backlogged
//   sample any of its sessions read on its carrier, a rate the path carried for
//   ≈2 s while the sender had more to send. It is no capacity either: a sample
//   partly stalled reads low, a viewer that stops reading holds the queue full,
//   and the bytes are the whole association's (docs/rate-control.md § Delivery
//   samples).
//   A CHANGE OF PATH is a flip of the FFmpeg heartbeat's `constrained` flag, or
//   a carrier or ICE line that repeats with another value (the ICE line comes
//   once a pump start), dated in seconds from the opening (the first heartbeat).
//   A session with a change stays in the pair of its carrier line (the last one
//   when it repeats), and its opening is judged there. Its demonstrated runs and
//   delivery lines count only on the side its class runs on (a run breaks where
//   the flag flips); a delivery line whose ≈10 s cross a flip counts nowhere. A
//   VP9-444 pump has no flag: its windows and lines are never split by side.
//
// THE STANDING QUEUE
//
//   A newer agent's delivery line also says how many of its samples were
//   QUEUED: the sample's least acknowledgement delay stood 250 ms or more over
//   the session's least, and it acknowledged enough bytes for that least to hold
//   no receiver's delayed SACK (docs/rate-control.md § The standing queue). Per
//   session, over every such line whatever its side (a standing queue is the
//   path's, not the flag's): the queued samples of the lines' samples, and each
//   line's highest standing, of which the row prints the p50 and the max. A
//   session with no such line reads `–`. Per agent: its sessions by relay
//   transport, and by carrier class the sessions with at least SQ_SESSION_MIN
//   queued samples, of those that carried the fields.
//
// THE PICTURE AND THE PROFILE
//
//   The picture LEFT the slow-link profile at the first heartbeat both larger
//   (long edge) and faster (target_fps) than the opening: AC14's "back" is
//   resolution and frame rate both. On a persisted flip to direct the frame rate
//   follows the transport while the size cap stays for the pump's life, so a
//   faster picture alone has not left. AT NOMINAL AFTER: for a session that
//   opened on a relay below RELAY_NOMINAL_BPS, the seconds from the opening to
//   the first heartbeat on the relay whose plan stood at or above it.
//
// THE ENTRY AT THE OPEN
//
//   The entry a session opened from (the seed line); "assumed …, passed over"
//   where the FR-148 P1 line says the entry had no measured provenance. That line
//   carries no session id: it goes to the session of the same agent whose pump
//   started first, at most OPEN_LAG_MS after it, and it is not the entry the
//   session opened from. OPENED WITHOUT THE PAIR'S ENTRY, INFERRED: a session
//   whose pump started constrained (where the memory is read), that read no
//   entry and passed over none, though the pair's last write with evidence was
//   at most MEMORY_TTL_MS before its pump start. A write with no evidence keeps
//   the entry's value and age, so it dates nothing. The lines cannot tell why:
//   the key differed between the two sessions, no key was formed at that open,
//   or the open ran in a process that keeps its memory in another data
//   directory.
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
//   figure alike. A third row counts the same openings against the pair's
//   backlogged max; it is not F1's verdict, and a pair with no backlogged sample
//   is not judged there.
//   F2, the plan moved and the encoder did not: the target's max is at least
//   F2_SPREAD × its min, the session lasted at least F2_MIN_SESSION_MS (its pump
//   start to its last heartbeat), and the encoder's applied rate moved 0 times.
//   F3, the slow-link profile never left: judged on a session whose pump started
//   with the profile on, that lasted at least F3_MIN_SESSION_MS (its pump start
//   to its last heartbeat) and whose opening heartbeat read a size and a frame
//   rate; raised when its plan stood above SLOW_LINK_PROFILE_BPS (an entry at or
//   under it engages the profile) for at least F3_RUN_MS in a row, and the
//   picture never left the profile. A run breaks at a plan at or under the
//   threshold, at a new pump and at a hole in the log.
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
//   - the delivery line adds at most one row a pump every 10 s
//     (`media_delivery::LINE_EVERY`): a fifth more rows than an FFmpeg pump's
//     heartbeats, a tenth more than a VP9-444 pump's. The FR-148 P1 line adds at
//     most one a pump start. Neither changes the documents examined: the
//     `$match`, the index, the `hint`, the day split and the options are the
//     same. The fold keeps a fixed handful of numbers a session for them, at
//     most MAX_CHANGES changes of path and MAX_STARTS pump starts.
//
// RUN
//
//   node scripts/rc-path-report.cjs --self-test
//   node scripts/rc-path-report.cjs [--json] [--label HEX=NAME]... [--agent HEX] FILE...   # rows or host log text
//   node scripts/rc-path-report.cjs --to-ejson [--agent HEX] [--tenant HEX] FILE...        # a seed from host logs
//   node scripts/rc-path-report.cjs --fixtures                                             # the self-test's fixtures, as a seed
//   bash scripts/rc-path-report.sh --docker NAME --seed FILE                               # the rehearsal
//   bash scripts/rc-path-report.sh --mars --yes [--since ISO] [--until ISO]                # the fleet read
//
// The rehearsal's mongo:7 starts with MONGO_INITDB_ROOT_USERNAME and _PASSWORD
// set (an --env-file): the read connects with them, and without them it stops at
// "No connected database" although the seed loaded.
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
/** `rate_profile::slow_link_profile_bps`'s default: an open whose entry is at or
 *  below it engages the slow-link profile, so a plan above it stands where no
 *  open engages the profile. The self-test checks it against the Rust source. */
const SLOW_LINK_PROFILE_BPS = 1000000;
/** `encode::relay_max_bps`'s default: the plan's nominal ceiling on a relay. */
const RELAY_NOMINAL_BPS = 3000000;
/** F3: judged on a session that opened with the slow-link profile and lasted at
 *  least this long; raised when its plan stood above SLOW_LINK_PROFILE_BPS for
 *  F3_RUN_MS in a row and its picture never left the profile. */
const F3_MIN_SESSION_MS = 5 * 60 * 1000;
const F3_RUN_MS = 60000;
/** `rate_memory::TTL`: an entry older than this seeds nothing. */
const MEMORY_TTL_MS = 7 * 24 * 3600 * 1000;
/** The assumed-entry line comes before its pump's start line by at most this
 *  much (`RateMemory::seed_at_open`, then `DC pump starting`). */
const OPEN_LAG_MS = 5000;
/** Per session, the path changes kept (the rest are counted) and the pump
 *  starts kept, so a flapping session stays a few numbers in the fold. */
const MAX_CHANGES = 8;
const MAX_STARTS = 16;
/** A session with at least this many queued samples had a standing queue. */
const SQ_SESSION_MIN = 3;
/** The carrier line's `relay_transport` vocabulary
 *  (`session_carrier::RelayTransport::as_str`), and the report's bucket for a
 *  session whose carrier line lacks the field or that has none. */
const RELAY_TRANSPORTS = ['udp', 'tcp', 'tls', 'none', 'unknown'];
const NO_LINE = 'no line';

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
  delivery: /path delivery \(FR-148 P0/,
  assumed: /entry has no measured provenance/,
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
  'span_ms',
  'delivery_samples',
  'backlogged_bps',
  'app_limited_bps',
  'stalled_ms',
  'queued',
  'standing_ms',
  'relay_transport',
  'assumed_bps',
  'age_s',
];
/** Identifiers and labels: emitted as text, so an all-digit value stays a string. */
const ID_FIELDS = new Set(['session_id', 'codec_label', 'encoder', 'carrier', 'relay_transport', 'peer_carrier', 'user6']);

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

/** A Debug tuple of three integers, `(a, b, c)`, bare or inside `Some(…)`, as
 *  the log prints it and as the uploader stores it; null for `None`. The
 *  delivery line's `delivery_samples` and `backlogged_bps` have this shape. */
function triple(v) {
  const s = text(v);
  const m = s && /^\(\s*(\d+)\s*,\s*(\d+)\s*,\s*(\d+)\s*\)$/.exec(s);
  return m ? [Number(m[1]), Number(m[2]), Number(m[3])] : null;
}

/** A Debug tuple of two integers, `(a, b)`, bare or inside `Some(…)`; null for
 *  `None`. The delivery line's `standing_ms` has this shape. */
function couple(v) {
  const s = text(v);
  const m = s && /^\(\s*(\d+)\s*,\s*(\d+)\s*\)$/.exec(s);
  return m ? [Number(m[1]), Number(m[2])] : null;
}

/** The carrier line's `relay_transport`, as one of RELAY_TRANSPORTS: a value
 *  this build does not know reads `unknown`, never a guess. */
function relayTransportOf(v) {
  const t = text(v);
  if (t === null) return null;
  return RELAY_TRANSPORTS.indexOf(t) >= 0 ? t : 'unknown';
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

/** `x`: { profile, durationMs, openSized, runMs, left }. F3 judges a session
 *  that opened with the slow-link profile, at a size and frame rate the
 *  heartbeat read, and lasted F3_MIN_SESSION_MS; it is raised when the plan
 *  stood above SLOW_LINK_PROFILE_BPS for F3_RUN_MS in a row and the picture
 *  never left the profile. */
function judgeF3(x) {
  if (!x.profile || !x.openSized || x.durationMs === null || x.durationMs < F3_MIN_SESSION_MS) return null;
  const held = x.runMs >= F3_RUN_MS;
  return { held, left: x.left, raised: held && !x.left };
}

/** One side's delivery figures, folded line by line: a few numbers a session. */
function newDelivery() {
  return { lines: 0, b: 0, a: 0, s: 0, lo: null, hi: null, meanNum: 0, meanDen: 0, appMax: null, stalledMs: 0, spanMs: 0 };
}

/** The figures of several sides as one, or null when none has a line. */
function mergeDelivery(parts) {
  const out = newDelivery();
  for (const d of parts) {
    if (!d) continue;
    out.lines += d.lines;
    out.b += d.b;
    out.a += d.a;
    out.s += d.s;
    out.meanNum += d.meanNum;
    out.meanDen += d.meanDen;
    out.stalledMs += d.stalledMs;
    out.spanMs += d.spanMs;
    if (d.lo !== null && (out.lo === null || d.lo < out.lo)) out.lo = d.lo;
    if (d.hi !== null && (out.hi === null || d.hi > out.hi)) out.hi = d.hi;
    if (d.appMax !== null && (out.appMax === null || d.appMax > out.appMax)) out.appMax = d.appMax;
  }
  return out.lines ? out : null;
}

function deliveryOut(d) {
  if (!d) return null;
  return {
    lines: d.lines,
    samples: { backlogged: d.b, appLimited: d.a, stalled: d.s },
    backloggedBps: d.lo === null ? null : { min: d.lo, mean: d.meanDen > 0 ? Math.round(d.meanNum / d.meanDen) : null, max: d.hi },
    appLimitedMaxBps: d.appMax,
    stalledS: Math.round(d.stalledMs / 100) / 10,
    spanS: Math.round(d.spanMs / 100) / 10,
  };
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
    this.assumed = [];
    this.assumedKeys = new Set();
    this.lines = 0;
    this.read = 0;
    this.heartbeats = 0;
    this.replays = 0;
    this.deliveryLines = 0;
    this.deliveryReplays = 0;
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
        // What the path carried, a change of path, and the picture.
        firstKind: null,
        starts: [],
        kindNow: null,
        cNow: null,
        flipAt: null,
        leftRelayAt: null,
        iceLast: null,
        changes: [],
        changeCount: 0,
        dl: { c: null, d: null, u: null, x: null },
        dlLast: null,
        // The standing queue (every side) and the relay transport.
        sq: null,
        relay: null,
        pic: null,
        above: null,
        aboveBestMs: 0,
        aboveBestAt: null,
        nominalAt: null,
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
    if (kind === 'assumed') {
      // No session id either: `finish` places it at the pump start that follows.
      // Its key is kept as the carrier half only, as every other line's.
      const a = { agent: agent || null, at, tag: keyCarrier(f), bps: num(f.assumed_bps), ageS: num(f.age_s) };
      const key = [a.agent, at, a.tag, a.bps, a.ageS].join('|');
      if (!this.assumedKeys.has(key)) {
        this.assumedKeys.add(key);
        this.assumed.push(a);
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
        if (c) {
          if (s.carrier !== null && c !== s.carrier) {
            this.change(s, at, `carrier ${CARRIER_CLASS[s.carrier] || s.carrier} → ${CARRIER_CLASS[c] || c}`, 'carrier line');
          }
          s.carrier = c;
        }
        // The last line names it, as it names the class.
        const rt = relayTransportOf(f.relay_transport);
        if (rt) s.relay = rt;
        break;
      }
      case 'ice': {
        const relay = bool(f.relay);
        if (relay !== null) {
          if (s.iceRelay === null) s.iceRelay = relay;
          if (s.iceLast !== null && relay !== s.iceLast) this.change(s, at, relay ? 'entered TURN' : 'left TURN', 'ICE line');
          s.iceLast = relay;
        }
        break;
      }
      case 'start': {
        if (s.startAt === null || at < s.startAt) s.startAt = at;
        if (s.starts.length < MAX_STARTS && s.starts.indexOf(at) < 0) s.starts.push(at);
        const sl = bool(f.slow_link);
        if (sl !== null && s.startSlow === null) s.startSlow = sl;
        break;
      }
      case 'delivery':
        this.delivery(s, f, at);
        break;
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
    if (s.hbFirst === null) {
      s.hbFirst = at;
      s.firstKind = kind;
    }
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
        c,
      };
    }
    this.pathAt(s, kind, f, at, c, target, fresh);
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

  /** The path and the picture at one heartbeat: a flip of the pump's
   *  `constrained` flag, the picture against the opening's, the plan's run
   *  above the slow-link profile's threshold, and the first time the plan stood
   *  at the relay's nominal rate while on a relay. */
  pathAt(s, kind, f, at, c, target, fresh) {
    s.kindNow = kind;
    if (c !== null) {
      if (s.cNow !== null && c !== s.cNow) {
        s.flipAt = at;
        if (!c && s.leftRelayAt === null) s.leftRelayAt = at;
        this.change(s, at, c ? 'entered a relay' : 'left the relay', 'pump flag');
      }
      s.cNow = c;
    }
    const o = s.open;
    if (kind !== 'vp9' && o.width !== null && o.height !== null && o.fps !== null) {
      const w = num(f.width);
      const h = num(f.height);
      const fps = num(f.target_fps);
      if (w !== null && h !== null && fps !== null) {
        const edge0 = Math.max(o.width, o.height);
        const edge = Math.max(w, h);
        const q = s.pic || (s.pic = { edgeMax: edge0, fpsMax: o.fps, edgeUpAt: null, fpsUpAt: null, leftAt: null });
        if (edge > q.edgeMax) q.edgeMax = edge;
        if (fps > q.fpsMax) q.fpsMax = fps;
        if (edge > edge0 && q.edgeUpAt === null) q.edgeUpAt = at;
        if (fps > o.fps && q.fpsUpAt === null) q.fpsUpAt = at;
        // Out of the profile: larger AND faster than the opening (AC14's "back").
        if (edge > edge0 && fps > o.fps && q.leftAt === null) q.leftAt = at;
      }
    }
    if (target !== null) {
      if (target > SLOW_LINK_PROFILE_BPS) {
        // A run breaks at a new pump and at a hole in the log, as a rate does.
        const r = s.above;
        if (r && !fresh && at - r.last <= RATE_MAX_GAP_MS) r.last = at;
        else s.above = { since: at, last: at };
        const run = s.above.last - s.above.since;
        if (s.aboveBestAt === null || run > s.aboveBestMs) {
          s.aboveBestMs = run;
          s.aboveBestAt = s.above.since;
        }
      } else s.above = null;
      if (s.nominalAt === null && c !== false && target >= RELAY_NOMINAL_BPS) s.nominalAt = at;
    }
  }

  change(s, at, what, by) {
    s.changeCount++;
    if (s.changes.length < MAX_CHANGES) s.changes.push({ at, what, by });
  }

  /** One `path delivery` line: its samples go to the side of the pump's flag at
   *  the line, or to `x` when a flip falls inside the 10 s the line covers. */
  delivery(s, f, at) {
    const counts = triple(f.delivery_samples);
    if (!counts) return;
    // A batch uploaded twice, or one that arrived after a later one.
    if (s.dlLast !== null && at <= s.dlLast) {
      this.deliveryReplays++;
      return;
    }
    s.dlLast = at;
    this.deliveryLines++;
    // The standing queue, on a line that carries it: every side, since a
    // standing queue is the path's and the flag's side says nothing of it.
    const queued = num(f.queued);
    if (queued !== null) {
      const q = s.sq || (s.sq = { lines: 0, samples: 0, queued: 0, highs: new Map(), max: null });
      q.lines++;
      q.samples += counts[0] + counts[1] + counts[2];
      q.queued += queued;
      const standing = couple(f.standing_ms);
      if (standing) {
        bump(q.highs, standing[1]);
        if (q.max === null || standing[1] > q.max) q.max = standing[1];
      }
    }
    const span = num(f.span_ms);
    const stalled = num(f.stalled_ms) || 0;
    const bl = triple(f.backlogged_bps);
    const app = num(f.app_limited_bps);
    // A VP9-444 pump has no flag: its lines count on no side, as its runs do.
    let side = s.kindNow === 'vp9' ? 'u' : s.cNow === true ? 'c' : s.cNow === false ? 'd' : 'u';
    // The line follows the heartbeat that closed its last sample, and its samples
    // start at the heartbeat that wrote the line before it. A flip after that
    // start puts part of the line on each side.
    if (s.flipAt !== null && span !== null && at - span < s.flipAt - RATE_MIN_GAP_MS) side = 'x';
    const d = s.dl[side] || (s.dl[side] = newDelivery());
    d.lines++;
    d.b += counts[0];
    d.a += counts[1];
    d.s += counts[2];
    d.stalledMs += stalled;
    if (span !== null) d.spanMs += span;
    if (app !== null && (d.appMax === null || app > d.appMax)) d.appMax = app;
    if (bl && counts[0] > 0) {
      if (d.lo === null || bl[0] < d.lo) d.lo = bl[0];
      if (d.hi === null || bl[2] > d.hi) d.hi = bl[2];
      // The line's backlogged time weighs its mean. Stalled samples carry no
      // bytes, so with no application-limited sample it is exact; with some, the
      // line's remaining time is shared out by sample count.
      if (span !== null) {
        const live = Math.max(0, span - stalled);
        const tb = counts[1] === 0 ? live : (live * counts[0]) / (counts[0] + counts[1]);
        if (tb > 0) {
          d.meanNum += bl[1] * tb;
          d.meanDen += tb;
        }
      }
    }
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
      // The delivery lines count for the pair on the side its carrier runs on,
      // as the demonstrated runs do; a line across a flip counts nowhere.
      const dl = s.dl;
      const credited = mergeDelivery(want === true ? [dl.c, dl.u] : want === false ? [dl.d, dl.u] : [dl.c, dl.d, dl.u]);
      const other = want === true ? mergeDelivery([dl.d]) : want === false ? mergeDelivery([dl.c]) : null;
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
        want,
        dlCredited: credited,
        dlOther: other,
        dlAcross: dl.x ? dl.x.lines : 0,
        anyDelivery: !!(dl.c || dl.d || dl.u || dl.x),
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
        p = {
          key,
          agent: row.s.agent,
          user: row.s.user,
          cls: row.cls,
          rows: [],
          demo: null,
          demoIn: null,
          writes: { measured: 0, standIn: 0, nothing: 0, unexplained: 0 },
          bl: null,
          blIn: null,
          realWrites: [],
        };
        pairs.set(key, p);
      }
      p.rows.push(row);
      if (row.demoSession !== null && (p.demo === null || row.demoSession > p.demo)) {
        p.demo = row.demoSession;
        p.demoIn = row.s.sid;
      }
      if (row.dlCredited && row.dlCredited.hi !== null && (p.bl === null || row.dlCredited.hi > p.bl)) {
        p.bl = row.dlCredited.hi;
        p.blIn = row.s.sid;
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
        row.blWindow = p.bl;
      }
    }

    // The memory trail.
    const byAgent = new Map();
    for (const row of rows) {
      const k = row.s.agent || '';
      if (!byAgent.has(k)) byAgent.set(k, []);
      byAgent.get(k).push(row);
    }
    // The entry an open passed over: the line carries no session id and comes
    // just before its pump's start line, so it goes to the session of the same
    // agent whose pump started first at or after it, within OPEN_LAG_MS.
    for (const row of rows) {
      row.assumed = null;
      row.assumedLines = 0;
    }
    let assumedUnplaced = 0;
    for (const a of this.assumed.slice().sort((x, y) => x.at - y.at)) {
      let best = null;
      for (const row of byAgent.get(a.agent || '') || []) {
        for (const st of row.s.starts) {
          if (st < a.at || st - a.at > OPEN_LAG_MS) continue;
          if (best === null || st < best.st || (st === best.st && row.s.sid < best.row.s.sid)) best = { row, st };
        }
      }
      if (!best) {
        assumedUnplaced++;
        continue;
      }
      best.row.assumedLines++;
      if (!best.row.assumed) best.row.assumed = a;
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
          // A write with evidence leaves an entry dated now. One with none keeps
          // the entry's value and its older age, so it dates nothing.
          if (w.evidence !== null && w.evidence > 0 && w.kept !== null && w.kept > 0) pairs.get(row.pairKey).realWrites.push({ at: w.at, kept: w.kept, tag: w.tag });
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

    // An open that read no entry, though the pair's last write with evidence, in
    // the memory's 7 days before it, left one. Judged where the memory is read:
    // an FFmpeg pump that started on a relay. Inferred, as the trail is.
    for (const row of rows) {
      const s = row.s;
      row.withoutEntry = null;
      if (!row.started || s.firstKind !== 'ffmpeg' || s.firstConstrained !== true) continue;
      if (entryOf(s) !== null || row.assumed) {
        row.withoutEntry = false;
        continue;
      }
      let last = null;
      for (const w of pairs.get(row.pairKey).realWrites) if (w.at < s.startAt && (last === null || w.at >= last.at)) last = w;
      // The window is the memory's own: an entry is fresh while its age is at
      // most TTL (`Entry::fresh`), and a write with no evidence leaves the age.
      row.withoutEntry = last !== null && s.startAt - last.at <= MEMORY_TTL_MS ? last : false;
    }

    // Sessions and their flags.
    const f1 = { judged: 0, below: 0, below85: 0, judgedBefore: 0, belowBefore: 0, below85Before: 0, judgedBacklogged: 0, belowBacklogged: 0, below85Backlogged: 0 };
    const f2 = { judged: 0, spread: 0, raised: 0 };
    const f3 = { judged: 0, held: 0, raised: 0 };
    const sessions = rows.map((row) => {
      const s = row.s;
      const t = spanOf(s.target);
      const a = spanOf(s.applied);
      const one = row.openJudged ? judgeF1(s.open.bps, row.demoWindow) : null;
      const before = row.openJudged ? judgeF1(s.open.bps, row.demoBefore) : null;
      const oneB = row.openJudged ? judgeF1(s.open.bps, row.blWindow) : null;
      const two = judgeF2({
        durationMs: row.endMs - row.startMs,
        tmin: t ? t.min : null,
        tmax: t ? t.max : null,
        appliedSeen: s.appliedSeen,
        moves: s.moves,
        ctrSeen: s.ctrSeen,
        ctrMoves: s.ctrMoves,
      });
      const o = s.open;
      const sized = !!(o && o.width && o.height && o.fps);
      const three = judgeF3({
        profile: s.startSlow === true || s.slow !== null,
        durationMs: row.endMs - row.startMs,
        openSized: sized,
        runMs: s.aboveBestAt === null ? 0 : s.aboveBestMs,
        left: !!(s.pic && s.pic.leftAt !== null),
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
      if (oneB) {
        f1.judgedBacklogged++;
        if (oneB.below) f1.belowBacklogged++;
        if (oneB.below85) f1.below85Backlogged++;
      }
      if (two) {
        f2.judged++;
        if (two.spread) f2.spread++;
        if (two.raised) f2.raised++;
      }
      if (three) {
        f3.judged++;
        if (three.held) f3.held++;
        if (three.raised) f3.raised++;
      }
      const pair = pairs.get(row.pairKey);
      // Every time in `path` counts from the opening, the first heartbeat, where
      // the plan is first read.
      const after = (at) => (at === null || at === undefined ? null : Math.round((at - o.at) / 1000));
      const onRelay = o.c === true || (o.c === null && row.want === true);
      const nominalJudged = onRelay && o.bps !== null && o.bps < RELAY_NOMINAL_BPS;
      const q = s.pic;
      const w = row.withoutEntry;
      const path = {
        changes: s.changes.map((c) => ({ afterS: after(c.at), change: c.what, seenIn: c.by })),
        changeCount: s.changeCount,
        delivery: deliveryOut(row.dlCredited),
        deliveryOtherSide: deliveryOut(row.dlOther),
        deliveryAcrossChange: row.dlAcross,
        openingOfBackloggedMax: oneB ? Math.round((1000 * o.bps) / row.blWindow) / 1000 : null,
        picture: sized
          ? {
              longEdgeMax: q ? q.edgeMax : Math.max(o.width, o.height),
              fpsMax: q ? q.fpsMax : o.fps,
              longEdgeUpAfterS: q ? after(q.edgeUpAt) : null,
              fpsUpAfterS: q ? after(q.fpsUpAt) : null,
              leftProfileAfterS: q ? after(q.leftAt) : null,
            }
          : null,
        planAboveProfileS: s.aboveBestAt === null ? null : Math.round(s.aboveBestMs / 1000),
        // Every line's highest standing gives the p50 and the max.
        standingQueue: s.sq
          ? { lines: s.sq.lines, samples: s.sq.samples, queued: s.sq.queued, standingP50Ms: percentile(s.sq.highs, 0.5), standingMaxMs: s.sq.max }
          : null,
        relayTransport: s.relay,
        atNominal: {
          judged: nominalJudged,
          afterS: nominalJudged ? after(s.nominalAt) : null,
          // It never stood there on the relay because the session left it.
          leftRelayFirst: nominalJudged && s.nominalAt === null && s.leftRelayAt !== null,
        },
        entryAtOpen: {
          seededBps: entryOf(s),
          keyCarrier: s.seedTag,
          assumed: row.assumed ? { bps: row.assumed.bps, ageS: row.assumed.ageS, keyCarrier: row.assumed.tag, passedOver: true, lines: row.assumedLines } : null,
          withoutPairEntry: w === null ? null : w === false ? false : { writtenAt: iso(w.at), keptBps: w.kept, keyCarrier: w.tag, inferred: true },
        },
        f3: three,
      };
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
          f1Backlogged: oneB ? oneB.below : null,
          f1BackloggedAt85: oneB ? oneB.below85 : null,
          f3: three ? three.raised : null,
        },
        path,
      };
    });

    const pairsOut = [...pairs.values()]
      .sort((a, b) => (a.key < b.key ? -1 : a.key > b.key ? 1 : 0))
      .map((p) => {
        let lo = null;
        let hi = null;
        let n1 = 0;
        let n2 = 0;
        const b = { judged: 0, below: 0, below85: 0 };
        let n3 = 0;
        let changed = 0;
        const without = { judged: 0, count: 0, inferred: true };
        for (const row of p.rows) {
          const i = rows.indexOf(row);
          const out = sessions[i];
          if (row.openJudged) {
            lo = lo === null ? out.opening.bps : Math.min(lo, out.opening.bps);
            hi = hi === null ? out.opening.bps : Math.max(hi, out.opening.bps);
          }
          if (out.flags.f1) n1++;
          if (out.flags.f2) n2++;
          if (out.flags.f1Backlogged !== null) {
            b.judged++;
            if (out.flags.f1Backlogged) b.below++;
            if (out.flags.f1BackloggedAt85) b.below85++;
          }
          if (out.flags.f3) n3++;
          if (out.path.changeCount) changed++;
          if (row.withoutEntry !== null) {
            without.judged++;
            if (row.withoutEntry) without.count++;
          }
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
          path: {
            backloggedMaxBps: p.bl,
            backloggedReadIn: last6(p.blIn),
            openingsAgainstBacklogged: b,
            changedPath: changed,
            f3: n3,
            withoutPairEntry: without,
          },
        };
      });

    // Per agent: its sessions by relay transport, and by carrier class the
    // sessions with a standing queue, of those that carried the fields.
    const devices = new Map();
    for (const row of rows) {
      const k = row.s.agent || '';
      let d = devices.get(k);
      if (!d) {
        d = { agent: agentName(row.s.agent, labels), sessions: 0, relayTransport: { udp: 0, tcp: 0, tls: 0, none: 0, unknown: 0, noLine: 0 }, byCarrier: new Map() };
        devices.set(k, d);
      }
      d.sessions++;
      if (row.s.relay === null) d.relayTransport.noLine++;
      else d.relayTransport[row.s.relay]++;
      if (row.s.sq) {
        const c = d.byCarrier.get(row.cls) || { carrier: row.cls, sessions: 0, standingQueue: 0 };
        c.sessions++;
        if (row.s.sq.queued >= SQ_SESSION_MIN) c.standingQueue++;
        d.byCarrier.set(row.cls, c);
      }
    }
    const byText = (a, b) => (a < b ? -1 : a > b ? 1 : 0);
    const devicesOut = [...devices.keys()].sort(byText).map((k) => {
      const d = devices.get(k);
      return {
        agent: d.agent,
        sessions: d.sessions,
        relayTransport: d.relayTransport,
        standingQueue: [...d.byCarrier.values()].sort((a, b) => byText(a.carrier, b.carrier)),
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
    const f3Out = Object.assign(
      { thresholdBps: SLOW_LINK_PROFILE_BPS, runS: F3_RUN_MS / 1000, minSessionS: F3_MIN_SESSION_MS / 1000 },
      f3,
      { verdict: verdict(f3.raised, f3.judged) },
    );
    f3Out.line = !f3.judged
      ? `No session opened with the slow-link profile and lasted ${F3_MIN_SESSION_MS / 60000} min.`
      : `${f3.raised} of ${f3.judged} judged sessions: the plan stood above ${mbps(SLOW_LINK_PROFILE_BPS)} Mbit/s for ${F3_RUN_MS / 1000} s or more in a row, and the picture never left the size and frame rate it opened with.`;

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
        deliveryLines: this.deliveryLines,
        deliveryReplays: this.deliveryReplays,
        sessionsWithDelivery: rows.filter((row) => row.anyDelivery).length,
        sessionsWithPathChange: rows.filter((row) => row.s.changeCount > 0).length,
        sessionsWithStandingQueue: rows.filter((row) => row.s.sq).length,
        assumedLines: this.assumed.length,
        assumedUnplaced,
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
        slowLinkProfileBps: SLOW_LINK_PROFILE_BPS,
        relayNominalBps: RELAY_NOMINAL_BPS,
        f3RunS: F3_RUN_MS / 1000,
        f3MinSessionS: F3_MIN_SESSION_MS / 1000,
        memoryTtlS: MEMORY_TTL_MS / 1000,
        openLagMs: OPEN_LAG_MS,
        sqSessionMin: SQ_SESSION_MIN,
      },
      flags: { f1: f1Out, f2: f2Out, f3: f3Out },
      pairs: pairsOut,
      devices: devicesOut,
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

/** 1092 -> "1,092", without a locale (mongosh may have none). */
function group(n) {
  return String(n).replace(/\B(?=(\d{3})+(?!\d))/g, ',');
}

/** Seconds from the opening: "+1,092 s". */
function plusS(n) {
  return `${n < 0 ? '−' : '+'}${group(Math.abs(n))} s`;
}

/** One session's row of the path table, as cells. */
function pathCells(s) {
  const P = s.path;
  const first = P.changes[0];
  let change = first ? `${first.change} at ${plusS(first.afterS)}${first.seenIn === 'pump flag' ? '' : ` (${first.seenIn})`}` : '–';
  if (P.changeCount > 1) change += ` (+${P.changeCount - 1} more)`;
  const oth = P.deliveryOtherSide;
  if (oth && oth.backloggedBps) change += `; on the other side, backlogged max ${mbps(oth.backloggedBps.max)}`;
  if (P.deliveryAcrossChange) change += `; ${P.deliveryAcrossChange} delivery line${P.deliveryAcrossChange === 1 ? '' : 's'} across it, counted nowhere`;
  const d = P.delivery;
  const samples = d ? `${d.samples.backlogged} · ${d.samples.appLimited} · ${d.samples.stalled}` : '–';
  const bl = d && d.backloggedBps ? `${mbps(d.backloggedBps.min)} / ${mbps(d.backloggedBps.mean)} / ${mbps(d.backloggedBps.max)}` : '–';
  const app = d ? mbps(d.appLimitedMaxBps) : '–';
  const stalled = d ? `${d.stalledS.toFixed(1)} s` : '–';
  const ratio = P.openingOfBackloggedMax === null ? '–' : `${P.openingOfBackloggedMax.toFixed(2)} ×`;
  const o = s.opening;
  let pic = '–';
  if (P.picture) {
    const q = P.picture;
    pic = `${o.width}×${o.height} · ${o.fps} fps`;
    if (q.leftProfileAfterS !== null) pic += `; left the profile at ${plusS(q.leftProfileAfterS)}`;
    else {
      if (q.longEdgeUpAfterS !== null) pic += `; long edge ${q.longEdgeMax} from ${plusS(q.longEdgeUpAfterS)}`;
      if (q.fpsUpAfterS !== null) pic += `; ${q.fpsMax} fps from ${plusS(q.fpsUpAfterS)}`;
    }
  }
  const N = P.atNominal;
  const nominal = !N.judged ? '–' : N.afterS !== null ? `${group(N.afterS)} s` : N.leftRelayFirst ? 'left the relay first' : 'never';
  const e = P.entryAtOpen;
  let entry = '–';
  if (e.seededBps !== null) entry = `${mbps(e.seededBps)}${e.keyCarrier ? ` (${e.keyCarrier})` : ''}`;
  else if (e.assumed) entry = `assumed ${mbps(e.assumed.bps)}, passed over`;
  else if (e.withoutPairEntry) {
    const we = e.withoutPairEntry;
    entry = `none, though the pair's write of ${minute(we.writtenAt)} left ${mbps(we.keptBps)} under ${we.keyCarrier || 'a key with no carrier'} (inferred)`;
  } else if (e.withoutPairEntry === false) entry = 'none';
  const f3 = !P.f3 ? '–' : P.f3.raised ? 'F3' : P.f3.left ? 'no: the picture left the profile' : `no: the plan was not above ${mbps(SLOW_LINK_PROFILE_BPS)} for ${F3_RUN_MS / 1000} s`;
  const Q = P.standingQueue;
  const relay = `relay ${P.relayTransport === null ? NO_LINE : P.relayTransport}`;
  const sq = Q
    ? `standing queue: queued ${Q.queued} of ${Q.samples} samples, standing ${dash(Q.standingP50Ms)}/${dash(Q.standingMaxMs)} ms, ${relay}`
    : `standing queue: –, ${relay}`;
  return [change, samples, bl, app, stalled, ratio, pic, nominal, entry, f3, sq];
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
  // Printed only when there is something to say, so a read of lines without
  // them is the report it was before, plus its new sections.
  if (P.deliveryLines) {
    L.push(`- ${P.sessionsWithDelivery} sessions carried \`path delivery\` lines (${P.deliveryLines} lines); the others read – in the delivery columns.`);
  }
  if (P.deliveryReplays) L.push(`- ${P.deliveryReplays} delivery lines repeated an earlier one of the same session and were skipped.`);
  if (P.sessionsWithStandingQueue) {
    L.push(`- ${P.sessionsWithStandingQueue} sessions carried the delivery line's standing-queue fields; the others read – in that column.`);
  }
  if (P.assumedLines) {
    L.push(
      `- Opens that passed over an entry with no measured provenance (the FR-148 P1 line): ${P.assumedLines}` +
        (P.assumedUnplaced ? `; ${P.assumedUnplaced} came before no pump start within ${OPEN_LAG_MS / 1000} s and belong to no session.` : '.'),
    );
  }
  L.push('');

  const f1 = r.flags.f1;
  const f2 = r.flags.f2;
  const f3 = r.flags.f3;
  L.push('## Flags', '');
  L.push(`**F1, opened below what the pair demonstrated: ${f1.verdict}.** ${f1.line}`, '');
  L.push(`| Yardstick | Judged | Opening < ${F1_RATIO} × (F1) | Opening < ${AC6_RATIO} × (AC6's line) |`);
  L.push('|---|---|---|---|');
  L.push(`| the pair's demonstrated rate in the read window | ${f1.judged} | ${f1.below} | ${f1.below85} |`);
  L.push(`| what the pair had demonstrated before the session opened | ${f1.judgedBefore} | ${f1.belowBefore} | ${f1.below85Before} |`);
  L.push(`| the pair's highest backlogged delivery sample in the read window (not F1's verdict) | ${f1.judgedBacklogged} | ${f1.belowBacklogged} | ${f1.below85Backlogged} |`);
  L.push('');
  L.push(`**F2, the plan moved and the encoder did not: ${f2.verdict}.** ${f2.line}`, '');
  L.push('| Sessions | Count |', '|---|---|');
  L.push(`| judged: at least ${F2_MIN_SESSION_MS / 60000} min, with a target and an encoder rate read | ${f2.judged} |`);
  L.push(`| the target's max at least ${F2_SPREAD} × its min | ${f2.spread} |`);
  L.push(`| … and the encoder's applied rate moved 0 times (F2) | ${f2.raised} |`);
  L.push('');
  L.push(`**F3, the slow-link profile never left: ${f3.verdict}.** ${f3.line}`, '');
  L.push('| Sessions | Count |', '|---|---|');
  L.push(`| judged: opened with the slow-link profile on, and lasted at least ${F3_MIN_SESSION_MS / 60000} min | ${f3.judged} |`);
  L.push(
    `| the plan stood above ${mbps(SLOW_LINK_PROFILE_BPS)} Mbit/s, where no open engages the profile, for at least ${F3_RUN_MS / 1000} s in a row | ${f3.held} |`,
  );
  L.push(`| … and no heartbeat's picture was both larger and faster than the opening (F3) | ${f3.raised} |`);
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

  L.push('## Pairs: what the path carried', '');
  L.push(
    '| Agent | User | Carrier | Sessions | Demonstrated | Backlogged max | Read in | Openings < 0.5 · < 0.85 × backlogged max | Changed path | F3 | Opened without the pair\'s entry (inferred) |',
  );
  L.push('|---|---|---|---|---|---|---|---|---|---|---|');
  for (const p of r.pairs) {
    const P = p.path;
    const ob = P.openingsAgainstBacklogged;
    const we = P.withoutPairEntry;
    L.push(
      `| ${p.agent} | ${dash(p.user)} | ${p.carrier} | ${p.sessions} | ${mbps(p.demonstrated)} | ${mbps(P.backloggedMaxBps)} | ${dash(P.backloggedReadIn)} | ` +
        `${ob.judged ? `${ob.below} · ${ob.below85} of ${ob.judged}` : '–'} | ${P.changedPath} | ${P.f3} | ${we.judged ? `${we.count} of ${we.judged}` : '–'} |`,
    );
  }
  if (!r.pairs.length) L.push('| – | – | – | 0 | – | – | – | – | 0 | 0 | – |');
  L.push('');

  L.push('## Devices: the relay transport and the standing queue', '');
  L.push(
    `| Agent | Sessions | Sessions by relay transport (udp, tcp, tls, none, unknown, no line) | Sessions with a standing queue (at least ${SQ_SESSION_MIN} queued samples), by carrier class |`,
  );
  L.push('|---|---|---|---|');
  for (const d of r.devices) {
    const t = d.relayTransport;
    const sq = d.standingQueue.length ? d.standingQueue.map((c) => `${c.carrier} ${c.standingQueue} of ${c.sessions}`).join(', ') : '–';
    L.push(`| ${d.agent} | ${d.sessions} | ${t.udp}, ${t.tcp}, ${t.tls}, ${t.none}, ${t.unknown}, ${t.noLine} | ${sq} |`);
  }
  if (!r.devices.length) L.push('| – | 0 | – | – |');
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

  L.push('## Sessions: the path', '');
  L.push(
    '| Start | Agent | Session | Carrier | Path change | Samples b · a · s | Backlogged min / mean / max | App-limited max | Stalled | ' +
      'Opening ÷ backlogged max | Picture | At nominal after | Entry at the open | F3 | Standing queue and relay |',
  );
  L.push('|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|');
  for (const s of r.sessions) L.push(`| ${minute(s.start)} | ${s.agent} | ${s.session} | ${s.carrier} | ${pathCells(s).join(' | ')} |`);
  if (!r.sessions.length) L.push('| – | – | – | – | – | – | – | – | – | – | – | – | – | – | – |');
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
      "Relay: that line's `relay_transport` on a newer agent, what carries the agent's own TURN allocation (`udp`, `tcp` or `tls`), " +
      "`none` where the agent's selected candidate is no relay, `unknown` where the agent could not say, and `no line` where the carrier line lacks the field or there is none. " +
      "A browser's own relay never reaches the agent: a `turn` session on it reads `none`, and its leg may be TURN over UDP, TCP or TLS.",
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
  L.push(
    '- Samples b · a · s: the `path delivery` lines (FR-148 P0, both DC pumps), about 2 s a sample: backlogged (bytes waited the whole sample and some were acknowledged), ' +
      'application-limited (the queue ran empty) and stalled (bytes waited, none acknowledged). They count on the side the session\'s carrier runs on. ' +
      "Backlogged min and max are single samples; the mean is over all their bytes, each line's mean weighted by its backlogged time, " +
      'which a line gives exactly when none of its samples is application-limited and otherwise is shared out by sample count. ' +
      'A session with no such line (an older agent, the RTP pump, `ROOMLERD_PATH_MODEL=off`) reads –.',
  );
  L.push(
    "- Backlogged max: the highest backlogged sample any of the pair's sessions read on its carrier, a rate the path carried for about 2 s while the sender had more to send. " +
      'It is not a capacity either: a sample partly stalled reads low, a viewer that stops reading holds the queue full, and the bytes are the whole association\'s ' +
      "(docs/rate-control.md § Delivery samples). F1's third row judges the opening against it; a pair with none is not judged there.",
  );
  L.push(
    '- Standing queue: on a newer agent the delivery line counts its queued samples, those whose least acknowledgement delay stood 250 ms or more over the session\'s least ' +
      'and that acknowledged enough bytes for that least to hold no delayed SACK (docs/rate-control.md § The standing queue). The row adds them over every line of the session, ' +
      "whatever its side, of all those lines' samples; the p50 and the max are of each line's highest standing. A session with no such line reads –. " +
      `Devices: per agent, its sessions by relay transport, and by carrier class the sessions with at least ${SQ_SESSION_MIN} queued samples, of those that carried the fields.`,
  );
  L.push(
    "- Path change: a flip of the FFmpeg heartbeat's `constrained` flag, or a carrier or ICE line that repeats with another value, at + seconds from the opening " +
      '(the first heartbeat). The session stays in the pair of its carrier line (the last one when it repeats), and its opening is judged there. Its demonstrated runs and its delivery lines ' +
      'count only on the side that carrier runs on, and a delivery line whose 10 s cross a flip counts nowhere. A VP9-444 pump has no flag: its windows and lines are never split by side.',
  );
  L.push(
    "- Picture and F3: the heartbeat's size and frame rate against the opening's. The picture left the profile at the first heartbeat both larger and faster. " +
      `F3 judges a session that opened with the slow-link profile and lasted ${F3_MIN_SESSION_MS / 60000} min: raised when its plan stood above ${mbps(SLOW_LINK_PROFILE_BPS)} Mbit/s ` +
      `(\`rate_profile::slow_link_profile_bps\`: an entry at or under it engages the profile) for ${F3_RUN_MS / 1000} s in a row, and the picture never left. ` +
      'A host that sets `ROOMLERD_SLOW_LINK_PROFILE_BPS` or `ROOMLERD_RELAY_MAX_KBPS` is read against the defaults, and a screen no larger than the profile never reads as left.',
  );
  L.push(
    `- At nominal after: for a session that opened on a relay below its nominal ${mbps(RELAY_NOMINAL_BPS)} Mbit/s (\`encode::relay_max_bps\`), ` +
      'the seconds from the opening to the first heartbeat on the relay whose plan stood at or above it; "left the relay first" where the session left it before that.',
  );
  L.push(
    '- Entry at the open: the entry the session opened from; "assumed …, passed over" where the FR-148 P1 line says the entry had no measured provenance; ' +
      "\"none, though the pair's write … left …\" where the pair's last write with evidence, in the memory's 7 days before the open, left an entry and the session read none. " +
      "That is inferred, as the memory trail is: the memory's key differed between the two sessions (another address, or the carrier tag's 400 ms lookup, " +
      '`overlay_carrier_tag`, gave none), no key was formed at that open (no nominated pair within 4 s), or the open ran in a process that keeps its memory ' +
      'in another data directory. The lines cannot tell which. It is judged where the memory is read: an FFmpeg pump that started on a relay.',
  );
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
  /** `pathFixtures`' agents: the relay it left, an open without the pair's
   *  entry, and the slow-link profile's edges. */
  leftAgent: hex24('f148', 'c0a004'),
  rekeyAgent: hex24('f148', 'c0a005'),
  profileAgent: hex24('f148', 'c0a006'),
  /** `standingFixtures`' agents: the standing queue and the relay transport. */
  queueAgent: hex24('f148', 'c0a007'),
  queueAgent2: hex24('f148', 'c0a008'),
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
  const key = o.seed || o.write || o.assumed ? `"ADDR-SECRET-P${o.tag ? '|' + o.tag : ''}"` : null;
  if (o.assumed) {
    // FR-148 P1: an entry with no measured provenance, read at the open and
    // passed over. The line carries the key and no session id.
    L.push(
      fxLine(
        t + 600,
        "FR-148 P1 rate memory: the pair's entry has no measured provenance, so it is read as assumed: no seed and no slow-link profile, the pair opens as one never seen",
        [
          ['peer', key],
          ['assumed_bps', String(o.assumed.bps)],
          ['age_s', String(o.assumed.ageS)],
        ],
      ),
    );
  }
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
      fxLine(
        t + 2600,
        'session carrier recorded (FR-102 P9c)',
        [['session_id', o.sid], ['carrier', q(o.carrier)]]
          // FR-148 P0: what carries the agent's own relay, on a newer agent.
          .concat(o.relayTransport ? [['relay_transport', q(o.relayTransport)]] : [])
          .concat([
            ['local_typ', 'Host'],
            ['local_addr', 'ADDR-SECRET-L'],
            ['remote_typ', 'Host'],
            ['remote_addr', 'ADDR-SECRET-R'],
            ['overlay', 'None'],
          ]),
      ),
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
    // The delivery line follows the heartbeat that closed its last sample:
    // `n` the samples (backlogged, application-limited, stalled), `bl` the
    // backlogged rates (lowest, mean, highest) or none, `app` the highest
    // application-limited rate or none; and on a newer agent `q`, the standing
    // queue's fields (`ack` and `standing` tuples or none, `queued`). The
    // shapes are media_delivery.rs's.
    const delivery = () => {
      if (!w.dl) return;
      const d = w.dl;
      const g = d.q;
      L.push(
        fxLine(
          ts + 1,
          'path delivery (FR-148 P0, shadow, ≈10s)',
          [
            ['session_id', o.sid],
            ['span_ms', String(d.span === undefined ? 10000 : d.span)],
            ['delivery_samples', `(${d.n.join(', ')})`],
            ['backlogged_bps', d.bl ? `Some((${d.bl.join(', ')}))` : 'None'],
            ['app_limited_bps', some(d.app)],
            ['stalled_ms', String(d.stalledMs || 0)],
            ['acked_bytes', String(d.acked === undefined ? 1200000 : d.acked)],
          ].concat(
            g
              ? [
                  ['ack_ms', g.ack ? `Some((${g.ack.join(', ')}))` : 'None'],
                  ['standing_ms', g.standing ? `Some((${g.standing.join(', ')}))` : 'None'],
                  ['queued', String(g.queued)],
                  ['inflight', '(1200, 60000)'],
                  ['cwnd', '(4380, 131072)'],
                  ['ssthresh', '1048576'],
                  ['rwnd_low', '0'],
                  ['pending_high', '131072'],
                  ['loss', '(0, 0)'],
                ]
              : [],
          ),
        ),
      );
    };
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
      delivery();
      return;
    }
    L.push(
      fxLine(ts, 'FFmpeg DC pump heartbeat (≈2s window)', [
        ['session_id', o.sid],
        ['codec_label', q(o.codec)],
        ['target_fps', String(w.fps === undefined ? o.fps : w.fps)],
        ['constrained', String(w.c === undefined ? !!o.constrained : w.c)],
        ['target_bps', String(w.target)],
        ['width', String(w.width === undefined ? o.w : w.width)],
        ['height', String(w.height === undefined ? o.h : w.height)],
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
    delivery();
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
    // Any other line, `dt` ms after this heartbeat and before the next.
    if (w.extra) for (const [dt, msg, fields] of w.extra) L.push(fxLine(ts + dt, msg, [['session_id', o.sid]].concat(fields)));
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

/** Two plans of 2026-11-16 as [target_bps, seconds from the opening]. Both
 *  opened at 0.282 Mbit/s with the slow-link profile on. The first stood at the
 *  relay's nominal 3.00 from +126 s; the second also left the relay at +1,092 s
 *  and climbed to 15.36 on the direct path, still at 1280×800. */
const PLAN_OPENED_LOW = [
  [281732, 0], [239472, 2], [269406, 6.1], [303081, 12.2], [340966, 16.2], [383586, 22.3], [431534, 26.4],
  [485475, 32.5], [412590, 34.5], [467718, 36.5], [526182, 40.5], [591954, 44.6], [665948, 48.7], [749191, 54.8],
  [842839, 58.9], [948193, 64.9], [1066717, 68.9], [1200056, 75], [1350063, 79], [1518820, 85.1], [1706320, 89.1],
  [1893820, 95.2], [2081320, 99.2], [2268820, 105.3], [2456320, 109.4], [2643820, 115.4], [2831320, 119.5], [3000000, 125.6],
];
const PLAN_LEFT_RELAY = [
  [281732, 0], [316948, 4.1], [356566, 10.1], [303025, 12.1], [257550, 14.2], [289743, 18.2], [299340, 22.3],
  [336757, 24.3], [378851, 28.4], [426207, 34.5], [479482, 38.6], [539417, 44.7], [606844, 48.8], [682699, 52.8],
  [768036, 58.9], [864040, 63], [972045, 69.1], [1093550, 73.2], [1230243, 79.3], [1384023, 83.4], [1557025, 89.5],
  [1744525, 93.5], [1932025, 99.6], [2119525, 103.6], [2307025, 109.7], [2494525, 113.7], [2682025, 119.7],
  [2869525, 123.8], [3000000, 129.9], [2550000, 1083.4], [2737500, 1087.5], [2977500, 1093.6], [3937500, 1097.7],
  [4897500, 1103.7], [5857500, 1107.8], [6817500, 1113.8], [7777500, 1117.9], [8737500, 1123.9], [9697500, 1128],
  [10657500, 1134.1], [11617500, 1138.2], [12577500, 1144.3], [13537500, 1148.4], [14497500, 1154.4], [15360000, 1158.5],
];

/** The plan's target `sec` seconds from the opening: the last step at or before it. */
function planAt(steps, sec) {
  let v = steps[0][0];
  for (const [bps, s] of steps) if (s <= sec + 1e-6) v = bps;
  return v;
}

/**
 * FR-148 P0's path questions as rolling-log lines with invented ids, beside
 * `fixtures`, whose chains stay as they were so the two sets also show that
 * the new lines change nothing the old ones said.
 *
 * - c0a004, one pair on the mesh relay (DERP): d40001 opened at 0.282 with the
 *   profile and stood at the relay's nominal rate from +126 s, with no delivery
 *   line (an older agent); d40002 did the same, left the relay at +1,092 s and
 *   kept 1280×800 for 20 minutes at up to 15.36 Mbit/s (F3), with delivery
 *   lines of every class on the relay, one across the change and two after it;
 *   d40003 passed over an assumed entry and opened at 2.55.
 * - c0a005, one pair on DERP: d50001 wrote a measured 2.40 in the evening,
 *   d50002 opened the next morning without it and wrote under a key with no
 *   carrier half, and d50003 read the evening's entry again.
 * - c0a006, one pair on TURN, the profile's edges: d60001's plan never stood
 *   above the profile's threshold, and d60002's picture left the profile
 *   through a new pump.
 */
function pathFixtures() {
  const out = [];
  const push = (agent, lines) => {
    for (const l of lines) out.push([agent, l]);
  };
  const at = (s) => Date.parse(s);
  const n = (count, f) => Array.from({ length: count }, (_, i) => f(i));
  const derp = { carrier: 'derp', tag: 'relay:derp/tcp', relay: false, overlay: true, constrained: true, enc: 'av1_nvenc', codec: 'AV1' };
  const profile = { seed: 281732, slow: true, w: 1280, h: 800, fps: 15, opener: { growth: 0, measured: null } };

  push(
    FX.leftAgent,
    fxSession(
      Object.assign({}, derp, profile, {
        sid: hex24('f148', 'd40001'),
        user: FX.op,
        at: at('2026-11-16T08:10:00Z'),
        // A still screen: the bytes stay far under the plan.
        wins: n(72, (i) => {
          const target = planAt(PLAN_OPENED_LOW, 2 * i);
          return { target, rate: Math.min(Math.round((target * 0.9) / 4) * 4, 440000 + (i % 5) * 8000), age: 55 + (i % 9), floor: 40, qp: 150 };
        }),
      }),
    ),
  );

  // d40002's delivery lines, by heartbeat: every class and shape on the relay,
  // one whose 10 s cross the change, and two on the direct path.
  const dl = {
    5: { n: [0, 5, 0], bl: null, app: 310112, acked: 387640 },
    10: { n: [5, 0, 0], bl: [2390112, 2401000, 2410334], app: null },
    15: { n: [2, 3, 0], bl: [2390112, 2401000, 2410334], app: 310112 },
    20: { n: [0, 0, 5], bl: null, app: null, stalledMs: 10000, acked: 0 },
    25: { n: [3, 1, 1], bl: [2950000, 3100000, 3212000], app: 402000, stalledMs: 2000 },
    550: { n: [5, 0, 0], bl: [7200000, 8400000, 9800000], app: null },
    555: { n: [5, 0, 0], bl: [10100000, 11200000, 12480000], app: null },
    560: { n: [4, 1, 0], bl: [9000000, 10500000, 11900000], app: 5200000 },
  };
  push(
    FX.leftAgent,
    fxSession(
      Object.assign({}, derp, profile, {
        sid: hex24('f148', 'd40002'),
        user: FX.op,
        at: at('2026-11-16T09:00:00Z'),
        wins: n(601, (i) => {
          const sec = 2 * i;
          const target = planAt(PLAN_LEFT_RELAY, sec);
          const direct = sec >= 1092;
          const w = {
            target,
            // A still screen on the relay holds the demonstrated rate at 0.616;
            // the direct path carries more of the plan.
            rate: direct ? Math.round((target * 0.6) / 4) * 4 : Math.min(Math.round((target * 0.9) / 4) * 4, 616000 + (i % 7) * 20000),
            age: 55 + (i % 9),
            floor: 40,
            qp: 150,
          };
          if (direct) w.c = false;
          // The pump's frame rate follows the transport; its size cap does not.
          if (sec >= 1098) w.fps = 60;
          if (dl[i]) w.dl = dl[i];
          return w;
        }),
        write: { evidence: null, kept: 281732 },
      }),
    ),
  );

  push(
    FX.leftAgent,
    fxSession(
      Object.assign({}, derp, {
        sid: hex24('f148', 'd40003'),
        user: FX.op,
        at: at('2026-11-16T10:30:00Z'),
        assumed: { bps: 281732, ageS: 432000 },
        w: 1920,
        h: 1200,
        fps: 30,
        opener: { growth: 0, measured: null },
        wins: n(30, (i) => ({
          target: i < 20 ? 2550000 : 3000000,
          rate: 500000 + (i % 3) * 10000,
          age: 55,
          floor: 40,
          qp: 30,
          dl: i === 10 ? { n: [5, 0, 0], bl: [2600000, 2700000, 2800000], app: null } : undefined,
        })),
      }),
    ),
  );

  const opensAt = (sid, when, extra) =>
    fxSession(
      Object.assign({}, derp, { sid: hex24('f148', sid), user: FX.op, at: at(when), w: 1920, h: 1200, fps: 30, opener: { growth: 0, measured: null } }, extra),
    );
  push(
    FX.rekeyAgent,
    opensAt('d50001', '2026-11-15T20:10:00Z', {
      wins: n(40, (i) => ({ target: i < 7 ? 2550000 : 3000000, rate: 2200000 + (i % 4) * 50000, age: 58, floor: 40, qp: 24, goodput: i === 39 ? 2400000 : undefined })),
      write: { evidence: 2400000, kept: 2400000 },
    }),
  );
  push(
    FX.rekeyAgent,
    opensAt('d50002', '2026-11-16T09:30:00Z', {
      tag: null,
      wins: n(40, (i) => ({ target: i < 7 ? 2550000 : 3000000, rate: 2500000 + (i % 4) * 50000, age: 58, floor: 40, qp: 24, goodput: i === 39 ? 2900000 : undefined })),
      write: { evidence: 2900000, kept: 2900000 },
    }),
  );
  push(
    FX.rekeyAgent,
    opensAt('d50003', '2026-11-16T15:00:00Z', {
      seed: 2400000,
      wins: n(20, (i) => ({ target: 2040000 + i * 40000, rate: 1900000, age: 58, floor: 40, qp: 24 })),
    }),
  );

  const turn = { carrier: 'turn', relay: true, overlay: false, constrained: true, enc: 'av1_nvenc', codec: 'AV1', seed: 600000, slow: true, w: 1280, h: 720, fps: 15 };
  push(
    FX.profileAgent,
    fxSession(
      Object.assign({}, turn, {
        sid: hex24('f148', 'd60001'),
        user: FX.other,
        at: at('2026-11-15T14:00:00Z'),
        // At exactly the threshold an open still engages the profile.
        wins: n(181, (i) => ({ target: Math.min(1000000, 600000 + i * 20000), rate: 450000, age: 60, floor: 40, qp: 120 })),
      }),
    ),
  );
  push(
    FX.profileAgent,
    fxSession(
      Object.assign({}, turn, {
        sid: hex24('f148', 'd60002'),
        user: FX.other,
        at: at('2026-11-15T15:00:00Z'),
        wins: n(181, (i) => {
          const w = { target: i < 10 ? 600000 : 2000000, rate: i < 10 ? 500000 : 1700000, age: 60, floor: 40, qp: 60 };
          // A new pump, which reads its ICE path again (unchanged) and opens
          // without the profile.
          if (i === 99) {
            w.extra = [
              [900, 'per-session ICE path detected (adaptive bitrate)', [['relay', 'true'], ['local_typ', 'Relay'], ['local_addr', 'ADDR-SECRET-L:50000']]],
              [1000, 'FFmpeg DC pump starting', [['codec_label', '"AV1"'], ['target_fps', '30'], ['slow_link', 'false']]],
            ];
          }
          if (i === 100) w.restart = 40000;
          if (i >= 100) Object.assign(w, { width: 1920, height: 1080, fps: 30 });
          return w;
        }),
      }),
    ),
  );
  return out;
}

/**
 * FR-148 P0's standing queue and relay transport, as rolling-log lines with
 * invented ids, beside the other sets, whose lines stay as they were. Every
 * session is under 30 s, and no window reads an age, so none demonstrates a
 * rate or is judged by a flag.
 *
 * - c0a007: f70001 on TURN over TLS (the agent's own relay) queued 8 of its 15
 *   samples, its first line loaded with no queue and the two after it queued;
 *   f70002 on TURN over UDP queued none; f70003, direct, queued 1; f70004 is an
 *   older agent's, whose carrier line has no relay transport and whose
 *   delivery lines no standing queue.
 * - c0a008: f80001 on the device's own TURN, whose relay transport the agent
 *   could not say, on a still screen; f80002 has no carrier line, and queued 4.
 */
function standingFixtures() {
  const out = [];
  const push = (agent, lines) => {
    for (const l of lines) out.push([agent, l]);
  };
  const at = (s) => Date.parse(s);
  const app = (queue) => ({ n: [0, 5, 0], bl: null, app: 1900000, q: queue });
  const session = (agent, sid, when, o, lines) =>
    push(
      agent,
      fxSession(
        Object.assign({ sid: hex24('f148', sid), user: FX.other, at: at(when), enc: 'av1_nvenc', codec: 'AV1', w: 1920, h: 1080, fps: 30 }, o, {
          wins: Array.from({ length: 15 }, (_, i) => ({ target: 2000000, rate: 1800000, age: null, floor: null, qp: 30, dl: lines[i] })),
        }),
      ),
    );
  const turn = { carrier: 'turn', relay: true, constrained: true };
  session(FX.queueAgent, 'f70001', '2026-11-17T09:00:00Z', Object.assign({ relayTransport: 'tls' }, turn), {
    4: app({ ack: [61, 61, 64], standing: [0, 3], queued: 0 }),
    9: app({ ack: [61, 240, 1318], standing: [179, 1257], queued: 3 }),
    14: app({ ack: [61, 361, 2271], standing: [300, 2210], queued: 5 }),
  });
  session(FX.queueAgent, 'f70002', '2026-11-17T10:00:00Z', Object.assign({ relayTransport: 'udp' }, turn), {
    4: app({ ack: [58, 58, 70], standing: [0, 12], queued: 0 }),
    9: app({ ack: [58, 59, 119], standing: [1, 61], queued: 0 }),
  });
  session(FX.queueAgent, 'f70003', '2026-11-17T11:00:00Z', { carrier: 'direct', relay: false, constrained: false, relayTransport: 'none' }, {
    4: app({ ack: [20, 20, 50], standing: [0, 30], queued: 0 }),
    9: app({ ack: [20, 22, 280], standing: [2, 260], queued: 1 }),
    14: app({ ack: [20, 20, 60], standing: [0, 40], queued: 0 }),
  });
  session(FX.queueAgent, 'f70004', '2026-11-17T12:00:00Z', turn, { 4: app(undefined), 9: app(undefined) });
  session(FX.queueAgent2, 'f80001', '2026-11-17T13:00:00Z', { carrier: 'turn_local', relay: true, constrained: false, relayTransport: 'unknown' }, {
    4: { n: [0, 5, 0], bl: null, app: 16384, acked: 20480, q: { ack: null, standing: null, queued: 0 } },
  });
  session(FX.queueAgent2, 'f80002', '2026-11-17T14:00:00Z', { relay: true, constrained: true }, {
    4: app({ ack: [70, 400, 900], standing: [330, 830], queued: 4 }),
  });
  return out;
}

/** Every fixture: the reference chains, the path questions and the standing queue. */
function allFixtures() {
  return fixtures().concat(pathFixtures(), standingFixtures());
}

/** The fixtures as `agent_logs` batches, canonical EJSON, one string each: every
 *  agent's lines in time order, as its uploader would have sent them. */
function fixtureSeed() {
  const byAgent = new Map();
  for (const [agent, l] of allFixtures()) {
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
  // The delivery line's shapes: the log prints `?Debug`, agent_logs stores that
  // text, and a printed row has `Some(…)` stripped by value().
  ok(JSON.stringify(triple('(2, 3, 0)')) === '[2,3,0]' && JSON.stringify(triple('(2,3,0)')) === '[2,3,0]', 'delivery_samples: the tuple reads, with or without spaces');
  ok(
    JSON.stringify(triple('Some((2390112, 2401000, 2410334))')) === '[2390112,2401000,2410334]' && triple('None') === null && triple(null) === null,
    'backlogged_bps: Some((lowest, mean, highest)) reads, and None is no rate',
  );
  ok(JSON.stringify(triple(value('Some((1, 2, 3))'))) === '[1,2,3]' && value('Some((1, 2, 3))') === '(1, 2, 3)', 'a printed row keeps the tuple: value() strips Some, triple() reads the rest');
  ok(num('Some(310112)') === 310112 && num('None') === null && num({ toNumber: () => 10012 }) === 10012, 'app_limited_bps: Some(n) and None; span_ms and stalled_ms as Int64');
  ok(triple('(2, 3)') === null && triple('Some(5)') === null && triple('(a, b, c)') === null && triple('') === null, 'anything else is no tuple');

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

  // --- FR-148 P0: what the path carried, a change of path, the profile, the entry at the open
  const pfx = pathFixtures();
  const rp = runPairs(pfx);
  const pc = sessionOf(rp, 'd40001');
  const pb = sessionOf(rp, 'd40002');
  const pe = sessionOf(rp, 'd40003');
  const leftPair = rp.pairs.find((p) => p.agent === 'c0a004');
  const cells = (s) => pathCells(s);

  // The delivery samples, one class a line, then all of them in one session.
  const dlOf = (sid, d) => runPairs(edge(sid, flat(6, (i) => (i === 5 ? { dl: d } : {})))).sessions[0].path.delivery;
  const appOnly = dlOf('e50001', { n: [0, 5, 0], bl: null, app: 310112 });
  ok(
    appOnly.samples.appLimited === 5 && appOnly.samples.backlogged === 0 && appOnly.backloggedBps === null && appOnly.appLimitedMaxBps === 310112,
    'an application-limited line (backlogged_bps=None): no backlogged rate, and its highest application-limited rate',
    appOnly,
  );
  const blOnly = dlOf('e50002', { n: [5, 0, 0], bl: [2390112, 2401000, 2410334], app: null });
  ok(
    blOnly.backloggedBps.min === 2390112 && blOnly.backloggedBps.mean === 2401000 && blOnly.backloggedBps.max === 2410334 && blOnly.appLimitedMaxBps === null,
    'a backlogged line (app_limited_bps=None): its lowest, mean and highest rates',
    blOnly,
  );
  const stOnly = dlOf('e50003', { n: [0, 0, 5], bl: null, app: null, stalledMs: 10000, acked: 0 });
  ok(stOnly.samples.stalled === 5 && stOnly.stalledS === 10 && stOnly.backloggedBps === null && stOnly.appLimitedMaxBps === null, 'a stalled line: its time, and no rate at all', stOnly);
  const weighed = runPairs(
    edge('e50004', flat(11, (i) => (i === 5 ? { dl: { n: [5, 0, 0], bl: [1000000, 2000000, 3000000] } } : i === 10 ? { dl: { n: [1, 4, 0], bl: [4000000, 4000000, 4000000], app: 100000 } } : {}))),
  ).sessions[0].path.delivery;
  ok(weighed.backloggedBps.mean === 2333333, 'the mean is over all the bytes: each line weighs by its backlogged time, shared out by count beside application-limited samples', weighed.backloggedBps);
  const dlb = pb.path.delivery;
  ok(dlb && dlb.lines === 5 && dlb.samples.backlogged === 10 && dlb.samples.appLimited === 9 && dlb.samples.stalled === 6, 'd40002 on the relay: its samples by class', dlb && dlb.samples);
  ok(
    dlb.backloggedBps.min === 2390112 && dlb.backloggedBps.mean === 2610700 && dlb.backloggedBps.max === 3212000 && dlb.appLimitedMaxBps === 402000 && dlb.stalledS === 12,
    'its backlogged rates (lowest, over all their bytes, highest), its highest application-limited rate, its stalled time',
    dlb,
  );
  ok(cells(pb)[1] === '10 · 9 · 6' && cells(pb)[2] === '2.39 / 2.61 / 3.21' && cells(pb)[3] === '0.402' && cells(pb)[4] === '12.0 s', 'and its row', cells(pb).slice(1, 5));
  ok(pc.path.delivery === null && cells(pc).slice(1, 5).every((c) => c === '–'), 'a session with no delivery line reads –, never 0', cells(pc));
  ok(rp.provenance.deliveryLines === 9 && rp.provenance.sessionsWithDelivery === 2, 'the lines are counted, and the sessions that carried them', rp.provenance);
  const vp9dl = runPairs(edge('e50005', flat(31, (i) => (i === 10 ? { dl: { n: [4, 1, 0], bl: [3000000, 3100000, 3200000], app: 900000 } } : {})), { vp9: true, codec: 'VP9' })).sessions[0];
  ok(vp9dl.encoder === 'vp9-444' && vp9dl.path.delivery && vp9dl.path.delivery.backloggedBps.max === 3200000, "a VP9-444 pump's lines count for its pair: it has no flag to split them by", vp9dl.path.delivery);
  // An FFmpeg pump left the relay, then a VP9-444 pump took over: the old pump's
  // flag says nothing of the new pump's path.
  const mixed = runPairs(
    edge('e50006', flat(31, (i) => (i >= 10 ? { c: false } : {}))).concat(
      edge('e50006', flat(31, (i) => (i === 10 ? { dl: { n: [4, 1, 0], bl: [3000000, 3100000, 3300000], app: 900000 } } : {})), {
        vp9: true,
        codec: 'VP9',
        at: Date.parse('2026-11-15T10:01:30Z'),
      }),
    ),
  ).sessions[0];
  ok(
    mixed.path.changes.length === 1 && mixed.path.delivery && mixed.path.delivery.backloggedBps.max === 3300000 && mixed.path.deliveryOtherSide === null,
    "a VP9-444 pump after an FFmpeg pump that left the relay: its line counts for the pair, not on the old pump's side",
    mixed.path,
  );
  const dup = runPairs(pfx.concat(pfx.filter(([, l]) => /path delivery/.test(l))));
  ok(dup.provenance.deliveryReplays === 9 && JSON.stringify(dup.sessions) === JSON.stringify(rp.sessions), 'a delivery line uploaded twice is a replay, and changes nothing', dup.provenance);

  // The pair's highest backlogged rate, and F1's backlogged row.
  ok(leftPair.path.backloggedMaxBps === 3212000 && leftPair.path.backloggedReadIn === 'd40002', "the pair's highest backlogged rate: the highest any of its sessions read on its carrier, and where", leftPair.path);
  ok(leftPair.demonstrated === 616000, 'beside the 0.616 a still screen held the demonstrated rate at', leftPair.demonstrated);
  ok(
    pc.flags.f1Backlogged === true && pb.flags.f1Backlogged === true && pe.flags.f1Backlogged === false && pe.flags.f1BackloggedAt85 === true,
    'against it, 0.282 opened under half, and 2.55 under 0.85 × only',
    [pc.flags, pe.flags],
  );
  ok(pe.flags.f1 === false && pc.flags.f1 === true && pe.path.openingOfBackloggedMax === 0.794, "F1 keeps its yardstick: against 0.616, 2.55 is no F1", pe.flags);
  ok(
    rp.flags.f1.judgedBacklogged === 3 && rp.flags.f1.belowBacklogged === 2 && rp.flags.f1.below85Backlogged === 3 && leftPair.path.openingsAgainstBacklogged.below === 2,
    "F1's table counts them on its own row",
    rp.flags.f1,
  );
  const q2 = sessionOf(rp, 'd50002');
  const qPair = rp.pairs.find((p) => p.agent === 'c0a005');
  ok(q2.flags.f1Backlogged === null && qPair.path.backloggedMaxBps === null && qPair.path.openingsAgainstBacklogged.judged === 0, 'a pair with no backlogged sample is not judged on that row');
  ok(/\| the pair's highest backlogged delivery sample in the read window \(not F1's verdict\) \| 3 \| 2 \| 3 \|/.test(render(rp)), "the row renders under F1's two");

  // A change of path inside a session, and how its windows are split.
  ok(
    pb.path.changeCount === 1 && pb.path.changes[0].afterS === 1092 && pb.path.changes[0].change === 'left the relay' && pb.path.changes[0].seenIn === 'pump flag',
    "d40002 left the relay at +1,092 s: the pump's flag says so",
    pb.path.changes,
  );
  ok(cells(pb)[0] === 'left the relay at +1,092 s; on the other side, backlogged max 12.48; 1 delivery line across it, counted nowhere', 'its row', cells(pb)[0]);
  ok(pb.carrier === 'mesh-relayed/derp' && pb.flags.f1 === true && pb.opening.bps === 281732, "it stays in its opening carrier's pair, and its opening is judged there");
  ok(pb.demonstrated.session === 616000, 'its demonstrated rate counts the windows on the relay only, not the 9 Mbit/s after it', pb.demonstrated);
  ok(
    pb.path.deliveryOtherSide.lines === 2 && pb.path.deliveryOtherSide.backloggedBps.max === 12480000 && pb.path.deliveryAcrossChange === 1 && leftPair.path.backloggedMaxBps === 3212000,
    'its delivery lines split the same way: 5 on the relay count, 2 after it and 1 across it do not',
    pb.path,
  );
  ok(leftPair.path.changedPath === 1 && rp.provenance.sessionsWithPathChange === 1, 'the pair counts one session that changed path');
  ok(sessionOf(rp, 'd60002').path.changeCount === 0, "a new pump's ICE line with the same path is no change");
  const moved = runPairs(
    edge('e60001', flat(20, (i) => {
      if (i === 9) {
        return {
          extra: [
            [500, 'per-session ICE path detected (adaptive bitrate)', [['relay', 'false'], ['local_typ', 'Host']]],
            [600, 'session carrier recorded (FR-102 P9c)', [['carrier', '"direct"']]],
          ],
        };
      }
      return i >= 10 ? { c: false } : {};
    })),
  ).sessions[0];
  ok(
    moved.path.changeCount === 3 &&
      moved.path.changes.map((c) => c.seenIn).join(',') === 'ICE line,carrier line,pump flag' &&
      moved.path.changes[1].change === 'carrier turn → direct' &&
      cells(moved)[0] === 'left TURN at +19 s (ICE line) (+2 more)',
    'an ICE line and a carrier line that repeat with another value are changes too, each with its source',
    moved.path.changes,
  );
  ok(moved.carrier === 'direct', 'the session stays in the pair of its last carrier line, as before');

  // F3: the slow-link profile never left.
  ok(pb.flags.f3 === true && pb.path.f3.held && !pb.path.f3.left && pb.path.planAboveProfileS === 1126, 'F3 on d40002: its plan stood above 1.00 for 1,126 s and its picture stayed in the profile', pb.path.f3);
  ok(
    pb.path.picture.longEdgeMax === 1280 && pb.path.picture.fpsUpAfterS === 1098 && pb.path.picture.leftProfileAfterS === null && cells(pb)[6] === '1280×800 · 15 fps; 60 fps from +1,098 s',
    'its frame rate rose to 60 on the direct path and its size never did: the profile did not leave',
    pb.path.picture,
  );
  const r1 = sessionOf(rp, 'd60001');
  const r2 = sessionOf(rp, 'd60002');
  ok(r1.path.f3 && r1.path.f3.held === false && r1.flags.f3 === false && r1.path.planAboveProfileS === null, 'no F3 where the plan never stood above the threshold (at exactly 1,000,000 an open engages the profile)', r1.path.f3);
  ok(
    r2.path.f3 && r2.path.f3.held && r2.path.f3.left && r2.flags.f3 === false && r2.path.picture.leftProfileAfterS === 200 && r2.pumps === 2,
    'no F3 where the picture left the profile: a new pump at 1920×1080 and 30 fps',
    r2.path,
  );
  ok(pc.path.f3 === null && pc.flags.f3 === null && pc.durationS < 300, 'too short: a session under 5 min is not judged', pc.durationS);
  ok(rp.flags.f3.judged === 3 && rp.flags.f3.held === 2 && rp.flags.f3.raised === 1 && rp.flags.f3.verdict === 'RAISED', 'F3 RAISED: 1 of 3 judged', rp.flags.f3);
  ok(cells(r1)[9] === 'no: the plan was not above 1.00 for 60 s' && cells(r2)[9] === 'no: the picture left the profile' && cells(pb)[9] === 'F3' && cells(pc)[9] === '–', 'each reason is in the row');
  const f3 = (o) => judgeF3(Object.assign({ profile: true, durationMs: F3_MIN_SESSION_MS, openSized: true, runMs: F3_RUN_MS, left: false }, o));
  ok(f3({}).raised === true, 'exactly 5 min, and exactly 60 s above: raised');
  ok(f3({ runMs: F3_RUN_MS - 1 }).held === false && f3({ runMs: F3_RUN_MS - 1 }).raised === false, 'a run a millisecond short is not held');
  ok(f3({ durationMs: F3_MIN_SESSION_MS - 1 }) === null && f3({ profile: false }) === null && f3({ openSized: false }) === null, 'a shorter session, one without the profile, one with no size read: not judged');
  ok(f3({ left: true }).raised === false, 'a picture that left: not raised');
  const prof = { seed: 600000, slow: true, w: 1280, h: 720, fps: 15 };
  const run60 = runPairs(edge('e70001', flat(151, (i) => ({ target: i <= 30 ? 1000001 : 900000 })), prof)).sessions[0];
  ok(run60.durationS >= 300 && run60.path.planAboveProfileS === 60 && run60.flags.f3 === true, '31 heartbeats 2 s apart above it are a 60 s run', run60.path);
  const dip = runPairs(edge('e70002', flat(151, (i) => ({ target: i <= 30 && i !== 15 ? 1000001 : 900000 })), prof)).sessions[0];
  ok(dip.path.planAboveProfileS === 28 && dip.flags.f3 === false, 'a heartbeat at or under the threshold breaks the run', dip.path.planAboveProfileS);
  const hole = runPairs(edge('e70003', flat(151, (i) => ({ target: i <= 30 ? 1000001 : 900000, gap: i === 15 ? 12000 : undefined })), prof)).sessions[0];
  ok(hole.path.planAboveProfileS === 30 && hole.flags.f3 === false, 'a hole in the log breaks it', hole.path.planAboveProfileS);
  const faster = runPairs(edge('e70004', flat(151, (i) => ({ target: 2000000, fps: i >= 50 ? 30 : undefined })), prof)).sessions[0];
  const larger = runPairs(edge('e70005', flat(151, (i) => ({ target: 2000000, width: i >= 50 ? 1920 : undefined, height: i >= 50 ? 1080 : undefined })), prof)).sessions[0];
  ok(faster.flags.f3 === true && larger.flags.f3 === true, 'faster alone, or larger alone, has not left the profile: AC14 brings both back');

  // At nominal after.
  ok(pc.path.atNominal.judged && pc.path.atNominal.afterS === 126 && cells(pc)[7] === '126 s', "d40001's plan stood at the relay's nominal 3.00 from +126 s", pc.path.atNominal);
  ok(pb.path.atNominal.afterS === 130 && pe.path.atNominal.afterS === 40, "d40002's from +130 s, and d40003's, opened at 2.55, from +40 s");
  ok(r1.path.atNominal.judged && r1.path.atNominal.afterS === null && cells(r1)[7] === 'never', 'a plan that never got there reads never');
  const leftFirst = runPairs(edge('e60002', flat(20, (i) => ({ target: 2000000, c: i >= 5 ? false : undefined })))).sessions[0];
  ok(leftFirst.path.atNominal.leftRelayFirst === true && cells(leftFirst)[7] === 'left the relay first', 'one that left the relay before it got there says so', leftFirst.path.atNominal);
  ok(sessionOf(r, 'c30001').path.atNominal.judged === false && cells(sessionOf(r, 'c30001'))[7] === '–', 'a session that opened direct is not judged');

  // The entry at the open.
  const ea = pe.path.entryAtOpen;
  ok(ea.assumed && ea.assumed.bps === 281732 && ea.assumed.ageS === 432000 && ea.assumed.keyCarrier === 'relay:derp/tcp' && ea.assumed.passedOver, 'd40003 passed over an assumed entry: its rate, its age, its key carrier', ea);
  ok(cells(pe)[8] === 'assumed 0.282, passed over' && pe.entryBps === null && ea.seededBps === null, '"assumed 0.282, passed over", and it is not the entry the session opened from');
  ok(rp.provenance.assumedLines === 1 && rp.provenance.assumedUnplaced === 0, 'the line goes to the pump start that follows it');
  const lone = runPairs([
    [
      FX.leftAgent,
      fxLine(Date.parse('2026-11-16T11:00:00Z'), "FR-148 P1 rate memory: the pair's entry has no measured provenance, so it is read as assumed: no seed and no slow-link profile, the pair opens as one never seen", [
        ['peer', '"ADDR-SECRET-P|relay:derp/tcp"'],
        ['assumed_bps', '281732'],
        ['age_s', '432000'],
      ]),
    ],
  ]);
  ok(lone.provenance.assumedUnplaced === 1 && /belong to no session/.test(render(lone)), 'one with no pump start after it belongs to no session, and the report says so');
  const we = q2.path.entryAtOpen.withoutPairEntry;
  ok(we && we.keptBps === 2400000 && we.keyCarrier === 'relay:derp/tcp' && we.inferred === true, "d50002 opened without the entry the pair's evening write left", q2.path.entryAtOpen);
  ok(cells(q2)[8] === "none, though the pair's write of 2026-11-15 20:11Z left 2.40 under relay:derp/tcp (inferred)", 'its cell', cells(q2)[8]);
  ok(qPair.path.withoutPairEntry.judged === 3 && qPair.path.withoutPairEntry.count === 1 && qPair.path.withoutPairEntry.inferred === true, 'per pair: 1 of 3, marked inferred', qPair.path.withoutPairEntry);
  ok(
    sessionOf(rp, 'd50001').path.entryAtOpen.withoutPairEntry === false &&
      sessionOf(rp, 'd50003').path.entryAtOpen.withoutPairEntry === false &&
      ea.withoutPairEntry === false &&
      leftPair.path.withoutPairEntry.count === 0,
    'not counted: the first session, one that read the entry, one that passed it over as assumed',
  );
  ok(sessionOf(r, 'a20002').path.entryAtOpen.withoutPairEntry === false, 'nor the session after the entry aged out: a write with no evidence dates nothing');
  ok(sessionOf(r, 'c30001').path.entryAtOpen.withoutPairEntry === null, 'a session that opened direct reads no memory, and is not judged');
  const T = Date.parse('2026-11-09T10:00:00Z');
  const afterWrite = (ms) =>
    runPairs(edge('e80001', flat(16), { at: T, write: { evidence: 2400000, kept: 2400000 } }).concat(edge('e80002', flat(16), { at: T + 33300 + MEMORY_TTL_MS - 800 + ms })));
  ok(sessionOf(afterWrite(0), 'e80002').path.entryAtOpen.withoutPairEntry.keptBps === 2400000, 'a write exactly 7 days before the pump start still left an entry (Entry::fresh is <=)');
  ok(sessionOf(afterWrite(1), 'e80002').path.entryAtOpen.withoutPairEntry === false, 'a millisecond older, it had aged out');
  const kept = runPairs(
    edge('e80003', flat(16), { at: T, write: { evidence: 2400000, kept: 2400000 } })
      .concat(edge('e80004', flat(16), { at: T + 6 * 86400000, seed: 2400000, write: { evidence: null, kept: 2400000 } }))
      .concat(edge('e80005', flat(16), { at: T + 7 * 86400000 + 3600000 })),
  );
  ok(sessionOf(kept, 'e80005').path.entryAtOpen.withoutPairEntry === false, 'a write with no evidence on day 6 does not renew the age of day 0');

  // --- FR-148 P0: the standing queue and the relay transport
  ok(
    JSON.stringify(couple('Some((3, 1257))')) === '[3,1257]' && JSON.stringify(couple('(1200, 60000)')) === '[1200,60000]' && couple('None') === null && couple('(1, 2, 3)') === null && couple(null) === null,
    'standing_ms: Some((low, high)) reads, None is no standing, and a three-tuple is no pair',
  );
  ok(
    relayTransportOf('"tls"') === 'tls' && relayTransportOf('none') === 'none' && relayTransportOf('quic') === 'unknown' && relayTransportOf(null) === null && relayTransportOf('None') === null,
    "relay_transport: its words read, a word this build does not know reads unknown, never a guess, and no field is no line's",
  );
  const sfx = standingFixtures();
  const rs = runPairs(sfx);
  const sqRow = (sid) => cells(sessionOf(rs, sid))[10];
  ok(sqRow('f70001') === 'standing queue: queued 8 of 15 samples, standing 1257/2210 ms, relay tls', "f70001 on TURN over TLS: 8 of its 15 samples queued, the p50 and the max of its lines' highest standing", sqRow('f70001'));
  ok(sqRow('f70002') === 'standing queue: queued 0 of 10 samples, standing 12/61 ms, relay udp', 'f70002 on TURN over UDP: none queued', sqRow('f70002'));
  ok(sqRow('f70003') === 'standing queue: queued 1 of 15 samples, standing 40/260 ms, relay none', 'f70003, direct: one queued, and no relay of its own', sqRow('f70003'));
  const older = sessionOf(rs, 'f70004');
  ok(
    sqRow('f70004') === 'standing queue: –, relay no line' && older.path.standingQueue === null && older.path.relayTransport === null && older.path.delivery.lines === 2,
    "an older agent's session: its delivery lines count as before, its standing queue reads –, and its relay no line",
    older.path,
  );
  ok(sqRow('f80001') === 'standing queue: queued 0 of 5 samples, standing –/– ms, relay unknown', 'a still screen: no sample counted, so no standing, on a relay the agent could not name', sqRow('f80001'));
  ok(
    sqRow('f80002') === 'standing queue: queued 4 of 5 samples, standing 830/830 ms, relay no line' && sessionOf(rs, 'f80002').carrier === 'turn (ice)',
    'no carrier line: the class from the ICE line, and no line for the relay',
    sqRow('f80002'),
  );
  const dev = (agent) => rs.devices.find((d) => d.agent === agent);
  ok(
    dev('c0a007').sessions === 4 && JSON.stringify(dev('c0a007').relayTransport) === JSON.stringify({ udp: 1, tcp: 0, tls: 1, none: 1, unknown: 0, noLine: 1 }),
    'per device: its sessions by relay transport, an older line counted as no line',
    dev('c0a007'),
  );
  ok(
    JSON.stringify(dev('c0a007').standingQueue) === JSON.stringify([{ carrier: 'direct', sessions: 1, standingQueue: 0 }, { carrier: 'turn', sessions: 2, standingQueue: 1 }]),
    `and by carrier class, the sessions with at least ${SQ_SESSION_MIN} queued samples of those that carried the fields: one queued sample is no standing queue`,
    dev('c0a007').standingQueue,
  );
  const mdS = render(rs);
  ok(
    /\| c0a007 \| 4 \| 1, 0, 1, 1, 0, 1 \| direct 0 of 1, turn 1 of 2 \|/.test(mdS) && /\| c0a008 \| 2 \| 0, 0, 0, 0, 1, 1 \| turn \(ice\) 1 of 1, turn-local 0 of 1 \|/.test(mdS),
    'the device summary renders',
    mdS.split('\n').filter((l) => /^\| c0a00[78] \| \d \|/.test(l)),
  );
  ok(rs.provenance.sessionsWithStandingQueue === 5 && /- 5 sessions carried the delivery line's standing-queue fields/.test(mdS), 'the sessions that carried the fields are counted', rs.provenance);
  const dupS = runPairs(sfx.concat(sfx.filter(([, l]) => /path delivery/.test(l))));
  ok(
    dupS.provenance.deliveryReplays === 12 && JSON.stringify(dupS.sessions) === JSON.stringify(rs.sessions) && JSON.stringify(dupS.devices) === JSON.stringify(rs.devices),
    'a delivery line uploaded twice changes no standing queue',
    dupS.provenance,
  );
  const relayLater = runPairs(
    edge(
      'e90001',
      flat(6, (i) => (i === 3 ? { extra: [[600, 'session carrier recorded (FR-102 P9c)', [['carrier', '"turn"'], ['relay_transport', '"tls"']]]] } : {})),
      { relayTransport: 'udp' },
    ),
  ).sessions[0];
  ok(relayLater.path.relayTransport === 'tls', 'a carrier line that repeats: the last one names the relay, as it names the class', relayLater.path);

  // The base chains carry none of the new lines: their delivery cells read –.
  ok(
    r.provenance.deliveryLines === 0 && r.provenance.assumedLines === 0 && r.sessions.every((s) => s.path.delivery === null && cells(s)[1] === '–'),
    'on the reference chains, which carry no delivery line, every delivery cell reads –',
  );
  ok(
    r.sessions.every((s) => s.path.standingQueue === null && s.path.relayTransport === null && cells(s)[10] === 'standing queue: –, relay no line') &&
      r.devices.every((d) => d.relayTransport.noLine === d.sessions && d.standingQueue.length === 0),
    'nor a standing queue or a relay transport: each reads – and no line',
  );

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
  // Every fixture from here on: the chains and the path questions together.
  const all = allFixtures();
  const rAll = runPairs(all, { labels });
  const mdAll = render(rAll);
  const jsonAll = JSON.stringify(rAll);
  ok(
    !/SECRET/.test(mdAll) && !/SECRET/.test(jsonAll) && !/[0-9a-f]{24}/i.test(mdAll) && !/[0-9a-f]{24}/i.test(jsonAll),
    "with the path fixtures, the assumed line's key among them: no address, no name, no full id in the report",
  );
  ok(rAll.flags.f1.below === 11 && rAll.flags.f1.judged === 19 && rAll.flags.f2.raised === 1 && rAll.flags.f3.raised === 1, 'the two sets read together', [rAll.flags.f1, rAll.flags.f2, rAll.flags.f3]);

  // --- the fleet read's trap: a field the fold reads and the projection drops
  // reads as missing there and nowhere else. Push every fixture line through
  // the server's projection with the driver's types, then through the row the
  // fleet read prints, and the report must not change.
  const viaRow = new PathReport({ labels });
  const viaPrinted = new PathReport({ labels });
  let leaked = false;
  let keyed = 0;
  let keyLeak = false;
  for (const [agent, l] of all) {
    const p = parseLogLine(l);
    const row = { a: { toHexString: () => agent }, t: new Date(p.ts), m: p.msg, f: projectLikeTheServer(p.fields) };
    viaRow.addRow(row);
    const printed = JSON.stringify(rowOut(row));
    if (/SECRET/.test(printed) || printed.indexOf(FX.op) >= 0) leaked = true;
    if ('peer' in p.fields) {
      keyed++;
      if ('peer' in row.f || /SECRET/.test(JSON.stringify(row.f))) keyLeak = true;
    }
    viaPrinted.addRow(JSON.parse(printed));
  }
  ok(JSON.stringify(viaRow.finish()) === jsonAll, 'every field the fold reads survives the projection');
  ok(JSON.stringify(viaPrinted.finish()) === jsonAll, 'the printed rows read back to the same report');
  ok(!leaked, 'a printed row carries no address, no name and no full user id');
  const assumedLine = parseLogLine(all.find(([, l]) => LINES.assumed.test(l))[1]);
  const assumedRow = projectLikeTheServer(assumedLine.fields);
  ok(
    keyed > 0 && !keyLeak && assumedRow.peer_carrier === 'relay:derp/tcp' && !('peer' in assumedRow) && assumedRow.assumed_bps.toNumber() === 281732,
    'every line with a memory key, the assumed one included, leaves the database with the carrier half only',
    assumedRow,
  );

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
  ok(JSON.stringify(viaSeed.finish()) === jsonAll, 'the rehearsal seed, in created_at order, reads back to the same report');
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
    const deliveryRs = src('media_delivery.rs');
    const memoryRs = src('encode/rate_memory.rs');
    const profileRs = src('encode/rate_profile.rs');
    const peerRs = src('peer.rs');
    // The ICE line left peer.rs for carrier_rank.rs (#4013); the seed and write
    // lines and the memory's key left it for media_pair_memory.rs (#4014).
    const pairRs = src('media_pair_memory.rs');
    const agentSrc = [peerRs, src('signaling.rs'), src('session_carrier.rs'), src('carrier_rank.rs'), deliveryRs, memoryRs, pairRs].join('\n');
    // The delivery line: its message, its field types and the `?Debug` forms the
    // parsers read, and the strings the daemon's own test pins, read back here.
    const lineConst = /pub const LINE: &str = "([^"]+)";/.exec(deliveryRs);
    ok(lineConst && LINES.delivery.test(lineConst[1]) && kindOf(lineConst[1]) === 'delivery', "media_delivery.rs's LINE is the delivery line this reads", lineConst && lineConst[1]);
    ok(
      /pub samples: \(u32, u32, u32\),/.test(deliveryRs) && /pub backlogged_bps: Option<\(u64, u64, u64\)>,/.test(deliveryRs) && /pub app_limited_bps: Option<u64>,/.test(deliveryRs),
      'its samples are a three-tuple, its backlogged rates an Option of one, its application-limited rate an Option',
    );
    ok(
      /delivery_samples = \?line\.samples,/.test(deliveryRs) &&
        /backlogged_bps = \?line\.backlogged_bps,/.test(deliveryRs) &&
        /app_limited_bps = \?line\.app_limited_bps,/.test(deliveryRs) &&
        /span_ms = line\.span_ms,/.test(deliveryRs) &&
        /stalled_ms = line\.stalled_ms,/.test(deliveryRs),
      'the tuples and Options are logged with ?Debug, span_ms and stalled_ms as integers',
    );
    const pinned = (field) => {
      const m = new RegExp('"' + field + '=([^"]+)"').exec(deliveryRs);
      return m ? parseLogLine(fxLine(0, lineConst ? lineConst[1] : 'x', [[field, m[1]]])).fields[field] : undefined;
    };
    ok(
      JSON.stringify(triple(pinned('delivery_samples'))) === '[2,3,0]' &&
        JSON.stringify(triple(pinned('backlogged_bps'))) === '[2390112,2401000,2410334]' &&
        num(pinned('app_limited_bps')) === 310112 &&
        num(pinned('span_ms')) === 10012 &&
        /backlogged_bps=None/.test(deliveryRs),
      "the daemon's pinned line reads back: (2, 3, 0), Some((…)), Some(310112), and None",
      [pinned('delivery_samples'), pinned('backlogged_bps'), pinned('app_limited_bps')],
    );
    // FR-148 P0: the standing queue's fields, and the carrier line's relay words.
    ok(
      num(pinned('queued')) === 2 &&
        JSON.stringify(couple(pinned('standing_ms'))) === '[3,1257]' &&
        /queued = q\.queued,/.test(deliveryRs) &&
        /standing_ms = \?q\.standing_ms,/.test(deliveryRs) &&
        /standing_ms=None/.test(deliveryRs),
      "the standing queue's fields: queued an integer, standing_ms a ?Debug Option of a pair, as the daemon's pinned line has them",
      [pinned('queued'), pinned('standing_ms')],
    );
    const carrierRs = src('session_carrier.rs');
    const words = [];
    const wordRe = /RelayTransport::\w+ => "([a-z]+)"/g;
    for (let m = wordRe.exec(carrierRs); m; m = wordRe.exec(carrierRs)) words.push(m[1]);
    ok(
      JSON.stringify(words.slice().sort()) === JSON.stringify(RELAY_TRANSPORTS.slice().sort()) && /relay_transport = relay\.as_str\(\),/.test(carrierRs),
      "the carrier line's relay_transport words are session_carrier.rs's, logged as text",
      words,
    );
    // The assumed line: its key is a &str, its rate and age integers.
    ok(
      /\bpeer,\s*assumed_bps = e\.stable_bps,\s*age_s = now_unix\.saturating_sub\(e\.at_unix\),/.test(memoryRs),
      "rate_memory.rs's assumed line carries peer, assumed_bps and age_s",
    );
    const ttl = /pub const TTL: Duration = Duration::from_secs\((\d+) \* (\d+) \* (\d+)\);/.exec(memoryRs);
    ok(
      ttl && Number(ttl[1]) * Number(ttl[2]) * Number(ttl[3]) * 1000 === MEMORY_TTL_MS && /now_unix\.saturating_sub\(self\.at_unix\) <= TTL\.as_secs\(\)/.test(memoryRs),
      "MEMORY_TTL_MS matches rate_memory.rs's TTL, and an entry is fresh at exactly its age",
      ttl && ttl[0],
    );
    const slow = /pub fn slow_link_profile_bps\(\) -> u32 \{[\s\S]*?\.unwrap_or\(([\d_]+)\)/.exec(profileRs);
    ok(
      slow && Number(slow[1].replace(/_/g, '')) === SLOW_LINK_PROFILE_BPS && /Some\(bps\) if bps > 0 && bps <= threshold =>/.test(profileRs),
      'SLOW_LINK_PROFILE_BPS matches slow_link_profile_bps, and an entry at or under it engages the profile',
      slow && slow[1],
    );
    const nominal = /fn relay_max_bps\(\) -> u32 \{[\s\S]*?\.unwrap_or\(([\d_]+)\)/.exec(modRs);
    ok(nominal && Number(nominal[1].replace(/_/g, '')) === RELAY_NOMINAL_BPS, 'RELAY_NOMINAL_BPS matches relay_max_bps in mod.rs', nominal && nominal[1]);
    // The pump hands its own `constrained` and `rate_hi_bps` to the memory's
    // open, whose key, and so its seed, needs both.
    ok(
      /media_pair_memory::open\(\s*&pc,\s*session_id,\s*codec_label,\s*delivery\.mode\(\),\s*constrained,\s*rate_hi_bps,/.test(peerRs) &&
        /pub async fn open<F, Fut>\(\s*pc: &RTCPeerConnection,\s*session_id: ObjectId,\s*codec_label: &'static str,\s*mode: Mode,\s*constrained: bool,\s*hi_bps: u32,/.test(pairRs) &&
        /let peer_key = if constrained && hi_bps > 0 \{/.test(pairRs),
      'the memory is read only by a pump that starts constrained, where the count without an entry is judged',
    );
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
  module.exports = {
    PathReport,
    parseLogLine,
    value,
    triple,
    render,
    damp,
    standIn,
    quarterSteps,
    judgeF1,
    judgeF2,
    judgeF3,
    fixtures,
    pathFixtures,
    standingFixtures,
    allFixtures,
    FIELDS,
    MSG_PATTERN,
  };
}
if (typeof Mongo === 'function' && typeof print === 'function') {
  runMongosh();
} else if (typeof require !== 'undefined' && typeof module !== 'undefined' && require.main === module) {
  nodeMain(process.argv.slice(2));
}
