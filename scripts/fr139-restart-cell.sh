#!/usr/bin/env bash
#
# fr139-restart-cell.sh — FR-139 P0: the restart cell (spec §5), the read that
# every RED and GREEN of FR-139 uses, and the promotion cell that P3 reads
# against.
#
# FR-139 (docs/fr/FR-139-carrier-transitions-make-before-break.md) stops a
# daemon restart from splitting a pair that holds each other on a direct
# carrier. Two lab guests run the overlay: A is the end that restarts, B is the
# silent end, which has nothing of its own to send. One run of the cell:
#
#   1. pre    on both ends: each reads the other on the cell's carrier (direct
#             `lan` for the LAN cell), handshaken, not stalled and not
#             upgrading, continuously for QUIET seconds. A run starts only then.
#   2. b-start on B: a journal mark, a log-file mark, a capture of every UDP
#             datagram to and from A's LAN address, and a 2 s sampler of B's
#             `roomler peers` row for A.
#   3. a-cell on A: `roomler restart`, then wait for a NEW daemon pid that holds
#             an overlay address (`daemon_up`). Ping B's overlay address every
#             0.2 s; `first_reply` is daemon_up -> the first echo reply, on A's
#             monotonic clock. Then a 0.5 s ping for SAMPLE seconds, cut into
#             gaps by when each reply ARRIVED, beside a 2 s sampler of A's own
#             `roomler peers` row for B.
#   4. b-stop  on B: the capture classified by WireGuard message length, B's
#             log since the mark, the sampler, and the counts the spec reads
#             (`demote-follow`, `UNREGISTERED receiver index`, `follow verdict`)
#             for A only: A's node id (B's `roomler peers` row, read at b-start)
#             or a datagram from A's LAN address. A lab org can hold other
#             guests on the same LAN, and a follow of one of them is counted
#             apart (`other_follow`).
#
# The promotion cell (spec §2.6 and §3.8, for P3's drain) holds the pair on a
# relay with A's UDP blocked, lifts the block, and times each end's echo gap (a
# 0.1 s ping both ways, by receive timestamp) around the make-before-break
# promotion back to the direct carrier. The end that promotes unregisters the
# relay session at once, so what the far end still sends on the relay is lost
# until it moves too.
#
# `summary` and `psummary` reduce each run to one line.
#
# HARD GUARD. Every guest command restarts the daemon, writes systemd drop-ins,
# iptables rules or captures, so each one refuses to run anywhere but a lab
# guest of this rig: hostname vmtest-fr139-<label>-<HHMMSS>, a virtual machine,
# as root. On a guest it changes nothing but that guest: its daemon's restart,
# two drop-ins under roomlerd.service.d, iptables rules tagged fr139, transient
# fr139-* units and files under /var/tmp/fr139.
#
# GUEST SIDE (as root; the driver copies this file to both guests first)
#
#   prep    The rig's baseline, once per guest before any cell: a drop-in with
#             ROOMLERD_AUTO_UPDATE=0 and LogRateLimitIntervalSec=0 (journald
#             keeps every line at DEBUG), then a clean restart. Prints what the
#             new daemon runs with, and where its log file is.
#   pre     PEER_OIP CARRIER [QUIET_S=30] [WAIT_S=900]
#             CARRIER is lan | public | srflx (direct tiers), relay:<kind>
#             (relay:turn, relay:derp) or relay:any. Exit 0 when it holds, 1 at
#             WAIT_S.
#   a-cell  LABEL B_OIP [PROBE_S=60] [SAMPLE_S=200]
#   b-start LABEL A_LAN_IP A_OIP
#   b-stop  LABEL A_LAN_IP
#   log     off | silent | restarting
#             The RUST_LOG drop-in. `silent` is B's (overlay::wg, the establish
#             loop and boringtun at DEBUG, as #3528's lab ran); `restarting`
#             puts all of tunnel_core at DEBUG, to name the frame the restarted
#             side sends over /derp (spec §2.7). It applies at the daemon's next
#             restart: for A that is the cell's own restart.
#   block   on PEER_LAN | off | show
#             on: drop UDP from PEER_LAN, and every UDP egress but DNS, NTP,
#             loopback, link-local, multicast and the overlay's own range, so
#             neither a direct tier nor TURN over UDP can carry the pair and it
#             rides a relay over TCP. off: remove exactly the fr139 rules.
#   pstart  LABEL PEER_OIP [PEER_LAN]   a 0.1 s ping with receive timestamps,
#             in a unit; the peer's node id and LAN address for the counts
#   pstop   LABEL            its gaps by arrival, and the daemon's carrier lines
#             about that peer
#
# DRIVER SIDE (from the lab driver, which reaches both guests)
#
#   run      RUNS TAG [CARRIER=lan]   the restart cell, RUNS times
#   promo    RUNS TAG                 the promotion cell, RUNS times
#   summary  TAG                      one RUN line per restart cell
#   psummary TAG                      one PROMO line per promotion cell
#
#   The driver needs, in its environment:
#     FR139_A, FR139_B   a command that runs ONE shell command line on A / B,
#                        e.g. a wrapper over ssh that refuses the wrong guest
#     FR139_A_LAN        A's LAN address (B's capture and counts filter)
#     FR139_B_LAN        B's LAN address (A's block and counts filter, `promo`)
#     FR139_A_OIP        A's overlay address
#     FR139_B_OIP        B's overlay address
#     FR139_LOGS         a local directory for the logs (created)
#     FR139_GUEST_SCRIPT this file's path on the guests
#                        (default /var/tmp/fr139/fr139-restart-cell.sh)
#     FR139_PROBE_S, FR139_SAMPLE_S, FR139_QUIET_S, FR139_WAIT_S (optional)
#     FR139_UNTIL        optional, epoch seconds: a batch stops (exit 5) before
#                        a run that would start past it, so a lab with a hard
#                        stop is never torn down under a run in flight
#   `summary` and `psummary` need GNU date.
#
# READING IT (spec §5)
#
#   RED today: first_reply about 15.7 s; B logs `demote-follow ... was=Some(Lan)`
#   and `UNREGISTERED receiver index` drops; B's row leaves `lan`.
#   GREEN: first_reply <= 1 s in 10 of 10 runs, no `UNREGISTERED receiver
#   index` and no `demote-follow` on B, both ends direct on the cell's tier at
#   + 60 s, and no relayed-instead holdoff booked.
#   PROMO: each end's gaps, in seconds from the lift (start:length), beside
#   each end's carrier events (P promoted, F demote-follow, G a /derp frame
#   inside the promote grace, U an unregistered-index drop) on the same clock.
#
set -u

