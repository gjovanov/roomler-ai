#!/usr/bin/env bash
#
# fr138-roll-read.sh — FR-138 P0: what a server roll did to the remote-control
# sessions that were live across it, and how many session rows were never ended.
# READ-ONLY.
#
# FR-138 (docs/fr/FR-138-the-session-outlives-its-control-socket.md) keeps a live
# session through the loss of its control socket. §6 is the field read every RED
# and GREEN of it uses; this script is that read, against production, for a roll
# that already happened. It never starts a roll, a session or anything else.
#
# For a roll window (--from, --to) it reads, per session that was live at --from
# (or per session named with --session):
#   - the old `remote_sessions` row's end (ended_at, end_reason) and the
#     SessionEnded audit behind it;
#   - the device's next row by the same controller (the page's redial), and
#     every other row created in the window;
#   - the viewer's `rc.connect` records (POST /api/log/browser) after the cut,
#     with `after_drop`, matched on the record's `fields.agent_id` and the
#     batch's user: a browser batch carries no document agent id;
#   - any consent prompt raised for the new row: its `remote_audit` events, and
#     the agent's own `consent prompt surface` line;
#   - the agent's lines around the cut: `torn down peers ...` (today's close),
#     the failed connect, the flat redial, `rc:agent.hello sent`. A daemon
#     enrolled in several orgs runs one connection per org and uploads all of
#     their lines under its first enrollment, so the read prints how many orgs
#     the machine is enrolled in. FR-138's detach, resume and close lines do not
#     exist before P2a, so this read prints them as absent at P0;
#   - the media gap, as §6 step 4 defines it at P0: from the old row's
#     ended_at to the redial's first_frame record.
# With --active it counts instead the rows that have no end, by phase and by age
# (under 1 h, 1-24 h, over 24 h), with the oldest five by started_at: the rows
# P1's registration read and sweep will meet.
#
# TARGETS (pick one)
#
#   --kubectl --yes      where kubectl reaches namespace roomler-ai (on mars).
#   --mars --yes         from the dev box: sends this file to mars over
#                        `roomler ssh` and runs --kubectl there. The CLI is
#                        $ROOMLER_CLI, else /c/Program Files/Roomler/roomler.exe,
#                        else `roomler` on PATH.
#
# OPTIONS
#
#   --from ISO           the roll's start: the first pod's restart (or the
#                        promote's dispatch, as an earlier bound). A session
#                        live at this moment is one the roll could cut.
#   --to ISO             the roll's end: every pod Ready. At most 2 h after --from.
#   --tail SECS          how long after --to to look for redials (300; at most 3600)
#   --lookback HOURS     how far before --from a live row may have been created
#                        (24; at most 168). Older ones: name them with --session.
#   --session HEX        read this session instead of discovering by window;
#                        repeat for more (at most 20)
#   --active             the count of rows with no end instead of a roll read
#
# OUTPUT
#
#   Sanitized by construction: a device is dev1, dev2 … in order of appearance
#   (with its OS, agent version, consent mode and the number of orgs its machine
#   is enrolled in), a session is s:<last 4 hex>,
#   and no name, user, device id or address is printed. Times are UTC, each with
#   its offset from --from. A record's `ts` is the browser's clock; the gap is
#   also given by the server's receipt of the record.
#
# BOUNDS
#
#   An unbounded read inside the mongodb pod OOM-killed production Mongo once
#   (2026-10-26). Every read here starts on an indexed field (created_at, the
#   session id, agent_id + created_at, tenant_id + phase), carries a limit and
#   maxTimeMS, and the browser read counts its window first and refuses a large
#   one. The mongodb pod's restart count is printed before and after; a change
#   fails the run. The database URL is read from the roomler2 pod and handed to
#   mongosh on stdin, never printed.
#
# examples
#   scripts/fr138-roll-read.sh --mars --yes --from 2026-11-09T13:19:24Z --to 2026-11-09T13:19:58Z
#   scripts/fr138-roll-read.sh --mars --yes --active

set -euo pipefail

# Git-Bash rewrites an argument that starts with `/` into a Windows path. No-ops
# off Windows.
export MSYS_NO_PATHCONV=1 MSYS2_ARG_CONV_EXCL='*'

NS="${ROOMLER_NS:-roomler-ai}"
POD="mongodb-0"

die() { echo "fr138-roll-read: $*" >&2; exit 2; }

