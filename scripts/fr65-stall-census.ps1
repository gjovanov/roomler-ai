<#
.SYNOPSIS
  FR-65 - the stall census: every "FFmpeg DC pump STALL" line in a Windows
  host's daemon logs, classified by what its phase breakdown says.

.DESCRIPTION
  The instrument for the before/after of every FR-65 change to the stall watch
  (AC7: "every fix carries a before/after from the SAME instrument"). It reads
  the daemon's dated logs and writes nothing; run it on the host through
  `roomler exec` with scripts/fr65-stall-census.sh, which sets $Days / $Rows.

  Each STALL line is put in ONE class:

    reset      the window-reset artifact (the pump before 2026-10-30): emitted
               within 5 ms of its session's heartbeat, every phase the
               heartbeat zeroes (capture, scale, encode, send, apply, open) at
               0.0, and other >= 90 % of the pass. The pass held the heartbeat,
               whose reset saturated those six deltas to 0. A build with the
               rebase reports these passes under their real phase instead, so
               on such a build this class should read 0.
    <phase>    the pass's dominant phase, from the line's `dominant=` field.
    untimed    dominant=None and not `reset`: the time is in no named phase.

  -Rows prints every line it classified; for a `reset` line it also prints the
  heartbeat just before it (open_ms, avg_capture_ms, avg_encode_ms), which is
  where the lost attribution can still be read.

  Only the FFmpeg DC pump has the watch. A host whose sessions ran another pump
  logs no STALL lines at all, which this reports as zero, not as health.
#>
$ProgressPreference = 'SilentlyContinue'
# The fleet's Windows hosts are not all en-US: a German culture formats 3586.0
# as "3.586,0" and parses "0.5" as 5. The logs are invariant, so is this.
[System.Threading.Thread]::CurrentThread.CurrentCulture = [Globalization.CultureInfo]::InvariantCulture
if (-not $Days) { $Days = 14 }
if (-not $Rows) { $Rows = $false }

$dir = 'C:\ProgramData\roomler\roomler\service-logs'
$files = @(Get-ChildItem $dir -Filter 'roomlerd.log.*' -ErrorAction SilentlyContinue |
  Sort-Object Name | Select-Object -Last $Days)
if ($files.Count -eq 0) { "no roomlerd.log.* under $dir"; return }

function Field([string]$line, [string]$key) {
  $m = [regex]::Match($line, " $key=([^ ]+)")
  if ($m.Success) { $m.Groups[1].Value } else { '' }
}
function Num([string]$s) {
  $v = 0.0
  if ([double]::TryParse($s, [Globalization.NumberStyles]::Float,
      [Globalization.CultureInfo]::InvariantCulture, [ref]$v)) { $v } else { 0.0 }
}

$lastHb = @{}      # session -> (time, line) of its latest heartbeat
$classes = @{}     # class -> @{n; max; sum}
$heartbeats = 0; $stalls = 0
$rowsOut = New-Object System.Collections.Generic.List[string]
$unreadable = New-Object System.Collections.Generic.List[string]
$lastLine = ''

foreach ($f in $files) {
  # ⚠️ The daemon holds TODAY's file open for writing. [IO.File]::ReadLines is
  # refused there (a sharing violation), PowerShell reports that as a
  # non-terminating error, and `roomler exec` delivers stderr as CLIXML that the
  # wrapper drops — so the first version of this census silently skipped today
  # (2026-10-30). Open it the way a tail does, and say so if a file still fails.
  try {
    $fs = [System.IO.File]::Open($f.FullName, 'Open', 'Read', [System.IO.FileShare]'ReadWrite, Delete')
  } catch { $unreadable.Add("$($f.Name): $($_.Exception.Message)"); continue }
  $sr = New-Object System.IO.StreamReader($fs)
  try {
  while ($null -ne ($line = $sr.ReadLine())) {
    if ($line.Length -ge 19) { $lastLine = $line }
    if ($line.Length -lt 40) { continue }
    $isHb = $line.Contains('DC pump heartbeat')
    $isSt = $line.Contains('DC pump STALL')
    if (-not ($isHb -or $isSt)) { continue }
    try { $ts = [datetime]::Parse($line.Substring(0, 27)).ToUniversalTime() } catch { continue }
    $m = [regex]::Match($line, 'session_id=([0-9a-f]{24})')
    $sid = if ($m.Success) { $m.Groups[1].Value } else { '?' }
    if ($isHb) { $lastHb[$sid] = @($ts, $line); $heartbeats++; continue }

    $stalls++
    $iter = Num (Field $line 'iter_ms')
    $other = Num (Field $line 'other_ms')
    $resetPhases = @('capture_ms', 'scale_ms', 'encode_ms', 'send_ms', 'apply_ms', 'open_ms')
    $allZero = $true
    foreach ($k in $resetPhases) { if ((Num (Field $line $k)) -ne 0.0) { $allZero = $false } }
    $sinceHb = [double]::PositiveInfinity
    if ($lastHb.ContainsKey($sid)) { $sinceHb = ($ts - $lastHb[$sid][0]).TotalMilliseconds }
    $dom = [regex]::Match($line, 'dominant=Some\(\("([a-z_]+)"').Groups[1].Value

    if ($allZero -and $sinceHb -lt 5 -and $iter -gt 0 -and $other -ge 0.9 * $iter) { $cls = 'reset' }
    elseif ($dom) { $cls = $dom }
    else { $cls = 'untimed' }

    if (-not $classes.ContainsKey($cls)) { $classes[$cls] = @{ n = 0; max = 0.0; sum = 0.0 } }
    $c = $classes[$cls]; $c.n++; $c.sum += $iter; if ($iter -gt $c.max) { $c.max = $iter }

    if ($Rows) {
      $sid6 = if ($sid.Length -ge 6) { $sid.Substring($sid.Length - 6) } else { $sid }
      $row = '{0} {1,-9} s={2} iter={3:F1} other={4:F1} since_hb_ms={5:F1}' -f $line.Substring(0, 23), $cls, $sid6, $iter, $other, $sinceHb
      if ($cls -eq 'reset') {
        $hb = $lastHb[$sid][1]
        $row += ' | hb: open_ms={0} avg_capture_ms={1} avg_encode_ms={2}' -f (Field $hb 'open_ms'), (Field $hb 'avg_capture_ms'), (Field $hb 'avg_encode_ms')
      }
      $rowsOut.Add($row)
    }
  }
  } finally { $sr.Dispose() }
}

'files={0} ({1} .. {2}, read through {3}) heartbeats={4} stalls={5} unreadable={6}' -f $files.Count, $files[0].Name, $files[-1].Name, $lastLine.Substring(0, [Math]::Min(19, $lastLine.Length)), $heartbeats, $stalls, $unreadable.Count
foreach ($u in $unreadable) { "  UNREADABLE $u" }
foreach ($k in ($classes.Keys | Sort-Object)) {
  $c = $classes[$k]
  '  {0,-10} n={1,4}  iter_ms max={2,9:F1}  mean={3,8:F1}' -f $k, $c.n, $c.max, ($c.sum / [math]::Max(1, $c.n))
}
if ($Rows) { $rowsOut }