D=/var/tmp/fr139
UNITD=/etc/systemd/system/roomlerd.service.d
DROPIN=$UNITD/fr139-log.conf
BASE=$UNITD/fr139-base.conf
LOGDIR=/root/.local/share/roomler/logs
TAGC=fr139

mono() { cut -d' ' -f1 /proc/uptime; }
wall() { date -u +%FT%T.%3NZ; }
epoch() { date -u +%s.%3N; }
ev() { printf 'EV %s %s %s\n' "$(mono)" "$(wall)" "$*"; }
sub() { awk -v a="$1" -v b="$2" 'BEGIN{printf "%.2f", a-b}'; }
older() { awk -v a="$1" -v b="$2" -v t="$3" 'BEGIN{print (a-b>=t)?1:0}'; }
nocolor() { sed 's/\x1b\[[0-9;]*m//g'; }

# The hard guard, run before every guest command (see the header).
guard() {
  local h
  h="$(hostname)"
  if ! [[ "$h" =~ ^vmtest-fr139-[a-z0-9]+-[0-9]{6}$ ]]; then
    echo "HOST GUARD: fr139 guest commands run only on a vmtest-fr139-<label>-<HHMMSS> lab guest; this is '$h'" >&2
    exit 97
  fi
  case "$(systemd-detect-virt --vm 2>/dev/null)" in
    "" | none) echo "HOST GUARD: $h is not a virtual machine" >&2; exit 97 ;;
  esac
  [ "$(id -u)" = 0 ] || { echo "fr139: run guest commands as root" >&2; exit 2; }
}

# The daemon is the unit's main process. `roomler` itself re-execs `roomlerd
# cli`, so a process-name match can catch the CLI instead.
mainpid() { systemctl show -p MainPID --value roomlerd 2>/dev/null; }
envof() { tr '\0' '\n' < "/proc/$1/environ" 2>/dev/null | sed -n "s/^$2=//p"; }

# Wait for a daemon pid other than $1 whose LocalAPI answers with an overlay
# address. Prints the pid; exit 1 after 120 s.
wait_up() {
  local p0="$1" p1="" _
  for _ in $(seq 1 480); do
    p1="$(mainpid)"
    if [ -n "$p1" ] && [ "$p1" != 0 ] && [ "$p1" != "$p0" ] &&
       roomler status --json 2>/dev/null | jq -e '.overlay_ip // empty' >/dev/null 2>&1; then
      echo "$p1"
      return 0
    fi
    sleep 0.25
  done
  echo "$p1"
  return 1
}

# The daemon's rolling log file has every line the filter passes, without ANSI
# colour and without journald's rate limit. `logpos` prints "<file> <size>" of
# the newest one (nothing when there is none); `logwin <file> <size>` prints
# what was written since, including a newer file (the appender rolls at
# midnight UTC).
logpos() {
  local f
  f="$(ls -1t "$LOGDIR"/roomlerd.log.* 2>/dev/null | head -1)"
  [ -n "$f" ] && printf '%s %s\n' "$f" "$(stat -c %s "$f")"
}
logwin() {
  local f="${1:-}" off="${2:-0}" n
  [ -n "$f" ] && [ -e "$f" ] || return 0
  tail -c +"$((off + 1))" "$f"
  for n in $(ls -1tr "$LOGDIR"/roomlerd.log.* 2>/dev/null); do
    [ "$n" != "$f" ] && [ "$n" -nt "$f" ] && cat "$n"
  done
}

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

