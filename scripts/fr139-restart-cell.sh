#!/usr/bin/env bash
#
# fr139-restart-cell.sh — FR-139 P0: the restart cell (spec §5), the read that
# every RED and GREEN of FR-139 uses.
#
# FR-139 (docs/fr/FR-139-carrier-transitions-make-before-break.md) stops a
# daemon restart from splitting a pair that holds each other on a direct
# carrier. Two lab guests run the overlay: A is the end that restarts, B is the
# silent end, which has nothing of its own to send. One run of the cell:
#
#   1. pre    on both ends: each reads the other on the cell's carrier (direct
#             `lan` for the LAN cell), handshaken, not stalled and not
#             upgrading, continuously for QUIET seconds. A run starts only then.
#   2. b-start on B: a journal mark, a capture of every UDP datagram to and from
#             A's LAN address, and a 2 s sampler of B's `roomler peers` row for A.
#   3. a-cell on A: `roomler restart`, then wait for a NEW daemon pid that holds
#             an overlay address (`daemon_up`). Ping B's overlay address every
#             0.2 s; `first_reply` is daemon_up -> the first echo reply, on A's
#             monotonic clock. Then a 0.5 s ping for SAMPLE seconds, cut into
#             gaps by when each reply ARRIVED, beside a 2 s sampler of A's own
#             `roomler peers` row for B.
#   4. b-stop  on B: the capture classified by WireGuard message length, B's
#             journal since the mark, the sampler, and the counts the spec reads
#             (`demote-follow`, `UNREGISTERED receiver index`, `follow verdict`).
#
# `summary` reduces a run's logs to one line: first_reply, the gaps, B's counts,
# and both ends' rows at daemon_up + 60 s and + 180 s, including the
# relayed-instead holdoff (`why.relayed_instead_s`) and its strikes.
#
# It changes nothing but the two guests it is pointed at: their daemon's
# restart, an optional log drop-in, and files under /var/tmp/fr139.
#
# GUEST SIDE (as root; the driver copies this file to both guests first)
#
#   pre     PEER_OIP CARRIER [QUIET_S=30] [WAIT_S=900]
#             CARRIER is lan | public | srflx (direct tiers) or relay:<kind>
#             (relay:turn, relay:derp). Exit 0 when it holds, 1 at WAIT_S.
#   a-cell  LABEL B_OIP [PROBE_S=60] [SAMPLE_S=200]
#   b-start LABEL A_LAN_IP A_OIP
#   b-stop  LABEL A_LAN_IP
#   log     off | silent | restarting
#             The RUST_LOG drop-in. `silent` is B's (overlay::wg and boringtun
#             at DEBUG, as #3528's lab ran); `restarting` puts all of
#             tunnel_core at DEBUG, to name the frame the restarted side sends
#             over /derp (spec §2.7). It applies at the daemon's next restart:
#             for A that is the cell's own restart.
#
# DRIVER SIDE (from the lab driver, which reaches both guests)
#
#   run     RUNS TAG [CARRIER=lan]
#   summary TAG
#
#   The driver needs, in its environment:
#     FR139_A, FR139_B   a command that runs ONE shell command line on A / B,
#                        e.g. a wrapper over ssh that refuses the wrong guest
#     FR139_A_LAN        A's LAN address (B's capture filter)
#     FR139_A_OIP        A's overlay address
#     FR139_B_OIP        B's overlay address
#     FR139_LOGS         a local directory for the logs (created)
#     FR139_GUEST_SCRIPT this file's path on the guests
#                        (default /var/tmp/fr139/fr139-restart-cell.sh)
#     FR139_PROBE_S, FR139_SAMPLE_S, FR139_QUIET_S, FR139_WAIT_S (optional)
#
# READING IT (spec §5)
#
#   RED today: first_reply about 15.7 s; B logs `demote-follow ... was=Some(Lan)`
#   and `UNREGISTERED receiver index` drops; B's row leaves `lan`.
#   GREEN: first_reply <= 1 s in 10 of 10 runs, no `UNREGISTERED receiver
#   index` and no `demote-follow` on B, both ends direct on the cell's tier at
#   + 60 s, and no relayed-instead holdoff booked.
#
set -u

