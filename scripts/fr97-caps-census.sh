#!/usr/bin/env bash
#
# fr97-caps-census.sh — FR-97 P0d: count the fleet's capability words by OS (and
# by native architecture, from agents new enough to report it). READ-ONLY.
#
# What a device advertises in its hello (`AgentCaps`) is a promise; FR-97 P0c
# removes the words an OS cannot keep. This is the count that shows it: AC15 asks
# for one taken before P0c ships and one after the last phase, with no word
# left on an OS that cannot serve it.
#
# Run on mars, where kubectl reaches the cluster and docker runs, from a checkout
# that has prod-mongo-read.sh beside this file:
#   scripts/fr97-caps-census.sh [days]      # agents seen in the last N days (30)
#
# mongosh runs OUTSIDE mongodb-0, through prod-mongo-read.sh (a port-forward and
# a throwaway client container), so its memory never counts against the
# database's limit. The runner reads the credentials from the secret into files:
# they are never printed, and never in an argument list. Only counts come out:
# no names, no ids.
set -euo pipefail
NS="${ROOMLER_NS:-roomler-ai}"
HERE="$(cd "$(dirname "$0")" && pwd)"
DAYS="${1:-30}"
case "$DAYS" in '' | *[!0-9]*) echo "days must be a number" >&2; exit 2 ;; esac
[ -f "$HERE/prod-mongo-read.sh" ] || { echo "missing $HERE/prod-mongo-read.sh" >&2; exit 2; }
# The cluster is named, never this host's current context: the runner refuses a
# host that has no production context, before any cluster call.
KUBE_CONTEXT="$(bash "$HERE/prod-mongo-read.sh" --print-context)" || exit 2
export ROOMLER_KUBE_CONTEXT="$KUBE_CONTEXT"
kubectl() { command kubectl --context "$KUBE_CONTEXT" "$@"; }

name="$(kubectl -n "$NS" exec deploy/roomler2 -- printenv ROOMLER__DATABASE__NAME)"
case "$name" in '' | *[!A-Za-z0-9_-]*) echo "unexpected database name" >&2; exit 3 ;; esac

# The aggregation. Agents not tombstoned and seen within the window; one row per
# (list, OS, word). `arch` is the native architecture the hello carries since
# FR-97 P0d; older agents read "unreported".
js="$(cat <<EOF
const db2 = db.getSiblingDB("${name}");
const since = new Date(Date.now() - ${DAYS} * 86400 * 1000);
const live = { deleted_at: null, last_seen_at: { \$gte: since } };
print("# agents seen in the last ${DAYS} days, not removed");
db2.agents.aggregate([
  { \$match: live },
  { \$group: { _id: { os: "\$os", arch: { \$ifNull: ["\$arch", "unreported"] } }, n: { \$sum: 1 } } },
  { \$sort: { "_id.os": 1, "_id.arch": 1 } },
]).forEach(r => print(["agents", r._id.os, r._id.arch, r.n].join("\t")));
const lists = ["audio", "clipboard", "input", "apps", "layout", "files", "video",
  "transports", "codecs", "hw_encoders", "permissions", "multi_org", "rpc"];
for (const f of lists) {
  db2.agents.aggregate([
    { \$match: live },
    { \$unwind: "\$capabilities." + f },
    { \$group: { _id: { os: "\$os", w: "\$capabilities." + f }, n: { \$sum: 1 } } },
    { \$sort: { "_id.os": 1, "_id.w": 1 } },
  ]).forEach(r => print([f, r._id.os, r._id.w, r.n].join("\t")));
}
// FR-97 P0c — the honest "no": each withheld feature as key:code.
db2.agents.aggregate([
  { \$match: live },
  { \$project: { os: 1, u: { \$objectToArray: { \$ifNull: ["\$capabilities.unavailable", {}] } } } },
  { \$unwind: "\$u" },
  { \$group: { _id: { os: "\$os", w: { \$concat: ["\$u.k", ":", { \$ifNull: ["\$u.v.code", "?"] }] } }, n: { \$sum: 1 } } },
  { \$sort: { "_id.os": 1, "_id.w": 1 } },
]).forEach(r => print(["unavailable", r._id.os, r._id.w, r.n].join("\t")));
for (const b of ["supports_clipboard", "supports_file_transfer", "has_input_permission"]) {
  db2.agents.aggregate([
    { \$match: Object.assign({}, live, { ["capabilities." + b]: true }) },
    { \$group: { _id: "\$os", n: { \$sum: 1 } } },
    { \$sort: { _id: 1 } },
  ]).forEach(r => print([b, r._id, "true", r.n].join("\t")));
}
EOF
)"

# The script on stdin; the runner opens `db` on the app database.
printf '%s\n' "$js" |
  bash "$HERE/prod-mongo-read.sh" --kubectl --yes --db "$name" - |
  { printf 'list\tos\tword\tcount\n'; cat; }
