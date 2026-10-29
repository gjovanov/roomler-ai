#!/usr/bin/env bash
# scripts/aws-testbox/guest-roomlerd.sh — runs ON a box up.sh made (as its default user, which has sudo): puts the
# RELEASED roomlerd on it the way a user does, as a root service with a virtual desktop of its own (a cloud box
# has no screen), and reads back what its encoder probe found. Ship it to the box with
#   TESTBOX_NAME=<name> scripts/aws-testbox/ssh.sh 'cat > guest-roomlerd.sh' < scripts/aws-testbox/guest-roomlerd.sh
#
#   prep                  apt prerequisites (Xvfb, openbox and a few X clients, PulseAudio, jq); apt's timers off
#   install [SERVER]      the served install.sh: --role daemon --system --no-enroll --no-desktop
#   enroll NAME [SERVER]  reads ONE line on stdin, an enrollment key or token, and enrolls with it (--ephemeral
#                         when ENROLL_EPHEMERAL=1, the default): stop the configless service, write the drop-in,
#                         enroll, ONE clean start. Never restart an ephemeral daemon: on SIGTERM it unenrolls
#   caps                  the daemon's probe verdict, the probe's own matrix, the cache
#   smoke                 encoder-smoke --encoder hardware (an offline NVENC/VAAPI open, no session)
#   record [hardware|auto|software]
#                         a ~10 s recording of a private Xvfb with --system-audio; prints the recorder's events and
#                         the file's sample entries (avc1 = H.264; mp4a + esds = AAC, Opus + dOps = Opus), then
#                         deletes the file
#   logs [PATTERN] [N]    the daemon's log lines
#   stop                  stop the daemon (an ephemeral device deletes its own row)
#
# Env: ROOMLERD_DROPIN_EXTRA — extra `Environment=` lines for the drop-in (one KEY=VALUE per line), e.g.
#      ROOMLERD_ENCODER_CELLS_DENY=... to steer a session onto the cell under test.
set -uo pipefail
SERVER_DEFAULT=https://roomler.ai
CFG=/etc/roomler/config.toml
UNIT=roomlerd.service
DROPIN=/etc/systemd/system/roomlerd.service.d/50-testbox.conf
say() { printf '%s %s\n' "$(date -u +%H:%M:%SZ)" "$*"; }
die() { say "FAILED: $*"; exit 1; }
strip() { sed -E 's/\x1b\[[0-9;]*m//g'; }
# The daemon's own log file, as the daemon resolves it (`roomler logs` prints the path first). Read whole, because
# `roomler logs --grep` is a substring filter over a 64 KiB tail and this script wants regexes over the run.
daemon_log() { sudo roomler logs -n 1 2>/dev/null | head -n 1 | awk '{print $1}'; }
log_grep() { # PATTERN N
  local f; f="$(daemon_log)"
  [ -n "$f" ] || { say "no daemon log (is $UNIT running?)"; return 1; }
  sudo grep -a -E "$1" "$f" | strip | tail -n "$2" | cut -c1-400
}

cmd="${1:-}"; shift || true
case "$cmd" in
prep)
  # The vmtest lesson (FR-61): REMOVE apt's competitors rather than race them for the lock.
  sudo systemctl stop apt-daily.timer apt-daily-upgrade.timer unattended-upgrades.service >/dev/null 2>&1
  sudo systemctl disable apt-daily.timer apt-daily-upgrade.timer >/dev/null 2>&1
  sudo systemctl mask unattended-upgrades.service >/dev/null 2>&1
  for _ in $(seq 1 60); do sudo fuser /var/lib/dpkg/lock-frontend >/dev/null 2>&1 || break; sleep 5; done
  # needrestart would restart roomlerd after a later apt install — and an ephemeral daemon unenrolls on SIGTERM.
  sudo mkdir -p /etc/needrestart/conf.d
  echo '$nrconf{restart} = "l";' | sudo tee /etc/needrestart/conf.d/50-testbox.conf >/dev/null
  export DEBIAN_FRONTEND=noninteractive
  sudo -E apt-get update -qq >/dev/null || die "apt-get update"
  # The sound server for `record --system-audio`: PulseAudio where the release still ships it, else PipeWire's.
  SOUND=pulseaudio
  apt-cache policy pulseaudio 2>/dev/null | grep -q 'Candidate: [0-9]' || SOUND="pipewire-pulse wireplumber"
  # shellcheck disable=SC2086
  sudo -E apt-get install -y -qq --no-install-recommends xvfb openbox xterm x11-apps x11-utils $SOUND \
    pulseaudio-utils jq curl ca-certificates >/dev/null || die "apt-get install"
  say "prep OK: $(Xvfb -version 2>&1 | grep -m1 -i 'x.org x server' || echo Xvfb), openbox, $SOUND, jq"
  ;;