# Does row $1 read as carrier $2? relay:any accepts a probe in flight, because
# with the block on every direct probe is one.
carrier_ok() {
  local r="$1" c="$2" kind=""
  case "$c" in relay:*) kind="${c#relay:}" ;; esac
  if [ "$kind" = any ]; then
    printf '%s' "$r" | jq -e '.connection == "relay" and .hs_done == true and (.stalled | not)' >/dev/null 2>&1
  elif [ -n "$kind" ]; then
    printf '%s' "$r" | jq -e --arg k "$kind" '.connection == "relay" and .relay_kind == $k
      and .hs_done == true and (.stalled | not) and (.upgrading | not)' >/dev/null 2>&1
  else
    printf '%s' "$r" | jq -e --arg t "$c" '.connection == "direct" and .tier == $t
      and .hs_done == true and (.stalled | not) and (.upgrading | not)' >/dev/null 2>&1
  fi
}

# This guest's overlay node id for the peer at overlay address $1 (the `peer=`
# its log lines carry); empty when the peer is not listed.
rowid() {
  roomler peers --json 2>/dev/null | jq -r --arg ip "$1" '
    [.. | objects | select((.overlay_ip? // "") == $ip)] | first | .node_id // empty' 2>/dev/null
}

# The lines of log window $1 about ONE peer: its node id $2, or a datagram from
# its LAN address $3 (an UNREGISTERED drop names the source, not the peer). A
# lab org can hold other guests on the same LAN, and their churn must not count.
peerlines() {
  awk -v id="peer=$2" -v src="src=$3:" '
    index($0, id " ") || substr($0, length($0) - length(id) + 1) == id || index($0, src)' "$1"
}

# The counts the spec reads, from one log window, for the peer (node id $2, LAN
# address $3). `other_follow` counts follows of any OTHER peer in the window.
# boringtun's REKEY_TIMEOUT names no peer, so it counts every peer.
counts() {
  local f="$1" id="${2:-}" lan="${3:-}" sel all
  if [ -z "$id" ]; then
    echo "peer=UNKNOWN (counts cover every peer)"
    sel="$(cat "$f")"
  else
    sel="$(peerlines "$f" "$id" "$lan")"
  fi
  all="$(grep -c 'onto DERP now (demote-follow)' "$f")"
  printf '%s\n' "$sel" | awk -v id="${id:-?}" -v all="$all" -v rk="$(grep -c 'REKEY_TIMEOUT' "$f")" '
    /onto DERP now \(demote-follow\)/ {df++}
    /demote-follow declined/ {dd++}
    /within the promote grace/ {ig++}
    /UNREGISTERED receiver index/ {ur++}
    /follow verdict/ {fv++}
    /DERP link ready/ {dl++}
    /direct carrier promoted/ {pr++}
    END { printf "peer=%s demote_follow=%d follow_declined=%d in_grace=%d unregistered=%d follow_verdict=%d derp_link_ready=%d promoted=%d other_follow=%d rekey_timeout_any=%d\n",
            id, df, dd, ig, ur, fv, dl, pr, all-df, rk }'
}

cmd_prep() {
  local p0 p1
  mkdir -p "$UNITD" "$D"
  printf '[Service]\nEnvironment=ROOMLERD_AUTO_UPDATE=0\nLogRateLimitIntervalSec=0\n' > "$BASE"
  systemctl daemon-reload
  p0="$(mainpid)"
  roomler restart --reason "fr139 prep" > "$D/prep.restart.out" 2>&1
  p1="$(wait_up "$p0")" || echo "prep: no new daemon with an overlay address within 120 s"
  echo "prep $(hostname): pid $p0->$p1" \
       "version=$(roomlerd --version 2>/dev/null | head -1 | tr ' ' '_')" \
       "auto_update_env=$(envof "$p1" ROOMLERD_AUTO_UPDATE)" \
       "rust_log=$(envof "$p1" RUST_LOG)" \
       "config_auto_update=$(sed -n 's/^[[:space:]]*auto_update[[:space:]]*=[[:space:]]*//p' /etc/roomler/config.toml 2>/dev/null | head -1)" \
       "etc_default_keys=$(sed -n 's/^\([A-Za-z_][A-Za-z0-9_]*\)=.*/\1/p' /etc/default/roomlerd 2>/dev/null | tr '\n' ',')" \
       "logfile=$(logpos | tr ' ' ':')"
}

cmd_pre() {
  local peer="$1" c="$2" quiet="${3:-30}" wait_s="${4:-900}" t0 since="" r now
  t0="$(mono)"
  while :; do
    r="$(row "$peer")"
    now="$(mono)"
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
  local L="$1" peer="$2" probe="${3:-60}" sample="${4:-200}" idle="${5:-0}"
  local cur lp p0 p1 t0 tq first="" n=0 tp tpe pf pp smp spid end dur
  mkdir -p "$D"
  cur="$(journalctl -u roomlerd -n 0 --show-cursor --no-pager 2>/dev/null | sed -n 's/^-- cursor: //p')"
  lp="$(logpos)"
  p0="$(mainpid)"
  ev start label="$L" peer="$peer" host="$(hostname)" pid="$p0" \
     version="$(roomlerd --version 2>/dev/null | head -1 | tr ' ' '_')" \
     synced="$(timedatectl show -p NTPSynchronized --value 2>/dev/null)"
  ev row_before "$(row "$peer")"
  roomler restart --reason "fr139 $L" > "$D/$L.restart.out" 2>&1
  ev restart_returned rc=$?
  p1="$(wait_up "$p0")" || ev UP_TIMEOUT
  ev daemon_up pid="$p0->$p1" rust_log="$(envof "$p1" RUST_LOG)"
  t0="$(mono)"
  # The idle variant: A sends nothing for IDLE seconds, so only the daemons'
  # own timers move the pair. The cell's echo is data, and data is what makes
  # A's WireGuard re-initiate after 15 s with nothing received.
  if [ "$idle" != 0 ]; then
    ev idle_start idle_s="$idle" "$(row "$peer")"
    sleep "$idle"
    ev idle_end from_s="$(sub "$(mono)" "$t0")" "$(row "$peer")"
  fi
  # The probe: one echo at a time, every 0.2 s, until the first reply. Its
  # after_s counts from daemon_up, so an idle run's includes the idle.
  tq="$(mono)"
  while :; do
    n=$((n + 1))
    if ping -n -c1 -W1 "$peer" >/dev/null 2>&1; then first="$(mono)"; break; fi
    [ "$(older "$(mono)" "$tq" "$probe")" = 1 ] && break
    sleep 0.2
  done
  if [ -n "$first" ]; then
    ev first_reply attempts="$n" after_s="$(sub "$first" "$t0")"
  else
    ev NO_REPLY attempts="$n" within_s="$probe"
  fi
  # The sample: a 0.5 s ping, and A's own row for B every 2 s, from_s on the
  # same clock as after_s.
  pf="$D/$L.ping.txt"
  smp="$D/$L.a-smp.txt"
  tp="$(mono)"
  tpe="$(epoch)"
  ping -D -n -O -i 0.5 -W 1 "$peer" > "$pf" 2>&1 &
  pp=$!
  ( while :; do printf 'S %s %s %s\n' "$(sub "$(mono)" "$t0")" "$(wall)" "$(row "$peer")"; sleep 2; done ) > "$smp" &
  spid=$!
  ev ping_start from_s="$(sub "$tp" "$t0")"
  end="$(awk -v a="$(mono)" -v s="$sample" 'BEGIN{print a+s}')"
  while [ "$(awk -v a="$(mono)" -v e="$end" 'BEGIN{print (a<e)?1:0}')" = 1 ]; do sleep 1; done
  kill -INT "$pp" 2>/dev/null
  wait "$pp" 2>/dev/null
  kill "$spid" 2>/dev/null
  wait "$spid" 2>/dev/null
  dur="$(sub "$(mono)" "$tp")"
  ev ping_stop lines="$(wc -l < "$pf")" ran_s="$dur"
  # A gap is a stretch with no reply ARRIVING: a ping queued during a split is
  # flushed and answered once a session works again, so counting by sequence
  # number would call it answered. Arrival is ping's own receive timestamp
  # (-D): seq x interval drifts by about 2 s over a 200 s sample.
  awk -v tpe="$tpe" '/icmp_seq=/ && /time=/ { ts=$1; gsub(/[][]/, "", ts)
                                s=$0; sub(/.*icmp_seq=/, "", s); sub(/[^0-9].*$/, "", s)
                                t=$0; sub(/.*time=/, "", t); sub(/ ms.*$/, "", t)
                                printf "%.3f %d %s\n", ts - tpe, s, t }' "$pf" | sort -n |
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
  echo "## A log file since the cell began (${lp:-none})"
  # shellcheck disable=SC2086 # "<file> <size>", two words on purpose
  logwin $lp | nocolor
  echo "## A journal since the cell began"
  journalctl -u roomlerd --after-cursor="$cur" --no-pager -o short-iso-precise 2>/dev/null | nocolor
}

cmd_b_start() {
  local L="$1" alan="$2" aoip="$3"
  mkdir -p "$D"
  journalctl -u roomlerd -n 0 --show-cursor --no-pager 2>/dev/null | sed -n 's/^-- cursor: //p' > "$D/$L.cursor"
  logpos > "$D/$L.logpos"
  systemd-run --unit="fr139-cap-$L" --collect sh -c \
    "exec tcpdump -n -tt -l -i any 'udp and host $alan' > $D/$L.cap.txt 2> $D/$L.cap.err" >/dev/null 2>&1
  systemd-run --unit="fr139-smp-$L" --collect bash "$0" b-sampler "$L" "$aoip" >/dev/null 2>&1
  rowid "$aoip" > "$D/$L.apeer"
  sleep 1
  echo "B mark $L at $(wall) mono=$(mono) capture=$(systemctl is-active "fr139-cap-$L") sampler=$(systemctl is-active "fr139-smp-$L") logfile=$(tr ' ' ':' < "$D/$L.logpos") a_peer=$(cat "$D/$L.apeer")"
  echo "B row_before $(row "$aoip")"
}

cmd_b_sampler() {
  local L="$1" aoip="$2"
  while :; do
    printf 'S %s %s %s\n' "$(epoch)" "$(wall)" "$(row "$aoip")" >> "$D/$L.b-smp.txt"
    sleep 2
  done
}

cmd_b_stop() {
  local L="$1" alan="$2" j lf src
  systemctl stop "fr139-smp-$L" "fr139-cap-$L" 2>/dev/null
  j="$D/$L.b-journal.txt"
  lf="$D/$L.b-log.txt"
  journalctl -u roomlerd --after-cursor="$(cat "$D/$L.cursor")" --no-pager -o short-iso-precise 2>/dev/null | nocolor > "$j"
  # shellcheck disable=SC2046 # "<file> <size>", two words on purpose
  logwin $(cat "$D/$L.logpos" 2>/dev/null) | nocolor > "$lf"
  if [ -s "$lf" ]; then src="$lf"; else src="$j"; fi
  echo "## B counts $L"
  echo "B count src=$(basename "$src") $(counts "$src" "$(cat "$D/$L.apeer" 2>/dev/null)" "$alan")"
  echo "## B capture $L (epoch dir len kind): 148 init, 92 response, 64 cookie, 32 keepalive"
  awk -v aip="$alan" '{
      ts=$1; len=$NF; dir="?"
      for (i=1;i<=NF;i++) if ($i==">") { src=$(i-1); dst=$(i+1) }
      if (index(src, aip".")==1) dir="A->B"; else if (index(dst, aip".")==1) dir="B->A"
      kind="data"; if (len==148) kind="init"; else if (len==92) kind="resp"; else if (len==64) kind="cookie"; else if (len==32) kind="keepalive"
      printf "%s %s %s %s\n", ts, dir, len, kind }' "$D/$L.cap.txt"
  echo "## B rows for A (S epoch wall row)"
  cat "$D/$L.b-smp.txt" 2>/dev/null
  echo "## B log since mark $L ($(basename "$src"))"
  cat "$src"
}

cmd_log() {
  local spec=""
  case "${1:-}" in
    off) rm -f "$DROPIN" ;;
    silent) spec="roomlerd=info,tunnel_core=info,tunnel_core::overlay::wg=debug,tunnel_core::overlay::runtime::establish=debug,boringtun=debug,warn" ;;
    restarting) spec="roomlerd=info,tunnel_core=debug,boringtun=debug,warn" ;;
    *) echo "log: off | silent | restarting" >&2; return 2 ;;
  esac
  if [ -n "$spec" ]; then
    mkdir -p "$UNITD"
    printf '[Service]\nEnvironment=RUST_LOG=%s\n' "$spec" > "$DROPIN"
  fi
  systemctl daemon-reload
  # The braces carry the redirect's own error too: after `off` the drop-in is gone.
  echo "log $1 on $(hostname): $( { tr '\n' ' ' < "$DROPIN"; } 2>/dev/null)"
}

