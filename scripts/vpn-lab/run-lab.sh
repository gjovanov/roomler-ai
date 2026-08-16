#!/bin/bash
# run-lab.sh — one command = one VPN-cycle experiment against CORPLAP-1.
#
# Deploys/refreshes vpn-lab.ps1 on the laptop (raw.githubusercontent, branch-
# aware), launches the cycle DETACHED there (SYSTEM scheduled task — survives
# control-WS churn), runs the dev-box measurement half locally, then collects
# both sides and extracts the outage windows per org.
#
# Usage: run-lab.sh <count> <hold-s> <rest-s> [branch]
set -euo pipefail
COUNT="${1:-2}"; HOLD="${2:-180}"; REST="${3:-120}"; BRANCH="${4:-vpn-lab}"
EXEC="/c/rwexec/target/release/roomler.exe"
RUNID=$(date -u +%Y%m%d-%H%M%S)
OUT="${VPNLAB_OUT:-$HOME/vpnlab}/$RUNID"
RAW="https://raw.githubusercontent.com/gjovanov/roomler-ai/$BRANCH/scripts/vpn-lab/vpn-lab.ps1"
mkdir -p "$OUT"

psexec() { # run an encoded PS command on CORPLAP-1, CLIXML noise stripped
  local enc; enc=$(printf '%s' "$1" | iconv -f UTF-8 -t UTF-16LE | base64 -w0)
  "$EXEC" exec CORPLAP-1 --timeout "${2:-60000}" -- "powershell -NoProfile -EncodedCommand $enc" 2>&1 |
    grep -vE "CLIXML|^<Objs"
}

echo "== deploy ($BRANCH) =="
psexec "New-Item -ItemType Directory -Path 'C:\ProgramData\roomler\vpnlab' -Force | Out-Null;
Invoke-WebRequest -UseBasicParsing -Uri '$RAW' -OutFile 'C:\ProgramData\roomler\vpnlab\vpn-lab.ps1';
(Get-Item 'C:\ProgramData\roomler\vpnlab\vpn-lab.ps1').Length" 90000

echo "== launch detached run $RUNID =="
psexec "\$a = New-ScheduledTaskAction -Execute 'powershell.exe' -Argument '-NoProfile -ExecutionPolicy Bypass -File C:\ProgramData\roomler\vpnlab\vpn-lab.ps1 -Cmd cycle -Count $COUNT -HoldSec $HOLD -RestSec $REST -RunId $RUNID';
\$t = New-ScheduledTaskTrigger -Once -At (Get-Date).AddSeconds(5);
Register-ScheduledTask -TaskName 'roomler-vpnlab-run' -Action \$a -Trigger \$t -User 'SYSTEM' -RunLevel Highest -Force | Out-Null;
Start-ScheduledTask -TaskName 'roomler-vpnlab-run'; 'LAUNCHED $RUNID'" 60000

# Total = optional normalize (disc+rest) + N*(connect~30s+hold+disc~15s+rest)
# + the 90 s post-transition sampler window + slack
TOTAL=$(( REST + COUNT * (HOLD + REST + 60) + 240 ))
echo "== local measurement for ${TOTAL}s → $OUT =="
"$(dirname "$0")/vpn-lab-neo16.sh" "$OUT" "$TOTAL" &
LOCAL_PID=$!
sleep "$TOTAL"
wait "$LOCAL_PID" || true

echo "== collect from CORPLAP-1 =="
RD="C:\\ProgramData\\roomler\\vpnlab\\run-$RUNID"
psexec "Get-Content '$RD\\events.csv' -ErrorAction SilentlyContinue" 60000 > "$OUT/pc-events.csv"
for t in 100_65_4_2 100_65_0_6; do
  psexec "Get-Content '$RD\\ping-$t.csv' -ErrorAction SilentlyContinue" 90000 > "$OUT/pc-ping-$t.csv"
done
psexec "Get-Content '$RD\\roomler-samples.txt' -ErrorAction SilentlyContinue | Select-String -Pattern '=== |version|srflx|warm|neo16|org:' | ForEach-Object { \$_.Line }" 90000 > "$OUT/pc-roomler-samples.txt"
psexec "Get-Content '$RD\\trac-connect.log' -ErrorAction SilentlyContinue" 60000 > "$OUT/pc-trac.log"

echo "== outage windows (>=3 s consecutive loss) =="
for f in "$OUT"/pc-ping-*.csv "$OUT"/ping-*.csv; do
  [ -s "$f" ] || continue
  echo "-- $(basename "$f")"
  awk -F, '
    $2!="Success" { if (!s) { s=$1 } ; n++ ; next }
    { if (n>=3) printf "  %s -> %s (%d s)\n", s, $1, n ; s="" ; n=0 }
    END { if (n>=3) printf "  %s -> end (%d s)\n", s, n }' "$f"
done
echo "== events =="
cat "$OUT/pc-events.csv" 2>/dev/null || true
echo "RUN $RUNID collected at $OUT"