D=/var/tmp/fr139
DROPIN=/etc/systemd/system/roomlerd.service.d/fr139-log.conf

mono() { cut -d' ' -f1 /proc/uptime; }
wall() { date -u +%FT%T.%3NZ; }
ev() { printf 'EV %s %s %s\n' "$(mono)" "$(wall)" "$*"; }
sub() { awk -v a="$1" -v b="$2" 'BEGIN{printf "%.2f", a-b}'; }
older() { awk -v a="$1" -v b="$2" -v t="$3" 'BEGIN{print (a-b>=t)?1:0}'; }

# This guest's `roomler peers` row for the peer at overlay address $1, reduced to
# what the cell reads. `{}` when the peer is not listed.
row() {
  roomler peers --json 2>/dev/null | jq -c --arg ip "$1" '
    [.. | objects | select((.overlay_ip? // "") == $ip)] | first // {}
    | {connection, relay_kind, stalled, upgrading, rtt_ms,
       tier: (.debug.tier // null), hs_done: (.debug.hs_done // null),
       rx_age_s: (.debug.last_rx_age_s // null),
       holdoff_s: (.why.relayed_instead_s // null),
       strikes: (.why.relayed_instead_strikes // null)}' 2>/dev/null || echo '{}'
}

# Does row $1 read as carrier $2?
carrier_ok() {
  local r="$1" c="$2" kind=""
  case "$c" in relay:*) kind="${c#relay:}" ;; esac
  if [ -n "$kind" ]; then
    printf '%s' "$r" | jq -e --arg k "$kind" '.connection == "relay" and .relay_kind == $k
      and .hs_done == true and (.stalled | not) and (.upgrading | not)' >/dev/null 2>&1
  else
    printf '%s' "$r" | jq -e --arg t "$c" '.connection == "direct" and .tier == $t
      and .hs_done == true and (.stalled | not) and (.upgrading | not)' >/dev/null 2>&1
  fi
}

cmd_pre() {
  local peer="$1" c="$2" quiet="${3:-30}" wait_s="${4:-900}" t0 since="" r now
  t0="$(mono)"
  while :; do
    r="$(row "$peer")"; now="$(mono)"
    if carrier_ok "$r" "$c"; then
      [ -n "$since" ] || since="$now"
      if [ "$(older "$now" "$since" "$quiet")" = 1 ]; then
        echo "PRE ok host=$(hostname) carrier=$c waited_s=$(sub "$now" "$t0") row=$r"
        return 0
      fi
    else
      since=""
    fi
    if [ "$(older "$now" "$t0" "$wait_s")" = 1 ]; then
      echo "PRE TIMEOUT host=$(hostname) carrier=$c after_s=$wait_s row=$r"
      return 1
    fi
    sleep 2
  done
}

cmd_a_cell() {
  local L="$1" peer="$2" probe="${3:-60}" sample="${4:-200}"
  local cur p0 p1 t0 first="" n=0 tp pf pp smp spid end dur
  mkdir -p "$D"
  cur="$(journalctl -u roomlerd -n 0 --show-cursor --no-pager 2>/dev/null | sed -n 's/^-- cursor: //p')"
  p0="$(pgrep -x roomlerd | head -1)"
  ev start label="$L" peer="$peer" host="$(hostname)" pid="$p0" \
     version="$(roomlerd --version 2>/dev/null | head -1 | tr ' ' '_')" \
     synced="$(timedatectl show -p NTPSynchronized --value 2>/dev/null)"
  ev row_before "$(row "$peer")"
  roomler restart --reason "fr139 $L" > "$D/$L.restart.out" 2>&1
  ev restart_returned rc=$?
  p1="$p0"
  for _ in $(seq 1 480); do
    p1="$(pgrep -x roomlerd | head -1)"
    if [ -n "$p1" ] && [ "$p1" != "$p0" ] &&
       roomler status --json 2>/dev/null | jq -e '.overlay_ip // empty' >/dev/null 2>&1; then
      break
    fi
    sleep 0.25
  done
  ev daemon_up pid="$p0->$p1" rust_log="$(tr '\0' '\n' < "/proc/$p1/environ" 2>/dev/null | sed -n 's/^RUST_LOG=//p')"
  # The probe: one echo at a time, every 0.2 s, until the first reply.
  t0="$(mono)"
  while :; do
    n=$((n + 1))
    if ping -n -c1 -W1 "$peer" >/dev/null 2>&1; then first="$(mono)"; break; fi
    [ "$(older "$(mono)" "$t0" "$probe")" = 1 ] && break
    sleep 0.2
  done
  if [ -n "$first" ]; then
    ev first_reply attempts="$n" after_s="$(sub "$first" "$t0")"
  else
    ev NO_REPLY attempts="$n" within_s="$probe"
  fi
  # The sample: a 0.5 s ping, and A's own row for B every 2 s, from_s on the
  # same clock as after_s.
  pf="$D/$L.ping.txt"; smp="$D/$L.a-smp.txt"
  tp="$(mono)"
  ping -n -O -i 0.5 -W 1 "$peer" > "$pf" 2>&1 &
  pp=$!
  ( while :; do printf 'S %s %s %s\n' "$(sub "$(mono)" "$t0")" "$(wall)" "$(row "$peer")"; sleep 2; done ) > "$smp" &
  spid=$!
  ev ping_start from_s="$(sub "$tp" "$t0")"
  end="$(awk -v a="$(mono)" -v s="$sample" 'BEGIN{print a+s}')"
  while [ "$(awk -v a="$(mono)" -v e="$end" 'BEGIN{print (a<e)?1:0}')" = 1 ]; do sleep 1; done
  kill -INT "$pp" 2>/dev/null; wait "$pp" 2>/dev/null
  kill "$spid" 2>/dev/null; wait "$spid" 2>/dev/null
  dur="$(sub "$(mono)" "$tp")"
  ev ping_stop lines="$(wc -l < "$pf")" ran_s="$dur"
  # A gap is a stretch with no reply ARRIVING: a ping queued during a split is
  # flushed and answered once a session works again, so counting by sequence
  # number would call it answered.
  awk '/icmp_seq=/ && /time=/ { s=$0; sub(/.*icmp_seq=/, "", s); sub(/[^0-9].*$/, "", s)
                                t=$0; sub(/.*time=/, "", t); sub(/ ms.*$/, "", t)
                                printf "%.3f %d %s\n", (s-1)*0.5 + t/1000, s, t }' "$pf" | sort -n |
  awk -v t0="$(sub "$tp" "$t0")" -v dur="$dur" '
    { if (NR>1 && $1-p >= 1.5) printf "EV - - gap from_s=%.1f len_s=%.1f\n", t0+p, $1-p
      if (NR==1 && $1 >= 1.5) printf "EV - - gap from_s=%.1f len_s=%.1f\n", t0, $1
      p=$1; n++; if ($3+0 > 1000) late++; if ($2>max) max=$2 }
    END { if (n==0) p=0
          if (dur-p >= 1.5) printf "EV - - gap from_s=%.1f len_s=%.1f trailing\n", t0+p, dur-p
          printf "EV - - ping_summary replies=%d late_over_1s=%d last_seq=%d interval_s=0.5\n", n, late+0, max }'
  ev end
  echo "## A rows for B (S from_s wall row)"
  cat "$smp"
  echo "## A journal since the cell began"
  journalctl -u roomlerd --after-cursor="$cur" --no-pager -o short-iso-precise 2>/dev/null | sed 's/\x1b\[[0-9;]*m//g'
}

cmd_b_start() {
  local L="$1" alan="$2" aoip="$3"
  mkdir -p "$D"
  journalctl -u roomlerd -n 0 --show-cursor --no-pager 2>/dev/null | sed -n 's/^-- cursor: //p' > "$D/$L.cursor"
  systemd-run --unit="fr139-cap-$L" --collect sh -c \
    "exec tcpdump -n -tt -l -i any 'udp and host $alan' > $D/$L.cap.txt 2> $D/$L.cap.err" >/dev/null 2>&1
  systemd-run --unit="fr139-smp-$L" --collect bash "$0" b-sampler "$L" "$aoip" >/dev/null 2>&1
  sleep 1
  echo "B mark $L at $(wall) mono=$(mono) capture=$(systemctl is-active "fr139-cap-$L") sampler=$(systemctl is-active "fr139-smp-$L")"
  echo "B row_before $(row "$aoip")"
}

cmd_b_sampler() {
  local L="$1" aoip="$2"
  while :; do
    printf 'S %s %s %s\n' "$(date -u +%s.%3N)" "$(wall)" "$(row "$aoip")" >> "$D/$L.b-smp.txt"
    sleep 2
  done
}

cmd_b_stop() {
  local L="$1" alan="$2" j
  systemctl stop "fr139-smp-$L" "fr139-cap-$L" 2>/dev/null
  j="$D/$L.b-journal.txt"
  journalctl -u roomlerd --after-cursor="$(cat "$D/$L.cursor")" --no-pager -o short-iso-precise 2>/dev/null \
    | sed 's/\x1b\[[0-9;]*m//g' > "$j"
  echo "## B counts $L"
  echo "B count demote_follow=$(grep -c 'demote-follow' "$j")" \
       "unregistered=$(grep -c 'UNREGISTERED receiver index' "$j")" \
       "follow_verdict=$(grep -c 'follow verdict' "$j")" \
       "derp_link_ready=$(grep -c 'DERP link ready' "$j")" \
       "mbb_probe=$(grep -c 'make-before-break' "$j")" \
       "promoted=$(grep -c 'direct carrier promoted' "$j")" \
       "rekey_timeout=$(grep -c 'REKEY_TIMEOUT' "$j")"
  echo "## B capture $L (epoch dir len kind): 148 init, 92 response, 64 cookie, 32 keepalive"
  awk -v aip="$alan" '{
      ts=$1; len=$NF; dir="?"
      for (i=1;i<=NF;i++) if ($i==">") { src=$(i-1); dst=$(i+1) }
      if (index(src, aip".")==1) dir="A->B"; else if (index(dst, aip".")==1) dir="B->A"
      kind="data"; if (len==148) kind="init"; else if (len==92) kind="resp"; else if (len==64) kind="cookie"; else if (len==32) kind="keepalive"
      printf "%s %s %s %s\n", ts, dir, len, kind }' "$D/$L.cap.txt"
  echo "## B rows for A (S epoch wall row)"
  cat "$D/$L.b-smp.txt" 2>/dev/null
  echo "## B journal since mark $L"
  cat "$j"
}

cmd_log() {
  case "$1" in
    off) rm -f "$DROPIN" ;;
    silent|restarting)
      local spec="roomlerd=info,tunnel_core=info,tunnel_core::overlay::wg=debug,boringtun=debug,warn"
      [ "$1" = restarting ] && spec="roomlerd=info,tunnel_core=debug,boringtun=debug,warn"
      mkdir -p "$(dirname "$DROPIN")"
      printf '[Service]\nEnvironment=RUST_LOG=%s\n' "$spec" > "$DROPIN" ;;
    *) echo "log: off | silent | restarting" >&2; return 2 ;;
  esac
  systemctl daemon-reload
  echo "log $1 on $(hostname): $(cat "$DROPIN" 2>/dev/null | tr '\n' ' ')"
}

# ---- driver side -------------------------------------------------------------

need() { for v in "$@"; do [ -n "${!v:-}" ] || { echo "set $v" >&2; exit 2; }; done; }

cmd_run() {
  local runs="$1" tag="$2" c="${3:-lan}" gs i L
  need FR139_A FR139_B FR139_A_LAN FR139_A_OIP FR139_B_OIP FR139_LOGS
  gs="${FR139_GUEST_SCRIPT:-$D/fr139-restart-cell.sh}"
  mkdir -p "$FR139_LOGS"
  echo "fr139 run $tag: $runs runs, carrier $c, start $(wall)"
  for i in $(seq 1 "$runs"); do
    L="$tag-$i"
    [ -e "$FR139_LOGS/$L.a.txt" ] && { echo "refusing: $FR139_LOGS/$L.a.txt exists"; exit 3; }
    $FR139_B "sudo bash $gs pre $FR139_A_OIP $c ${FR139_QUIET_S:-30} ${FR139_WAIT_S:-900}" > "$FR139_LOGS/$L.pre.txt" 2>&1 \
      && $FR139_A "sudo bash $gs pre $FR139_B_OIP $c 0 ${FR139_WAIT_S:-900}" >> "$FR139_LOGS/$L.pre.txt" 2>&1
    if ! grep -q '^PRE ok' "$FR139_LOGS/$L.pre.txt" || grep -q '^PRE TIMEOUT' "$FR139_LOGS/$L.pre.txt"; then
      echo "$L: the precondition did not hold; stopping"; cat "$FR139_LOGS/$L.pre.txt"; exit 4
    fi
    $FR139_B "sudo bash $gs b-start $L $FR139_A_LAN $FR139_A_OIP" > "$FR139_LOGS/$L.bstart.txt" 2>&1
    $FR139_A "sudo bash $gs a-cell $L $FR139_B_OIP ${FR139_PROBE_S:-60} ${FR139_SAMPLE_S:-200}" > "$FR139_LOGS/$L.a.txt" 2>&1
    sleep 3
    $FR139_B "sudo bash $gs b-stop $L $FR139_A_LAN" > "$FR139_LOGS/$L.b.txt" 2>&1
    cmd_summary_one "$L"
  done
  echo "fr139 run $tag end $(wall)"
}

# The row nearest to time T in an `S <t> <wall> <row>` section of file F, between
# the header lines H1 and H2.
nearest() {
  sed -n "/^## $3/,/^## $4/p" "$1" | grep '^S ' |
    awk -v t="$2" '{ d=$2-t; if (d<0) d=-d; if (b=="" || d<b) { b=d; r=$0 } }
                   END { sub(/^S [^ ]* [^ ]* /, "", r); print r }'
}

# One line per run. The +60 s and +180 s rows are the samples nearest to
# daemon_up + 60 / + 180 s: A's by its own clock (from_s), B's by wall time.
cmd_summary_one() {
  local L="$1" a="$FR139_LOGS/$1.a.txt" b="$FR139_LOGS/$1.b.txt" up upe fr gaps cnt
  up="$(grep -m1 ' daemon_up ' "$a" | awk '{print $3}')"
  upe="$(date -u -d "$up" +%s.%3N 2>/dev/null || echo 0)"
  fr="$(grep -m1 -E ' (first_reply|NO_REPLY) ' "$a" | sed 's/^EV [^ ]* [^ ]* //')"
  gaps="$(grep -E '^EV - - (gap|ping_summary)' "$a" | sed 's/^EV - - //' | tr '\n' ';')"
  cnt="$(grep -m1 '^B count ' "$b" | sed 's/^B count //')"
  echo "RUN $L up=$up $fr | $gaps | B: $cnt" \
       "| +60 A=$(nearest "$a" 60 'A rows for B' 'A journal') B=$(nearest "$b" "$(awk -v t="$upe" 'BEGIN{printf "%.3f", t+60}')" 'B rows for A' 'B journal')" \
       "| +180 A=$(nearest "$a" 180 'A rows for B' 'A journal') B=$(nearest "$b" "$(awk -v t="$upe" 'BEGIN{printf "%.3f", t+180}')" 'B rows for A' 'B journal')"
}

cmd_summary() {
  local tag="$1" f
  need FR139_LOGS
  for f in "$FR139_LOGS/$tag"-*.a.txt; do
    [ -e "$f" ] || continue
    cmd_summary_one "$(basename "$f" .a.txt)"
  done
}

case "${1:-}" in
  pre) shift; cmd_pre "$@" ;;
  a-cell) shift; cmd_a_cell "$@" ;;
  b-start) shift; cmd_b_start "$@" ;;
  b-sampler) shift; cmd_b_sampler "$@" ;;
  b-stop) shift; cmd_b_stop "$@" ;;
  log) shift; cmd_log "$@" ;;
  run) shift; cmd_run "$@" ;;
  summary) shift; cmd_summary "$@" ;;
  *) sed -n '2,/^set -u/p' "$0" | sed 's/^# \{0,1\}//'; exit 2 ;;
esac