# Every fr139 rule, out of INPUT and OUTPUT, v4 and v6.
block_del() {
  local T c n k
  for T in iptables ip6tables; do
    for c in INPUT OUTPUT; do
      for k in $(seq 1 64); do
        n="$($T -L "$c" -n --line-numbers 2>/dev/null | awk -v t="/* $TAGC */" 'index($0, t) {print $1; exit}')"
        [ -n "$n" ] || break
        $T -D "$c" "$n" || break
      done
    done
  done
}
block_cnt() { "$1" -S 2>/dev/null | grep -c -- "--comment $TAGC"; }

cmd_block() {
  local i r
  case "${1:-show}" in
    on)
      [ -n "${2:-}" ] || { echo "block on PEER_LAN" >&2; return 2; }
      block_del
      i=1
      for r in "--dport 53" "--dport 123" "-d 127.0.0.0/8" "-d 100.64.0.0/10" "-d 169.254.0.0/16" "-d 224.0.0.0/4" "-d 255.255.255.255/32"; do
        # shellcheck disable=SC2086 # a match, several words on purpose
        iptables -I OUTPUT "$i" -p udp $r -m comment --comment "$TAGC" -j ACCEPT
        i=$((i + 1))
      done
      iptables -I OUTPUT "$i" -p udp -m comment --comment "$TAGC" -j DROP
      iptables -I INPUT 1 -p udp -s "$2" -m comment --comment "$TAGC" -j DROP
      i=1
      for r in "--dport 53" "--dport 123" "-d ::1/128" "-d fe80::/10" "-d ff00::/8"; do
        # shellcheck disable=SC2086
        ip6tables -I OUTPUT "$i" -p udp $r -m comment --comment "$TAGC" -j ACCEPT
        i=$((i + 1))
      done
      ip6tables -I OUTPUT "$i" -p udp -m comment --comment "$TAGC" -j DROP
      echo "BLOCK on $(hostname) peer_lan=$2 at $(wall) epoch=$(epoch): v4 $(block_cnt iptables) rules, v6 $(block_cnt ip6tables) rules" ;;
    off)
      block_del
      echo "LIFT on $(hostname) at $(wall) epoch=$(epoch): v4 $(block_cnt iptables) left, v6 $(block_cnt ip6tables) left" ;;
    show)
      iptables -S
      ip6tables -S ;;
    *) echo "block: on PEER_LAN | off | show" >&2; return 2 ;;
  esac
}

