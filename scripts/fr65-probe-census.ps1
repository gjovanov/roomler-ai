<#
.SYNOPSIS
  FR-65 AC6 - the caps-probe census: how often a Windows host's daemon ran the
  encoder capability probe, and how many of those runs were at steady state.

.DESCRIPTION
  P4 was opened on a probe child spawned every ~10 s for ~3 h on one host,
  despite the probe's cache. AC6 asks for 0 spawns an hour at steady state on a
  measured host. This reads the daemon's dated logs, writes nothing, and is run
  on the host by scripts/fr65-stall-census.sh --probes (which sets $Days).

  What it counts, and why that sees every spawn:
    - the probe child has ONE spawn site (encode/caps.rs, run_child), and every
      run of it is logged by the daemon that ran it: the decision to probe
      (`cache miss`, `cache disabled`, `no directory to cache in`) and the
      outcome (`child reported`, `the probe child hung`, `the probe child DIED`,
      `could not spawn the probe child`). Every caller is a roomlerd process,
      and on Windows they all write this log.
    - a daemon start is `agent starting version=...`. A probe line within 60 s
      after a start is a start's probe (a new version misses the cache); any
      other is steady state.

  The positive control is in the same output: every start that changed version
  shows its probe lines, so a steady-state probe would show the same way.
#>
$ProgressPreference = 'SilentlyContinue'
[System.Threading.Thread]::CurrentThread.CurrentCulture = [Globalization.CultureInfo]::InvariantCulture
if (-not $Days) { $Days = 14 }

$dir = 'C:\ProgramData\roomler\roomler\service-logs'
$files = @(Get-ChildItem $dir -Filter 'roomlerd.log.*' -ErrorAction SilentlyContinue |
  Sort-Object Name | Select-Object -Last $Days)
if ($files.Count -eq 0) { "no roomlerd.log.* under $dir"; return }

$probeMarks = @('caps probe: child reported', 'the probe child hung', 'the probe child DIED',
  'caps probe: cache miss', 'caps probe: cache disabled', 'caps probe: no directory to cache in')
$starts = New-Object System.Collections.Generic.List[datetime]
$probes = New-Object System.Collections.Generic.List[datetime]
$fails = 0; $firstLine = $null; $lastLine = $null
function Ts([string]$l) { try { [datetime]::Parse($l.Substring(0, 27)).ToUniversalTime() } catch { $null } }

$unreadable = New-Object System.Collections.Generic.List[string]
foreach ($f in $files) {
  # ⚠️ The daemon holds today's file open for writing; [IO.File]::ReadLines is
  # refused there and the refusal never reaches `roomler exec`'s stdout. Open
  # it the way a tail does (see fr65-stall-census.ps1).
  try {
    $fs = [System.IO.File]::Open($f.FullName, 'Open', 'Read', [System.IO.FileShare]'ReadWrite, Delete')
  } catch { $unreadable.Add("$($f.Name): $($_.Exception.Message)"); continue }
  $sr = New-Object System.IO.StreamReader($fs)
  try {
    while ($null -ne ($l = $sr.ReadLine())) {
      if ($l.Length -lt 30 -or $l[4] -ne '-') { continue }
      if (-not $firstLine) { $firstLine = $l }
      $lastLine = $l
      if ($l.Contains('agent starting version')) { $t = Ts $l; if ($t) { $starts.Add($t) }; continue }
      if ($l.Contains('could not spawn the probe child')) { $fails++; $t = Ts $l; if ($t) { $probes.Add($t) }; continue }
      foreach ($m in $probeMarks) { if ($l.Contains($m)) { $t = Ts $l; if ($t) { $probes.Add($t) }; break } }
    }
  } finally { $sr.Dispose() }
}

$atStart = 0; $steady = New-Object System.Collections.Generic.List[string]
foreach ($p in $probes) {
  $near = $false
  foreach ($s in $starts) { $d = ($p - $s).TotalSeconds; if ($d -ge 0 -and $d -le 60) { $near = $true; break } }
  if ($near) { $atStart++ } else { $steady.Add($p.ToString('yyyy-MM-ddTHH:mm:ss')) }
}
$from = Ts $firstLine; $to = Ts $lastLine
$hours = if ($from -and $to) { ($to - $from).TotalHours } else { 0 }
'window {0:yyyy-MM-dd HH:mm} .. {1:yyyy-MM-dd HH:mm} UTC ({2:F0} h)  starts={3}  probe_lines={4}  within_60s_of_a_start={5}  steady_state={6}  spawn_failures={7}  unreadable={8}' -f $from, $to, $hours, $starts.Count, $probes.Count, $atStart, $steady.Count, $fails, $unreadable.Count
foreach ($u in $unreadable) { "  UNREADABLE $u" }
if ($steady.Count) { 'steady-state probe lines: ' + ($steady -join ' ') }
