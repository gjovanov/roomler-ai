// FR-134: the fleet's `resource watch` lines, from `agent_logs`.
//
// Runs inside the Mongo container, where its memory counts against the
// database's own limit, so it is bounded by construction: the text index picks
// the batches, $limit caps them, and only the matching lines leave the server.
//
//   roomler ssh mars -- 'kubectl -n roomler-ai exec -i mongodb-0 -- mongosh --nodb --quiet' \
//     < scripts/resource-watch-read.cjs | grep -o 'OUT .*'
//
// Prints one line per reading, then a count. `agent_logs` keeps 7 days.
//
// ⚠️ mongosh reads stdin as a REPL, so a line that parses alone runs alone. Keep
// `coll.aggregate([` together and `.forEach` on the closing line: split them and
// every line errors, and the only output left is a misleading `lines 0`.
const HOURS = 168;
const MAX_BATCHES = 5000;

const u = process.env.MONGO_INITDB_ROOT_USERNAME;
const p = process.env.MONGO_INITDB_ROOT_PASSWORD;
const database = new Mongo(`mongodb://${encodeURIComponent(u)}:${encodeURIComponent(p)}@127.0.0.1:27017/?authSource=admin`).getDB('roomler2');
const coll = database.getCollection('agent_logs');
const since = new Date(Date.now() - HOURS * 3600e3);
print('OUT est-batches ' + coll.estimatedDocumentCount());

let n = 0;
coll.aggregate([
  { $match: { $text: { $search: '"resource watch"' }, created_at: { $gte: since } } },
  { $limit: MAX_BATCHES },
  { $project: { _id: 0, agent_id: 1, agent_version: 1, lines: { $filter: { input: '$lines', as: 'l',
      cond: { $regexMatch: { input: { $ifNull: ['$$l.msg', ''] }, regex: /^resource watch/ } } } } } },
  { $unwind: '$lines' },
  { $project: { a: '$agent_id', v: '$agent_version', t: '$lines.ts', l: '$lines.level',
      h: '$lines.fields.handles', r: '$lines.fields.rise', s: '$lines.fields.per_second' } },
], { maxTimeMS: 60000, allowDiskUse: false }).forEach((row) => {
  n++;
  print(['OUT', row.t.toISOString(), row.a ? row.a.toHexString() : '-', row.v || '-', row.l, 'handles=' + row.h, 'rise=' + (row.r ?? ''), 'per_s=' + (row.s ?? '')].join(' '));
});
print('OUT lines ' + n);