cmd_pstart() {
  local L="$1" peer="$2" plan="${3:-}"
  mkdir -p "$D"
  epoch > "$D/$L.pstart"
  printf '%s %s\n' "$(rowid "$peer")" "$plan" > "$D/$L.ppeer"
  logpos > "$D/$L.plogpos"
  journalctl -u roomlerd -n 0 --show-cursor --no-pager 2>/dev/null | sed -n 's/^-- cursor: //p' > "$D/$L.pcursor"
  systemd-run --unit="fr139-pp-$L" --collect sh -c \
    "exec ping -D -n -O -i 0.1 -W 1 $peer > $D/$L.pping.txt 2>&1" >/dev/null 2>&1
  sleep 1
  echo "P start $L on $(hostname) at $(wall) peer=$peer peer_node=$(cut -d' ' -f1 "$D/$L.ppeer") pinger=$(systemctl is-active "fr139-pp-$L") row=$(row "$peer")"
}

cmd_pstop() {
  local L="$1" t0 t1 w j pid plan
  systemctl stop "fr139-pp-$L" 2>/dev/null
  read -r pid plan < "$D/$L.ppeer"
  t0="$(cat "$D/$L.pstart")"
  t1="$(epoch)"
  w="$D/$L.p-log.txt"
  # shellcheck disable=SC2046 # "<file> <size>", two words on purpose
  logwin $(cat "$D/$L.plogpos" 2>/dev/null) | nocolor > "$w"
  if ! [ -s "$w" ]; then
    j="$(cat "$D/$L.pcursor" 2>/dev/null)"
    journalctl -u roomlerd --after-cursor="$j" --no-pager -o short-iso-precise 2>/dev/null | nocolor > "$w"
  fi
  echo "## ping gaps $L on $(hostname) (epoch, len_s): no reply arriving for >= 0.35 s, at a 0.1 s interval"
  # The ping's start lags pstart's stamp by up to a second, so a leading gap
  # counts from 1 s.
  awk -v th=0.35 -v t0="$t0" -v t1="$t1" '
    /icmp_seq=/ && /time=/ { ts=$1; sub(/^\[/, "", ts); sub(/\].*$/, "", ts); ts+=0
      if (n==0 && ts-t0 >= 1.0) printf "GAP start=%.3f len_s=%.3f leading\n", t0, ts-t0
      if (n>0 && ts-p >= th) printf "GAP start=%.3f len_s=%.3f\n", p, ts-p
      p=ts; n++ }
    END { if (n==0) p=t0
          if (t1-p >= th) printf "GAP start=%.3f len_s=%.3f trailing\n", p, t1-p
          printf "PSUM replies=%d ran_s=%.1f\n", n, t1-t0 }' "$D/$L.pping.txt"
  echo "## B-style counts $L on $(hostname)"
  echo "P count $(counts "$w" "${pid:-}" "${plan:-}")"
  echo "## carrier lines $L (the peer's only: node ${pid:-?}, LAN ${plan:-?})"
  peerlines "$w" "${pid:-NONE}" "${plan:-NONE}" |
    grep -E 'direct carrier promoted|onto DERP now \(demote-follow\)|demote-follow declined|within the promote grace|UNREGISTERED receiver index|DERP link ready|pathmon|relay link|stamp|make-before-break' | head -400
  echo "## log window $L"
  cat "$w"
}