install)
  S="${1:-$SERVER_DEFAULT}"
  # Fetch to a file and check it: under `curl | bash` a failed fetch "succeeds" with bash reading nothing.
  curl -fsSL -o /tmp/roomler-install.sh "$S/api/setup/install.sh" || die "fetch $S/api/setup/install.sh"
  [ -s /tmp/roomler-install.sh ] || die "install.sh is empty"
  bash /tmp/roomler-install.sh --role daemon --system --no-enroll --no-desktop --server "$S" </dev/null \
    || die "install.sh rc=$?"
  sudo systemctl disable --now roomler-update.timer roomler-update.path >/dev/null 2>&1
  say "install OK: $(roomlerd --version 2>&1 | head -n1) · $(dpkg-query -W -f='${Package} ${Version} ${Architecture}' roomlerd 2>/dev/null)"
  ;;

enroll)
  NAME="${1:?enroll NAME [SERVER]}"; S="${2:-$SERVER_DEFAULT}"
  IFS= read -r KEY || true
  [ -n "${KEY:-}" ] || die "no enrollment key on stdin"
  sudo systemctl stop "$UNIT" >/dev/null 2>&1
  # The daemon starts each startup app as a bare program (no arguments), so the moving picture a session needs —
  # a clock ticking every second, top redrawing — comes from one wrapper.
  printf '%s\n' '#!/bin/sh' 'xclock -update 1 -geometry 320x320+1520+60 &' \
    'xterm -geometry 150x48+40+40 -e top -d 1 &' 'wait' | sudo tee /usr/local/bin/testbox-desktop-apps >/dev/null
  sudo chmod 755 /usr/local/bin/testbox-desktop-apps
  sudo mkdir -p "$(dirname "$DROPIN")"
  {
    echo '[Service]'
    # The box has no screen: the daemon runs a desktop of its own (Xvfb + openbox + the apps above).
    echo 'Environment="ROOMLERD_VIRTUAL_DESKTOP=1"'
    echo 'Environment="ROOMLERD_VIRTUAL_DESKTOP_RESOLUTION=1920x1080"'
    echo 'Environment="ROOMLERD_VIRTUAL_DESKTOP_STARTUP=/usr/local/bin/testbox-desktop-apps"'
    # A field test pins the build it tests: no release may replace the binary mid-run.
    echo 'Environment="ROOMLERD_AUTO_UPDATE=0"'
    printf '%s\n' "${ROOMLERD_DROPIN_EXTRA:-}" | sed -n 's/^\([A-Z][A-Z0-9_]*=[^"]*\)$/Environment="\1"/p'
  } | sudo tee "$DROPIN" >/dev/null
  EPH=(); [ "${ENROLL_EPHEMERAL:-1}" = 1 ] && EPH=(--ephemeral)
  sudo roomlerd --config "$CFG" enroll --server "$S" --token "$KEY" --name "$NAME" "${EPH[@]}" 2>&1 | strip | tail -n 3 \
    || die "enroll"
  unset KEY
  sudo systemctl daemon-reload
  # A configless crash-loop trips systemd's start limit: reset-failed before each blocking start (FR-61).
  ok=""
  for _ in 1 2 3 4 5; do
    sudo systemctl reset-failed "$UNIT" >/dev/null 2>&1
    sudo systemctl start "$UNIT" && { ok=1; break; }
    sleep 3
  done
  [ -n "$ok" ] || die "$UNIT would not start"
  for _ in $(seq 1 40); do sudo roomler status --json >/dev/null 2>&1 && break; sleep 3; done
  sudo roomler status --json 2>/dev/null | jq -c '{version: (.version // .agent_version), online: (.connected // .online), name: (.name // .machine_name)}' 2>/dev/null \
    || sudo roomler status 2>&1 | head -n 8
  say "enroll OK: $NAME"
  ;;

caps)
  say "the daemon's probe verdict:"
  log_grep 'caps probe' 16
  say "the probe's own matrix (roomlerd caps-probe, the base phase, run now):"
  sudo roomlerd caps-probe 2>/dev/null | grep -a '^ROOMLER_CAPS_JSON:' | sed 's/^ROOMLER_CAPS_JSON://' \
    | jq -c '(.caps // .) | {hw_encoders, codecs, transports, cells: [.video_cells[]? | "\(.codec)/\(.backend) \(.chroma | join("+")) hw=\(.hw)"]}'
  for f in $(sudo find / -xdev -name caps-cache.json 2>/dev/null); do
    say "cache $f: $(sudo jq -c '.caps.hw_encoders // .hw_encoders // empty' "$f" 2>/dev/null)"
  done
  ;;

