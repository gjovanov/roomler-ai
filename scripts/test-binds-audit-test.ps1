<#
.SYNOPSIS
  #4500: the bind audit's report, on made-up events. Runs on any OS with pwsh
  and touches nothing: no audit policy, no event log, no socket.

.DESCRIPTION
  Each case writes a state file (what `arm` notes) and an events file (the
  objects the report reads from the Security log), runs
  scripts/test-binds-audit.ps1 report on them, and checks its exit code and,
  where it matters, a line of its output:

    clean       canaries seen, every UDP bind and TCP listen on loopback  -> 0
    any-udp     a UDP bind on 0.0.0.0                                     -> 1
    lan-udp     a UDP bind on a LAN address                               -> 1
    any-listen  a TCP listen on [::]                                      -> 1
    outgoing    a TCP bind on 0.0.0.0 that is never listened on           -> 0
    other-proc  a 0.0.0.0 bind by a process the pattern does not name     -> 0
    no-canary   the audit did not record its own canary                   -> 2
    not-run     no event from a process the pattern names                 -> 1
#>
$ErrorActionPreference = 'Stop'
Set-StrictMode -Version 3

$script = Join-Path $PSScriptRoot 'test-binds-audit.ps1'
$dir = Join-Path ([System.IO.Path]::GetTempPath()) ("test-binds-audit-test-" + [guid]::NewGuid())
New-Item -ItemType Directory -Path $dir | Out-Null

$canaryPid = 4242
$start = [DateTime]::new(2026, 11, 27, 10, 0, 0, [DateTimeKind]::Utc)
$state = [pscustomobject]@{ start = $start.ToString('o'); ticks = $start.Ticks; pid = $canaryPid; any = 50001; loopback = 50002 }
$canaries = @(
    [pscustomobject]@{ Id = 5158; Pid = $canaryPid; Path = '\device\harddiskvolume1\program files\powershell\7\pwsh.exe'; Address = '0.0.0.0'; Port = 50001; Protocol = 17 },
    [pscustomobject]@{ Id = 5158; Pid = $canaryPid; Path = '\device\harddiskvolume1\program files\powershell\7\pwsh.exe'; Address = '127.0.0.1'; Port = 50002; Protocol = 17 }
)
$bin = '\device\harddiskvolume1\a\target\debug\deps\roomlerd-0123456789abcdef.exe'
function Ev($Id, $Address, $Port, $Protocol, $Path = $bin, $ProcessId = 900) {
    [pscustomobject]@{ Id = $Id; Pid = $ProcessId; Path = $Path; Address = $Address; Port = $Port; Protocol = $Protocol }
}
$loopbackRun = @(
    (Ev 5158 '127.0.0.1' 40001 17),
    (Ev 5158 '::1' 40002 17),
    (Ev 5158 '127.0.0.1' 40003 6),
    (Ev 5154 '127.0.0.1' 40003 6)
)

$cases = @(
    @{ Name = 'clean'; Events = $canaries + $loopbackRun; Want = 0; Says = 'Every UDP bind and every TCP listen was on loopback' },
    @{ Name = 'any-udp'; Events = $canaries + $loopbackRun + @(Ev 5158 '0.0.0.0' 5353 17); Want = 1; Says = 'bind UDP 0.0.0.0:5353' },
    @{ Name = 'lan-udp'; Events = $canaries + $loopbackRun + @(Ev 5158 '192.0.2.10' 40100 17); Want = 1; Says = 'bind UDP 192.0.2.10:40100' },
    @{ Name = 'any-listen'; Events = $canaries + $loopbackRun + @((Ev 5158 '::' 40200 6), (Ev 5154 '::' 40200 6)); Want = 1; Says = 'listen TCP :::40200' },
    @{ Name = 'outgoing'; Events = $canaries + $loopbackRun + @(Ev 5158 '0.0.0.0' 40300 6); Want = 0; Says = 'TCP bind, no listen (outgoing) | 0.0.0.0' },
    @{ Name = 'other-proc'; Events = $canaries + $loopbackRun + @(Ev 5158 '0.0.0.0' 40400 17 '\device\harddiskvolume1\windows\system32\svchost.exe' 4); Want = 0; Says = 'Every UDP bind and every TCP listen was on loopback' },
    @{ Name = 'no-canary'; Events = @($canaries[1]) + $loopbackRun; Want = 2; Says = 'measured nothing' },
    @{ Name = 'not-run'; Events = $canaries; Want = 1; Says = 'it never ran under the audit' }
)

$failed = 0
$saved = $env:GITHUB_STEP_SUMMARY
$env:GITHUB_STEP_SUMMARY = $null
try {
    foreach ($c in $cases) {
        $statePath = Join-Path $dir "$($c.Name)-state.json"
        $eventsPath = Join-Path $dir "$($c.Name)-events.json"
        $state | ConvertTo-Json | Set-Content -Path $statePath -Encoding utf8
        ConvertTo-Json -InputObject @($c.Events) -Depth 3 | Set-Content -Path $eventsPath -Encoding utf8
        $out = & pwsh -NoProfile -File $script report -Image 'roomlerd-*.exe' -State $statePath -Events $eventsPath 2>&1 | Out-String
        $code = $LASTEXITCODE
        $says = $out.Contains($c.Says)
        if ($code -eq $c.Want -and $says) {
            Write-Host "ok    $($c.Name): exit $code"
        } else {
            $failed++
            Write-Host "FAIL  $($c.Name): exit $code (want $($c.Want)); output has '$($c.Says)': $says"
            Write-Host $out
        }
    }
} finally {
    $env:GITHUB_STEP_SUMMARY = $saved
    Remove-Item -Recurse -Force -Path $dir
}
if ($failed -gt 0) {
    Write-Host "$failed case(s) failed"
    exit 1
}
Write-Host "all $($cases.Count) cases passed"
# Said outright: the last case's child exited 1 on purpose, and a caller that
# reads $LASTEXITCODE (GitHub's pwsh shell does) would take that as ours.
exit 0