# ---- driver side -------------------------------------------------------------

need() { for v in "$@"; do [ -n "${!v:-}" ] || { echo "set $v" >&2; exit 2; }; done; }

# Exit 5 when FR139_UNTIL is set and has passed, before run $1 starts.
until_ok() {
  [ -n "${FR139_UNTIL:-}" ] || return 0
  [ "$(date -u +%s)" -lt "$FR139_UNTIL" ] && return 0
  echo "$1: past FR139_UNTIL ($(date -u -d "@$FR139_UNTIL" +%FT%TZ)); stopping before this run"
  exit 5
}

cmd_run() {
  local runs="$1" tag="$2" c="${3:-lan}" gs i L
  need FR139_A FR139_B FR139_A_LAN FR139_A_OIP FR139_B_OIP FR139_LOGS
  gs="${FR139_GUEST_SCRIPT:-$D/fr139-restart-cell.sh}"
  mkdir -p "$FR139_LOGS"
  echo "fr139 run $tag: $runs runs, carrier $c, start $(wall)"
  for i in $(seq 1 "$runs"); do
    L="$tag-$i"
    [ -e "$FR139_LOGS/$L.a.txt" ] && { echo "refusing: $FR139_LOGS/$L.a.txt exists"; exit 3; }
    until_ok "$L"
    $FR139_B "sudo bash $gs pre $FR139_A_OIP $c ${FR139_QUIET_S:-30} ${FR139_WAIT_S:-900}" > "$FR139_LOGS/$L.pre.txt" 2>&1 \
      && $FR139_A "sudo bash $gs pre $FR139_B_OIP $c 0 ${FR139_WAIT_S:-900}" >> "$FR139_LOGS/$L.pre.txt" 2>&1
    if [ "$(grep -c '^PRE ok' "$FR139_LOGS/$L.pre.txt")" != 2 ]; then
      echo "$L: the precondition did not hold; stopping"
      cat "$FR139_LOGS/$L.pre.txt"
      exit 4
    fi
    $FR139_B "sudo bash $gs b-start $L $FR139_A_LAN $FR139_A_OIP" > "$FR139_LOGS/$L.bstart.txt" 2>&1
    $FR139_A "sudo bash $gs a-cell $L $FR139_B_OIP ${FR139_PROBE_S:-60} ${FR139_SAMPLE_S:-200} ${FR139_IDLE_S:-0}" > "$FR139_LOGS/$L.a.txt" 2>&1
    sleep 3
    $FR139_B "sudo bash $gs b-stop $L $FR139_A_LAN" > "$FR139_LOGS/$L.b.txt" 2>&1
    cmd_summary_one "$L"
  done
  echo "fr139 run $tag end $(wall)"
}

