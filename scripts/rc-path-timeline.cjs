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
//   | carrier    | `session carrier recorded (FR-102 P9c)` (session_carrier.rs), with its   |
//   |            | `relay_transport` (FR-148 P0: what carries the agent's TURN allocation), |
//   |            | `per-session ICE path detected (adaptive bitrate)` (carrier_rank.rs),    |
//   |            | `rc ICE: overlay …` (peer.rs, rc_overlay.rs),                            |
//   |            | `rc ICE: carrier rank (FR-148 P4)` and `…, the mesh link 10 s on …`      |
//   | encoder    | `ffmpeg encoder opened (…)`, `FR-148: the QSV encoder opened at QVBR…`,  |
//   |            | `ffmpeg open_as_with rejected the quality options…` (ffmpeg/encoder.rs)  |
//   | rate       | `FFmpeg DC pump heartbeat (≈2s window)`, `VP9-444 DC pump heartbeat…`    |
//   |            | (peer.rs): bytes_written and frames_sent are CUMULATIVE per pump         |
//   | target     | `VP9-444 set_bitrate (AIMD)`, `VP9-444 AIMD decrease (DC buffer over     |
//   |            | high watermark)` and `FFmpeg DC pump set_bitrate (AIMD)`, each a move    |
//   |            | handed out, and `FFmpeg DC pump: deferred bitrate …`, a held target      |
//   |            | landing (peer.rs). From 0.4.230 (FR-148 P0, #4161) each carries `why`,   |
//   |            | and each heartbeat `target_moves`: the moves since the one before it     |
//   | delivery   | `path delivery (FR-148 P0, shadow, ≈10s)`, `path model (…)`              |
//   |            | (media_delivery.rs); a delivery line with a queued sample prints as a    |
//   |            | `queue` event: `queued`, `standing_ms`, `inflight`, `cwnd`, `rwnd_low`,  |
//   |            | `ack_ms`; one with an SCTP loss or a one-packet window as a `loss`       |
//   |            | event (THE MESH RELAY'S T3 below); the model line with its floor's       |
//   |            | push-back, its rule and its fall (FR-148 D46 B2, D48, D50)               |
//   | /derp      | `overlay derp: carrier frames dropped (stale, full, backpressure,        |
//   |            | unrouted)` (FR-148 D52, derp.rs `DropLine`), and the line it replaced,   |
//   |            | `overlay derp: shed stale carrier frames (queue age bound)`, which an    |
//   |            | agent before D52 writes; the /derp WS owner's lifecycle lines (derp.rs   |
//   |            | `run`): connected, closed and why, a connect that failed or timed out    |
//   |            | (DERP_STATES), as `drops`, `shed` and `derp` events; the relay they      |
//   |            | name stays in the database. With --viewer-agent, the same lines of the   |
//   |            | viewer's node (THE MESH RELAY'S T3)                                      |
//   | open       | `path open (FR-148 P0, O10)` (media_delivery.rs): once a pump, at its    |
//   |            | first media write, which way SCTP opened the association (FR-148 O10)    |
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
// THE TARGET (FR-148 P0)
//
//   A move of the live target prints as a `target` row, with why:
//   - at its own target line, `a -> b bps why=…`, the `why` as the line
//     carries it: the causes in the order they happened, joined by `+`, from
//     open; increase(app-limited|loaded|no-window); decrease(full|overflow|
//     stall|age|link|transit-stall); hard-decrease; ceiling(plan|pipe|learned|
//     arrival|opener); floor(area|relief|ramp); reapply; or `none`, and
//     `+more` past eight (encode/aimd.rs `Cause`, encode/governor.rs `Why`;
//     the self-test reads these words from the source). A line without `why`,
//     from an agent before 0.4.230, prints "(reason: not logged)";
//   - else at the first heartbeat that shows the number changed, `a -> b bps,
//     no target line`, with that heartbeat's `target_moves`, or "(reason: not
//     logged)" when it carries none.
//   A held FFmpeg target landing (`deferred bitrate …`) prints `held b bps …`
//   with the `why` it was handed out with. It is no new move.
//
// THE MESH RELAY'S T3 (FR-148 AC4)
//
//   An expiry of SCTP's retransmit timer (T3-rtx) sets the congestion window
//   to one packet, 1,228 bytes (T3_CWND, the vendored SCTP's MTU), and nothing
//   else sets it that low. A delivery line is a T3 line when its `loss` (fast
//   retransmissions, T3 expiries over its samples) counts a T3, or when its
//   `cwnd` was one packet at a sample's close. A line with a loss or a
//   one-packet window prints a `loss` row, a T3 line's with `; T3`.
//   On the mesh relay (carrier `derp`) a node's DERP mux drops a carrier frame
//   at four places, each counted (FR-148 D52, DROP_CAUSES): `stale`, an
//   outbound frame that waited past its age bound; `full`, one the full
//   outbound queue refused; `backpressure`, an inbound one whose consumer's
//   queue was full or closed; `unrouted`, an inbound one for a peer with no
//   route. The keepalive tick writes the drop line when any count changed: at
//   most one every 25 s per relay node (SHED_AFTER_MS), with each count's
//   move since the line before (`since_last` is the stale count's, as on the
//   shed line; `full_since_last` and so on the others'). An agent before D52
//   writes the shed line instead, the stale count alone. In no line: a frame
//   too short to name a peer (not counted), and a 0.4.199 peer's Hive frame
//   (`dropped_hive`, FR-137, counted apart). For each T3 line of a session on
//   the mesh relay, its row and the summaries say, for each node read, which
//   drop line that told a move (a count's since_last > 0) comes nearest, and
//   which causes moved in it (`pairOf`):
//   - `after`: one at most 25 s after the line. A drop could have fired the
//     T3: FR-148 AC4's Read A takes this one;
//   - `before`: none in the 25 s after, but one inside the line's own span
//     (`span_ms` before it). The tick fell between a drop and the line's
//     write, so the drop was told first;
//   - `none moved`: none from the start of the span to 25 s after the line.
//   Each also says whether a /derp lifecycle line of that node falls between:
//   from the span's start to the later of the line and its drop line (to 25 s
//   after the line when there is none). A reconnect there could have fired the
//   T3 itself, or restarted the tick that tells a drop.
//   The nodes: the agent read, and with --viewer-agent the viewer's node, the
//   other end of the mesh relay pair, read over the same window for its /derp
//   lines only (DERP_KINDS). A T3 line across which no count moved on either
//   node answered a hold, or a loss below the overlay (the relay's own drops
//   are in the relay's log, not here), and its row says so.
//   The /derp lines carry no session id. Each prints in the session its time
//   falls in (as an encoder line does; the viewer node's as `v.drops`,
//   `v.shed` and `v.derp`), and the pairing reads every one of each node's,
//   whichever session printed it.
//
//   `target_moves` counts every move the governor handed out since the
//   heartbeat before it (`record_applied`, `take_moves`: governor.rs), as
//   `why:n` joined by `,`. The pumps write a target line for only some: the
//   FFmpeg pump only when the number changed (a re-apply is in the count
//   alone; the VP9-444 pump writes one when the quality changed with it) and
//   not for the tick a rebuild's open runs first; neither pump at the gate's
//   skip; the FFmpeg pump not at a send overflow. A line lost on upload is
//   lost too. So over a session read whole, the target lines that hand a
//   move out are at most the heartbeats' total, plus the moves after a pump's
//   last heartbeat, which no heartbeat counts, unless a heartbeat line was
//   lost. The session summary prints both, each by `why`.
//
// WHAT IT CANNOT TELL
//
//   - Why the target moved, on an agent before 0.4.230: its target lines carry
//     no `why` and its heartbeats no `target_moves`, so its moves print
//     "(reason: not logged)". From 0.4.230 the reason is told (THE TARGET). A
//     cause names what moved the number, never what the path carries
//     (docs/rate-control.md, "Why the target moved").
//   - Which session a line belongs to when the line names none: the encoder
//     lines and the rate-memory line. Such a line goes to the session of this
//     agent whose lines span its time (from OPEN_LAG_MS before the session's
//     first line to CLOSE_LAG_MS after its last), the latest-started one when
//     two do; otherwise it is counted as unattributed.
//   - Whether a session ended: a session whose last line is within
//     OPEN_AT_END_MS of the window's end is printed as open at the window's end.
//   - Which relay node a /derp line came from. Its `relay` field names the
//     node (`central`, or a regional relay's host) and stays in the database,
//     so the lines of a device on two nodes read as one device's, while each
//     node counts and ticks its own drops (a DerpMux each).
//   - Whether a drop fired a T3. A drop line counts the drops of up to 25 s
//     before it (later when a blocked send held its tick up), and a delivery
//     line the T3 expiries of its ≈10 s, so the pairing says only that a drop
//     could have come first. The relay server's own drops (an unregistered
//     destination, a full destination queue: the central relay's and a
//     regional relay's log) and a lost SACK are in no drop line.
//   - The viewer node's drops, without --viewer-agent: an inbound drop there
//     (`backpressure`, `unrouted`) is in no line this agent writes. A node
//     uploads its lines under its primary enrollment, so a viewer node in
//     another org than the agent's is read with --viewer-tenant.
//   - A T3 count, on an agent before 0.4.227: its delivery lines carry no
//     `loss`, so its T3 lines show only by the one-packet window, and their
//     count prints `-`. A line whose samples were read without SCTP's gauges
//     carries neither `loss` nor `cwnd`.
//   - The model's push-back, rule and fall, on an agent before FR-148 D46 B2,
//     D48 (#4191) and D50 (#4231): its model lines carry none of the three
//     fields, which print `-`, as the agent's own `-` (none yet) does.
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
//   - What leaves the database: the named fields of the line's own kind only
//     (`fieldsOf`): the projection picks them by the line's message, as the
//     fold does, so a field named for one kind never leaves from another's
//     line. The ICE line's `relay` is a yes or no, while the /derp lines'
//     `relay` names the relay node, a host, and a failed connect's `e` is an
//     error text that can name its address: both stay in the database
//     (KEPT). The read is not built if either is named (`projectionOf`), the
//     /derp lines' counts leave as numbers only (NUMBER_KINDS), and the
//     self-test fails on either, or on a /derp line the agent writes with a
//     field the reader neither names nor keeps. `viewers` leaves only as a
//     number: the rank line's count (the overlay line names no `viewers`, and
//     its auto line's list of viewer addresses stays). The two path-memory
//     lines leave every field but a key that could name an address, a host or
//     a person, removed on the server (DENY_KEY). --rows keeps a line to its
//     kind's fields again. Ids print as their last 4 hex in the report and in
//     --rows alike (`masks`), and every printed line, like every string --rows
//     prints, is scrubbed of addresses and 24-hex ids again. `why` and
//     `target_moves` leave as written: the agent builds both from fixed words
//     and counts only (encode/aimd.rs `Cause`, encode/governor.rs `Why` and
//     `Moves`), never an id, an address or free text.
//
//   THE ESTIMATE, a ten-hour window with eight hours of sessions, FFmpeg pump:
//   heartbeats one per ≈2 s (14,400), delivery and model lines one each per
//   10 s (2,880 + 2,880), capture-timing lines one per 30 s of capture (960),
//   and a few dozen carrier, encoder, open and memory lines a session: about 21,200
//   rows. A projected heartbeat is about 600 bytes of JSON and the others
//   about 300: about 11 MB through `--rows`, and a report of a few thousand
//   lines (minute rows by default). The documents examined are this agent's
//   batches in the window only (about 6,000 at the upload rate of a session
//   hour, 10 a minute), through the index. DEFAULT_LIMIT (60,000) is about
//   three such days of one device; a VP9-444 pump doubles the heartbeats.
//   Split a longer window with --from/--to.
//   The target lines add one a move a pump writes a line for: an increase at
//   most every 5 s, a ×0.85 at most every 500 ms and only while congested
//   above the floor (encode/aimd.rs AI_SETTLE, MD_MIN_INTERVAL), a bound's
//   move when its number crosses the target (the measured bounds change at
//   most once a ≈1 s window). That is a few a minute where a session climbs
//   and backs off; even one a second for all eight hours adds 28,800 rows of
//   about 200 bytes, about 50,000 rows in all, under DEFAULT_LIMIT.
//   The /derp lines add a drop line at most every 25 s per relay node while it
//   drops (1,440 in the ten hours if it never stops) and a few lifecycle lines
//   a reconnect, about 150 bytes each (a D52 drop line about 300): about
//   51,500 rows with one relay node, still under DEFAULT_LIMIT. Choosing a
//   line's fields by its kind runs on the rows read, never on the documents
//   examined, which do not change.
//   --viewer-agent adds ONE more read, bounded the same way: the viewer
//   node's agent, one org (--viewer-tenant, by default --tenant), the same
//   window and index, its /derp lines only (DERP_KINDS) and the same --limit,
//   TRUNCATED on its own. Its documents examined are that agent's batches in
//   the window; its rows are its drop and lifecycle lines, at most 1,440 a
//   relay node in ten hours and a few a reconnect.
//
// RUN
//
//   node scripts/rc-path-timeline.cjs --self-test
//   node scripts/rc-path-timeline.cjs [--intervals] [--from ISO --to ISO] ROWS.ndjson   # offline
//   bash scripts/rc-path-report.sh --timeline --mars --yes --tenant HEX --agent HEX \
//        --from 2026-11-18T07:00:00Z --to 2026-11-18T18:15:00Z                         # the read
//        [--viewer-agent HEX [--viewer-tenant HEX]]           # and the viewer node's /derp lines
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
/** FR-148 AC4: the congestion window a T3-rtx expiry leaves, one packet: the
 *  vendored SCTP's INITIAL_MTU (association/mod.rs), its `mtu` for good, to
 *  which the expiry sets the window (`self.cwnd = self.mtu;`,
 *  association_internal.rs). A fast retransmit sets it to ssthresh, four
 *  packets at least, and nothing else sets it. The self-test reads all three. */
const T3_CWND = 1228;
/** The /derp keepalive tick (derp.rs DERP_KEEPALIVE): the drop line (and the
 *  shed line before FR-148 D52) is written on it, so a drop is told at most
 *  this long after it, later only when a send held the tick up or a reconnect
 *  restarted it. The self-test reads it. */
const SHED_AFTER_MS = 25000;
/** A delivery line's span when it names none (media_delivery.rs LINE_EVERY). */
const LINE_EVERY_MS = 10000;
/** A session on the mesh relay: its carrier word (session_carrier.rs). */
const MESH_RELAY = 'derp';
/** A model line's rule or fall whose start (the line's time less its age)
 *  moved more than this is a new one: its row prints again. */
const MODEL_ONSET_SLACK_MS = 1000;
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

/** FR-148 AC4: the /derp WS owner's lifecycle lines (derp.rs `run`), each by
 *  the word the report prints for it and the start of its message as the
 *  agent writes it. The self-test finds each in `run`, and fails on a line
 *  there that it does not name. */
const DERP_STATES = [
  ['registration-failed', 'overlay derp: registration send failed; reconnecting'],
  ['connected', 'overlay derp: /derp WS connected + registered'],
  ['suspend-resume', 'overlay derp: suspend/resume detected — cycling the /derp WS'],
  ['frozen', 'overlay derp: no inbound frames within the RX deadline, but this agent did not run for ~'],
  ['half-open', 'overlay derp: no inbound frames within the RX deadline — half-open /derp WS; reconnecting'],
  ['ping-failed', 'overlay derp: keepalive ping failed; reconnecting'],
  ['closed', 'overlay derp: /derp WS closed; reconnecting'],
  ['connect-failed', 'overlay derp: /derp connect failed'],
  ['lb-no-pod', 'overlay derp: the load balancer has no pod for this org yet; redialling flat (FR-28)'],
  ['connect-timed-out', 'overlay derp: /derp connect timed out'],
];

/** FR-148 D52: the /derp drop line (derp.rs `DropLine::log`), and FR-18's
 *  shed line, which it replaced and which an agent before D52 writes. */
const DROP_LINE = 'overlay derp: carrier frames dropped (stale, full, backpressure, unrouted)';
const SHED_LINE = 'overlay derp: shed stale carrier frames (queue age bound)';
/** FR-148 D52: the drops a node's DERP mux counts, each as the word the
 *  report prints, its lifetime count and its move since the line before, as
 *  the drop line writes them (the shed line writes the first two only). */
const DROP_CAUSES = [
  ['stale', 'dropped_stale', 'since_last'],
  ['full', 'dropped_full', 'full_since_last'],
  ['backpressure', 'dropped_backpressure', 'backpressure_since_last'],
  ['unrouted', 'dropped_unrouted', 'unrouted_since_last'],
];
/** FR-148 D52: the kinds the viewer node's read takes (--viewer-agent): its
 *  /derp lines only. */
const DERP_KINDS = ['shed', 'drops', 'derp'];
/** A row of the viewer node's read carries `node` with this word. */
const VIEWER = 'viewer';

/** The lines read: kind, the message's start, and the call site whose
 *  literal the self-test finds in the source. A kind with `was` is a line the
 *  agent wrote before and writes no more: still read, for an older agent, and
 *  the self-test finds its literal gone from the source. */