TARGET=""
YES=0
MODE="roll"
FROM=""
TO=""
TAIL=300
LOOKBACK=24
SESSIONS=""
PASS=()

iso_ok() { [[ "$1" =~ ^[0-9]{4}-[0-9]{2}-[0-9]{2}T[0-9]{2}:[0-9]{2}:[0-9]{2}(\.[0-9]{1,3})?Z$ ]]; }

while [ $# -gt 0 ]; do
  case "$1" in
    --kubectl)  TARGET=kubectl; shift ;;
    --mars)     TARGET=mars; shift ;;
    --yes)      YES=1; shift ;;
    --active)   MODE=active; PASS+=("$1"); shift ;;
    --from)     FROM="${2:-}"; iso_ok "$FROM" || die "--from must be an ISO time like 2026-11-09T13:19:24Z"; PASS+=("$1" "$2"); shift 2 ;;
    --to)       TO="${2:-}"; iso_ok "$TO" || die "--to must be an ISO time like 2026-11-09T13:19:58Z"; PASS+=("$1" "$2"); shift 2 ;;
    --tail)     TAIL="${2:-}"; PASS+=("$1" "$2"); shift 2 ;;
    --lookback) LOOKBACK="${2:-}"; PASS+=("$1" "$2"); shift 2 ;;
    --session)
      [[ "${2:-}" =~ ^[0-9a-f]{24}$ ]] || die "--session takes a 24-hex session id"
      SESSIONS="${SESSIONS:+$SESSIONS,}$2"; PASS+=("$1" "$2"); shift 2 ;;
    -h|--help)  sed -n '2,77p' "$0"; exit 0 ;;
    *)          die "unknown argument: $1 (--help)" ;;
  esac
done

case "$TAIL" in '' | *[!0-9]*) die "--tail takes seconds" ;; esac
case "$LOOKBACK" in '' | *[!0-9]*) die "--lookback takes hours" ;; esac
[ "$TAIL" -le 3600 ] || die "--tail caps at 3600"
[ "$LOOKBACK" -ge 1 ] && [ "$LOOKBACK" -le 168 ] || die "--lookback is 1 to 168"
[ "$(printf '%s' "$SESSIONS" | tr ',' '\n' | grep -c .)" -le 20 ] || die "at most 20 --session"
if [ "$MODE" = roll ]; then
  [ -n "$FROM" ] && [ -n "$TO" ] || die "a roll read needs --from and --to (or --active)"
fi
[ -n "$TARGET" ] || die "pick a target: --kubectl --yes or --mars --yes (--help)"
[ "$YES" = 1 ] || die "this reads PRODUCTION Mongo; re-run with --yes to start it"

if [ "$TARGET" = mars ]; then
  CLI="${ROOMLER_CLI:-}"
  if [ -z "$CLI" ]; then
    if [ -x "/c/Program Files/Roomler/roomler.exe" ]; then CLI="/c/Program Files/Roomler/roomler.exe"
    else CLI="$(command -v roomler || true)"; fi
  fi
  [ -n "$CLI" ] || die "no roomler CLI: set ROOMLER_CLI"
  # One `roomler ssh`, this file on stdin: a device caps SSH requests per caller.
  exec "$CLI" ssh mars "bash -s -- --kubectl --yes $(printf '%q ' "${PASS[@]+"${PASS[@]}"}")" < "$0"
fi

command -v kubectl >/dev/null || die "kubectl is not on PATH"
restarts() { kubectl -n "$NS" get pod "$POD" -o jsonpath='{.status.containerStatuses[0].restartCount}'; }
before="$(restarts)"
echo "mongodb pod: restarts=$before before the read"