cmd_promo() {
  local runs="$1" tag="$2" gs i L w="${FR139_WAIT_S:-900}"
  need FR139_A FR139_B FR139_A_LAN FR139_B_LAN FR139_A_OIP FR139_B_OIP FR139_LOGS
  gs="${FR139_GUEST_SCRIPT:-$D/fr139-restart-cell.sh}"
  mkdir -p "$FR139_LOGS"
  echo "fr139 promo $tag: $runs runs, start $(wall)"
  for i in $(seq 1 "$runs"); do
    L="$tag-$i"
    [ -e "$FR139_LOGS/$L.pa.txt" ] && { echo "refusing: $FR139_LOGS/$L.pa.txt exists"; exit 3; }
    until_ok "$L"
    # 1. Hold the pair on a relay: A's UDP blocked, both ends on a relay.
    $FR139_A "sudo bash $gs block on $FR139_B_LAN" > "$FR139_LOGS/$L.hold.txt" 2>&1
    $FR139_A "sudo bash $gs pre $FR139_B_OIP relay:any ${FR139_QUIET_S:-30} $w" >> "$FR139_LOGS/$L.hold.txt" 2>&1 \
      && $FR139_B "sudo bash $gs pre $FR139_A_OIP relay:any 0 $w" >> "$FR139_LOGS/$L.hold.txt" 2>&1
    if [ "$(grep -c '^PRE ok' "$FR139_LOGS/$L.hold.txt")" != 2 ]; then
      echo "$L: the pair did not settle on a relay; lifting the block and stopping"
      $FR139_A "sudo bash $gs block off" >> "$FR139_LOGS/$L.hold.txt" 2>&1
      cat "$FR139_LOGS/$L.hold.txt"
      exit 4
    fi
    # 2. Both ends ping each other every 0.1 s.
    $FR139_A "sudo bash $gs pstart $L $FR139_B_OIP $FR139_B_LAN" > "$FR139_LOGS/$L.pstart.txt" 2>&1
    $FR139_B "sudo bash $gs pstart $L $FR139_A_OIP $FR139_A_LAN" >> "$FR139_LOGS/$L.pstart.txt" 2>&1
    sleep 10
    # 3. Lift the block; wait for both ends to read the pair direct again, then
    #    30 s more.
    $FR139_A "sudo bash $gs block off" > "$FR139_LOGS/$L.lift.txt" 2>&1
    $FR139_A "sudo bash $gs pre $FR139_B_OIP lan 0 $w" >> "$FR139_LOGS/$L.lift.txt" 2>&1
    $FR139_B "sudo bash $gs pre $FR139_A_OIP lan 0 $w" >> "$FR139_LOGS/$L.lift.txt" 2>&1
    sleep 30
    $FR139_A "sudo bash $gs pstop $L" > "$FR139_LOGS/$L.pa.txt" 2>&1
    $FR139_B "sudo bash $gs pstop $L" > "$FR139_LOGS/$L.pb.txt" 2>&1
    cmd_psummary_one "$L"
  done
  echo "fr139 promo $tag end $(wall)"
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
  local L="$1" a="$FR139_LOGS/$1.a.txt" b="$FR139_LOGS/$1.b.txt" up upe fr gaps cnt idl
  up="$(grep -m1 ' daemon_up ' "$a" | awk '{print $3}')"
  upe="$(date -u -d "$up" +%s.%3N 2>/dev/null || echo 0)"
  fr="$(grep -m1 -E ' (first_reply|NO_REPLY) ' "$a" | sed 's/^EV [^ ]* [^ ]* //')"
  gaps="$(grep -E '^EV - - (gap|ping_summary)' "$a" | sed 's/^EV - - //' | tr '\n' ';')"
  cnt="$(grep -m1 '^B count ' "$b" | sed 's/^B count //')"
  idl="$(grep -m1 ' idle_end ' "$a" | sed 's/^EV [^ ]* [^ ]* idle_end //')"
  echo "RUN $L up=$up $fr | $gaps | B: $cnt${idl:+ | A at idle_end: $idl}" \
       "| +60 A=$(nearest "$a" 60 'A rows for B' 'A log file') B=$(nearest "$b" "$(awk -v t="$upe" 'BEGIN{printf "%.3f", t+60}')" 'B rows for A' 'B log since')" \
       "| +180 A=$(nearest "$a" 180 'A rows for B' 'A log file') B=$(nearest "$b" "$(awk -v t="$upe" 'BEGIN{printf "%.3f", t+180}')" 'B rows for A' 'B log since')"
}