const KINDS = [
  { k: 'heartbeat', re: /^(FFmpeg|VP9-444) DC pump heartbeat/, src: ['peer.rs', 'FFmpeg DC pump heartbeat (≈2s window)', 'VP9-444 DC pump heartbeat (≈1s window)'] },
  // FR-148 P0: the lines that hand a move of the target out, and a held one landing.
  { k: 'target', re: /^(VP9-444 set_bitrate \(AIMD\)|VP9-444 AIMD decrease \(DC buffer over high watermark\)|FFmpeg DC pump set_bitrate \(AIMD\))/, src: ['peer.rs', 'VP9-444 set_bitrate (AIMD)', 'VP9-444 AIMD decrease (DC buffer over high watermark)', 'FFmpeg DC pump set_bitrate (AIMD)'] },
  { k: 'landed', re: /^FFmpeg DC pump: deferred bitrate (scheduled|applied at quiet)/, src: ['peer.rs', 'FFmpeg DC pump: deferred bitrate scheduled (background rebuild)', 'FFmpeg DC pump: deferred bitrate applied at quiet'] },
  { k: 'carrier', re: /^session carrier recorded/, src: ['session_carrier.rs', 'session carrier recorded (FR-102 P9c)'] },
  { k: 'ice', re: /^per-session ICE path detected/, src: ['carrier_rank.rs', 'per-session ICE path detected (adaptive bitrate)'] },
  { k: 'rank', re: /^rc ICE: carrier rank/, src: ['rc_overlay.rs', 'rc ICE: carrier rank (FR-148 P4)', 'rc ICE: carrier rank, the mesh link 10 s on (FR-148 P4)'] },
  { k: 'overlay', re: /^rc ICE: overlay /, src: ['peer.rs', 'rc ICE: overlay {}'] },
  { k: 'qsv', re: /^FR-148: the QSV encoder opened at QVBR/, src: ['encode/ffmpeg/encoder.rs', 'FR-148: the QSV encoder opened at QVBR, its target under its cap'] },
  { k: 'encOpen', re: /^ffmpeg encoder opened \(/, src: ['encode/ffmpeg/encoder.rs', 'ffmpeg encoder opened (constant-quality + maxrate cap)'] },
  { k: 'rejected', re: /^ffmpeg open_as_with rejected the quality options/, src: ['encode/ffmpeg/encoder.rs', 'ffmpeg open_as_with rejected the quality options'] },
  { k: 'delivery', re: /^path delivery \(FR-148 P0/, src: ['media_delivery.rs', 'path delivery (FR-148 P0, shadow, ≈10s)'] },
  { k: 'model', re: /^path model \(FR-148 P0/, src: ['media_delivery.rs', 'path model (FR-148 P0, shadow, ≈10s)'] },
  { k: 'open', re: /^path open \(FR-148 P0, O10\)/, src: ['media_delivery.rs', 'path open (FR-148 P0, O10)'] },
  // FR-148 AC4 and D52: the /derp drop line, the shed line it replaced, and
  // the WS owner's lifecycle lines.
  { k: 'shed', re: /^overlay derp: shed stale carrier frames/, src: ['derp.rs'], was: SHED_LINE },
  { k: 'drops', re: /^overlay derp: carrier frames dropped/, src: ['derp.rs', DROP_LINE] },
  {
    k: 'derp',
    re: /^overlay derp: (registration send failed|\/derp WS (connected|closed)|suspend\/resume detected|no inbound frames within the RX deadline|keepalive ping failed|\/derp connect (failed|timed out)|the load balancer has no pod for this org)/,
    src: ['derp.rs'].concat(DERP_STATES.map((x) => x[1])),
  },
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
/** FR-148 AC4: the kinds whose every field leaves only as a number: the /derp
 *  lines', whose counts are all they let out. */
const NUMBER_KINDS = ['shed', 'drops', 'derp'];
/** The fields every line leaves with: its session's id, by either name (the
 *  id prints as its last 4 hex). */
const IDS = ['session_id', 'session'];
/**
 * FR-148 AC4: what the /derp lines write and the read leaves in the database.
 * `relay` names the relay node: `central`, or a regional relay's host
 * (derp.rs `spawn_regional`); a failed connect's `e` is an error text, which
 * can name the relay's address. `projectionOf` refuses to build a read that
 * would project either, and the self-test fails on it, and on a /derp line
 * that writes a field neither FIELDS nor this names. FR-148 D52's drop line
 * names its relay as the shed line did.
 */
const KEPT = { shed: ['relay'], drops: ['relay'], derp: ['relay', 'e'] };

/** True when a field leaves only as a number: NUMBER_ONLY, or a field of a
 *  NUMBER_KINDS line. */
function numberOnly(kind, k) {
  return NUMBER_ONLY.includes(k) || NUMBER_KINDS.includes(kind);
}

/**
 * The named fields that leave the database, by the row that prints them: a
 * line of a kind (KINDS) prints as a row of `k=v` in this order, a field the
 * line does not carry left out (the model, queue, loss, shed and drops rows
 * print it as `-`). A line leaves with its own kind's fields only (`fieldsOf`): its
 * session's id, what the code reads of it, and its rows'. Adding a field to a
 * line kind is ONE entry here and nothing else: the read projects it for that
 * kind's lines (`projectionOf`), and the row prints its list. The path-memory
 * lines are the exception the other way: every field they carry leaves and
 * prints (MEM_PATTERN), named here or not.
 */
const FIELDS = {
  // Projected, and printed only where the code reads them, by kind: the
  // heartbeat's (the minute rows, the pipe row, the summary) and a target
  // line's new target (`a -> b bps`). The rank lines' `read_ms` and the
  // capture-timing line's two print nowhere.
  read: {
    // heartbeat (peer.rs)
    heartbeat: [
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
      // FR-148 P0: the moves since the heartbeat before, `why:n` joined by `,`
      'target_moves',
    ],
    // the VP9-444 watermark line's new target is `new_target`, the other target lines' `target_bps`
    target: ['target_bps', 'new_target'],
    landed: ['target_bps'],
    rank: ['read_ms'],
    // capture timing (system_context/capture_pump.rs)
    capTiming: ['worker_avg_capture_ms', 'calls'],
  },
  // carrier + ICE (session_carrier.rs, carrier_rank.rs)
  carrier: ['carrier', 'relay_transport', 'local_typ', 'remote_typ'],
  ice: ['relay', 'local_typ', 'local_proto', 'remote_typ', 'remote_proto', 'waited_ms', 'opening'],
  // carrier rank + overlay (rc_overlay.rs, peer.rs); `viewers` a number only (NUMBER_ONLY)
  rank: ['carrier_rank', 'link', 'link_at_offer', 'upgrading', 'stalled', 'viewers', 'viewers_from', 'ranked_last'],
  overlay: ['overlay_blocks'],
  // encoder (ffmpeg/encoder.rs)
  qsv: ['encoder', 'rate_control', 'fallback', 'bit_rate_bps', 'maxrate_bps'],
  encOpen: ['encoder', 'width', 'height', 'fps', 'cq', 'maxrate_bps', 'input'],
  rejected: ['encoder', 'open_err'],
  // delivery (media_delivery.rs): a window with stalled time prints `stall`,
  // one with a queued sample `queue` (FR-148 P0's standing queue, as `emit`
  // names it; its `cwnd` is a pair there, a number on the open line), one
  // with an SCTP loss or a one-packet window `loss` (FR-148 AC4: `loss` is
  // fast retransmissions and T3-rtx expiries over the line's samples)
  stall: ['span_ms', 'delivery_samples', 'backlogged_bps', 'app_limited_bps', 'stalled_ms'],
  queue: ['queued', 'standing_ms', 'inflight', 'cwnd', 'rwnd_low', 'ack_ms'],
  loss: ['span_ms', 'stalled_ms', 'acked_bytes', 'loss', 'cwnd', 'pending_high'],
  // the model line (`emit_model`), with the floor's push-back (FR-148 D46 B2),
  // the rule that last kept a reading out (D48) and the last fall (D50)
  model: ['viewer_rx_bps', 'model_capacity_bps', 'model_floor_bps', 'model_floor_push_back', 'model_rule', 'model_fell'],
  // open path (media_delivery.rs, FR-148 O10), in the order `emit_open` writes them
  open: ['pump', 'open_path', 'ssthresh', 'rwnd', 'cwnd', 'inflight_bytes', 'pending_bytes', 'max_payload', 'fast_retransmits', 't3_expiries'],
  // FR-148 P0: the target lines (peer.rs), after `a -> b bps`
  target: ['why', 'deferred'],
  landed: ['why'],
  // rate memory (encode/rate_memory.rs) — its `peer` never leaves
  rateMem: ['assumed_bps', 'age_s'],
  // FR-148 AC4: the /derp lines (derp.rs `run` and `DropLine::log`), numbers
  // only (NUMBER_KINDS); their `relay` and a failed connect's `e` stay in the
  // database (KEPT). D52's drop line: each cause's count and move, in the
  // order DROP_CAUSES names them and the agent writes them
  shed: ['dropped_stale', 'since_last'],
  drops: ['dropped_stale', 'since_last', 'dropped_full', 'full_since_last', 'dropped_backpressure', 'backpressure_since_last', 'dropped_unrouted', 'unrouted_since_last'],
  derp: ['napped_s', 'silent_s', 'paused_s', 'wait_ms', 'left'],
};

/** The rows a kind's line prints as, where they are not one row of its name. */
const ROWS = { delivery: ['stall', 'queue', 'loss'] };

/** The fields a line of one kind leaves the database with, and --rows keeps:
 *  its session's id, what the code reads of it, and its rows', in that order.
 *  Built on each call, so it never lags FIELDS. */
function fieldsOf(kind) {
  const lists = [IDS, FIELDS.read[kind] || []].concat((ROWS[kind] || [kind]).map((r) => (Array.isArray(FIELDS[r]) ? FIELDS[r] : [])));
  return Array.from(new Set([].concat(...lists)));
}

/** Every field a line of some kind leaves with, once. */
function named() {
  return Array.from(new Set([].concat(...KINDS.map((d) => fieldsOf(d.k)))));
}

/** The close line's sample counts, both generations (0.4.225 carries
 *  `samples_not_counted` only; #4029 adds the rest). FR-148 O7. */
const O7_COUNTED = ['samples_counted', 'samples_counted_overproduced', 'samples_counted_clear'];
const O7_ASIDE = ['samples_unread', 'samples_viewer_late', 'samples_under_plan', 'samples_not_counted'];

/** The open line's answers, as the device row counts them (FR-148 O10):
 *  path C, path I, a loss before the first write, no gauges. */
const OPEN_PATHS = ['cookie_echo', 'init_ack', 'unknown', 'unread'];

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

/** A Debug tuple of two integers, `(a, b)`, bare or inside `Some(…)`; null for
 *  `None`. The delivery line's `standing_ms` has this shape. A copy of
 *  rc-path-report.cjs `couple`, so both read the field alike. */
function couple(v) {
  const s = text(v);
  const m = s && /^\(\s*(\d+)\s*,\s*(\d+)\s*\)$/.exec(s);
  return m ? [Number(m[1]), Number(m[2])] : null;
}

/** `<bps>/<basis>/<age>ms` (media_delivery.rs `Shown`), or null. */
function shown(v) {
  const s = text(v);
  if (!s) return null;
  const m = /^(\d+)\/([^/]+)(?:\/(\d+)ms)?$/.exec(s);
  return m ? { bps: Number(m[1]), basis: m[2], ageMs: m[3] === undefined ? null : Number(m[3]) } : { raw: s };
}

/** `<what>/<age>ms` as the model line writes its rule (media_delivery.rs
 *  `RuledShown`: `stall/3400ms`) and its fall (FR-148 D50 `FellShown`:
 *  `2131000->1162000/61300ms`): the what and the age; the agent's `-` (none
 *  yet) as the what `-`; null on a line without the field. */
function aged(v) {
  const s = text(v);
  if (s === null) return null;
  const m = /^(.+)\/(\d+)ms$/.exec(s);
  return m ? { what: m[1], ageMs: Number(m[2]) } : { what: s, ageMs: null };
}

/** A heartbeat's `target_moves` (governor.rs `Moves`: `why:n` joined by `,`,
 *  then `more:n` past 16 whys, or `none`) as [why, n] pairs; null on a line
 *  without it. A part that is not `why:n` counts once as `unreadable`. */
function tally(v) {
  const s = text(v);
  if (s === null) return null;
  if (s === 'none') return [];
  return s.split(',').map((part) => {
    const m = /^(.+):(\d+)$/.exec(part);
    return m ? [m[1], Number(m[2])] : ['unreadable', 1];
  });
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

/** A /derp lifecycle line's word (DERP_STATES), from its message. */
function derpState(msg) {
  const s = DERP_STATES.find((x) => String(msg).startsWith(x[1]));
  return s ? s[0] : 'unknown';
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

/** Counts by `why`, the commonest first and a tie in the order first seen:
 *  `increase(app-limited) 6, decrease(full) 3`; `-` for none. */
function whyCells(counts) {
  const keys = Object.keys(counts);
  return keys.length ? keys.sort((a, b) => counts[b] - counts[a]).map((k) => `${k} ${counts[k]}`).join(', ') : '-';
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

/** Every one of `keys` as `k=v`, a field the line does not carry as `k=-`, so
 *  the row has one shape whatever the agent's version. */
function kvAll(f, keys) {
  return keys
    .filter((k) => !DENY_KEY.test(k))
    .map((k) => `${k}=${ID_KEYS.has(k) ? mask((f || {})[k]) : dash(text((f || {})[k]))}`)
    .join(' ');
}

/** One decimal second, signed: `+7.3 s`, `-2.0 s`. */
function signedSecs(ms) {
  return `${ms < 0 ? '-' : '+'}${(Math.abs(ms) / 1000).toFixed(1)} s`;
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
    // FR-148 D52: a row of the viewer node's read, of its /derp lines only.
    const node = r && r.node === VIEWER ? VIEWER : null;
    if (!kind || t === null || (node && !DERP_KINDS.includes(kind))) return;
    // A path-memory line's fields in the order it wrote them (fm), so a named
    // field it also carries (its `why`) keeps its place there.
    const f = Object.assign({}, r.fm || {}, r.f || {}, r.fm || {});
    recs.push({ i, t, kind, m, f, mem: !!r.fm, node });
  });
  recs.sort((a, b) => a.t - b.t || a.i - b.i);
  const sessions = new Map();
  for (const rec of recs) {
    // The viewer node's lines name none of this agent's sessions.
    rec.sid = rec.node ? null : text(rec.f.session_id) || text(rec.f.session);
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
    else {
      const k = (rec.node ? 'v.' : '') + rec.kind;
      unattributed[k] = (unattributed[k] || 0) + 1;
    }
  }
  const to = epochMs(opts.to);
  // FR-148 AC4 and D52: the /derp lines name no session; the pairing reads
  // each node's every one, whichever session printed it. The viewer node is a
  // node of the read when it was read, whether or not it wrote a line.
  const derpOf = (node) =>
    recs.filter((x) => x.node === node && DERP_KINDS.includes(x.kind)).map((x) => ({ t: x.t, kind: x.kind, moved: movedOf(x), state: x.kind === 'derp' ? derpState(x.m) : null }));
  const nodes = [{ name: 'agent', derp: derpOf(null) }];
  if (opts.viewer) nodes.push({ name: VIEWER, derp: derpOf(VIEWER) });
  for (const s of list) finishSession(s, to, nodes);
  return { sessions: list, device: deviceOf(list, unattributed, nodes), truncated: !!opts.truncated, rows: rows.length, opts };
}

/** FR-148 D52: a /derp drop line's moves by cause (DROP_CAUSES): each a
 *  number, or null where the line carries none (the shed line carries the
 *  stale count's only). Null for a lifecycle line. */
function movedOf(rec) {
  if (rec.kind !== 'shed' && rec.kind !== 'drops') return null;
  const out = {};
  for (const [w, , since] of DROP_CAUSES) out[w] = rec.kind === 'shed' && w !== 'stale' ? null : num(rec.f[since]);
  return out;
}

/**
 * FR-148 AC4 and D52: a T3 line at `t`, its samples from `span` ms before it,
 * against each node's /derp lines in time order (THE MESH RELAY'S T3). For
 * each node, the drop line that told a move (a count's since_last > 0)
 * nearest after it, within SHED_AFTER_MS (`after`: a drop could have fired
 * the T3, Read A's rule); else the last one inside its span (`before`: the
 * tick fell between a drop and the line); else `none`; and the causes that
 * moved in it. `between`: a lifecycle line of the node from the span's start
 * to the later of the line and its drop line (SHED_AFTER_MS after the line
 * when none). `quiet`: two nodes read, and no count moved on either.
 */
function pairOf(t, span, nodes) {
  const start = t - span;
  const movedIn = (d) => DROP_CAUSES.filter(([w]) => d.moved && d.moved[w] !== null && d.moved[w] > 0).map(([w]) => w);
  const per = nodes.map((n) => {
    const told = n.derp.filter((d) => movedIn(d).length > 0);
    const after = told.find((d) => d.t > t && d.t <= t + SHED_AFTER_MS) || null;
    const before = after ? null : told.filter((d) => d.t > start && d.t <= t).pop() || null;
    const end = after ? after.t : before ? t : t + SHED_AFTER_MS;
    const between = n.derp.some((d) => d.kind === 'derp' && d.t > start && d.t <= end);
    const line = after || before;
    const causes = line ? movedIn(line) : [];
    const says = line
      ? `${after ? 'after' : 'before'} ${signedSecs(line.t - t)}${before ? ' (inside the span)' : ''}: ${causes.map((w) => `${w} ${line.moved[w]}`).join(', ')}`
      : `none moved (the span's start to ${signedSecs(SHED_AFTER_MS)})`;
    return { name: n.name, bucket: after ? 'after' : before ? 'before' : 'none', between, causes, says: `${n.name}: ${says}${between ? ', a /derp lifecycle line between' : ''}` };
  });
  const quiet = per.length > 1 && per.every((p) => p.bucket === 'none');
  return { per, quiet, says: per.map((p) => p.says).join('; ') + (quiet ? '; no count moved on either node: a hold, or a loss below the overlay' : '') };
}

/** The T3 lines by their pairing (pairOf), each with how many had a /derp
 *  lifecycle line between. */
function newBuckets() {
  return { after: { n: 0, between: 0 }, before: { n: 0, between: 0 }, none: { n: 0, between: 0 } };
}

function bucketCells(by) {
  return ['after', 'before', 'none'].map((k) => `${by[k].n} (${by[k].between})`).join(' / ');
}

/** FR-148 D52: counts by cause, in DROP_CAUSES order: `stale 41, full 3`;
 *  `-` for none. */
function causeCells(by) {
  const out = DROP_CAUSES.filter(([w]) => by[w] !== undefined).map(([w]) => `${w} ${by[w]}`);
  return out.length ? out.join(', ') : '-';
}

/** One cell per node read: `agent …; viewer …`. */
function nodeCells(byNode, cells) {
  return Object.keys(byNode)
    .map((n) => `${n} ${cells(byNode[n])}`)
    .join('; ');
}

/** One value per node read, each a fresh `make()`. */
function perNode(nodes, make) {
  return Object.fromEntries(nodes.map((n) => [n.name, make()]));
}

function finishSession(s, windowEnd, nodes) {
  const ev = [];
  const push = (t, what, line) => {
    const e = { t, what, line };
    ev.push(e);
    return e;
  };
  const hb = { n: 0, noCounters: 0, resets: 0, holes: 0, ivs: [], prev: null, firstT: null, lastT: null };
  // The last target any line gave: a heartbeat's applied target, or a target
  // line's new one. A held target landing gives none: it was handed out before.
  let lastTarget = null;
  let targetMax = null;
  // FR-148 P0: the target lines that hand a move out, by `why`, the held
  // targets landing, and the changes a heartbeat showed with no target line;
  // and the heartbeats' own count of every move (`target_moves`), by `why`.
  const tl = { moves: 0, byWhy: {}, landed: 0, unlined: 0 };
  const tm = { carried: 0, total: 0, byWhy: {} };
  // A target row's cause: the row's fields (`why=…`), after "(reason: not
  // logged)" on a line without `why`.
  const cause = (f, kind) => [text(f.why) === null ? '(reason: not logged)' : '', kv(f, FIELDS[kind])].filter(Boolean).join(' ');
  let lastPipeKey = null;
  const ka = { frames: 0, bytes: 0, carried: 0, missing: 0 };
  const cap = { judged: 0, at: 0, nominal: 0 };
  const enc = { opens: 0, qsv: 0, rejected: 0 };
  // FR-148 P0: `carried` counts the lines with the standing queue's fields,
  // `queued` those with a queued sample, `standingHigh` the highest standing.
  const del = { windows: 0, stalled: 0, longestMs: 0, stalledMs: 0, carried: 0, queued: 0, standingHigh: null };
  // FR-148 AC4: the T3 lines (a T3 in `loss`, or a one-packet `cwnd`), the
  // expiries `loss` counts and the lines carrying it, the stalled lines by
  // whether they had a T3, and each T3 line's row for the pairing; by node
  // (FR-148 D52) the pairings, the causes that moved, and the lines across
  // which none moved on either node.
  const t3 = { lines: 0, expiries: 0, carried: 0, stalledWith: 0, stalledWithout: 0, stalledUnread: 0, at: [], by: perNode(nodes, newBuckets), causes: perNode(nodes, () => ({})), quiet: 0 };
  const model = { n: 0, lastRx: null, maxRx: null, lastCap: null };
  let lastModelKey = null;
  let lastRuleAt = null;
  let lastFellAt = null;
  // FR-148 O10: the open lines, and the first one's answer.
  const open = { n: 0, path: null, ssthresh: null, rwnd: null };
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
        const moves = tally(f.target_moves);
        if (moves !== null) {
          tm.carried++;
          for (const [why, n] of moves) {
            tm.total += n;
            tm.byWhy[why] = (tm.byWhy[why] || 0) + n;
          }
        }
        if (target !== null) {
          // A move no target line printed: its cause is in this heartbeat's count.
          if (lastTarget !== null && target !== lastTarget) {
            tl.unlined++;
            push(r.t, 'target', `${lastTarget} -> ${target} bps, no target line (${moves !== null ? `target_moves=${text(f.target_moves)}` : 'reason: not logged'})`);
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
      case 'target': {
        // A move handed out: its new target (the watermark line's `new_target`).
        const bps = num(f.target_bps) !== null ? num(f.target_bps) : num(f.new_target);
        tl.moves++;
        const why = text(f.why) === null ? 'reason not logged' : text(f.why);
        tl.byWhy[why] = (tl.byWhy[why] || 0) + 1;
        push(r.t, 'target', `${lastTarget !== null ? `${lastTarget} -> ` : ''}${dash(bps)} bps ${cause(f, 'target')}`);
        if (bps !== null) {
          lastTarget = bps;
          targetMax = targetMax === null ? bps : Math.max(targetMax, bps);
        }
        break;
      }
      case 'landed':
        // A held FFmpeg target reaching the encoder: no new move.
        tl.landed++;
        push(r.t, 'target', `held ${dash(num(f.target_bps))} bps ${r.m.replace(/^FFmpeg DC pump: deferred bitrate /, '')} ${cause(f, 'landed')}`);
        break;
      case 'carrier':
        carrier = text(f.carrier);
        if (carrierAt === null) carrierAt = r.t - s.first;
        push(r.t, 'carrier', kv(f, FIELDS.carrier));
        break;
      case 'ice':
        push(r.t, 'ice', kv(f, FIELDS.ice));
        break;
      case 'rank': {
        rankSeen = true;
        const cr = text(f.carrier_rank);
        if (cr !== null && !/^(off|0|false|none)$/i.test(cr)) rankOn = true;
        push(r.t, 'rank', kv(f, FIELDS.rank));
        break;
      }
      case 'overlay':
        push(r.t, 'overlay', [r.m.slice(0, 120), kv(f, FIELDS.overlay)].filter(Boolean).join(' '));
        break;
      case 'qsv':
        enc.qsv++;
        push(r.t, 'encoder', `QVBR open: ${kv(f, FIELDS.qsv)}`);
        break;
      case 'encOpen':
        enc.opens++;
        // The message's own rest, "(constant-quality + maxrate cap)", has its parentheses.
        push(r.t, 'encoder', `${enc.opens > 1 ? 'REOPEN' : 'open'}: ${kv(f, FIELDS.encOpen)} ${r.m.replace(/^ffmpeg encoder opened /, '')}`);
        break;
      case 'rejected':
        enc.rejected++;
        push(r.t, 'encoder', `quality options rejected, retried with defaults: ${kv(f, FIELDS.rejected)}`);
        break;
      case 'delivery': {
        del.windows++;
        const st = num(f.stalled_ms);
        if (st !== null && st > 0) {
          del.stalled++;
          del.stalledMs += st;
          del.longestMs = Math.max(del.longestMs, st);
          push(r.t, 'stall', kv(f, FIELDS.stall));
        }
        // FR-148 P0: the standing queue, on a line that carries it (an older
        // agent's does not). The highest standing is over every such line.
        const queued = num(f.queued);
        if (queued !== null) {
          del.carried++;
          const standing = couple(f.standing_ms);
          if (standing) del.standingHigh = del.standingHigh === null ? standing[1] : Math.max(del.standingHigh, standing[1]);
          if (queued >= 1) {
            del.queued++;
            push(r.t, 'queue', kvAll(f, FIELDS.queue));
          }
        }
        // FR-148 AC4: a T3-rtx expiry, by its count (`loss`, from 0.4.227) or
        // by the one-packet window it leaves; a fast retransmission prints
        // its row too, untagged. A line read without SCTP's gauges carries
        // neither field, and its stall is counted apart.
        const loss = couple(f.loss);
        const cw = couple(f.cwnd);
        if (loss) t3.carried++;
        const isT3 = (loss !== null && loss[1] > 0) || (cw !== null && cw[0] === T3_CWND);
        if (st !== null && st > 0) {
          if (isT3) t3.stalledWith++;
          else if (loss !== null || cw !== null) t3.stalledWithout++;
          else t3.stalledUnread++;
        }
        if (isT3 || (loss !== null && loss[0] > 0)) {
          const e = push(r.t, 'loss', kvAll(f, FIELDS.loss) + (isT3 ? '; T3' : ''));
          if (isT3) {
            t3.lines++;
            if (loss !== null) t3.expiries += loss[1];
            const span = num(f.span_ms);
            t3.at.push({ t: r.t, span: span === null ? LINE_EVERY_MS : span, e });
          }
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
        // The row prints when a basis, the floor's push-back, the rule or the
        // fall changes, and again when a rule or a fall starts anew: its
        // start (the line's time less its age) moved (MODEL_ONSET_SLACK_MS).
        const rule = aged(f.model_rule);
        const fell = aged(f.model_fell);
        const startOf = (a) => (a && a.ageMs !== null ? r.t - a.ageMs : null);
        const moved = (a, b) => a !== null && b !== null && Math.abs(a - b) > MODEL_ONSET_SLACK_MS;
        const ruleAt = startOf(rule);
        const fellAt = startOf(fell);
        const key = [rx ? rx.basis || rx.raw : null, mc ? mc.basis || mc.raw : null, text(f.model_floor_push_back), rule && rule.what, fell && fell.what].join('|');
        if (key !== lastModelKey || moved(ruleAt, lastRuleAt) || moved(fellAt, lastFellAt)) {
          lastModelKey = key;
          push(r.t, 'model', kvAll(f, FIELDS.model));
        }
        lastRuleAt = ruleAt;
        lastFellAt = fellAt;
        break;
      }
      case 'open':
        // One a pump: the session's first is its answer.
        if (open.n++ === 0) Object.assign(open, { path: text(f.open_path), ssthresh: num(f.ssthresh), rwnd: num(f.rwnd) });
        push(r.t, 'open', kv(f, FIELDS.open));
        break;
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
        push(r.t, 'memory', `rate memory, assumed: ${kv(f, FIELDS.rateMem)}`);
        break;
      case 'capTiming':
        capTiming++;
        break;
      // FR-148 AC4 and D52: the /derp lines print in the session their time
      // falls in, the viewer node's as `v.…`; the relay they name never left
      // the database (KEPT).
      case 'shed':
      case 'drops':
        push(r.t, (r.node ? 'v.' : '') + r.kind, kvAll(f, FIELDS[r.kind]));
        break;
      case 'derp':
        push(r.t, (r.node ? 'v.' : '') + 'derp', [derpState(r.m), kv(f, FIELDS.derp)].filter(Boolean).join(' '));
        break;
    }
  }
  // FR-148 AC4 and D52: on the mesh relay, each T3 line by each node's drop
  // line (pairOf).
  if (carrier === MESH_RELAY) {
    for (const x of t3.at) {
      const p = pairOf(x.t, x.span, nodes);
      for (const q of p.per) {
        const b = t3.by[q.name][q.bucket];
        b.n++;
        if (q.between) b.between++;
        for (const w of q.causes) t3.causes[q.name][w] = (t3.causes[q.name][w] || 0) + 1;
      }
      if (p.quiet) t3.quiet++;
      x.e.line += '; ' + p.says;
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
    targetMax,
    targetLines: tl,
    targetMoves: tm,
    cap,
    ka,
    enc,
    del,
    t3,
    model,
    open,
    mem,
    capTiming,
    openAtEnd: windowEnd !== null && windowEnd !== undefined && s.last >= windowEnd - OPEN_AT_END_MS,
  };
  s.ivs = hb.ivs;
}

function deviceOf(list, unattributed, nodes) {
  const d = {
    // FR-148 AC4 and D52: the sessions on the mesh relay, summed; and each
    // node's /derp lines, every relay node's, in sessions or not.
    mesh: {
      sessions: 0,
      windows: 0,
      stalled: 0,
      lines: 0,
      expiries: 0,
      carried: 0,
      stalledWith: 0,
      stalledWithout: 0,
      stalledUnread: 0,
      pipes: {},
      by: perNode(nodes, newBuckets),
      causes: perNode(nodes, () => ({})),
      quiet: 0,
    },
    derp: perNode(nodes, () => ({ lines: 0, moved: {}, states: {} })),
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
    // FR-148 O10: sessions by their first open line's answer, and those with none.
    openPaths: {},
    noOpenLine: 0,
    unattributed,
  };
  for (const k of O7_COUNTED.concat(O7_ASIDE)) d.o7[k] = { sum: 0, carried: 0, missing: 0 };
  for (const k of OPEN_PATHS) d.openPaths[k] = 0;
  for (const s of list) {
    const x = s.summary;
    for (const k of Object.keys(x.pipes)) d.pipes[k] = (d.pipes[k] || 0) + x.pipes[k];
    if (!x.open.n) d.noOpenLine++;
    else {
      // An answer from a later agent counts under its own word.
      const p = dash(x.open.path);
      d.openPaths[p] = (d.openPaths[p] || 0) + 1;
    }
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
    if (x.carrier === MESH_RELAY) {
      const m = d.mesh;
      m.sessions++;
      m.windows += x.del.windows;
      m.stalled += x.del.stalled;
      for (const k of ['lines', 'expiries', 'carried', 'stalledWith', 'stalledWithout', 'stalledUnread']) m[k] += x.t3[k];
      for (const k of Object.keys(x.pipes)) m.pipes[k] = (m.pipes[k] || 0) + x.pipes[k];
      for (const n of Object.keys(m.by)) {
        for (const k of Object.keys(m.by[n])) {
          m.by[n][k].n += x.t3.by[n][k].n;
          m.by[n][k].between += x.t3.by[n][k].between;
        }
        for (const w of Object.keys(x.t3.causes[n])) m.causes[n][w] = (m.causes[n][w] || 0) + x.t3.causes[n][w];
      }
      m.quiet += x.t3.quiet;
    }
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
  for (const n of nodes) {
    const c = d.derp[n.name];
    for (const x of n.derp) {
      if (x.kind === 'derp') {
        c.states[x.state] = (c.states[x.state] || 0) + 1;
        continue;
      }
      c.lines++;
      for (const [w] of DROP_CAUSES) if (x.moved[w] !== null) c.moved[w] = (c.moved[w] || 0) + x.moved[w];
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
  if (o.viewer) L.push(`viewer node: agent ${mask(o.viewer)}, its /derp lines over the same window (--viewer-agent)`);
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
    L.push(`| target: highest / time at the encoder's cap (target = maxrate_bps) / time at >= 3 Mbit/s | ${dash(x.targetMax)} / ${pct(x.cap.at, x.cap.judged)} / ${pct(x.cap.nominal, x.whole ? x.whole.ms : 0)} |`);
    // FR-148 P0: the moves by why, as the target lines print them and as the
    // heartbeats count them (THE TARGET in the header says how the two relate).
    const tl = x.targetLines;
    const tm = x.targetMoves;
    L.push(`| target moves on the target lines, by why | moves ${tl.moves}${tl.moves ? ': ' + whyCells(tl.byWhy) : ''} |`);
    L.push(`| the heartbeats' target_moves (every move since the heartbeat before), by why (heartbeats carrying it) | ${tm.carried ? `moves ${tm.total}${tm.total ? ': ' + whyCells(tm.byWhy) : ''}` : '-'} (${tm.carried} of ${x.heartbeats}) |`);
    L.push(`| held targets landing / target changes a heartbeat showed with no target line | ${tl.landed} / ${tl.unlined} |`);
    L.push(`| heartbeats / without counters / counter resets / holes | ${x.heartbeats} / ${x.heartbeatsWithoutCounters} / ${x.resets} / ${x.holes} |`);
    L.push(`| encoder: opens / QVBR opens / rejected quality options | ${x.enc.opens} / ${x.enc.qsv} / ${x.enc.rejected} |`);
    L.push(`| delivery: windows / stalled / share / longest stall | ${x.del.windows} / ${x.del.stalled} / ${pct(x.del.stalled, x.del.windows)} / ${x.del.windows ? x.del.longestMs + ' ms' : '-'} |`);
    L.push(`| delivery: windows with a queued sample / carrying the standing queue / highest standing | ${x.del.carried ? x.del.queued : '-'} / ${x.del.carried} / ${x.del.standingHigh === null ? '-' : x.del.standingHigh + ' ms'} |`);
    // FR-148 AC4's Read A: a session on the mesh relay's T3 lines.
    if (x.carrier === MESH_RELAY) {
      const t = x.t3;
      L.push(`| mesh relay T3 (FR-148 AC4): delivery lines with a T3 / T3 expiries counted (delivery lines carrying loss) | ${t.lines} / ${t.carried ? t.expiries : '-'} (${t.carried} of ${x.del.windows}) |`);
      L.push(`| mesh relay T3: stalled delivery lines with a T3 / without one / carrying neither loss nor cwnd | ${t.stalledWith} / ${t.stalledWithout} / ${t.stalledUnread} |`);
      L.push(`| mesh relay T3: T3 lines by each node's nearest drop line with a count moved: within ${SHED_AFTER_MS / 1000} s after / only before, inside the line's span / none moved (of each, with a /derp lifecycle line between) | ${nodeCells(t.by, bucketCells)} |`);
      L.push(`| mesh relay T3: the causes that moved in that drop line, by node (T3 lines) | ${nodeCells(t.causes, causeCells)} |`);
      if (o.viewer) L.push(`| mesh relay T3: T3 lines across which no count moved on either node (a hold, or a loss below the overlay) | ${t.quiet} |`);
    }
    const rx = x.model.lastRx;
    const mc = x.model.lastCap;
    L.push(`| path model: lines / last viewer_rx (basis, age) / highest viewer_rx / last model_capacity (basis) | ${x.model.n} / ${rx ? `${dash(rx.bps)} (${dash(rx.basis || rx.raw)}, ${dash(rx.ageMs)} ms)` : '-'} / ${dash(x.model.maxRx)} / ${mc ? `${dash(mc.bps)} (${dash(mc.basis || mc.raw)})` : '-'} |`);
    const op = x.open;
    L.push(`| open path / ssthresh / rwnd at the first write | ${dash(op.path)} / ${dash(op.ssthresh)} / ${dash(op.rwnd)}${op.n > 1 ? ` (the first of ${op.n} open lines)` : ''} |`);
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
  // FR-148 AC4's Read A, over the sessions on the mesh relay; and the /derp lines.
  const mr = d.mesh;
  L.push(`| mesh relay (carrier ${MESH_RELAY}, FR-148 AC4): sessions / delivery lines / stalled / with a T3 / T3 expiries counted (lines carrying loss) | ${mr.sessions} / ${mr.windows} / ${mr.stalled} / ${mr.lines} / ${mr.carried ? mr.expiries : '-'} (${mr.carried} of ${mr.windows}) |`);
  L.push(`| mesh relay: stalled delivery lines with a T3 / without one / carrying neither loss nor cwnd; heartbeat windows transit-stalled | ${mr.stalledWith} / ${mr.stalledWithout} / ${mr.stalledUnread}; ${pipeCells(mr.pipes).share} |`);
  L.push(`| mesh relay: T3 lines by each node's nearest drop line with a count moved: within ${SHED_AFTER_MS / 1000} s after / only before, inside the line's span / none moved (of each, with a /derp lifecycle line between) | ${nodeCells(mr.by, bucketCells)} |`);
  L.push(`| mesh relay: the causes that moved in that drop line, by node (T3 lines) | ${nodeCells(mr.causes, causeCells)} |`);
  if (o.viewer) L.push(`| mesh relay: T3 lines across which no count moved on either node (a hold, or a loss below the overlay) | ${mr.quiet} |`);
  for (const n of Object.keys(d.derp)) {
    const c = d.derp[n];
    const states = Object.keys(c.states);
    L.push(`| /derp lines, ${n === VIEWER ? "the viewer node's" : "the agent's"}, every relay node: drop lines (moves summed by cause) / lifecycle lines by state | ${c.lines} (${causeCells(c.moved)}) / ${states.map((k) => `${k} ${c.states[k]}`).join(', ') || '-'} |`);
  }
  L.push(`| sessions by open path (O10) | ${Object.keys(d.openPaths).map((k) => `${k} ${d.openPaths[k]}`).join(', ')}; no open line ${d.noOpenLine} |`);
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
 * One kind's fields as the read projects them (`fieldsOf`): each as stored,
 * or only as a number where `numberOnly` says so. A field KEPT names for the
 * kind throws: no read is built that would let it out.
 */
function projectionOf(kind) {
  const p = {};
  for (const k of fieldsOf(kind)) {
    if ((KEPT[kind] || []).includes(k)) throw new Error(`the read would project ${kind}.${k}, which stays in the database (KEPT)`);
    const v = '$lines.fields.' + k;
    p[k] = numberOnly(kind, k) ? { $cond: [{ $isNumber: v }, v, '$$REMOVE'] } : v;
  }
  return p;
}

/**
 * The read for one agent and one window. `tenant` and `agent` are ObjectIds
 * (strings in the self-test); `from` and `to` are Dates. A line's fields are
 * its own kind's: the `$switch` takes the first kind whose message pattern
 * matches, in KINDS order, as `kindOf` does. `kinds` (FR-148 D52, the viewer
 * node's read: DERP_KINDS) keeps the read to those kinds' lines: its filter
 * and its `$switch` know no other.
 */
function timelinePipeline(o) {
  const kinds = o.kinds ? KINDS.filter((d) => o.kinds.includes(d.k)) : KINDS;
  const pattern = kinds.map((d) => d.re.source).join('|');
  const project = {
    $switch: {
      branches: kinds.map((d) => ({ case: { $regexMatch: { input: '$lines.msg', regex: d.re.source } }, then: projectionOf(d.k) })),
      default: { $literal: {} },
    },
  };
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
                { $regexMatch: { input: { $ifNull: ['$$l.msg', ''] }, regex: pattern } },
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
 * offline re-read renders byte for byte as the direct read: the projection
 * again (the line's own kind's fields, `fieldsOf`, less KEPT, numbers only
 * where `numberOnly` says so; the ids alone for a message no kind matches;
 * the memory lines' `fm`), ids masked through `ids` (one `masks()` per read),
 * every key DENY_KEY names left out, every string scrubbed as a printed line
 * is. FR-148 D52: a row of the viewer node's read carries `node` (VIEWER),
 * and leaves with a /derp kind's fields only (DERP_KINDS), never an `fm`.
 */
function rowOut(row, ids, node) {
  const idOf = ids || masks();
  const msg = typeof row.m === 'string' ? row.m : '';
  const kind = kindOf(msg);
  const ours = kind && (!node || DERP_KINDS.includes(kind));
  const keep = ours ? fieldsOf(kind).filter((k) => !(KEPT[kind] || []).includes(k)) : IDS;
  const plain = (o, only) => {
    if (!o) return undefined;
    const f = {};
    for (const k of Object.keys(o)) {
      if (DENY_KEY.test(k) || (only && !only.includes(k))) continue;
      let v = o[k];
      if (v !== null && typeof v === 'object' && typeof v.toNumber === 'function') v = v.toNumber();
      if (only && numberOnly(kind, k) && typeof v !== 'number') continue;
      if (ID_KEYS.has(k)) v = idOf(ID_KEYS.get(k), v);
      else if (typeof v === 'string') v = scrub(v);
      f[k] = v;
    }
    return f;
  };
  const out = { t: row.t instanceof Date ? row.t.toISOString() : row.t, m: typeof row.m === 'string' ? scrub(row.m) : row.m, f: plain(row.f, keep) };
  if (!node && row.fm && new RegExp(MEM_PATTERN).test(msg)) out.fm = plain(row.fm, null);
  if (node) out.node = VIEWER;
  return out;
}

/** The `--rows` text's first lines: the read's window, which the offline
 *  reader takes (the agent as its last 4 hex, and the limit; the viewer
 *  node's agent so, when it was read), and the server's record. */
function rowsHead(s, carrierRecord) {
  const w = { from: s.from.toISOString(), to: s.to.toISOString(), agent4: s.agent.slice(-4), limit: s.limit };
  if (s.viewer) w.viewer4 = s.viewer.agent.slice(-4);
  return [`# window: ${JSON.stringify(w)}`, `# carrier record: ${JSON.stringify(carrierRecord)}`];
}

/** The `--rows` text's last line of a read, after `n` rows came back for
 *  `limit`; the viewer node's read's (`node`) says so. */
function rowsTail(n, limit, node) {
  const cut = ` TRUNCATED at ${limit}: every batch stored after the last one read is missing, and some of its lines can be older than the last row's time; narrow --from/--to or raise --limit`;
  return `# ${node ? 'viewer node ' : ''}rows: ${Math.min(n, limit)}${n > limit ? cut : ''}`;
}

/** What the direct read renders with. */
function readOpts(s, truncated, carrierRecord) {
  return { truncated, limit: s.limit, agent: s.agent, viewer: s.viewer ? s.viewer.agent : null, from: s.from, to: s.to, carrierRecord, read: { from: s.from, to: s.to } };
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
  return render(fold(rows, { truncated, intervals: !!o.intervals, agent: o.agent || w.agent4, viewer: w.viewer4 || null, from: o.from || w.from, to: o.to || w.to, carrierRecord, limit: w.limit, read }));
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
  // FR-148 D52: the viewer's node, the other end of the mesh relay pair: ONE
  // more agent, in its own org when it uploads under another (a node uploads
  // under its primary enrollment), read for its /derp lines only.
  const vAgent = env.RC_PATH_VIEWER || '';
  const vTenant = env.RC_PATH_VIEWER_TENANT || '';
  let viewer = null;
  if (vAgent || vTenant) {
    if (!hex.test(vAgent)) throw new Error('--viewer-agent takes the viewer node\'s 24-hex agent id');
    if (vTenant && !hex.test(vTenant)) throw new Error('--viewer-tenant takes a 24-hex tenant id');
    viewer = { tenant: vTenant || env.RC_PATH_TENANT, agent: vAgent };
    if (vAgent.toLowerCase() === agents[0].toLowerCase()) throw new Error('--viewer-agent names the other node: not the --agent read');
  }
  return { tenant: env.RC_PATH_TENANT, agent: agents[0], from, to, limit, viewer };
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
  // FR-148 D52: the viewer node's read, its /derp lines only, the same window.
  const vPipeline = s.viewer ? timelinePipeline({ tenant: ObjectId(s.viewer.tenant), agent: ObjectId(s.viewer.agent), from: s.from, to: s.to, limit: s.limit, kinds: DERP_KINDS }) : null;
  const sessions = database.getCollection('remote_sessions');
  if (env.RC_PATH_EXPLAIN === '1') {
    const stages = planStages(coll.explain('queryPlanner').aggregate(pipeline, QUERY_OPTIONS));
    print(`plan: ${stages.join(' <- ')}`);
    print(`blocking SORT stage: ${stages.some((x) => /^SORT\b/.test(x)) ? 'YES' : 'none'}`);
    if (vPipeline) {
      const vStages = planStages(coll.explain('queryPlanner').aggregate(vPipeline, QUERY_OPTIONS));
      print(`viewer node plan: ${vStages.join(' <- ')}`);
      print(`viewer node blocking SORT stage: ${vStages.some((x) => /^SORT\b/.test(x)) ? 'YES' : 'none'}`);
    }
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
  let truncated = n > s.limit;
  if (emitRows) print(rowsTail(n, s.limit));
  if (vPipeline) {
    let vn = 0;
    coll.aggregate(vPipeline, QUERY_OPTIONS).forEach((row) => {
      vn++;
      if (vn > s.limit) return;
      const out = rowOut(row, idMasks, VIEWER);
      if (emitRows) print(JSON.stringify(out));
      else rows.push(out);
    });
    truncated = truncated || vn > s.limit;
    if (emitRows) print(rowsTail(vn, s.limit, VIEWER));
  }
  if (emitRows) return;
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

/** FR-148 P0: every cause a `why` is written with, as the agent writes it.
 *  The self-test compares this list with the source (`whyWordsOf`) and puts
 *  each on a fixture target line. */
const WHY_WORDS = [
  'open',
  'increase(app-limited)',
  'increase(loaded)',
  'increase(no-window)',
  'decrease(full)',
  'decrease(overflow)',
  'decrease(stall)',
  'decrease(age)',
  'decrease(link)',
  'decrease(transit-stall)',
  'hard-decrease',
  'ceiling(plan)',
  'ceiling(pipe)',
  'ceiling(learned)',
  'ceiling(arrival)',
  'ceiling(opener)',
  'floor(area)',
  'floor(relief)',
  'floor(ramp)',
  'reapply',
];

/** The causes a `why` is written with, read from the agent's source, sorted:
 *  encode/aimd.rs `Cause`'s Display (a cause with a parameter writes its
 *  enum's `as_str` words) and encode/governor.rs `Why`'s, which writes an
 *  increase with the last window's verdict. Both texts with LF line ends. */
function whyWordsOf(aimd, gov) {
  const block = (src, head) => {
    const i = src.indexOf(head);
    return i < 0 ? '' : src.slice(i, src.indexOf('\n}\n', i));
  };
  const strs = (name) => Array.from(block(aimd, `impl ${name} {`).matchAll(new RegExp(`${name}::\\w+ => "([^"]+)"`, 'g')), (m) => m[1]);
  const param = {};
  for (const m of block(aimd, 'pub enum Cause {').matchAll(/^\s*(\w+)\((\w+)\),$/gm)) param[m[1]] = m[2];
  const words = [];
  for (const m of block(aimd, 'impl fmt::Display for Cause {').matchAll(/Cause::(\w+)(?:\(\w+\))? => (?:f\.write_str\("([^"]+)"\)|write!\(f, "([\w-]+)\(\{\}\)", \w+\.as_str\(\)\))/g)) {
    if (m[2] === 'increase') continue;
    if (m[2]) words.push(m[2]);
    else for (const w of strs(param[m[1]])) words.push(`${m[3]}(${w})`);
  }
  for (const m of block(gov, 'impl std::fmt::Display for Why {').matchAll(/\(Cause::Increase, (?:Some\((?:true|false)\)|None)\) => f\.write_str\("([^"]+)"\)/g)) words.push(m[1]);
  return words.sort();
}

/**
 * FR-148 AC4: the tracing calls in a Rust text (LF line ends), each as its
 * level, the fields written before its message (`%relay`, `?x`, `name = …`
 * by name) and the message: the first string literal that starts an
 * argument, a line continued with `\` joined as Rust joins it.
 */
function derpCalls(body) {
  const out = [];
  for (const m of body.matchAll(/\b(trace|debug|info|warn|error)!\(/g)) {
    const args = [];
    let cur = '';
    let depth = 1;
    let msg = null;
    for (let i = m.index + m[0].length; i < body.length && depth > 0; ) {
      const c = body[i];
      if (c === '"') {
        const lit = /^"((?:[^"\\]|\\[\s\S])*)"/.exec(body.slice(i));
        if (!lit) break;
        if (depth === 1 && cur.trim() === '') {
          msg = lit[1].replace(/\\\n[ \t]*/g, '');
          break;
        }
        cur += lit[0];
        i += lit[0].length;
        continue;
      }
      if ('([{'.includes(c)) depth++;
      if (')]}'.includes(c)) depth--;
      if (c === ',' && depth === 1) {
        args.push(cur.trim());
        cur = '';
      } else cur += c;
      i++;
    }
    if (msg === null) continue;
    const fields = args.map((a) => (/^[%?]?\s*([A-Za-z_]\w*)\s*(?:=|$)/.exec(a) || [])[1]).filter(Boolean);
    out.push({ level: m[1], fields, msg });
  }
  return out;
}

/**
 * The read's two `$project` stages, evaluated as the server would on lines
 * as the agent writes them (`{ ts, msg, fields }`): the filter by message
 * and time, then `t`, `m`, the kind's fields (`f`, the first `$switch`
 * branch that matches) and a memory line's `fm`. It knows the shapes the
 * read is built of ($switch, $regexMatch on the message, a field path, $cond
 * on $isNumber, $literal, the memory filter) and throws on any other, so it
 * never passes a shape it did not evaluate. The self-test's (FR-148 AC4).
 */
function serverRows(pipeline, lines) {
  const fail = (what, x) => {
    throw new Error(`serverRows: ${what}: ${JSON.stringify(x)}`);
  };
  const keep = pipeline[2].$project.lines.$filter.cond.$and;
  const second = pipeline[4].$project;
  const field = (p, line) => {
    const m = typeof p === 'string' && /^\$lines\.fields\.(\w+)$/.exec(p);
    if (!m) fail('not a field path', p);
    return line.fields && Object.prototype.hasOwnProperty.call(line.fields, m[1]) ? line.fields[m[1]] : undefined;
  };
  const matches = (c, line) => {
    const r = c && c.$regexMatch;
    if (!r || Object.keys(c).length !== 1 || r.input !== '$lines.msg' || Object.keys(r).join(',') !== 'input,regex') fail('not a $regexMatch on the message', c);
    return new RegExp(r.regex).test(line.msg);
  };
  const valueOf = (v, line) => {
    if (typeof v === 'string') return field(v, line);
    const c = v && v.$cond;
    if (Array.isArray(c) && Object.keys(v).length === 1 && c.length === 3 && c[0] && Object.keys(c[0]).join(',') === '$isNumber' && c[0].$isNumber === c[1] && c[2] === '$$REMOVE') {
      const x = field(c[1], line);
      return typeof x === 'number' ? x : undefined;
    }
    return fail('an expression it does not evaluate', v);
  };
  const objectOf = (o, line) => {
    const out = {};
    for (const k of Object.keys(o)) {
      if (k[0] === '$') fail('an operator where a field was', k);
      const x = valueOf(o[k], line);
      if (x !== undefined) out[k] = x;
    }
    return out;
  };
  const sw = second.f && second.f.$switch;
  if (!sw || Object.keys(second.f).length !== 1 || Object.keys(sw).join(',') !== 'branches,default') fail('f is not one $switch', second.f);
  if (JSON.stringify(sw.default) !== JSON.stringify({ $literal: {} })) fail('a default other than {}', sw.default);
  const fm = second.fm && second.fm.$cond;
  if (!fm || fm[2] !== '$$REMOVE' || fm[0].$regexMatch.regex !== MEM_PATTERN || fm[1].$arrayToObject.$filter.cond.$not[0].$regexMatch.regex !== DENY_KEY.source) fail('fm is not the memory filter', second.fm);
  if (second.t !== '$lines.ts' || second.m !== '$lines.msg' || Object.keys(second).join(',') !== 't,m,f,fm') fail('the stage projects more than t, m, f and fm', Object.keys(second));
  // Every branch's shape, whether a line takes it or not.
  for (const b of sw.branches) {
    if (Object.keys(b).join(',') !== 'case,then') fail('a branch other than case and then', b);
    matches(b.case, { msg: '' });
    objectOf(b.then, { msg: '', fields: {} });
  }
  const out = [];
  for (const line of lines) {
    if (!(new RegExp(keep[0].$regexMatch.regex).test(line.msg || '') && line.ts >= keep[1].$gte[1] && line.ts < keep[2].$lt[1])) continue;
    const b = sw.branches.find((x) => matches(x.case, line));
    const row = { t: line.ts, m: line.msg, f: b ? objectOf(b.then, line) : {} };
    if (matches(fm[0], line)) row.fm = Object.fromEntries(Object.entries(line.fields || {}).filter((kv) => !DENY_KEY.test(kv[0])));
    out.push(row);
  }
  return out;
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
  const firstDiffOf = (a, b) => {
    const x = String(a).split('\n');
    const y = String(b).split('\n');
    const i = x.findIndex((l, j) => l !== y[j]);
    return i < 0 ? `lengths ${x.length} / ${y.length}` : `line ${i + 1}: ${x[i]} | ${y[i]}`;
  };
  // 1. Every message literal is in the agent's source, at the file named.
  try {
    const fs = require('fs');
    const path = require('path');
    const src = path.join(__dirname, '..', 'agents', 'roomlerd', 'src');
    // A checkout can have CRLF line ends; a Rust string continued with `\`
    // at a line's end goes on after the next line's indent.
    const lf = (s) => s.replace(/\r\n/g, '\n');
    const cont = (s) => lf(s).replace(/\\\n[ \t]*/g, '');
    for (const d of KINDS) {
      const body = cont(fs.readFileSync(path.join(src, d.src[0]), 'utf8'));
      for (const lit of d.src.slice(1)) {
        check(`source: ${d.k} literal in ${d.src[0]}`, body.includes(lit), lit);
        check(`source: ${d.k} pattern matches its literal`, d.re.test(lit), lit);
      }
      // A line the agent wrote before: still read, and gone from the source.
      if (d.was) check(`source: ${d.k} is read as an older agent wrote it, and ${d.src[0]} writes it no more`, d.re.test(d.was) && !body.includes(d.was), d.was);
    }
    const pm = fs.readFileSync(path.join(src, 'media_pair_memory.rs'), 'utf8');
    for (const k of O7_COUNTED.concat(O7_ASIDE, ['taken', 'taken_why', 'wrote', 'why'])) check(`source: close/open field ${k}`, pm.includes(`"${k}"`));
    const md = fs.readFileSync(path.join(src, 'media_delivery.rs'), 'utf8');
    for (const k of FIELDS.open) check(`source: open line field ${k}`, md.includes(`${k} = `));
    for (const k of OPEN_PATHS) check(`source: open line answer ${k}`, md.includes(`"${k}"`));
    // FR-148 P0: the delivery line's standing-queue fields, and FR-148 AC4's
    // loss row's, as `emit` writes them.
    for (const k of new Set(FIELDS.queue.concat(FIELDS.loss))) check(`source: delivery line field ${k}`, new RegExp(`\\b${k} = \\??(?:q|line)\\.${k},`).test(md));
    // FR-148 D46 B2 and D48: the model line's push-back and rule, as
    // `emit_model` writes them (a fixed word; `<word>/<age>ms`, which `aged`
    // reads); D50's fall where the agent writes it (#4231), `<from>-><to>/<age>ms`.
    for (const lit of ['model_floor_push_back = %PushedBy::word_of(v.model_floor_by),', 'model_rule = %RuledShown(v.model_ruled),', 'write!(f, "{}/{}ms", rule.word(), age.as_millis())']) check(`source: model line ${lit}`, md.includes(lit));
    check('source: model_fell, where the agent writes it, as D50 writes it', !md.includes('model_fell') || md.includes('model_fell = %FellShown(v.model_fell),'));
    for (const k of FIELDS.model.filter((k) => k !== 'model_fell')) check(`source: model line field ${k}`, md.includes(`${k} = %`));
    const sc = fs.readFileSync(path.join(src, 'session_carrier.rs'), 'utf8');
    check('source: carrier line field relay_transport', /\brelay_transport = relay\.as_str\(\),/.test(sc));
    // FR-148 AC4: the constants the T3 rows stand on, from the code.
    check(`source: the mesh relay's carrier word is ${MESH_RELAY}`, sc.includes(`SessionCarrier::Derp => "${MESH_RELAY}",`));
    const every = /pub const LINE_EVERY: Duration = Duration::from_secs\((\d+)\);/.exec(md);
    check('source: LINE_EVERY_MS is the delivery line\'s period', every && Number(every[1]) * 1000 === LINE_EVERY_MS, every && every[1]);
    const sctp = path.join(__dirname, '..', 'crates', 'vendored', 'webrtc-sctp', 'src', 'association');
    const amod = lf(fs.readFileSync(path.join(sctp, 'mod.rs'), 'utf8'));
    const aint = lf(fs.readFileSync(path.join(sctp, 'association_internal.rs'), 'utf8'));
    const mtu = /\bconst INITIAL_MTU: u32 = (\d+);/.exec(amod);
    check('source: T3_CWND is the vendored SCTP\'s MTU, which nothing changes', mtu && Number(mtu[1]) === T3_CWND && aint.includes('let mtu = INITIAL_MTU;') && !/\bself\.mtu\s*=[^=]/.test(aint), mtu && mtu[1]);
    const halve = 'self.ssthresh = std::cmp::max(self.cwnd / 2, 4 * self.mtu);';
    const sets = Array.from(aint.matchAll(/^\s*(.*)\n\s*self\.cwnd = ([^;]+);/gm), (m) => `${m[1]} => ${m[2]}`);
    check(
      'source: only a T3 expiry sets cwnd to one packet: it starts at 4,380 at least two packets, a fast retransmit sets it to ssthresh (four packets at least), and the rest adds',
      aint.includes('let cwnd = std::cmp::min(4 * mtu, std::cmp::max(2 * mtu, 4380));') && sets.length === 2 && sets.includes(`${halve} => self.ssthresh`) && sets.includes(`${halve} => self.mtu`),
      sets.join(' | '),
    );
    // FR-148 AC4 and D52: derp.rs `run` writes the drop line on its keepalive
    // tick when a count moved (`DropLine::due`, `DropLine::log`), and every
    // tracing line there or in `DropLine` that opens "overlay derp:" is read
    // under the kind its message names, with only fields the read names
    // (FIELDS) or keeps (KEPT); a debug line is not read at all.
    const derpSrc = lf(fs.readFileSync(path.join(src, 'derp.rs'), 'utf8'));
    const ka = /\bconst DERP_KEEPALIVE: Duration = Duration::from_secs\((\d+)\);/.exec(derpSrc);
    check('source: SHED_AFTER_MS is the /derp keepalive tick', ka && Number(ka[1]) * 1000 === SHED_AFTER_MS, ka && ka[1]);
    const blockOf = (head) => {
      const i = derpSrc.indexOf(head);
      return i < 0 ? '' : derpSrc.slice(i, derpSrc.indexOf('\n}\n', i));
    };
    const run = blockOf('\nasync fn run(');
    const dropImpl = blockOf('\nimpl DropLine {');
    const tick = run.indexOf('due = keepalive.tick() =>');
    const dueAt = run.indexOf('if let Some(line) = DropLine::due(now, last_reported) {');
    check(
      'source: the drop line is written on the keepalive tick, when a count moved since the last one reported',
      tick > 0 && dueAt > tick && run.indexOf('out = outbound_rx.recv() =>') > dueAt && run.slice(tick, dueAt).includes('let now = m.drops();') && run.slice(dueAt).includes('line.log(&relay);\n') && run.slice(dueAt).includes('last_reported = now;'),
    );
    check('source: DropLine::due is due when any of the four counts moved', /fn due\(now: DerpDrops, last: DerpDrops\) -> Option<Self> \{\s*\(now != last\)\.then_some\(Self \{ now, last \}\)\s*\}/.test(dropImpl));
    const calls = derpCalls(run).concat(derpCalls(dropImpl));
    const seen = {};
    for (const c of calls.filter((x) => x.msg.startsWith('overlay derp:'))) {
      if (c.level === 'debug' || c.level === 'trace') {
        check(`source: derp.rs ${c.level} line not read (${c.msg.slice(14, 50)})`, !new RegExp(MSG_PATTERN).test(c.msg));
        continue;
      }
      const kind = kindOf(c.msg);
      const word = kind === 'drops' ? 'drops' : kind === 'derp' ? derpState(c.msg) : null;
      check(`source: derp.rs ${c.level} line read as the drop line or a lifecycle word (${c.msg.slice(14, 50)})`, word !== null && word !== 'unknown', c.msg);
      if (word === null || word === 'unknown') continue;
      seen[word] = (seen[word] || 0) + 1;
      const known = fieldsOf(kind).concat(KEPT[kind] || []);
      const stray = c.fields.filter((k) => !known.includes(k));
      check(`source: derp.rs ${word} line: every field named or kept`, stray.length === 0, stray.join(','));
      for (const k of KEPT[kind] || []) seen[`${kind}.${k}`] = (seen[`${kind}.${k}`] || 0) + (c.fields.includes(k) ? 1 : 0);
    }
    check('source: derp.rs writes the drop line once, no shed line, and each lifecycle word once', seen.drops === 1 && !seen.shed && DERP_STATES.every((x) => seen[x[0]] === 1), JSON.stringify(seen));
    const writes = Object.keys(KEPT).filter((kind) => !KINDS.find((d) => d.k === kind).was);
    check('source: what KEPT names, the /derp lines write (relay on all, e on the failed connect)', writes.every((kind) => KEPT[kind].every((k) => seen[`${kind}.${k}`] >= 1)) && seen['derp.relay'] === DERP_STATES.length && seen['drops.relay'] === 1, JSON.stringify(seen));
    check(
      'source: the drop line writes relay, then each cause\'s count and move in DROP_CAUSES order, and nothing else',
      calls.filter((x) => x.msg === DROP_LINE).map((x) => x.fields.join(',')).join(' | ') === ['relay'].concat(FIELDS.drops).join(',') &&
        FIELDS.drops.join(',') === [].concat(...DROP_CAUSES.map((c) => c.slice(1))).join(',') &&
        FIELDS.shed.join(',') === DROP_CAUSES[0].slice(1).join(','),
      calls.filter((x) => x.msg === DROP_LINE).map((x) => x.fields.join(',')).join(' | '),
    );
    // FR-148 P0: the target lines' fields and the heartbeats' count, as the
    // pumps write them, and the words a `why` is made of, from the code that
    // writes them.
    const peer = fs.readFileSync(path.join(src, 'peer.rs'), 'utf8');
    for (const lit of ['target_bps = applied.bps, why = %applied.why,', 'new_target = applied.bps, why = %applied.why,', 'target_bps = held.bps, why = %held.why,', 'deferred = deferred_bps.is_some(),']) {
      check(`source: target line fields ${lit}`, peer.includes(lit));
    }
    check('source: both heartbeats carry target_moves', peer.split('target_moves = %governor.take_moves(),').length - 1 === 2);
    const aimd = lf(fs.readFileSync(path.join(src, 'encode', 'aimd.rs'), 'utf8'));
    const gov = lf(fs.readFileSync(path.join(src, 'encode', 'governor.rs'), 'utf8'));
    const words = whyWordsOf(aimd, gov);
    check("source: the why words are the code's, every one", words.join(' ') === WHY_WORDS.slice().sort().join(' '), words.join(' '));
    // The written forms the parse reads: a why's causes joined by `+`, `none`,
    // `+more`; a count's `why:n` joined by `,`, `more:n`, `none`.
    for (const lit of ['sep = "+";', '("", _) => f.write_str("none"),', '(_, true) => f.write_str("+more"),', 'write!(f, "{sep}{why}:{n}")?;', 'sep = ",";', 'write!(f, "{sep}more:{}", self.more)?;', 'return f.write_str("none");']) {
      check(`source: the written form ${lit}`, gov.includes(lit));
    }
    // `why` leaves as written from a kind that names it and from the path
    // memory's lines, so every line read that writes one must write the
    // governor's `Why` (a target line) or the path memory's word (the close
    // line): fixed words, never free text. A line read that writes any other
    // `why` fails here. A call's message is its first string literal at the
    // start of a line after the field, a `{CONST}` in it read from its file.
    const whyOk = ['%applied.why', '%held.why', '%why.word()'];
    const whyRead = [];
    for (const file of Array.from(new Set(KINDS.map((d) => d.src[0])))) {
      const body = lf(fs.readFileSync(path.join(src, file), 'utf8'));
      for (const w of body.matchAll(/(?<!let |let mut |\.)\bwhy = ([^,\n]+),/g)) {
        const lit = /^\s*"((?:[^"\\]|\\[\s\S])*)"/m.exec(body.slice(w.index + w[0].length));
        const msg = (lit ? lit[1] : '').replace(/\{([A-Z_]+)\}/g, (x, name) => {
          const c = new RegExp(`const ${name}: &str = "([^"]*)";`).exec(body);
          return c ? c[1] : x;
        });
        if (!new RegExp(MSG_PATTERN).test(msg)) continue;
        whyRead.push(`${file}: ${w[1]}`);
        check(`source: a line read writes why from fixed words (${file}: ${msg.slice(0, 40)})`, whyOk.includes(w[1]), w[1]);
      }
    }
    check('source: the lines read that write why: the five target lines and the close line', whyRead.length === 6 && whyRead.filter((x) => x.startsWith('peer.rs: ')).length === 5, whyRead.join('; '));
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
  // FR-148 O10: the first write opened on the peer's COOKIE ECHO; the reopen's
  // pump writes a second open line, after a loss, which is not the answer.
  const openFx = (sec, sid, pump, open_path, ssthresh, rwnd, o) => ({
    t: at(sec),
    m: 'path open (FR-148 P0, O10)',
    f: Object.assign({ session_id: sid, pump, open_path, ssthresh, rwnd, cwnd: 4380, inflight_bytes: 0, pending_bytes: 2404, max_payload: 1188, fast_retransmits: 0, t3_expiries: 0 }, o),
  });
  rows.push(openFx(3.5, FX.s1, 'ffmpeg-dc', 'cookie_echo', 0, 1043200));
  rows.push(openFx(51, FX.s1, 'ffmpeg-dc', 'unknown', 17520, 1040800, { fast_retransmits: 2 }));
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
  rows.push(openFx(105.5, FX.s2, 'vp9-444-dc', 'init_ack', 1048576, 1046020, { pending_bytes: 0 }));
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
  check('a target move with no target line and its reason unlogged', s1.ev.some((e) => e.what === 'target' && e.line === '3000000 -> 2000000 bps, no target line (reason: not logged)'));
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
  check(
    "open: a session's first open line is its answer",
    s1.summary.open.n === 2 && s1.summary.open.path === 'cookie_echo' && s1.summary.open.ssthresh === 0 && s1.summary.open.rwnd === 1043200 && s2.summary.open.n === 1 && s2.summary.open.path === 'init_ack',
    JSON.stringify([s1.summary.open, s2.summary.open]),
  );
  const out = render(r);
  check('open: the line among the session\'s lines, every field in order', /^  07:00:03\.500  open     pump=ffmpeg-dc open_path=cookie_echo ssthresh=0 rwnd=1043200 cwnd=4380 inflight_bytes=0 pending_bytes=2404 max_payload=1188 fast_retransmits=0 t3_expiries=0$/m.test(out));
  check(
    'open: the session rows',
    /^\| open path \/ ssthresh \/ rwnd at the first write \| cookie_echo \/ 0 \/ 1043200 \(the first of 2 open lines\) \|$/m.test(out) && /^\| open path \/ ssthresh \/ rwnd at the first write \| init_ack \/ 1048576 \/ 1046020 \|$/m.test(out),
  );
  check('open: the device row', /^\| sessions by open path \(O10\) \| cookie_echo 1, init_ack 1, unknown 0, unread 0; no open line 0 \|$/m.test(out), (out.match(/^\| sessions by open path.*$/m) || [''])[0]);
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
  // FR-148 O10: a session for each answer, and one with no open line.
  const oid = (x) => `f1480000000000000000${x}`;
  const four = render(
    fold(
      [
        openFx(10, oid('c0e1'), 'ffmpeg-dc', 'cookie_echo', 0, 1043200),
        openFx(20, oid('c0e2'), 'vp9-444-dc', 'init_ack', 1048576, 1046020),
        openFx(30, oid('c0e3'), 'ffmpeg-dc', 'unknown', 17520, 1040800, { t3_expiries: 1 }),
        { t: at(40), m: 'path open (FR-148 P0, O10)', f: { session_id: oid('c0e4'), pump: 'ffmpeg-dc', open_path: 'unread' } },
        { t: at(50), m: 'session carrier recorded (FR-102 P9c)', f: { session_id: oid('c0e5'), carrier: 'direct' } },
      ],
      { to: at(600) },
    ),
  );
  check('open: the device row counts each answer, and the sessions with no open line', /^\| sessions by open path \(O10\) \| cookie_echo 1, init_ack 1, unknown 1, unread 1; no open line 1 \|$/m.test(four), (four.match(/^\| sessions by open path.*$/m) || [''])[0]);
  check(
    'open: an unread line prints what it carries, and the summary dashes the rest',
    /^  07:00:40\.000  open     pump=ffmpeg-dc open_path=unread$/m.test(four) &&
      /^\| open path \/ ssthresh \/ rwnd at the first write \| unread \/ - \/ - \|$/m.test(four) &&
      /^\| open path \/ ssthresh \/ rwnd at the first write \| - \/ - \/ - \|$/m.test(four),
  );
  // FR-148 P0: the relay transport and the standing queue. A session on TURN
  // over TCP: a delivery line with two queued samples, one with none. And an
  // older agent's session, whose delivery line carries no queue fields.
  const sq = fold(
    [
      { t: at(10), m: 'session carrier recorded (FR-102 P9c)', f: { session_id: oid('5a01'), carrier: 'turn', relay_transport: 'tcp', local_typ: 'Relay', remote_typ: 'Relay' } },
      {
        t: at(20),
        m: 'path delivery (FR-148 P0, shadow, ≈10s)',
        f: { session_id: oid('5a01'), span_ms: 9994, delivery_samples: '(0, 5, 0)', backlogged_bps: 'None', app_limited_bps: 'Some(391566)', stalled_ms: 0, standing_ms: 'Some((40, 610))', queued: 2, inflight: '(327500, 540000)', cwnd: '(332780, 544380)', rwnd_low: 508576 },
      },
      {
        t: at(30),
        m: 'path delivery (FR-148 P0, shadow, ≈10s)',
        f: { session_id: oid('5a01'), span_ms: 10002, delivery_samples: '(0, 5, 0)', backlogged_bps: 'None', app_limited_bps: 'Some(52000)', stalled_ms: 0, standing_ms: 'Some((0, 19))', queued: 0, inflight: '(0, 2400)', cwnd: '(544380, 544380)', rwnd_low: 1046020 },
      },
      { t: at(40), m: 'path delivery (FR-148 P0, shadow, ≈10s)', f: { session_id: oid('5a02'), span_ms: 10000, delivery_samples: '(5, 0, 0)', backlogged_bps: 'Some((1, 2, 3))', app_limited_bps: 'None', stalled_ms: 0 } },
    ],
    { to: at(600) },
  );
  const sqOut = render(sq);
  check('relay_transport: the carrier line prints it after the carrier', /^  07:00:10\.000  carrier  carrier=turn relay_transport=tcp local_typ=Relay remote_typ=Relay$/m.test(sqOut), (sqOut.match(/^.*  carrier  .*$/m) || [''])[0]);
  check(
    'queue: a delivery line with a queued sample is a queue row, every field, the one it does not carry (ack_ms) as -',
    /^  07:00:20\.000  queue    queued=2 standing_ms=\(40, 610\) inflight=\(327500, 540000\) cwnd=\(332780, 544380\) rwnd_low=508576 ack_ms=-$/m.test(sqOut),
    (sqOut.match(/^.*  queue  .*$/m) || [''])[0],
  );
  check('queue: a line with none queued, or none of the fields, is no row', (sqOut.match(/^ {2}\S+ {2}queue /gm) || []).length === 1);
  const sd = sq.sessions.map((x) => x.summary.del);
  check('queue: the session counts its queued windows, the lines carrying the fields, and the highest standing', sd[0].queued === 1 && sd[0].carried === 2 && sd[0].standingHigh === 610 && sd[1].queued === 0 && sd[1].carried === 0 && sd[1].standingHigh === null, JSON.stringify(sd));
  check(
    "queue: the session row, and an older agent's session reads - where it carries nothing",
    /^\| delivery: windows with a queued sample \/ carrying the standing queue \/ highest standing \| 1 \/ 2 \/ 610 ms \|$/m.test(sqOut) &&
      /^\| delivery: windows with a queued sample \/ carrying the standing queue \/ highest standing \| - \/ 0 \/ - \|$/m.test(sqOut),
  );
  // FR-148 P0: why the target moved. A VP9-444 session from 0.4.230 whose
  // target lines carry every cause there is, one each, a heartbeat after
  // every fifth counting the five; an FFmpeg session with a folded why,
  // `none`, `+more`, a held target landing both ways, a move no target line
  // printed and a count past the whys the governor keeps; and a session from
  // an agent before 0.4.230, whose lines carry neither `why` nor
  // `target_moves`.
  const sv = oid('7a01');
  const sf = oid('7a02');
  const so = oid('7a03');
  const row = (sec) => `  ${clock(epochMs(at(sec)))}  target   `;
  const moved = [{ t: at(600), m: 'session carrier recorded (FR-102 P9c)', f: { session_id: sv, carrier: 'direct', local_typ: 'Host', remote_typ: 'Srflx' } }];
  const wantV = [];
  let bps = null;
  let since = [];
  WHY_WORDS.forEach((w, i) => {
    const sec = 600.5 + i;
    const prev = bps;
    bps = w === 'open' ? 3000000 : w === 'reapply' ? bps : /^(increase|floor)/.test(w) ? bps + 250000 : w === 'hard-decrease' ? Math.round(bps / 2000) * 1000 : Math.round((bps * 0.85) / 1000) * 1000;
    // A ×0.85 at the DC buffer's watermark has a line of its own, with `new_target`.
    moved.push(
      w === 'decrease(overflow)'
        ? { t: at(sec), m: 'VP9-444 AIMD decrease (DC buffer over high watermark)', f: { session_id: sv, new_target: bps, why: w } }
        : { t: at(sec), m: 'VP9-444 set_bitrate (AIMD)', f: { session_id: sv, target_bps: bps, why: w } },
    );
    wantV.push(`${row(sec)}${prev === null ? '' : prev + ' -> '}${bps} bps why=${w}`);
    since.push(`${w}:1`);
    if (since.length === 5) {
      moved.push({ t: at(sec + 0.5), m: 'VP9-444 DC pump heartbeat (≈1s window)', f: { session_id: sv, codec_label: 'VP9-444', target_bps: bps, target_fps: 30, bytes_written: 400000 * (i + 1), frames_sent: 30 * (i + 1), target_moves: since.join(',') } });
      since = [];
    }
  });
  const FOLD = 'decrease(stall)+ceiling(plan)';
  const MORE = 'floor(area)+ceiling(plan)+decrease(full)+decrease(stall)+ceiling(learned)+floor(ramp)+decrease(age)+ceiling(arrival)+more';
  const ffSet = (sec, sid, o) => ({ t: at(sec), m: 'FFmpeg DC pump set_bitrate (AIMD)', f: Object.assign({ session_id: sid, codec_label: 'HEVC' }, o) });
  const ffLand = (sec, sid, how, o) => ({ t: at(sec), m: `FFmpeg DC pump: deferred bitrate ${how}`, f: Object.assign({ session_id: sid, codec_label: 'HEVC' }, o) });
  const ffHb = (sec, sid, from, target, moves) => hbRow(sec, sid, Object.assign({ target_bps: target, maxrate_bps: `Some(${target})`, bytes_written: 250000 * (sec - from), frames_sent: 25 * (sec - from) }, moves === undefined ? {} : { target_moves: moves }));
  moved.push(
    { t: at(700), m: 'session carrier recorded (FR-102 P9c)', f: { session_id: sf, carrier: 'turn', relay_transport: 'udp', local_typ: 'Relay', remote_typ: 'Srflx' } },
    ffSet(700.5, sf, { target_bps: 2000000, why: 'open', deferred: false }),
    ffHb(701, sf, 700, 2000000, 'open:1'),
    ffSet(702.5, sf, { target_bps: 1500000, why: FOLD, deferred: true }),
    ffHb(703, sf, 700, 1500000, `${FOLD}:1`),
    ffLand(704.2, sf, 'applied at quiet', { target_bps: 1500000, why: FOLD }),
    // The gate's skip handed a ×0.85 out: no line, the heartbeat counts it.
    ffHb(705, sf, 700, 1275000, 'decrease(full):1'),
    ffSet(706.5, sf, { target_bps: 1400000, why: 'none', deferred: true }),
    ffHb(707, sf, 700, 1400000, 'none:1'),
    ffLand(708.1, sf, 'scheduled (background rebuild)', { target_bps: 1400000, why: 'none' }),
    // The rebuilt encoder told its target again: counted, no line, no change.
    ffHb(709, sf, 700, 1400000, 'reapply:1'),
    ffSet(710.5, sf, { target_bps: 1100000, why: MORE, deferred: false }),
    ffHb(711, sf, 700, 1100000, `${MORE}:1,more:2`),
    ffHb(713, sf, 700, 1100000, 'none'),
    { t: at(800), m: 'session carrier recorded (FR-102 P9c)', f: { session_id: so, carrier: 'direct', local_typ: 'Host', remote_typ: 'Host' } },
    ffSet(800.5, so, { target_bps: 2500000, deferred: false }),
    ffHb(801, so, 800, 2500000),
    ffHb(803, so, 800, 2000000),
    ffLand(804.5, so, 'applied at quiet', { target_bps: 2000000 }),
    ffHb(805, so, 800, 2000000),
  );
  const wr = fold(moved.slice().reverse(), { to: at(900) });
  const wo = render(wr);
  const wl = wo.split('\n');
  const [xv, xf, xo] = wr.sessions;
  // The target rows of session n, as printed.
  const targetRows = (n) => {
    const a = wl.findIndex((l) => l.startsWith(`## session ${n}: `));
    const b = wl.findIndex((l, i) => i > a && /^## /.test(l));
    return wl.slice(a, b < 0 ? wl.length : b).filter((l) => /^ {2}\S+ {2}target {3}/.test(l));
  };
  check('target: three sessions, in order', wr.sessions.length === 3 && xv.sid === sv && xf.sid === sf && xo.sid === so, wr.sessions.map((x) => x.sid).join(','));
  const missing = wantV.filter((l) => !wl.includes(l));
  check('target: every why there is, on its own line, printed as the line carries it', missing.length === 0, missing.join(' | '));
  check('target: the watermark line takes its new target from new_target', wl.includes(wantV[WHY_WORDS.indexOf('decrease(overflow)')]), wantV[WHY_WORDS.indexOf('decrease(overflow)')]);
  check('target: a row per move, nothing more, where every move has its line', targetRows(1).length === 20 && xv.summary.targetLines.unlined === 0, String(targetRows(1).length));
  check('target: an old agent prints its three rows and no more', targetRows(3).length === 3, targetRows(3).join(' | '));
  check(
    'target: the session counts the moves by why, on the lines and in the heartbeats',
    wl.includes(`| target moves on the target lines, by why | moves 20: ${WHY_WORDS.map((w) => `${w} 1`).join(', ')} |`) &&
      wl.includes(`| the heartbeats' target_moves (every move since the heartbeat before), by why (heartbeats carrying it) | moves 20: ${WHY_WORDS.map((w) => `${w} 1`).join(', ')} (4 of 4) |`),
  );
  const wantF = [
    `${row(700.5)}2000000 bps why=open deferred=false`,
    `${row(702.5)}2000000 -> 1500000 bps why=${FOLD} deferred=true`,
    `${row(704.2)}held 1500000 bps applied at quiet why=${FOLD}`,
    `${row(705)}1500000 -> 1275000 bps, no target line (target_moves=decrease(full):1)`,
    `${row(706.5)}1275000 -> 1400000 bps why=none deferred=true`,
    `${row(708.1)}held 1400000 bps scheduled (background rebuild) why=none`,
    `${row(710.5)}1400000 -> 1100000 bps why=${MORE} deferred=false`,
  ];
  const gotF = targetRows(2);
  check("target: the FFmpeg session's moves in order: a folded why, a held target landing both ways, a move with no line, none, +more", gotF.join('\n') === wantF.join('\n'), gotF.join(' | '));
  const tlf = xf.summary.targetLines;
  const tmf = xf.summary.targetMoves;
  check(
    'target: lines handing a move out 4, landings 2, a change with no line 1; the heartbeats counted 8 in 7, more than the lines',
    tlf.moves === 4 && tlf.landed === 2 && tlf.unlined === 1 && tmf.total === 8 && tmf.carried === 7 && tmf.byWhy.more === 2 && tlf.moves <= tmf.total,
    JSON.stringify([tlf, tmf]),
  );
  check(
    "target: the FFmpeg session's rows",
    wl.includes(`| target moves on the target lines, by why | moves 4: open 1, ${FOLD} 1, none 1, ${MORE} 1 |`) &&
      wl.includes(`| the heartbeats' target_moves (every move since the heartbeat before), by why (heartbeats carrying it) | moves 8: more 2, open 1, ${FOLD} 1, decrease(full) 1, none 1, reapply 1, ${MORE} 1 (7 of 7) |`) &&
      wl.includes('| held targets landing / target changes a heartbeat showed with no target line | 2 / 1 |'),
  );
  const wantO = [`${row(800.5)}2500000 bps (reason: not logged) deferred=false`, `${row(803)}2500000 -> 2000000 bps, no target line (reason: not logged)`, `${row(804.5)}held 2000000 bps applied at quiet (reason: not logged)`];
  check("target: an agent before 0.4.230: its moves print '(reason: not logged)'", wantO.every((l) => wl.includes(l)), wantO.filter((l) => !wl.includes(l)).join(' | '));
  check(
    'target: an agent before 0.4.230: its rows',
    wl.includes('| target moves on the target lines, by why | moves 1: reason not logged 1 |') &&
      wl.includes("| the heartbeats' target_moves (every move since the heartbeat before), by why (heartbeats carrying it) | - (0 of 3) |") &&
      wl.includes('| held targets landing / target changes a heartbeat showed with no target line | 1 / 1 |'),
  );
  check('target: highest target from the lines and the heartbeats', xv.summary.targetMax === 3750000 && xf.summary.targetMax === 2000000 && xo.summary.targetMax === 2500000, [xv, xf, xo].map((x) => x.summary.targetMax).join(','));
  check('target: no undefined or NaN', !/undefined|NaN/.test(wo), (wo.match(/.{0,40}(undefined|NaN).{0,40}/) || [''])[0]);
  check(
    'tally: why:n joined by `,`, `none` for no move, `more:n`, an unreadable part counted once',
    JSON.stringify([tally(`open:1,${FOLD}:1`), tally('none'), tally('none:1'), tally('more:3'), tally(undefined), tally('x')]) ===
      JSON.stringify([[['open', 1], [FOLD, 1]], [], [['none', 1]], [['more', 3]], null, [['unreadable', 1]]]),
  );
  check('scrub: every why and every count as written', WHY_WORDS.concat([FOLD, MORE, 'none', `${MORE}:1,more:2`, `open:1,${FOLD}:1,hard-decrease:12`]).every((s) => scrub(s) === s));
  // FR-148 AC4 (THE MESH RELAY'S T3), end to end: lines as the agent writes
  // them, the server's read evaluated on them (`serverRows`), --rows, the
  // report. Session M on the mesh relay, from an agent with `loss`: a T3 line
  // whose shed line comes 8 s after; one with none in the 25 s after but one
  // inside its span; one whose next shed line comes 43 s after; one with a
  // relay node's reconnect between it and its shed line; one whose only shed
  // line in reach told no drop (since_last 0), with a freeze in its window; a
  // stall without a T3, a fast retransmission, quiet lines; model lines with
  // the floor's push-back, the rule (again later) and D50's fall. Session T
  // on TURN with a T3 (no pairing). Session O on the mesh relay from an agent
  // before 0.4.227: a T3 by its one-packet window alone, no `loss`, no
  // ack_ms, a stall read without SCTP's gauges, a model line without D48's
  // fields. Relay names and addresses are reserved examples (RFC 2606, 5737,
  // 3849), and appear in no output.
  const B = 1000;
  const raw = (sec, msg, fields) => ({ ts: new Date(at(B + sec)), msg, fields });
  const DL = 'path delivery (FR-148 P0, shadow, ≈10s)';
  const ML = 'path model (FR-148 P0, shadow, ≈10s)';
  const SHED = 'overlay derp: shed stale carrier frames (queue age bound)';
  const said = (w) => DERP_STATES.find((x) => x[0] === w)[1];
  const REGIONAL = 'derp-eu.example.net:443';
  const sm = oid('d3a1');
  const sn = oid('d3a2');
  const sp = oid('d3a3');
  const dq = (sec, sid, o) =>
    raw(
      sec,
      DL,
      Object.assign(
        { session_id: sid, span_ms: 10000, delivery_samples: '(5, 0, 0)', backlogged_bps: 'None', app_limited_bps: 'None', stalled_ms: 0, acked_bytes: 1250000, ack_ms: 'Some((12, 14, 40))', standing_ms: 'Some((2, 28))', queued: 0, inflight: '(4096, 61440)', cwnd: '(60000, 120000)', ssthresh: 98304, rwnd_low: 1040000, pending_high: 2400, loss: '(0, 0)' },
        o,
      ),
    );
  const mq = (sec, o) => raw(sec, ML, Object.assign({ session_id: sm, viewer_rx_bps: '2100000/backlogged/150ms', model_capacity_bps: 'None', model_floor_bps: '1000000/pushed-back/900ms', model_floor_push_back: 'p1', model_rule: '-', model_fell: '-' }, o));
  const agentLines = [
    raw(-30, said('connected'), { relay: 'central' }),
    raw(0, 'session carrier recorded (FR-102 P9c)', { session_id: sm, carrier: MESH_RELAY, relay_transport: 'none', local_typ: 'Host', remote_typ: 'Host' }),
    raw(1, 'per-session ICE path detected (adaptive bitrate)', { session_id: sm, relay: false, local_typ: 'Host', local_proto: 'Udp', remote_typ: 'Host', remote_proto: 'Udp', waited_ms: 900, opening: 'moved' }),
    dq(12, sm, { stalled_ms: 2100, delivery_samples: '(3, 0, 2)', acked_bytes: 51200, queued: 3, standing_ms: 'Some((40, 2900))', cwnd: '(1228, 98304)', pending_high: 180000, loss: '(0, 2)' }),
    mq(12.5, {}),
    raw(20, SHED, { relay: 'central', dropped_stale: 41, since_last: 41 }),
    dq(22, sm, { cwnd: '(1228, 64000)', loss: '(0, 1)' }),
    mq(22.5, { model_rule: 'stall/400ms' }),
    dq(32, sm, { stalled_ms: 3000, delivery_samples: '(3, 0, 2)', cwnd: '(4912, 30000)' }),
    mq(32.5, { model_rule: 'stall/10400ms' }),
    dq(42, sm, { cwnd: '(4912, 40000)', loss: '(2, 0)' }),
    mq(42.5, { model_rule: 'stall/1200ms' }),
    dq(52, sm, { stalled_ms: 4100, delivery_samples: '(1, 0, 4)', acked_bytes: 0, cwnd: '(1228, 9000)', loss: '(0, 3)' }),
    mq(52.5, { model_rule: 'stall/11200ms', model_fell: '2131000->1162000/61300ms' }),
    dq(62, sm, {}),
    raw(95, SHED, { relay: 'central', dropped_stale: 46, since_last: 5 }),
    dq(102, sm, { stalled_ms: 2500, delivery_samples: '(3, 0, 2)', cwnd: '(1228, 20000)', loss: '(1, 1)' }),
    raw(110, said('closed'), { relay: REGIONAL }),
    raw(110.5, said('connect-failed'), { relay: '203.0.113.7:443', e: 'IO error: connection refused (os error 111) at 198.51.100.9:443 for wss://derp-eu.example.net/derp' }),
    raw(111, said('lb-no-pod'), { relay: '[2001:db8::7]:443', wait_ms: 2000, left: 3 }),
    raw(113, said('connected'), { relay: REGIONAL }),
    raw(120, SHED, { relay: 'central', dropped_stale: 55, since_last: 9 }),
    dq(142, sm, { cwnd: '(1228, 30000)', loss: '(0, 1)' }),
    raw(145, SHED, { relay: 'central', dropped_stale: 55, since_last: 0 }),
    raw(150, `${said('frozen')}29 s of it (frozen or suspended): the silence is its own, not the relay's; reconnecting`, { relay: 'central', silent_s: 31, paused_s: 29 }),
    raw(150, said('closed'), { relay: 'central' }),
    raw(151, said('connected'), { relay: 'central' }),
    dq(172, sm, {}),
    raw(176, SHED, { relay: 'central', dropped_stale: 58, since_last: 3 }),
    raw(188, SHED, { relay: REGIONAL, dropped_stale: 2, since_last: 2 }),
    raw(300, 'session carrier recorded (FR-102 P9c)', { session_id: sn, carrier: 'turn', relay_transport: 'tls', local_typ: 'Relay', remote_typ: 'Srflx' }),
    dq(310, sn, { stalled_ms: 1800, delivery_samples: '(4, 0, 1)', cwnd: '(1228, 40000)', loss: '(0, 1)' }),
    raw(400, 'session carrier recorded (FR-102 P9c)', { session_id: sp, carrier: MESH_RELAY, local_typ: 'Host', remote_typ: 'Host' }),
    raw(410, DL, { session_id: sp, span_ms: 10000, delivery_samples: '(3, 0, 2)', backlogged_bps: 'None', app_limited_bps: 'None', stalled_ms: 2000, standing_ms: 'Some((40, 1900))', queued: 1, inflight: '(52000, 52000)', cwnd: '(1228, 50000)', rwnd_low: 1040000 }),
    raw(410.5, ML, { session_id: sp, viewer_rx_bps: '900000/backlogged/150ms', model_capacity_bps: 'None', model_floor_bps: 'None' }),
    raw(420, DL, { session_id: sp, span_ms: 10000, delivery_samples: '(2, 0, 3)', backlogged_bps: 'None', app_limited_bps: 'None', stalled_ms: 2600 }),
  ];
  const acSet = { tenant: FX.tenant, agent: FX.agent, from: new Date(at(B - 60)), to: new Date(at(B + 600)), limit: 1000 };
  const acPipe = timelinePipeline(acSet);
  const served = serverRows(acPipe, agentLines);
  const acIds = masks();
  const acPrinted = served.map((x) => rowOut(x, acIds));
  const acText = rowsHead(acSet, {})
    .concat(
      acPrinted.map((x) => JSON.stringify(x)),
      [rowsTail(acPrinted.length, acSet.limit)],
    )
    .join('\n');
  const acr = fold(acPrinted, readOpts(acSet, false, {}));
  const ac = render(acr);
  const acl = ac.split('\n');
  const at3 = (sec, what) => `  ${clock(epochMs(at(B + sec)))}  ${what.padEnd(8)} `;
  check('AC4: every line read, three sessions, in order', served.length === agentLines.length && acr.sessions.map((x) => x.sid).join(',') === [sm, sn, sp].map(mask).join(','), acr.sessions.map((x) => x.sid).join(','));
  const want3 = [
    `${at3(12, 'loss')}span_ms=10000 stalled_ms=2100 acked_bytes=51200 loss=(0, 2) cwnd=(1228, 98304) pending_high=180000; T3; agent: after +8.0 s: stale 41`,
    `${at3(22, 'loss')}span_ms=10000 stalled_ms=0 acked_bytes=1250000 loss=(0, 1) cwnd=(1228, 64000) pending_high=2400; T3; agent: before -2.0 s (inside the span): stale 41`,
    `${at3(42, 'loss')}span_ms=10000 stalled_ms=0 acked_bytes=1250000 loss=(2, 0) cwnd=(4912, 40000) pending_high=2400`,
    `${at3(52, 'loss')}span_ms=10000 stalled_ms=4100 acked_bytes=0 loss=(0, 3) cwnd=(1228, 9000) pending_high=2400; T3; agent: none moved (the span's start to +25.0 s)`,
    `${at3(102, 'loss')}span_ms=10000 stalled_ms=2500 acked_bytes=1250000 loss=(1, 1) cwnd=(1228, 20000) pending_high=2400; T3; agent: after +18.0 s: stale 9, a /derp lifecycle line between`,
    `${at3(142, 'loss')}span_ms=10000 stalled_ms=0 acked_bytes=1250000 loss=(0, 1) cwnd=(1228, 30000) pending_high=2400; T3; agent: none moved (the span's start to +25.0 s), a /derp lifecycle line between`,
    `${at3(310, 'loss')}span_ms=10000 stalled_ms=1800 acked_bytes=1250000 loss=(0, 1) cwnd=(1228, 40000) pending_high=2400; T3`,
    `${at3(410, 'loss')}span_ms=10000 stalled_ms=2000 acked_bytes=- loss=- cwnd=(1228, 50000) pending_high=-; T3; agent: none moved (the span's start to +25.0 s)`,
  ];
  const lossRows = acl.filter((l) => / {2}loss {5}/.test(l));
  check(
    'AC4: the loss rows: a T3 by its count or its window, the 25 s rule (after, before inside the span, none, a lifecycle line between, since_last 0 not a drop), a fast retransmission untagged, TURN unpaired, an older agent dashed',
    lossRows.join('\n') === want3.join('\n'),
    firstDiffOf(want3.join('\n'), lossRows.join('\n')),
  );
  check(
    'AC4: a stall without a T3, and a quiet line, print no loss row',
    !acl.some((l) => l.startsWith(at3(32, 'loss')) || l.startsWith(at3(62, 'loss')) || l.startsWith(at3(172, 'loss'))) && acl.includes(`${at3(32, 'stall')}span_ms=10000 delivery_samples=(3, 0, 2) backlogged_bps=- app_limited_bps=- stalled_ms=3000`),
  );
  check(
    'AC4: the queue row prints ack_ms, and - on an agent that writes none',
    acl.includes(`${at3(12, 'queue')}queued=3 standing_ms=(40, 2900) inflight=(4096, 61440) cwnd=(1228, 98304) rwnd_low=1040000 ack_ms=(12, 14, 40)`) &&
      acl.includes(`${at3(410, 'queue')}queued=1 standing_ms=(40, 1900) inflight=(52000, 52000) cwnd=(1228, 50000) rwnd_low=1040000 ack_ms=-`),
  );
  const wantShed = [
    `${at3(20, 'shed')}dropped_stale=41 since_last=41`,
    `${at3(95, 'shed')}dropped_stale=46 since_last=5`,
    `${at3(120, 'shed')}dropped_stale=55 since_last=9`,
    `${at3(145, 'shed')}dropped_stale=55 since_last=0`,
    `${at3(176, 'shed')}dropped_stale=58 since_last=3`,
    `${at3(188, 'shed')}dropped_stale=2 since_last=2`,
  ];
  const wantDerp = [
    `${at3(110, 'derp')}closed`,
    `${at3(110.5, 'derp')}connect-failed`,
    `${at3(111, 'derp')}lb-no-pod wait_ms=2000 left=3`,
    `${at3(113, 'derp')}connected`,
    `${at3(150, 'derp')}frozen silent_s=31 paused_s=29`,
    `${at3(150, 'derp')}closed`,
    `${at3(151, 'derp')}connected`,
  ];
  check('AC4: each shed line a row, its time, dropped_stale and since_last', acl.filter((l) => / {2}shed {5}/.test(l)).join('\n') === wantShed.join('\n'), acl.filter((l) => / {2}shed {5}/.test(l)).join(' | '));
  check('AC4: each lifecycle line a row, its word and its counts', acl.filter((l) => / {2}derp {5}/.test(l)).join('\n') === wantDerp.join('\n'), acl.filter((l) => / {2}derp {5}/.test(l)).join(' | '));
  const MESH = '| mesh relay T3 (FR-148 AC4): delivery lines with a T3 / T3 expiries counted (delivery lines carrying loss) | ';
  const STALLED = '| mesh relay T3: stalled delivery lines with a T3 / without one / carrying neither loss nor cwnd | ';
  const PAIRS = "| mesh relay T3: T3 lines by each node's nearest drop line with a count moved: within 25 s after / only before, inside the line's span / none moved (of each, with a /derp lifecycle line between) | ";
  const CAUSES = '| mesh relay T3: the causes that moved in that drop line, by node (T3 lines) | ';
  const QUIET = '| mesh relay T3: T3 lines across which no count moved on either node (a hold, or a loss below the overlay) | ';
  const sessionRows = (n) => {
    const a = acl.findIndex((l) => l.startsWith(`## session ${n}: `));
    const b = acl.findIndex((l, i) => i > a && /^## /.test(l));
    return acl.slice(a, b);
  };
  check(
    'AC4: the session rows on the mesh relay, from an agent with loss',
    [`${MESH}5 / 8 (9 of 9) |`, `${STALLED}3 / 1 / 0 |`, `${PAIRS}agent 2 (1) / 1 (0) / 2 (1) |`, `${CAUSES}agent stale 3 |`].every((l) => sessionRows(1).includes(l)) && !sessionRows(1).some((l) => l.startsWith(QUIET)),
    sessionRows(1).filter((l) => l.startsWith('| mesh')).join(' | '),
  );
  check('AC4: a TURN session prints no mesh relay rows', !sessionRows(2).some((l) => l.startsWith('| mesh')));
  check(
    'AC4: the session rows of an agent before 0.4.227: expiries not counted (-), a stall read without gauges apart',
    [`${MESH}1 / - (0 of 2) |`, `${STALLED}1 / 0 / 1 |`, `${PAIRS}agent 0 (0) / 0 (0) / 1 (0) |`, `${CAUSES}agent - |`].every((l) => sessionRows(3).includes(l)),
    sessionRows(3).filter((l) => l.startsWith('| mesh')).join(' | '),
  );
  check(
    'AC4: the device rows: the mesh relay sessions summed, every /derp line counted, the one outside the sessions too',
    [
      `| mesh relay (carrier ${MESH_RELAY}, FR-148 AC4): sessions / delivery lines / stalled / with a T3 / T3 expiries counted (lines carrying loss) | 2 / 11 / 6 / 6 / 8 (9 of 11) |`,
      '| mesh relay: stalled delivery lines with a T3 / without one / carrying neither loss nor cwnd; heartbeat windows transit-stalled | 4 / 1 / 1; 0 of 0 (-) |',
      "| mesh relay: T3 lines by each node's nearest drop line with a count moved: within 25 s after / only before, inside the line's span / none moved (of each, with a /derp lifecycle line between) | agent 2 (1) / 1 (0) / 3 (1) |",
      '| mesh relay: the causes that moved in that drop line, by node (T3 lines) | agent stale 3 |',
      "| /derp lines, the agent's, every relay node: drop lines (moves summed by cause) / lifecycle lines by state | 6 (stale 60) / connected 3, closed 2, connect-failed 1, lb-no-pod 1, frozen 1 |",
      '| lines in no session | derp 1 |',
    ].every((l) => acl.includes(l)),
    acl.filter((l) => /^\| (mesh relay[:( ]|\/derp|lines in no)/.test(l)).join(' | '),
  );
  const wantModel = [
    `${at3(12.5, 'model')}viewer_rx_bps=2100000/backlogged/150ms model_capacity_bps=- model_floor_bps=1000000/pushed-back/900ms model_floor_push_back=p1 model_rule=- model_fell=-`,
    `${at3(22.5, 'model')}viewer_rx_bps=2100000/backlogged/150ms model_capacity_bps=- model_floor_bps=1000000/pushed-back/900ms model_floor_push_back=p1 model_rule=stall/400ms model_fell=-`,
    `${at3(42.5, 'model')}viewer_rx_bps=2100000/backlogged/150ms model_capacity_bps=- model_floor_bps=1000000/pushed-back/900ms model_floor_push_back=p1 model_rule=stall/1200ms model_fell=-`,
    `${at3(52.5, 'model')}viewer_rx_bps=2100000/backlogged/150ms model_capacity_bps=- model_floor_bps=1000000/pushed-back/900ms model_floor_push_back=p1 model_rule=stall/11200ms model_fell=2131000->1162000/61300ms`,
    `${at3(410.5, 'model')}viewer_rx_bps=900000/backlogged/150ms model_capacity_bps=- model_floor_bps=- model_floor_push_back=- model_rule=- model_fell=-`,
  ];
  check(
    "model: the push-back, the rule and the fall print with the fields it printed; a row when one changes or starts anew, none for the same rule older; - where a line carries none",
    acl.filter((l) => / {2}model {4}/.test(l)).join('\n') === wantModel.join('\n'),
    firstDiffOf(wantModel.join('\n'), acl.filter((l) => / {2}model {4}/.test(l)).join('\n')),
  );
  check('aged: a rule, a fall, the agent\'s -, none', JSON.stringify([aged('stall/3400ms'), aged('2131000->1162000/61300ms'), aged('-'), aged(undefined)]) === JSON.stringify([{ what: 'stall', ageMs: 3400 }, { what: '2131000->1162000', ageMs: 61300 }, { what: '-', ageMs: null }, null]));
  check('AC4: no undefined or NaN', !/undefined|NaN/.test(ac), (ac.match(/.{0,40}(undefined|NaN).{0,40}/) || [''])[0]);
  check('AC4: the --rows text, read back offline, renders byte for byte as the direct read', renderRows([acText], {}) === ac, firstDiffOf(ac, renderRows([acText], {})));
  // FR-148 AC4: no relay URL, host or address leaves the database. Three
  // layers, each shown to hold alone: the projection names no KEPT field
  // (and refuses to: above); the /derp kinds' fields leave as numbers only;
  // --rows keeps a line to its kind's fields. The search for the relay's
  // values runs on the rows as the server returns them (before any scrub),
  // on --rows and on the report, and is shown to find a leak where one is.
  const RELAY_SAID = ['central', 'example.net', '203.0.113', '198.51.100', '2001:db8', 'wss:', 'refused'];
  const leaks = (s) => RELAY_SAID.filter((x) => String(s).includes(x));
  const derpServed = served.filter((x) => DERP_KINDS.includes(kindOf(x.m)));
  check('address: the read returns the /derp lines with their counts and no relay, no e', derpServed.length === 14 && derpServed.every((x) => !('relay' in x.f) && !('e' in x.f)) && served.find((x) => x.m === SHED).f.dropped_stale === 41, JSON.stringify(derpServed[0]));
  check('address: the ICE line still leaves its relay, a yes or no', served.find((x) => kindOf(x.m) === 'ice').f.relay === false);
  check('address: no relay value or error text in the rows the server returns, in --rows or in the report', leaks(JSON.stringify(served.map((x) => [x.m, x.f]))).length === 0 && leaks(acText).length === 0 && leaks(ac).length === 0, [leaks(JSON.stringify(served.map((x) => [x.m, x.f]))), leaks(acText), leaks(ac)].join(' / '));
  check('address: and no address reached the scrub', !/<ip>/.test(acText) && !/<ip>/.test(ac));
  // The search finds a leak where there is one: a read whose shed branch lets
  // `relay` out as stored, built by hand (projectionOf refuses to).
  const leaky = timelinePipeline(acSet);
  leaky[4].$project.f.$switch.branches[KINDS.findIndex((d) => d.k === 'shed')].then.relay = '$lines.fields.relay';
  check('address: the search finds the relay in a read that lets it out (the control)', leaks(JSON.stringify(serverRows(leaky, agentLines).map((x) => x.f))).join(',') === 'central,example.net');
  // The numbers-only layer alone: the same field, as the /derp kinds project, stays in.
  leaky[4].$project.f.$switch.branches[KINDS.findIndex((d) => d.k === 'shed')].then.relay = { $cond: [{ $isNumber: '$lines.fields.relay' }, '$lines.fields.relay', '$$REMOVE'] };
  leaky[4].$project.f.$switch.branches[KINDS.findIndex((d) => d.k === 'derp')].then.e = { $cond: [{ $isNumber: '$lines.fields.e' }, '$lines.fields.e', '$$REMOVE'] };
  check('address: a relay or an e named as the /derp kinds name their fields still stays in (numbers only)', leaks(JSON.stringify(serverRows(leaky, agentLines).map((x) => x.f))).length === 0);
  // The --rows layer alone: a row that came back with them anyway.
  const back = [
    rowOut({ t: new Date(at(B)), m: SHED, f: { relay: REGIONAL, dropped_stale: 2, since_last: 'derp-eu.example.net' } }),
    rowOut({ t: new Date(at(B)), m: said('connect-failed'), f: { relay: '203.0.113.7:443', e: 'refused at 198.51.100.9:443', wait_ms: 5 } }),
  ];
  check('address: --rows keeps a /derp line to its counts, numbers only, whatever the read returned', JSON.stringify(back.map((x) => x.f)) === JSON.stringify([{ dropped_stale: 2 }, { wait_ms: 5 }]), JSON.stringify(back.map((x) => x.f)));
  let unknownShape = null;
  try {
    const odd = timelinePipeline(acSet);
    odd[4].$project.f.$switch.branches[0].then.extra = { $toString: '$lines.fields.relay' };
    serverRows(odd, agentLines);
  } catch (e) {
    unknownShape = e.message;
  }
  check('address: the evaluation refuses a shape it does not know, so it cannot pass one', unknownShape !== null && /does not evaluate/.test(unknownShape), String(unknownShape));
  // FR-148 D52, end to end: session Q on the mesh relay from a D52 agent,
  // whose drop lines count every cause, and its viewer's node read with
  // --viewer-agent: an older agent's shed lines first, then, after a
  // reconnect, D52's line; two lines of the viewer node's own that its read
  // never takes; one outside the session. Four T3 lines: each node's drop line
  // after one, before one inside its span, or none; and one across which no
  // count moved on either node.
  const sd5 = oid('d3a4');
  const VIEW = 'f1480000000000000000b019';
  const dropRaw = (sec, relay, c) => raw(sec, DROP_LINE, Object.assign({ relay }, ...DROP_CAUSES.map(([w, count, since]) => ({ [count]: c[w][0], [since]: c[w][1] }))));
  const d5Agent = [
    raw(1000, 'session carrier recorded (FR-102 P9c)', { session_id: sd5, carrier: MESH_RELAY, relay_transport: 'none', local_typ: 'Host', remote_typ: 'Host' }),
    dq(1012, sd5, { cwnd: '(1228, 64000)', loss: '(0, 1)' }),
    dropRaw(1020, 'central', { stale: [10, 0], full: [7, 7], backpressure: [0, 0], unrouted: [0, 0] }),
    dq(1052, sd5, { cwnd: '(1228, 9000)', loss: '(0, 2)' }),
    dq(1072, sd5, {}),
    dropRaw(1090, REGIONAL, { stale: [10, 0], full: [9, 2], backpressure: [0, 0], unrouted: [0, 0] }),
    dq(1102, sd5, { cwnd: '(1228, 20000)', loss: '(0, 1)' }),
    dropRaw(1140, 'central', { stale: [14, 4], full: [9, 0], backpressure: [3, 3], unrouted: [0, 0] }),
    dq(1142, sd5, { cwnd: '(1228, 30000)', loss: '(0, 1)' }),
  ];
  const d5Viewer = [
    raw(1015, SHED, { relay: 'central', dropped_stale: 3, since_last: 3 }),
    raw(1030, DL, { session_id: oid('d3b1'), span_ms: 10000, stalled_ms: 0, acked_bytes: 9 }),
    raw(1031, 'FFmpeg DC pump heartbeat (≈2s window)', { session_id: oid('d3b1'), target_bps: 2000000 }),
    raw(1050, SHED, { relay: 'central', dropped_stale: 8, since_last: 5 }),
    raw(1105, said('closed'), { relay: 'central' }),
    raw(1106, said('connected'), { relay: 'central' }),
    dropRaw(1160, 'central', { stale: [0, 0], full: [2, 2], backpressure: [1, 1], unrouted: [4, 4] }),
    raw(1250, said('connect-failed'), { relay: '203.0.113.7:443', e: 'IO error: connection refused at 198.51.100.9:443 for wss://derp-eu.example.net/derp' }),
  ];
  const d5Set = { tenant: FX.tenant, agent: FX.agent, from: new Date(at(B + 900)), to: new Date(at(B + 1500)), limit: 1000, viewer: { tenant: FX.tenant, agent: VIEW } };
  const d5Pipe = timelinePipeline(d5Set);
  const vPipe = timelinePipeline(Object.assign({}, d5Set, { agent: VIEW, kinds: DERP_KINDS }));
  const d5Served = serverRows(d5Pipe, d5Agent);
  const vServed = serverRows(vPipe, d5Viewer);
  const d5Ids = masks();
  const d5Mine = d5Served.map((x) => rowOut(x, d5Ids));
  const d5Theirs = vServed.map((x) => rowOut(x, d5Ids, VIEWER));
  const d5Text = rowsHead(d5Set, {})
    .concat(
      d5Mine.map((x) => JSON.stringify(x)),
      [rowsTail(d5Mine.length, d5Set.limit)],
      d5Theirs.map((x) => JSON.stringify(x)),
      [rowsTail(d5Theirs.length, d5Set.limit, VIEWER)],
    )
    .join('\n');
  const d5 = render(fold(d5Mine.concat(d5Theirs), readOpts(d5Set, false, {})));
  const d5l = d5.split('\n');
  const derpPattern = KINDS.filter((d) => DERP_KINDS.includes(d.k))
    .map((d) => d.re.source)
    .join('|');
  check(
    "D52: the viewer node's read takes its /derp lines only: its filter and its $switch know no other kind, the rest of the read is the agent's",
    vServed.length === 6 &&
      vServed.every((x) => DERP_KINDS.includes(kindOf(x.m))) &&
      vPipe[2].$project.lines.$filter.cond.$and[0].$regexMatch.regex === derpPattern &&
      JSON.stringify(vPipe[4].$project.f.$switch.branches.map((b) => b.case.$regexMatch.regex)) === JSON.stringify(KINDS.filter((d) => DERP_KINDS.includes(d.k)).map((d) => d.re.source)) &&
      vPipe.map((x) => Object.keys(x)[0]).join(',') === d5Pipe.map((x) => Object.keys(x)[0]).join(',') &&
      vPipe[0].$match.agent_id === VIEW &&
      vPipe[0].$match.tenant_id === FX.tenant &&
      vPipe[5].$limit === d5Set.limit + 1,
    vServed.map((x) => x.m).join(' | '),
  );
  const d5Derp = d5Served.filter((x) => DERP_KINDS.includes(kindOf(x.m))).concat(vServed);
  check(
    "D52: the server's rows and the client's re-projection (--rows) agree on every /derp line of both nodes, numbers only, no relay and no e",
    d5Derp.length === 9 &&
      d5Derp.every((x) => JSON.stringify(rowOut(x, masks(), null).f) === JSON.stringify(x.f) && Object.keys(x.f).every((k) => typeof x.f[k] === 'number')) &&
      d5Served.find((x) => x.m === DROP_LINE).f.full_since_last === 7,
    JSON.stringify(d5Derp.map((x) => x.f)),
  );
  const want5 = [
    `${at3(1012, 'loss')}span_ms=10000 stalled_ms=0 acked_bytes=1250000 loss=(0, 1) cwnd=(1228, 64000) pending_high=2400; T3; agent: after +8.0 s: full 7; viewer: after +3.0 s: stale 3`,
    `${at3(1052, 'loss')}span_ms=10000 stalled_ms=0 acked_bytes=1250000 loss=(0, 2) cwnd=(1228, 9000) pending_high=2400; T3; agent: none moved (the span's start to +25.0 s); viewer: before -2.0 s (inside the span): stale 5`,
    `${at3(1102, 'loss')}span_ms=10000 stalled_ms=0 acked_bytes=1250000 loss=(0, 1) cwnd=(1228, 20000) pending_high=2400; T3; agent: none moved (the span's start to +25.0 s); viewer: none moved (the span's start to +25.0 s), a /derp lifecycle line between; no count moved on either node: a hold, or a loss below the overlay`,
    `${at3(1142, 'loss')}span_ms=10000 stalled_ms=0 acked_bytes=1250000 loss=(0, 1) cwnd=(1228, 30000) pending_high=2400; T3; agent: before -2.0 s (inside the span): stale 4, backpressure 3; viewer: after +18.0 s: full 2, backpressure 1, unrouted 4`,
  ];
  const loss5 = d5l.filter((l) => / {2}loss {5}/.test(l));
  check(
    'D52: each T3 line by both nodes: after, before inside the span, none moved, the causes that moved, a lifecycle line between, and none moved on either node said so',
    loss5.join('\n') === want5.join('\n'),
    firstDiffOf(want5.join('\n'), loss5.join('\n')),
  );
  const whatOf = (l) => (/^ {2}\S+ {2}(\S+)/.exec(l) || [])[1];
  const wantRows5 = [
    `${at3(1015, 'v.shed')}dropped_stale=3 since_last=3`,
    `${at3(1020, 'drops')}dropped_stale=10 since_last=0 dropped_full=7 full_since_last=7 dropped_backpressure=0 backpressure_since_last=0 dropped_unrouted=0 unrouted_since_last=0`,
    `${at3(1050, 'v.shed')}dropped_stale=8 since_last=5`,
    `${at3(1090, 'drops')}dropped_stale=10 since_last=0 dropped_full=9 full_since_last=2 dropped_backpressure=0 backpressure_since_last=0 dropped_unrouted=0 unrouted_since_last=0`,
    `${at3(1105, 'v.derp')}closed`,
    `${at3(1106, 'v.derp')}connected`,
    `${at3(1140, 'drops')}dropped_stale=14 since_last=4 dropped_full=9 full_since_last=0 dropped_backpressure=3 backpressure_since_last=3 dropped_unrouted=0 unrouted_since_last=0`,
    `${at3(1160, 'v.drops')}dropped_stale=0 since_last=0 dropped_full=2 full_since_last=2 dropped_backpressure=1 backpressure_since_last=1 dropped_unrouted=4 unrouted_since_last=4`,
  ];
  const rows5 = d5l.filter((l) => ['drops', 'shed', 'derp', 'v.drops', 'v.shed', 'v.derp'].includes(whatOf(l)));
  check("D52: each node's /derp lines a row in time order, both forms, the viewer node's as v.…", rows5.join('\n') === wantRows5.join('\n'), firstDiffOf(wantRows5.join('\n'), rows5.join('\n')));
  check(
    'D52: the session and device rows, by node: the pairings, the causes that moved, the T3 lines none moved across, each node\'s /derp lines',
    [
      `${MESH}4 / 5 (5 of 5) |`,
      `${PAIRS}agent 1 (0) / 1 (0) / 2 (0); viewer 2 (0) / 1 (0) / 1 (1) |`,
      `${CAUSES}agent stale 1, full 1, backpressure 1; viewer stale 2, full 1, backpressure 1, unrouted 1 |`,
      `${QUIET}1 |`,
      "| mesh relay: T3 lines by each node's nearest drop line with a count moved: within 25 s after / only before, inside the line's span / none moved (of each, with a /derp lifecycle line between) | agent 1 (0) / 1 (0) / 2 (0); viewer 2 (0) / 1 (0) / 1 (1) |",
      '| mesh relay: the causes that moved in that drop line, by node (T3 lines) | agent stale 1, full 1, backpressure 1; viewer stale 2, full 1, backpressure 1, unrouted 1 |',
      '| mesh relay: T3 lines across which no count moved on either node (a hold, or a loss below the overlay) | 1 |',
      "| /derp lines, the agent's, every relay node: drop lines (moves summed by cause) / lifecycle lines by state | 3 (stale 4, full 9, backpressure 3, unrouted 0) / - |",
      "| /derp lines, the viewer node's, every relay node: drop lines (moves summed by cause) / lifecycle lines by state | 3 (stale 8, full 2, backpressure 1, unrouted 4) / closed 1, connected 1, connect-failed 1 |",
      '| lines in no session | v.derp 1 |',
      'viewer node: agent …b019, its /derp lines over the same window (--viewer-agent)',
    ].every((l) => d5l.includes(l)),
    d5l.filter((l) => /^(\| (mesh relay|\/derp|lines in no)|viewer node)/.test(l)).join(' | '),
  );
  check('D52: the --rows text of both reads, read back offline, renders byte for byte as the direct read', renderRows([d5Text], {}) === d5, firstDiffOf(d5, renderRows([d5Text], {})));
  check(
    'D52: no relay value or error text in either node\'s rows, in --rows or in the report; no undefined or NaN',
    leaks(JSON.stringify(d5Served.concat(vServed).map((x) => [x.m, x.f]))).length === 0 && leaks(d5Text).length === 0 && leaks(d5).length === 0 && !/<ip>/.test(d5Text + d5) && !/undefined|NaN/.test(d5),
    [leaks(d5Text), leaks(d5)].join(' / '),
  );
  const back5 = [
    rowOut({ t: new Date(at(B)), m: DROP_LINE, f: { relay: REGIONAL, dropped_full: 3, full_since_last: 'derp-eu.example.net' } }),
    rowOut({ t: new Date(at(B)), m: DL, f: { session_id: FX.s1, stalled_ms: 5 }, fm: { wrote: 'x' } }, null, VIEWER),
  ];
  check(
    "D52: --rows keeps a drop line to its counts, numbers only, and a viewer node's row of another kind to the ids alone",
    JSON.stringify(back5) === JSON.stringify([{ t: at(B), m: DROP_LINE, f: { dropped_full: 3 } }, { t: at(B), m: DL, f: { session_id: '…5e51' }, node: VIEWER }]),
    JSON.stringify(back5),
  );
  const leaky5 = timelinePipeline(d5Set);
  leaky5[4].$project.f.$switch.branches[KINDS.findIndex((d) => d.k === 'drops')].then.relay = '$lines.fields.relay';
  const found5 = leaks(JSON.stringify(serverRows(leaky5, d5Agent).map((x) => x.f))).join(',');
  leaky5[4].$project.f.$switch.branches[KINDS.findIndex((d) => d.k === 'drops')].then.relay = { $cond: [{ $isNumber: '$lines.fields.relay' }, '$lines.fields.relay', '$$REMOVE'] };
  check("D52: the search finds a drop line's relay in a read that lets it out, and the numbers-only layer alone keeps it in", found5 === 'central,example.net' && leaks(JSON.stringify(serverRows(leaky5, d5Agent).map((x) => x.f))).length === 0, found5);
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
  check('no address key in the named fields', named().every((k) => !DENY_KEY.test(k)));
  let threw = 0;
  for (const bad of [{}, { RC_PATH_TENANT: FX.tenant, RC_PATH_AGENTS: FX.agent + ',' + FX.agent, RC_PATH_SINCE: at(0), RC_PATH_UNTIL: at(10) }, { RC_PATH_TENANT: FX.tenant, RC_PATH_AGENTS: FX.agent, RC_PATH_SINCE: at(0), RC_PATH_UNTIL: at(90000) }]) {
    try {
      readSettings(bad);
    } catch (_) {
      threw++;
    }
  }
  check('settings refuse no org, two agents, a window over 24 h', threw === 3);
  // FR-148 D52: the viewer node, ONE more agent, in this org or its own.
  const vBase = { RC_PATH_TENANT: FX.tenant, RC_PATH_AGENTS: FX.agent, RC_PATH_SINCE: at(0), RC_PATH_UNTIL: at(10) };
  const vOf = (o) => {
    try {
      return readSettings(Object.assign({}, vBase, o)).viewer;
    } catch (_) {
      return 'refused';
    }
  };
  const VA = 'f1480000000000000000b019';
  const VT = 'f148000000000000000000b1';
  check(
    'settings: the viewer node in this org or its own; no viewer, none; a malformed id, a tenant alone, or the agent read again refused',
    JSON.stringify([vOf({}), vOf({ RC_PATH_VIEWER: VA }), vOf({ RC_PATH_VIEWER: VA, RC_PATH_VIEWER_TENANT: VT })]) === JSON.stringify([null, { tenant: FX.tenant, agent: VA }, { tenant: VT, agent: VA }]) &&
      [{ RC_PATH_VIEWER: 'f148' }, { RC_PATH_VIEWER_TENANT: VT }, { RC_PATH_VIEWER: VA, RC_PATH_VIEWER_TENANT: 'x' }, { RC_PATH_VIEWER: FX.agent }, { RC_PATH_VIEWER: FX.agent.toUpperCase(), RC_PATH_VIEWER_TENANT: VT }].every((o) => vOf(o) === 'refused'),
  );
  // The second $project's `f`: one branch a kind, in KINDS order, with the
  // kind's message pattern and its fields (`projectionOf`); `{}` for a line
  // no kind matches, which the filter has already left out.
  const sw = p[4].$project.f.$switch;
  const thenOf = (k) => (sw.branches[KINDS.findIndex((d) => d.k === k)] || {}).then || {};
  check(
    'f: one $switch branch a kind, in KINDS order, with its pattern and its fields',
    Object.keys(p[4].$project.f).join(',') === '$switch' &&
      sw.branches.length === KINDS.length &&
      sw.branches.every((b, i) => b.case.$regexMatch.input === '$lines.msg' && b.case.$regexMatch.regex === KINDS[i].re.source && JSON.stringify(Object.keys(b.then)) === JSON.stringify(fieldsOf(KINDS[i].k))) &&
      JSON.stringify(sw.default) === JSON.stringify({ $literal: {} }),
  );
  const asStored = (k, x) => thenOf(k)[x] === '$lines.fields.' + x;
  const asNumber = (k, x) => JSON.stringify(thenOf(k)[x]) === JSON.stringify({ $cond: [{ $isNumber: '$lines.fields.' + x }, '$lines.fields.' + x, '$$REMOVE'] });
  const projecting = (x) =>
    KINDS.filter((d) => x in thenOf(d.k))
      .map((d) => d.k)
      .join(',');
  check(
    "viewers leaves only as a number and from the rank lines only, the /derp lines' fields only as numbers, every other field as stored",
    asNumber('rank', 'viewers') && projecting('viewers') === 'rank' && KINDS.every((d) => fieldsOf(d.k).every((x) => (numberOnly(d.k, x) ? asNumber(d.k, x) : asStored(d.k, x)))),
  );
  check('the read projects the named fields and nothing else', Array.from(new Set([].concat(...sw.branches.map((b) => Object.keys(b.then))))).sort().join(',') === named().sort().join(','));
  check(
    "a line leaves with the ids and its own kind's fields only: no heartbeat field from a delivery line, the ICE line's relay from no other",
    !('target_bps' in thenOf('delivery')) && !('bytes_written' in thenOf('shed')) && projecting('relay') === 'ice' && KINDS.every((d) => IDS.every((x) => x in thenOf(d.k))),
    projecting('relay'),
  );
  check("open: the query's filter keeps the line, and its fields leave as stored", new RegExp(cond[0].$regexMatch.regex).test('path open (FR-148 P0, O10)') && FIELDS.open.every((x) => asStored('open', x)));
  check(
    "queue, loss and relay transport: the delivery line's queue and loss fields and the carrier line's relay_transport leave as stored",
    FIELDS.queue.concat(FIELDS.loss).every((x) => asStored('delivery', x)) && asStored('carrier', 'relay_transport'),
  );
  check("model: the floor's push-back, the rule and the fall leave as stored", ['model_floor_push_back', 'model_rule', 'model_fell'].every((x) => asStored('model', x)));
  // FR-148 AC4: no relay address leaves. Neither /derp kind projects what
  // KEPT names, no kind projects `e`, and the counts leave as numbers only.
  check('KEPT: no /derp kind projects relay or e, and no kind e', Object.keys(KEPT).every((k) => KEPT[k].every((x) => !(x in thenOf(k)))) && projecting('e') === '');
  check(
    'the /derp lines leave their counts, as numbers only',
    FIELDS.shed.concat(FIELDS.drops, FIELDS.derp).length === 15 && DERP_KINDS.every((k) => FIELDS[k].every((x) => asNumber(k, x))) && DERP_KINDS.every((k) => NUMBER_KINDS.includes(k)),
  );
  check(
    'the /derp patterns read the drop line, the shed line and the lifecycle lines, and none of the other "overlay derp:" lines',
    new RegExp(cond[0].$regexMatch.regex).test(SHED_LINE) &&
      kindOf(SHED_LINE) === 'shed' &&
      kindOf(DROP_LINE) === 'drops' &&
      DERP_STATES.every((x) => kindOf(x[1]) === 'derp') &&
      ['overlay derp: regional relay pushed but no admission ticket yet', 'overlay derp: regional /derp carrier opened', 'overlay derp: /derp WS recovered — walking now', 'overlay derp: send buffer capped below the request'].every((m) => kindOf(m) === null),
  );
  // A read that would project what KEPT names is refused before it is built.
  FIELDS.shed.push('relay');
  let refused = null;
  try {
    timelinePipeline({ tenant: FX.tenant, agent: FX.agent, from: new Date(at(0)), to: new Date(at(60)), limit: 7 });
  } catch (e) {
    refused = e.message;
  } finally {
    FIELDS.shed.pop();
  }
  check("KEPT: a read that would project the shed line's relay is refused, not built", refused !== null && /shed\.relay/.test(refused) && !fieldsOf('shed').includes('relay'), String(refused));
  // FR-148 P0: the target lines are read, and their `why`, the heartbeats'
  // `target_moves`, the watermark line's `new_target` and the FFmpeg line's
  // `deferred` leave as stored; the capture-wait line, which writes a `why`
  // of its own, is not read.
  const msgRe = new RegExp(cond[0].$regexMatch.regex);
  check(
    'target: the query keeps the five target lines',
    [
      'VP9-444 set_bitrate (AIMD)',
      'VP9-444 AIMD decrease (DC buffer over high watermark)',
      'FFmpeg DC pump set_bitrate (AIMD)',
      'FFmpeg DC pump: deferred bitrate scheduled (background rebuild)',
      'FFmpeg DC pump: deferred bitrate applied at quiet',
    ].every((m) => msgRe.test(m)),
  );
  check('target: the capture-wait line is not read', !msgRe.test('capture: waiting on the host (a portal dialog, an unlock, a display waking, a user switch); telling the controller'));
  check('target: why, target_moves, new_target and deferred leave as stored', asStored('target', 'why') && asStored('landed', 'why') && asStored('heartbeat', 'target_moves') && asStored('target', 'new_target') && asStored('target', 'deferred'));
  // Adding a field to a line kind is ONE entry in FIELDS: the read projects
  // it and the kind's row prints it, nothing else changed. A made-up field on
  // the ICE path line and on the path model line, taken out again after.
  FIELDS.ice.push('probe_extra');
  FIELDS.model.push('probe_extra');
  try {
    const pb = timelinePipeline({ tenant: FX.tenant, agent: FX.agent, from: new Date(at(0)), to: new Date(at(60)), limit: 7 })[4].$project.f.$switch.branches;
    const pt = (k) => pb[KINDS.findIndex((d) => d.k === k)].then;
    const po = render(
      fold(
        [
          { t: at(1), m: 'per-session ICE path detected (adaptive bitrate)', f: { session_id: FX.s1, relay: false, local_typ: 'Host', probe_extra: 'p1' } },
          { t: at(2), m: 'path model (FR-148 P0, shadow, ≈10s)', f: { session_id: FX.s1, viewer_rx_bps: 'None', model_capacity_bps: 'None', model_floor_bps: 'None', probe_extra: 'p2' } },
        ],
        { to: at(60) },
      ),
    );
    check('one FIELDS entry: the read projects the field, for its kind only', pt('ice').probe_extra === '$lines.fields.probe_extra' && pt('model').probe_extra === '$lines.fields.probe_extra' && !('probe_extra' in pt('delivery')));
    check(
      'one FIELDS entry: the row prints it',
      /^ {2}07:00:01\.000 {2}ice {6}relay=false local_typ=Host probe_extra=p1$/m.test(po) &&
        /^ {2}07:00:02\.000 {2}model {4}viewer_rx_bps=- model_capacity_bps=- model_floor_bps=- model_floor_push_back=- model_rule=- model_fell=- probe_extra=p2$/m.test(po),
      (po.match(/^ {2}07:00:0[12].*$/gm) || []).join(' | '),
    );
  } finally {
    FIELDS.ice.pop();
    FIELDS.model.pop();
  }
  check('one FIELDS entry: taken out again', !named().includes('probe_extra'));
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
    // FR-148 P0: a move held, its count, and the held target landing.
    { t: at(303), m: 'FFmpeg DC pump set_bitrate (AIMD)', f: { session_id: s3, codec_label: 'HEVC', target_bps: 2550000, why: 'decrease(transit-stall)+ceiling(pipe)', deferred: true } },
    hbRow(304, s3, { bytes_written: 500000, frames_sent: 50, target_bps: 2550000, target_moves: 'decrease(transit-stall)+ceiling(pipe):1' }),
    { t: at(305), m: 'FFmpeg DC pump: deferred bitrate applied at quiet', f: { session_id: s3, codec_label: 'HEVC', target_bps: 2550000, why: 'decrease(transit-stall)+ceiling(pipe)' } },
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
  check('the masked --rows, read back offline, renders byte for byte as the direct read', offline === direct && !/[0-9a-fA-F]{24}/.test(full), firstDiffOf(direct, offline));
  check(
    '--rows: why, target_moves and deferred as the lines carry them, and the rows they print',
    /"why":"decrease\(transit-stall\)\+ceiling\(pipe\)","deferred":true/.test(body) &&
      /"target_moves":"decrease\(transit-stall\)\+ceiling\(pipe\):1"/.test(body) &&
      /^ {2}07:05:03\.000 {2}target {3}3000000 -> 2550000 bps why=decrease\(transit-stall\)\+ceiling\(pipe\) deferred=true$/m.test(offline) &&
      /^ {2}07:05:05\.000 {2}target {3}held 2550000 bps applied at quiet why=decrease\(transit-stall\)\+ceiling\(pipe\)$/m.test(offline),
    (body.match(/^.*"why".*$/m) || [''])[0],
  );
  const sids = fold(printed, {}).sessions.map((x) => x.sid);
  check('two ids with one last 4 hex stay two sessions (…5e51, …5e51~2)', sids.join(',') === '…5e51,…5e52,…5e51~2' && /^## session 3: …5e51~2, /m.test(direct) && /^## session 3: …5e51~2, /m.test(offline), sids.join(','));
  const cut = 40;
  const directCut = render(fold(printed.slice(0, cut), readOpts(Object.assign({}, set, { limit: cut }), true, { turn: 2 })));
  check('a TRUNCATED --rows read back offline renders as the direct read, its limit too', renderRows([rowsText(cut)], {}) === directCut && /stopped at 40 rows/.test(directCut), firstDiffOf(directCut, renderRows([rowsText(cut)], {})));
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
    check('the wrapper refuses --from/--since, --to/--until, --tenant, --agent, --limit, --viewer-agent or --viewer-tenant given twice, before the target', ['--from', '--to', '--tenant', '--agent', '--limit', '--viewer-agent', '--viewer-tenant'].every(twice));
    // FR-148 D52: the viewer node's flags, a 24-hex id each, the timeline's only.
    check(
      'the wrapper hands --viewer-agent and --viewer-tenant on as RC_PATH_VIEWER and RC_PATH_VIEWER_TENANT, checked as ids, for --timeline only, before the target',
      sh.includes('ENVS+=("RC_PATH_VIEWER=$2")') && sh.includes('ENVS+=("RC_PATH_VIEWER_TENANT=$2")') && shl.filter((l) => /--viewer-(agent|tenant) takes a 24-hex/.test(l) && l.includes('=~ $HEX24')).length === 2 && before(lineOf(/--viewer-agent is for --timeline/)) && before(lineOf(/--viewer-tenant needs --viewer-agent/)),
    );
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
    KEPT,
    DERP_STATES,
    DERP_KINDS,
    DROP_CAUSES,
    named,
    fieldsOf,
    projectionOf,
    pairOf,
    serverRows,
    derpCalls,
    tally,
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