smoke)
  sudo roomlerd encoder-smoke --encoder hardware 2>&1 | strip | tail -n 15 | cut -c1-300
  ;;

record)
  ENC="${1:-hardware}"
  OUT=/tmp/testbox-rec; rm -rf "$OUT"; mkdir -p "$OUT"
  XDG_RUNTIME_DIR="/run/user/$(id -u)"; export XDG_RUNTIME_DIR
  # A sound server with a monitor to record: the recorder's system audio is the default sink's monitor. Ubuntu
  # socket-activates PulseAudio per user; a `pulseaudio --start` beside it races the unit into its start limit and
  # leaves a socket that refuses every connection, so drive the units and nothing else.
  if systemctl --user cat pulseaudio.socket >/dev/null 2>&1; then
    pulseaudio --kill >/dev/null 2>&1
    systemctl --user reset-failed pulseaudio.service pulseaudio.socket >/dev/null 2>&1
    systemctl --user restart pulseaudio.socket pulseaudio.service >/dev/null 2>&1
  else
    systemctl --user restart pipewire.socket pipewire-pulse.socket pipewire.service pipewire-pulse.service \
      wireplumber.service >/dev/null 2>&1
  fi
  for _ in $(seq 1 20); do pactl info >/dev/null 2>&1 && break; sleep 0.5; done
  pactl info >/dev/null 2>&1 || die "no sound server for --system-audio"
  pactl list short sinks 2>/dev/null | grep -q testbox || pactl load-module module-null-sink sink_name=testbox >/dev/null
  pactl set-default-sink testbox >/dev/null 2>&1
  setsid Xvfb :77 -screen 0 1920x1080x24 -nolisten tcp >/tmp/testbox-xvfb.log 2>&1 </dev/null &
  XP=$!
  for _ in $(seq 1 50); do [ -S /tmp/.X11-unix/X77 ] && break; sleep 0.1; done
  [ -S /tmp/.X11-unix/X77 ] || { kill "$XP" 2>/dev/null; die "no Xvfb on :77"; }
  DISPLAY=:77 xclock -update 1 >/dev/null 2>&1 &
  CP=$!
  ( sleep 10; echo '{"cmd":"stop"}'; sleep 25 ) | env -u WAYLAND_DISPLAY DISPLAY=:77 XDG_SESSION_TYPE=x11 \
      ROOMLERD_DRM_CAPTURE=0 ROOMLERD_PORTAL_CAPTURE=0 ROOMLERD_AUTO_UPDATE=0 \
      roomlerd record --out "$OUT" --encoder "$ENC" --max-minutes 1 --system-audio 2>/tmp/testbox-rec.log \
    | grep -a '^ROOMLER_REC_JSON:' | sed -E 's/"(path|partial)":"[^"]*"/"\1":"…"/g' | cut -c1-400
  grep -a -i -E 'encoder|audio|aac|opus|recording: ' /tmp/testbox-rec.log | strip | cut -c1-260 | head -n 10
  F="$(find "$OUT" -name '*.mp4' | head -n 1)"
  say "file: $(basename "${F:-none}") $(stat -c %s "$F" 2>/dev/null) bytes"
  [ -n "$F" ] && python3 -c 'import sys; d=open(sys.argv[1],"rb").read(); print("sample entries:", {k: d.count(k.encode()) for k in ("avc1","hvc1","mp4a","esds","Opus","dOps")})' "$F"
  J="$(find "$OUT" -name '*.roomler.json' | head -n 1)"
  [ -n "$J" ] && say "sidecar: $(jq -c '{codec: (.video.codec // .codec), encoder: (.video.encoder // .encoder), audio}' "$J" 2>/dev/null | cut -c1-300)"
  rm -rf "$OUT" /tmp/testbox-rec.log; say "deleted: $([ -e "${F:-/nonexistent}" ] && echo NO || echo yes)"
  kill "$CP" "$XP" 2>/dev/null; wait "$XP" 2>/dev/null
  ;;

logs)
  log_grep "${1:-encoder selected|media pump|caps probe|session}" "${2:-40}"
  ;;

stop)
  sudo systemctl stop "$UNIT"; say "stopped $UNIT ($(systemctl is-active "$UNIT"))"
  ;;

*)
  sed -n '2,22p' "$0"; exit 2 ;;
esac