cmd_summary() {
  local tag="$1" f
  need FR139_LOGS
  for f in "$FR139_LOGS/$tag"-*.a.txt; do
    [ -e "$f" ] || continue
    cmd_summary_one "$(basename "$f" .a.txt)"
  done
}

# An end's gaps, as start:length in seconds from the lift.
gaps_rel() {
  grep '^GAP ' "$1" | awk -v l="$2" '{ s=$2; sub(/start=/, "", s); n=$3; sub(/len_s=/, "", n)
    printf "%+.1f:%.2f%s ", s-l, n, ($4 != "" ? "(" $4 ")" : "") }'
}

# An end's carrier events, as a letter and seconds from the lift (the first 16).
events_rel() {
  local f="$1" l="$2" ts rest k e
  sed -n '/^## carrier lines/,/^## log window/p' "$f" |
    grep -E 'direct carrier promoted|onto DERP now \(demote-follow\)|within the promote grace|UNREGISTERED receiver index' |
    head -16 |
    while read -r ts rest; do
      case "$rest" in
        *"direct carrier promoted"*) k=P ;;
        *"(demote-follow)"*) k=F ;;
        *"promote grace"*) k=G ;;
        *) k=U ;;
      esac
      e="$(date -u -d "$ts" +%s.%3N 2>/dev/null)" || continue
      awk -v k="$k" -v e="$e" -v l="$l" 'BEGIN{printf "%s%+.1f ", k, e-l}'
    done
}

cmd_psummary_one() {
  local L="$1" pa="$FR139_LOGS/$1.pa.txt" pb="$FR139_LOGS/$1.pb.txt" lift
  lift="$(sed -n 's/^LIFT .* epoch=\([0-9.]*\):.*/\1/p' "$FR139_LOGS/$L.lift.txt" | head -1)"
  echo "PROMO $L lift=${lift:-?}" \
       "| to direct: $(grep '^PRE ' "$FR139_LOGS/$L.lift.txt" | sed -n 's/^PRE \([A-Za-z]*\) host=\([^ ]*\) .*\(waited_s\|after_s\)=\([0-9.]*\).*/\2:\1:\4/p' | tr '\n' ' ')" \
       "| A gaps: $(gaps_rel "$pa" "${lift:-0}")| B gaps: $(gaps_rel "$pb" "${lift:-0}")" \
       "| A events: $(events_rel "$pa" "${lift:-0}")| B events: $(events_rel "$pb" "${lift:-0}")" \
       "| A: $(grep -m1 '^P count ' "$pa" | sed 's/^P count //') | B: $(grep -m1 '^P count ' "$pb" | sed 's/^P count //')"
}

cmd_psummary() {
  local tag="$1" f
  need FR139_LOGS
  for f in "$FR139_LOGS/$tag"-*.pa.txt; do
    [ -e "$f" ] || continue
    cmd_psummary_one "$(basename "$f" .pa.txt)"
  done
}

case "${1:-}" in
  prep | pre | a-cell | b-start | b-sampler | b-stop | log | block | pstart | pstop) guard ;;
esac
case "${1:-}" in
  prep) shift; cmd_prep "$@" ;;
  pre) shift; cmd_pre "$@" ;;
  a-cell) shift; cmd_a_cell "$@" ;;
  b-start) shift; cmd_b_start "$@" ;;
  b-sampler) shift; cmd_b_sampler "$@" ;;
  b-stop) shift; cmd_b_stop "$@" ;;
  log) shift; cmd_log "$@" ;;
  block) shift; cmd_block "$@" ;;
  pstart) shift; cmd_pstart "$@" ;;
  pstop) shift; cmd_pstop "$@" ;;
  run) shift; cmd_run "$@" ;;
  promo) shift; cmd_promo "$@" ;;
  summary) shift; cmd_summary "$@" ;;
  psummary) shift; cmd_psummary "$@" ;;
  *) sed -n '2,/^set -u/p' "$0" | sed 's/^# \{0,1\}//'; exit 2 ;;
esac
