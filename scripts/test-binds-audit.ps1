<#
.SYNOPSIS
  #4500: every address a test binary binds on Windows, read from the Windows
  Filtering Platform's own audit. A UDP bind or a TCP listen on anything but
  loopback fails the report.

.DESCRIPTION
  Windows asks the person at the machine whether a program may accept
  connections the first time that program binds UDP, or listens on TCP, on
  an address other than loopback. Cargo names each test binary after its
  crate plus a hash, inside the target dir, so every new worktree, target dir
  or rebuilt hash is a program Windows has never seen, and asks about again.
  The tests therefore bind loopback (docs/testing.md, "How tests bind"). This
  script is how CI proves it, on a runner where nobody sees a prompt.

    arm     Turns on the audit of the "Filtering Platform Connection"
            subcategory (named by GUID, so a localized Windows reads it too),
            grows the Security log so a long run cannot overwrite its start,
            and notes the time. Then it binds two canaries of its own, UDP on
            0.0.0.0 and on 127.0.0.1, which the report must find.
    report  Reads every bind (event 5158) and every listen (event 5154) since
            `arm`. It first proves the audit saw the canaries; otherwise the
            run measured nothing and the report fails. Then it lists, for each
            -Image pattern, what the matching processes bound, and fails on a
            UDP bind or a TCP listen whose address is not 127.0.0.0/8 or ::1.

  A TCP bind that is never listened on is an outgoing connection, and an
  outgoing connection never raises the prompt. The report counts those
  separately and does not fail on them.

  CI only: `arm` changes the machine's audit policy and the size of its
  Security log, and its 0.0.0.0 canary would itself raise the prompt on a
  desktop. It refuses to run outside GitHub Actions unless -OnThisMachine is
  given.

.PARAMETER Image
  `report`: a file-name pattern of the test binaries to audit, as -like reads
  it, for example 'roomlerd-*.exe'. Repeatable. Each pattern must match at
  least one bind, or the binary never ran under the audit.

.EXAMPLE
  pwsh -NoProfile -File scripts/test-binds-audit.ps1 arm
  cargo test -p roomlerd --lib
  pwsh -NoProfile -File scripts/test-binds-audit.ps1 report -Image 'roomlerd-*.exe'
#>
param(
    [Parameter(Mandatory = $true, Position = 0)]
    [ValidateSet('arm', 'report')]
    [string]$Mode,
    [string[]]$Image = @(),
    [string]$State = '',
    # `report` only: read the events from this JSON file (the objects
    # Read-Fields makes) instead of the Security log. How the report's own
    # cases are tested, on any machine: scripts/test-binds-audit-test.ps1.
    [string]$Events = '',
    [switch]$OnThisMachine
)

$ErrorActionPreference = 'Stop'
Set-StrictMode -Version 3

if (-not $State) {
    $dir = if ($env:RUNNER_TEMP) { $env:RUNNER_TEMP } else { [System.IO.Path]::GetTempPath() }
    $State = Join-Path $dir 'test-binds-audit.json'
}
if ($Mode -eq 'arm' -and $env:GITHUB_ACTIONS -ne 'true' -and -not $OnThisMachine) {
    Write-Error 'test-binds-audit: CI only (it changes the audit policy, and its canary binds 0.0.0.0); pass -OnThisMachine to run it here anyway'
}

# "Filtering Platform Connection": events 5154 (listen), 5156/5157
# (connection), 5158/5159 (bind). By GUID, because auditpol takes the
# subcategory's display name in the system's language.
$Subcategory = '{0CCE9226-69AE-11D9-BED3-505054503030}'

function Test-Loopback([string]$Address) {
    if ($Address -eq '::1') { return $true }
    if ($Address -like '127.*') { return $true }
    if ($Address -like '::ffff:127.*') { return $true }
    return $false
}

function Read-Fields($Record) {
    # 5154 and 5158 carry the same first five fields, in this order.
    $p = $Record.Properties
    [pscustomobject]@{
        Id       = $Record.Id
        Pid      = [int]$p[0].Value
        Path     = [string]$p[1].Value
        Address  = [string]$p[2].Value
        Port     = [int]$p[3].Value
        Protocol = [int]$p[4].Value
    }
}

function Get-Leaf([string]$Path) {
    # The event's path is a device path (\device\harddiskvolumeN\...). Split
    # by hand: Split-Path splits on '/' alone off Windows, where the report's
    # own test runs too.
    return ($Path -split '[\\/]')[-1]
}

function Write-Summary([string]$Text) {
    Write-Host $Text
    if ($env:GITHUB_STEP_SUMMARY) { Add-Content -Path $env:GITHUB_STEP_SUMMARY -Value $Text }
}

if ($Mode -eq 'arm') {
    & auditpol.exe /set /subcategory:$Subcategory /success:enable /failure:enable | Out-Host
    if ($LASTEXITCODE -ne 0) { throw "auditpol /set exited $LASTEXITCODE" }
    & auditpol.exe /get /subcategory:$Subcategory | Out-Host
    # 1 GiB: a whole suite's connections fit with room to spare.
    & wevtutil.exe sl Security /ms:1073741824
    if ($LASTEXITCODE -ne 0) { throw "wevtutil sl exited $LASTEXITCODE" }
    $start = [DateTime]::UtcNow
    # The positive control. A report that cannot find these binds proves the
    # audit recorded nothing, which would read as a clean run.
    $any = [System.Net.Sockets.UdpClient]::new([System.Net.IPEndPoint]::new([System.Net.IPAddress]::Any, 0))
    $loop = [System.Net.Sockets.UdpClient]::new([System.Net.IPEndPoint]::new([System.Net.IPAddress]::Loopback, 0))
    # The start as UTC ticks: pwsh's ConvertFrom-Json turns an ISO date string
    # into a DateTime of its own choosing, and Windows PowerShell does not.
    $canary = [pscustomobject]@{
        start    = $start.ToString('o')
        ticks    = $start.Ticks
        pid      = $PID
        any      = $any.Client.LocalEndPoint.Port
        loopback = $loop.Client.LocalEndPoint.Port
    }
    $any.Dispose()
    $loop.Dispose()
    $canary | ConvertTo-Json | Set-Content -Path $State -Encoding utf8
    Write-Host "armed at $($canary.start); canaries: UDP 0.0.0.0:$($canary.any) and 127.0.0.1:$($canary.loopback) from pid $PID"
    Get-NetFirewallProfile | Format-Table Name, Enabled, DefaultInboundAction, NotifyOnListen | Out-Host
    exit 0
}

