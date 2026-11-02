// #3272: the fleet's RTP sessions, from `agent_logs`. How many, on which encoder,
// how hard the pump worked on an idle screen, and how many shipped no video.
// docs/encoders.md holds the baseline. The debt on #3272 retires against this read,
// until `rc:session.stats` carries the transport and the encoder itself.
//
// Runs inside the Mongo container, where its memory counts against the database's
// own limit, so it is bounded by construction: the text index picks the batches,
// $limit caps them, $filter keeps the matching lines, $group folds them server-side.
//
//   roomler ssh mars -- 'kubectl -n roomler-ai exec -i mongodb-0 -- mongosh --nodb --quiet' \
//     < scripts/rtp-reach-read.cjs | grep -o 'OUT .*'
//
// Prints one line per RTP session, then the totals. `agent_logs` keeps 7 days.
// A session is RTP if its pump logged the RTP heartbeat: the DataChannel pumps name
// theirs differently, so a session whose transport fell through is still counted.
// Counters are cumulative, so empties/s holds when the uploader lost heartbeats. A
// flood drops lines, and then `hb` is a floor.
// A rate runs from the session's request line to its last heartbeat. A still screen
// heartbeats about every 30 s and the counters start at the session's start, so the
// span between heartbeats alone read a short session high: 106 for a true 72. A
// session whose request line was lost falls back to its first heartbeat.
// The totals cover the 7 days, which mix releases, so `OUT version` splits them.
//
// ⚠️ mongosh reads stdin as a REPL, so a line that parses alone runs alone. Keep
// `coll.aggregate([` together and `.forEach` on the closing line: split them and
// every line errors, and the only output left is a misleading total of 0.
const HOURS = 168;
const MAX_BATCHES = 5000;
const MIN_RATE_SECS = 5;

const u = process.env.MONGO_INITDB_ROOT_USERNAME;
const p = process.env.MONGO_INITDB_ROOT_PASSWORD;
const database = new Mongo(`mongodb://${encodeURIComponent(u)}:${encodeURIComponent(p)}@127.0.0.1:27017/?authSource=admin`).getDB('roomler2');
const coll = database.getCollection('agent_logs');
const since = new Date(Date.now() - HOURS * 3600e3);
print('OUT est-batches ' + coll.estimatedDocumentCount());

const linesLike = (regex) => ({ $filter: { input: '$lines', as: 'l', cond: { $regexMatch: { input: { $ifNull: ['$$l.msg', ''] }, regex } } } });
const opts = { maxTimeMS: 60000, allowDiskUse: false };

// Every session's request line: what the browser listed, and what was negotiated.
const requests = {};
let total = 0;
let onDc = 0;
coll.aggregate([
  { $match: { $text: { $search: '"incoming session request"' }, created_at: { $gte: since } } },
  { $limit: MAX_BATCHES },
  { $project: { _id: 0, lines: linesLike(/^incoming session request/) } },
  { $unwind: '$lines' },
  { $project: { s: '$lines.fields.session_id', t: '$lines.ts', c: '$lines.fields.chosen_codec', q: '$lines.fields.requested_transport', n: '$lines.fields.negotiated_transport', b: '$lines.fields.browser_caps' } },
], opts).forEach((row) => {
  total++;
  if ((row.n || '').includes('data-channel')) onDc++;
  requests[row.s] = row;
});

// The RTP pump's heartbeats, folded per session.
const sessions = [];
coll.aggregate([
  { $match: { $text: { $search: '"media pump heartbeat"' }, created_at: { $gte: since } } },
  { $limit: MAX_BATCHES },
  { $project: { _id: 0, agent_id: 1, agent_version: 1, lines: linesLike(/^media pump heartbeat/) } },
  { $unwind: '$lines' },
  { $group: { _id: '$lines.fields.session_id', a: { $first: '$agent_id' }, v: { $max: '$agent_version' }, first: { $min: '$lines.ts' }, last: { $max: '$lines.ts' }, hb: { $sum: 1 }, backends: { $addToSet: '$lines.fields.backend' }, empty: { $max: '$lines.fields.frames_empty' }, enc: { $max: '$lines.fields.frames_encoded' }, bytes: { $max: '$lines.fields.bytes_written' }, fps: { $max: '$lines.fields.target_fps' } } },
  { $sort: { first: 1 } },
], opts).forEach((row) => { sessions.push(row); });