if [ "$MODE" = roll ]; then
  # The rollout, for picking --from and --to: each roomler2 ReplicaSet's
  # creation (a roll's first pod), and the pods now running. No pod or node names.
  echo "# roomler2 replicasets: created, image tag, replicas"
  kubectl -n "$NS" get rs -o jsonpath='{range .items[*]}{.metadata.name}{" "}{.metadata.creationTimestamp}{" "}{.spec.template.spec.containers[0].image}{" "}{.status.replicas}{"\n"}{end}' |
    grep '^roomler2-' | grep -v prepull | sed -E 's#^[^ ]+ #rs created=#; s# [^ ]*/[^ /]*:([^ :]+) # tag=\1 replicas=#' | sort || true
  echo "# roomler2 pods now: started, ready since, image tag, restarts"
  kubectl -n "$NS" get pods -o jsonpath='{range .items[*]}{.metadata.name}{" "}{.status.startTime}{" "}{.status.conditions[?(@.type=="Ready")].lastTransitionTime}{" "}{.spec.containers[0].image}{" "}{.status.containerStatuses[0].restartCount}{"\n"}{end}' |
    grep '^roomler2-' | grep -v prepull | sed -E 's#^[^ ]+ #pod started=#; s# ([^ ]+) [^ ]*/[^ /]*:([^ :]+) # ready_since=\1 tag=\2 restarts=#' | sort || true
fi

url="$(kubectl -n "$NS" exec deploy/roomler2 -- printenv ROOMLER__DATABASE__URL)"
name="$(kubectl -n "$NS" exec deploy/roomler2 -- printenv ROOMLER__DATABASE__NAME)"
[ -n "$url" ] || { echo "could not read the database URL" >&2; exit 3; }
case "$name" in '' | *[!A-Za-z0-9_-]*) echo "unexpected database name" >&2; exit 3 ;; esac