# ---- report ----
if (-not (Test-Path $State)) { throw "no state at ${State}: run 'arm' first" }
$canary = Get-Content -Raw -Path $State | ConvertFrom-Json
$start = [DateTime]::new([long]$canary.ticks, [DateTimeKind]::Utc)
if ($Events) {
    $seen = @(Get-Content -Raw -Path $Events | ConvertFrom-Json)
} else {
    $filter = @{ LogName = 'Security'; Id = @(5154, 5158); StartTime = $start.ToLocalTime() }
    $seen = @(Get-WinEvent -FilterHashtable $filter -ErrorAction SilentlyContinue | ForEach-Object { Read-Fields $_ })
}
Write-Host "read $($seen.Count) bind and listen events since $($start.ToString('o'))"

$sawAny = @($seen | Where-Object { $_.Pid -eq $canary.pid -and $_.Id -eq 5158 -and $_.Protocol -eq 17 -and $_.Port -eq $canary.any -and $_.Address -eq '0.0.0.0' }).Count
$sawLoop = @($seen | Where-Object { $_.Pid -eq $canary.pid -and $_.Id -eq 5158 -and $_.Protocol -eq 17 -and $_.Port -eq $canary.loopback -and $_.Address -eq '127.0.0.1' }).Count
if ($sawAny -lt 1 -or $sawLoop -lt 1) {
    Write-Summary "### Bind audit (#4500): measured nothing"
    Write-Summary "The audit did not record its own canaries (0.0.0.0: $sawAny, 127.0.0.1: $sawLoop), so it cannot tell a clean run from a run it did not see."
    exit 2
}
Write-Host "canaries seen: UDP 0.0.0.0:$($canary.any) and 127.0.0.1:$($canary.loopback)"

$failed = $false
foreach ($pattern in $Image) {
    $mine = @($seen | Where-Object { (Get-Leaf $_.Path) -like $pattern })
    Write-Summary "### Bind audit (#4500): ``$pattern``"
    if ($mine.Count -eq 0) {
        Write-Summary "No bind and no listen by a process named ``$pattern``: it never ran under the audit."
        $failed = $true
        continue
    }
    $images = @($mine | ForEach-Object { Get-Leaf $_.Path } | Sort-Object -Unique)
    Write-Summary ("{0} events from {1} process(es) of {2}" -f $mine.Count, @($mine.Pid | Sort-Object -Unique).Count, ($images -join ', '))
    $listens = @($mine | Where-Object { $_.Id -eq 5154 })
    $udp = @($mine | Where-Object { $_.Id -eq 5158 -and $_.Protocol -eq 17 })
    $tcpBinds = @($mine | Where-Object { $_.Id -eq 5158 -and $_.Protocol -eq 6 })
    $offenders = @(@($udp + $listens) | Where-Object { -not (Test-Loopback $_.Address) })
    Write-Summary ''
    Write-Summary '| What | Address | Count | Ports (first 8) |'
    Write-Summary '|---|---|---|---|'
    $rows = @($udp | ForEach-Object { $_ | Add-Member -PassThru -NotePropertyName Kind -NotePropertyValue 'UDP bind' -Force }) +
            @($listens | ForEach-Object { $_ | Add-Member -PassThru -NotePropertyName Kind -NotePropertyValue 'TCP listen' -Force }) +
            @($tcpBinds | ForEach-Object { $_ | Add-Member -PassThru -NotePropertyName Kind -NotePropertyValue 'TCP bind, no listen (outgoing)' -Force })
    foreach ($g in ($rows | Group-Object Kind, Address | Sort-Object Name)) {
        $ports = @($g.Group.Port | Sort-Object -Unique | Select-Object -First 8) -join ' '
        $first = $g.Group[0]
        Write-Summary ("| {0} | {1} | {2} | {3} |" -f $first.Kind, $first.Address, $g.Count, $ports)
    }
    Write-Summary ''
    if ($offenders.Count -gt 0) {
        $failed = $true
        Write-Summary ("**{0} UDP bind(s) or TCP listen(s) on an address that is not loopback.** Each raises the firewall prompt for a binary Windows has not seen; docs/testing.md says how a test binds." -f $offenders.Count)
        foreach ($o in ($offenders | Select-Object -First 40)) {
            Write-Host ("::error title=Non-loopback bind (#4500)::{0} {1} {2}:{3} by pid {4} ({5})" -f $(if ($o.Id -eq 5154) { 'listen' } else { 'bind' }), $(if ($o.Protocol -eq 17) { 'UDP' } else { 'TCP' }), $o.Address, $o.Port, $o.Pid, (Get-Leaf $o.Path))
        }
    } else {
        Write-Summary 'Every UDP bind and every TCP listen was on loopback.'
    }
}
if ($failed) { exit 1 }
exit 0