// What the hosts behind those sessions are: a throwaway node, and whether H.264 has a hardware cell.
const ids = [...new Set(sessions.map((s) => s.a && s.a.toHexString()).filter(Boolean))];
const hosts = {};
database.getCollection('agents').find({ _id: { $in: ids.map((id) => ObjectId(id)) } }, { ephemeral: 1, 'capabilities.video_cells': 1 }).limit(1000).forEach((row) => {
  const cells = (row.capabilities && row.capabilities.video_cells) || [];
  hosts[row._id.toHexString()] = { ephemeral: !!row.ephemeral, h264Hw: cells.some((c) => c.codec === 'h264' && c.hw) };
});

const byBackend = {};
const byVersion = {};
const rates = [];
let noVideo = 0;
let rtpH264OnHw = 0;
sessions.forEach((s) => {
  const id = s.a ? s.a.toHexString() : '-';
  const host = hosts[id] || {};
  const req = requests[s._id] || {};
  const secs = (s.last - (req.t || s.first)) / 1000;
  const emptyRate = secs >= MIN_RATE_SECS ? Math.round((s.empty || 0) / secs) : null;
  const encRate = secs >= MIN_RATE_SECS ? ((s.enc || 0) / secs).toFixed(1) : '-';
  const name = s.backends.join('+');
  const ver = byVersion[s.v || '-'] || (byVersion[s.v || '-'] = { rtp: 0, noVideo: 0, rates: [] });
  byBackend[name] = (byBackend[name] || 0) + 1;
  ver.rtp++;
  if (emptyRate !== null) {
    rates.push(emptyRate);
    ver.rates.push(emptyRate);
  }
  if (!s.bytes) {
    noVideo++;
    ver.noVideo++;
  }
  if (req.c === 'h264' && host.h264Hw && !host.ephemeral) rtpH264OnHw++;
  print(['OUT rtp', s.first.toISOString(), id, s.v || '-', 'secs=' + Math.round(secs), 'hb=' + s.hb, 'backend=' + name, 'chosen=' + (req.c || '?'), 'requested=' + (req.q || '?'), 'negotiated=' + (req.n || '?'), 'caps=' + (req.b || '?').replace(/[\s"]/g, ''), 'enc/s=' + encRate, 'empties/s=' + (emptyRate === null ? '-' : emptyRate), 'fps=' + (s.fps ?? '-'), 'bytes=' + (s.bytes ?? 0), 'ephemeral=' + (host.ephemeral ?? '?'), 'h264hw=' + (host.h264Hw ?? '?')].join(' '));
});

rates.sort((a, b) => a - b);
const at = (q) => (rates.length ? rates[Math.min(rates.length - 1, Math.floor(rates.length * q))] : '-');
print('OUT sessions ' + total + ' dc ' + onDc + ' rtp ' + sessions.length + ' agents ' + ids.length);
print('OUT no-video ' + noVideo);
print('OUT backends ' + Object.keys(byBackend).sort().map((k) => k + '=' + byBackend[k]).join(' '));
print('OUT empties/s p50 ' + at(0.5) + ' p90 ' + at(0.9) + ' max ' + (rates.length ? rates[rates.length - 1] : '-') + ' over ' + rates.length + ' sessions of ' + MIN_RATE_SECS + ' s or more');
print('OUT rtp-h264-on-a-hardware-cell-not-ephemeral ' + rtpH264OnHw);
Object.keys(byVersion).sort().forEach((v) => {
  const r = byVersion[v].rates.sort((a, b) => a - b);
  print('OUT version ' + v + ' rtp ' + byVersion[v].rtp + ' no-video ' + byVersion[v].noVideo + ' empties/s p50 ' + (r.length ? r[Math.floor(r.length / 2)] : '-') + ' max ' + (r.length ? r[r.length - 1] : '-'));
});