# The read, one block (mongosh must not see a statement split across lines).
# Placeholders are filled below; every value was checked above.
js="$(cat <<'JS'
{
const db2 = db.getSiblingDB("__DBNAME__");
const MODE = "__MODE__";
const H = 3600 * 1000;
const hex = (v) => (v && v.toHexString) ? v.toHexString() : String(v);
const sid = (v) => "s:" + hex(v).slice(-4);
const iso = (d) => (d ? new Date(d).toISOString() : "-");
const devs = new Map();
const dev = (aid) => { const k = hex(aid); if (!devs.has(k)) devs.set(k, "dev" + (devs.size + 1)); return devs.get(k); };
print("mongo_version=" + db.version());
for (const c of ["remote_sessions", "remote_audit", "agent_logs", "agents"]) {
  print("estimated " + c + "=" + db2.getCollection(c).estimatedDocumentCount());
}

if (MODE === "active") {
  // Rows with no end. The tenant list comes from the {tenant_id, phase} index
  // (a distinct scan, one key per tenant); the read then seeks tenant x phase.
  const now = new Date();
  const OPEN = ["pending", "awaiting_consent", "negotiating", "active"];
  const LIM = 5000;
  const tids = db2.remote_sessions.distinct("tenant_id", {}, { maxTimeMS: 10000 });
  print("tenants_with_sessions=" + tids.length);
  const rows = db2.remote_sessions.aggregate([
    { $match: { tenant_id: { $in: tids }, phase: { $in: OPEN }, ended_at: null } },
    { $limit: LIM },
    { $project: { agent_id: 1, phase: 1, created_at: 1, started_at: 1, carrier: 1, bytes: "$stats.bytes_sent" } },
    { $lookup: { from: "remote_sessions", localField: "agent_id", foreignField: "agent_id", let: { c: "$created_at" },
        pipeline: [{ $match: { $expr: { $gt: ["$created_at", "$$c"] } } }, { $limit: 1 }, { $project: { _id: 1 } }], as: "later" } },
    { $lookup: { from: "agents", localField: "agent_id", foreignField: "_id",
        pipeline: [{ $project: { _id: 0, os: 1, status: 1, last_seen_at: 1, deleted_at: 1 } }], as: "a" } }
  ], { maxTimeMS: 30000, hint: { tenant_id: 1, phase: 1 } }).toArray();
  print("rows_with_no_end=" + rows.length + (rows.length >= LIM ? " (LIMIT REACHED: more exist, this count is a floor)" : ""));
  print("# age = now - started_at (created_at when it never started); superseded = the device has a later row;");
  print("# device_offline = the device's row does not read online now; device_gone = no device row, or it is deleted");
  const B = ["under_1h", "1h_to_24h", "over_24h"];
  const g = {};
  const perDev = {};
  for (const r of rows) {
    const t0 = r.started_at || r.created_at;
    r.age = now - t0;
    r.bucket = r.age < H ? B[0] : (r.age < 24 * H ? B[1] : B[2]);
    const a = r.a[0];
    const k = r.phase + " " + r.bucket;
    const e = g[k] || (g[k] = { n: 0, superseded: 0, device_offline: 0, device_gone: 0, hours: 0 });
    e.n += 1;
    if (r.later.length > 0) e.superseded += 1;
    if (!a || a.deleted_at) e.device_gone += 1;
    else if (a.status !== "online") e.device_offline += 1;
    e.hours += r.age / H;
    perDev[hex(r.agent_id)] = (perDev[hex(r.agent_id)] || 0) + 1;
  }
  for (const p of OPEN) for (const b of B) {
    const e = g[p + " " + b];
    if (e) print("phase=" + p + " age=" + b + " n=" + e.n + " superseded=" + e.superseded + " device_offline=" + e.device_offline
      + " device_gone=" + e.device_gone + " hours=" + e.hours.toFixed(1));
  }
  const counts = Object.values(perDev).sort((x, y) => y - x);
  print("devices_with_open_rows=" + counts.length + " most_on_one_device=" + (counts[0] || 0));
  const actAll = rows.filter((r) => r.phase === "active")
    .sort((x, y) => (x.started_at || x.created_at) - (y.started_at || y.created_at));
  if (actAll.length) {
    const nw = actAll[actAll.length - 1];
    print("newest_active_started=" + iso(nw.started_at) + " age_h=" + (nw.age / H).toFixed(1));
  }
  const act = actAll.slice(0, 5);
  act.forEach((r, i) => {
    const a = r.a[0];
    print("oldest" + (i + 1) + " started=" + iso(r.started_at) + " age_h=" + (r.age / H).toFixed(1)
      + " os=" + (a ? a.os : "(no device row)")
      + " device=" + (!a ? "gone" : a.deleted_at ? "deleted" : a.status)
      + " device_last_seen_h=" + (a && a.last_seen_at ? ((now - a.last_seen_at) / H).toFixed(1) : "-")
      + " superseded=" + (r.later.length > 0) + " carrier=" + (r.carrier || "-") + " bytes_sent=" + (r.bytes || 0));
  });
} else {
  const FROM = new Date("__FROM__");
  const TO = new Date("__TO__");
  const TEND = new Date(TO.getTime() + __TAIL__ * 1000);
  const LB = new Date(FROM.getTime() - __LOOKBACK__ * H);
  const SIDS = "__SESSIONS__".split(",").filter((s) => s.length === 24).map((s) => ObjectId(s));
  const off = (d) => { const s = (new Date(d) - FROM) / 1000; return (s >= 0 ? "+" : "") + s.toFixed(1) + "s"; };
  const at = (d) => (d ? iso(d) + " (" + off(d) + ")" : "-");
  const sec = (a, b) => ((new Date(a) - new Date(b)) / 1000).toFixed(1) + " s";
  if (!(TO >= FROM) || TO - FROM > 2 * H) {
    print("refused: --to must be after --from and at most 2 h later");
  } else {
    print("window from=" + iso(FROM) + " to=" + iso(TO) + " tail_end=" + iso(TEND) + " lookback_from=" + iso(LB));
    const PROJ = { agent_id: 1, tenant_id: 1, controller_user_id: 1, phase: 1, created_at: 1, started_at: 1, ended_at: 1, end_reason: 1, carrier: 1 };
    const rows = db2.remote_sessions.aggregate([
      { $match: { created_at: { $gte: LB, $lt: TEND } } },
      { $sort: { created_at: 1 } },
      { $limit: 1000 },
      { $project: PROJ }
    ], { maxTimeMS: 20000, hint: { created_at: 1 } }).toArray();
    print("rows_created_from_lookback_to_tail_end=" + rows.length + (rows.length >= 1000 ? " (LIMIT REACHED)" : ""));
    const liveAt = (r) => !!r.started_at && r.started_at < FROM && (!r.ended_at || r.ended_at >= FROM);
    const named = SIDS.length
      ? db2.remote_sessions.find({ _id: { $in: SIDS } }, PROJ).limit(20).maxTimeMS(5000).toArray() : [];
    if (SIDS.length) print("sessions_named=" + SIDS.length + " found=" + named.length);
    const study = SIDS.length ? named : rows.filter(liveAt);
    const inflight = rows.filter((r) => !r.started_at && r.created_at < FROM && (!r.ended_at || r.ended_at >= FROM));
    const fresh = rows.filter((r) => r.created_at >= FROM);
    print("live_at_from=" + rows.filter(liveAt).length + " requested_not_live_at_from=" + inflight.length
      + " created_from_to_tail_end=" + fresh.length);

    // The browser's rc.connect records, fleet-wide: a cross-org page files its
    // record under its own org, so no tenant filter. Counted first.
    const bFrom = new Date(FROM.getTime() - 30000);
    const bTo = new Date(TEND.getTime() + 120000);
    const nb = db2.agent_logs.countDocuments({ created_at: { $gte: bFrom, $lt: bTo } },
      { hint: { created_at: 1 }, maxTimeMS: 10000, limit: 40001 });
    print("agent_logs_batches_in_window=" + nb);
    let recs = [];
    if (nb > 40000) {
      print("rc.connect read skipped: the window holds more than 40000 batches");
    } else {
      recs = db2.agent_logs.aggregate([
        { $match: { created_at: { $gte: bFrom, $lt: bTo }, source: "browser" } },
        { $sort: { created_at: 1 } },
        { $limit: 3000 },
        { $project: { _id: 0, user_id: 1, session_id: 1, created_at: 1,
            l: { $filter: { input: "$lines", as: "l", cond: { $eq: ["$$l.target", "rc.connect"] } } } } },
        { $unwind: "$l" },
        { $project: { user_id: 1, session_id: 1, created_at: 1, ts: "$l.ts", level: "$l.level", f: "$l.fields" } }
      ], { maxTimeMS: 30000, hint: { created_at: 1 } }).toArray();
    }
    const ad = recs.filter((x) => x.f && x.f.after_drop === true);
    print("rc.connect records in window=" + recs.length + " after_drop=" + ad.length);
    const recLine = (x) => at(x.ts) + " " + x.level + " v=" + x.f.v + " attempt=" + x.f.attempt + " outcome=" + x.f.outcome
      + " after_drop=" + x.f.after_drop + " ttff_ms=" + (x.f.ttff_ms === null || x.f.ttff_ms === undefined ? "-" : x.f.ttff_ms)
      + " stalled_at=" + (x.f.stalled_at || "-") + " error=" + (x.f.error_code || "-")
      + " session=" + (x.session_id ? "s:" + String(x.session_id).slice(-4) : "-");

    // The agent's lines around the cut (P0's close is the teardown line).
    const MSGS = ["torn down peers + hid indicator overlays on ws disconnect", "signaling connect failed; backing off",
      "signaling: the load balancer has no pod for this org yet; redialling flat (FR-28)", "rc:agent.hello sent",
      "consent prompt surface", "operator consent decision", "ws closed by peer"];
    const agentLines = new Map();
    const linesOf = (r) => {
      const k = hex(r.agent_id);
      if (agentLines.has(k)) return agentLines.get(k);
      const q = { tenant_id: r.tenant_id, agent_id: r.agent_id, created_at: { $gte: bFrom, $lt: bTo } };
      const batches = db2.agent_logs.countDocuments(q, { maxTimeMS: 10000, limit: 1001 });
      const ls = db2.agent_logs.aggregate([
        { $match: q },
        { $sort: { created_at: 1 } },
        { $limit: 400 },
        { $project: { _id: 0, created_at: 1, l: { $filter: { input: "$lines", as: "l", cond: { $in: ["$$l.msg", MSGS] } } } } },
        { $unwind: "$l" },
        { $project: { created_at: 1, ts: "$l.ts", msg: "$l.msg", f: "$l.fields" } }
      ], { maxTimeMS: 20000, hint: { tenant_id: 1, agent_id: 1, created_at: -1 } }).toArray();
      const v = { batches: batches, lines: ls };
      agentLines.set(k, v);
      return v;
    };
    const cls = (s) => { const t = String(s || "");
      return /without closing handshake/.test(t) ? "reset_without_close" : /502/.test(t) ? "lb_502"
        : /refused/i.test(t) ? "refused" : /timed? ?out/i.test(t) ? "timeout" : "other"; };
    const lineText = (x) => {
      const f = x.f || {};
      if (x.msg.startsWith("torn down peers")) return "teardown (closed " + f.count + " peer(s), today's close)";
      if (x.msg.startsWith("signaling connect failed")) return "control socket lost (" + cls(f.cause) + ")";
      if (x.msg.startsWith("signaling: the load balancer")) return "flat redial (FR-28)";
      if (x.msg === "rc:agent.hello sent") return "hello sent";
      if (x.msg === "ws closed by peer") return "socket closed by the server";
      if (x.msg === "consent prompt surface") return "consent prompt raised for s:" + String(f.session || "").slice(-4)
        + " (native=" + f.native + " notified=" + f.notified + ")";
      if (x.msg === "operator consent decision") return "consent decided for s:" + String(f.session || "").slice(-4);
      return x.msg;
    };

    // A daemon enrolled in several orgs runs one connection per org and uploads
    // every connection's lines under its first enrollment. The count of its
    // enrollments seeks the {tenant_id, machine_id} index, one tenant at a time.
    const agentTids = study.length ? db2.agents.distinct("tenant_id", {}, { maxTimeMS: 10000 }) : [];
    const nextOf = new Map();
    for (const r of study) {
      const A = r.agent_id, aH = hex(A), U = hex(r.controller_user_id);
      const d = db2.agents.find({ _id: A }, { _id: 0, os: 1, agent_version: 1, machine_id: 1, "access_policy.consent_mode": 1 })
        .limit(1).maxTimeMS(5000).toArray()[0] || {};
      const orgs = d.machine_id
        ? db2.agents.countDocuments({ tenant_id: { $in: agentTids }, machine_id: d.machine_id, deleted_at: null }, { limit: 50, maxTimeMS: 5000 })
        : "?";
      print("");
      print("SESSION " + sid(r._id) + " on " + dev(A) + " os=" + (d.os || "?") + " agent=" + (d.agent_version || "?")
        + " consent_mode=" + ((d.access_policy && d.access_policy.consent_mode) || "unset") + " orgs_on_machine=" + orgs
        + " carrier=" + (r.carrier || "-") + (liveAt(r) ? "" : " [not live at --from]"));
      print("  old row: started " + at(r.started_at) + ", ended " + at(r.ended_at) + ", end_reason=" + (r.end_reason || "-")
        + ", phase=" + r.phase);
      const oa = db2.remote_audit.find({ session_id: r._id, "event.kind": "session_ended" }, { _id: 0, at: 1, "event.reason": 1 })
        .sort({ at: 1 }).limit(3).maxTimeMS(5000).toArray();
      print("  old row's session_ended audit: " + (oa.length ? oa.map((e) => at(e.at) + " " + e.event.reason).join("; ") : "none"));
      const nx = db2.remote_sessions.find({ agent_id: A, created_at: { $gt: r.created_at } }, PROJ)
        .sort({ created_at: 1 }).limit(5).maxTimeMS(5000).toArray();
      const N = nx.find((x) => hex(x.controller_user_id) === U);
      const others = nx.filter((x) => x !== N);
      if (!N) {
        print("  next row (same device, same controller): none" + (nx.length ? "" : ", and no later row on the device"));
      } else {
        nextOf.set(hex(r._id), hex(N._id));
        print("  next row (same device, same controller): " + sid(N._id) + " created " + at(N.created_at)
          + (r.ended_at ? ", " + sec(N.created_at, r.ended_at) + " after the old end" : "")
          + ", started " + at(N.started_at) + ", phase=" + N.phase
          + ", end " + (N.ended_at ? at(N.ended_at) + " " + N.end_reason : "-") + ", carrier=" + (N.carrier || "-"));
        const au = db2.remote_audit.find({ session_id: N._id }, { _id: 0, at: 1, "event.kind": 1 })
          .sort({ at: 1 }).limit(30).maxTimeMS(5000).toArray();
        const t0 = au.length ? au[0].at : null;
        print("  new row's audit: " + (au.length ? au.map((e) => e.event.kind + " +" + (e.at - t0) + "ms").join(", ") : "none"));
        const p = au.find((e) => e.event.kind === "consent_prompted"), gr = au.find((e) => e.event.kind === "consent_granted");
        print("  consent_prompted to consent_granted: " + (p && gr ? (gr.at - p.at) + " ms" : "-")
          + " (consent_prompted is audited on every request; a prompt someone saw is the agent's line below)");
      }
      if (others.length) print("  other later rows on the device: " + others.map((x) => sid(x._id) + " created " + at(x.created_at)).join("; "));

      const mine = recs.filter((x) => x.f && x.f.agent_id === aH && x.user_id && hex(x.user_id) === U && x.ts >= FROM)
        .sort((p, q) => p.ts - q.ts);
      const mad = mine.filter((x) => x.f.after_drop === true);
      print("  rc.connect from this controller to this device after --from: " + mine.length + ", with after_drop=true: " + mad.length);
      for (const x of mine) print("    " + recLine(x));
      const ff = mad.find((x) => x.f.outcome === "first_frame");
      if (!r.ended_at) print("  media gap: n/a, the old row has no ended_at");
      else if (!ff) print("  media gap: n/a, no after_drop first_frame record for this device and controller in the window");
      else print("  media gap (old row's ended_at to the redial's first_frame record): " + sec(ff.ts, r.ended_at)
        + "; by the server's receipt of the record: " + sec(ff.created_at, r.ended_at));

      const al = linesOf(r);
      print("  agent lines (" + al.batches + " batches under this enrollment in the window"
        + (orgs !== "?" && orgs > 1 ? "; this daemon has " + orgs + " enrollments and uploads every connection's lines"
          + " under its first one, so a line may be another org's connection, and a session in a later org has none here" : "") + "):");
      if (!al.lines.length) print("    none of the read lines");
      for (const x of al.lines) {
        if (x.msg === "consent prompt surface" || x.msg === "operator consent decision") {
          const s = String((x.f || {}).session || "");
          if (!N || s !== hex(N._id)) continue;
        }
        print("    " + at(x.ts) + " " + lineText(x));
      }
      if (N) {
        const pr = al.lines.find((x) => x.msg === "consent prompt surface" && String((x.f || {}).session || "") === hex(N._id));
        print("  consent prompt raised for the new row: " + (pr ? "yes, at " + at(pr.ts) : "no `consent prompt surface` line"));
      }
      print("  detach / resume / close lines: absent at P0 (none exist before P2a)");
    }

    if (inflight.length) {
      print("");
      print("REQUESTED, NOT LIVE AT --from (a session not yet live ends as today):");
      for (const r of inflight) print("  " + sid(r._id) + " on " + dev(r.agent_id) + " created " + at(r.created_at)
        + ", phase=" + r.phase + ", end " + (r.ended_at ? at(r.ended_at) + " " + r.end_reason : "-"));
    }
    print("");
    print("ROWS CREATED FROM --from TO THE TAIL'S END:");
    if (!fresh.length) print("  none");
    for (const n of fresh) {
      const own = recs.filter((x) => x.session_id && String(x.session_id) === hex(n._id));
      const same = study.find((o) => hex(o.agent_id) === hex(n.agent_id) && hex(o.controller_user_id) === hex(n.controller_user_id)
        && o.created_at < n.created_at);
      const isNext = same && nextOf.get(hex(same._id)) === hex(n._id);
      print("  " + sid(n._id) + " on " + dev(n.agent_id) + " created " + at(n.created_at) + ", started " + at(n.started_at)
        + ", end " + (n.ended_at ? at(n.ended_at) + " " + n.end_reason : "-") + ", carrier=" + (n.carrier || "-")
        + ", its rc.connect records=" + own.length + " (after_drop on " + own.filter((x) => x.f && x.f.after_drop === true).length + ")"
        + (same ? ", same device and controller as " + sid(same._id) + (isNext ? " (its next row)" : "") : ""));
    }
    print("");
    print("AFTER_DROP RECORDS IN THE WINDOW, ANY DEVICE:");
    if (!ad.length) print("  none");
    for (const x of ad) print("  " + (x.f.agent_id ? dev(x.f.agent_id) : "dev?") + " " + recLine(x));
  }
}
print("done");
}
JS
)"
js="${js//__DBNAME__/$name}"
js="${js//__MODE__/$MODE}"
js="${js//__FROM__/$FROM}"
js="${js//__TO__/$TO}"
js="${js//__TAIL__/$TAIL}"
js="${js//__LOOKBACK__/$LOOKBACK}"
js="${js//__SESSIONS__/$SESSIONS}"

rc=0
{ printf '%s\n' "$url"; printf '%s\n' "$js"; } |
  kubectl -n "$NS" exec -i "$POD" -- sh -c 'IFS= read -r u; exec mongosh --quiet "$u" --file /dev/stdin' || rc=$?
after="$(restarts)"
echo "mongodb pod: restarts=$after after the read"
if [ "$after" != "$before" ]; then
  echo "fr138-roll-read: the mongodb pod RESTARTED during the read; treat this run as harmful, not as data" >&2
  exit 3
fi
exit "$rc"
