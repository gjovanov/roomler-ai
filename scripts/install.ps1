# roomler install.ps1 -- terminal-driven install of the Roomler node stack on
# Windows, replicating the roomler-setup wizard's steps without a GUI:
# resolve via the roomler.ai proxy -> download -> sha256-verify -> install ->
# enroll -> autostart. Installs roomlerd + the roomler CLI (both inside the
# MSI since P4b) and places the roomler-desktop companion beside them.
#
# NB this file is deliberately ASCII-only: it is served raw over HTTP
# (GET /api/setup/install.ps1) and must parse identically under Windows
# PowerShell 5.1 (which reads BOM-less files as ANSI -- multibyte UTF-8
# punctuation decodes into smart quotes that BREAK the parser) and pwsh 7.
#
# Usage (download-then-run keeps parameters simple):
#
#   irm https://roomler.ai/api/setup/install.ps1 -OutFile install.ps1
#   powershell -ExecutionPolicy Bypass -File .\install.ps1 `
#       -Role daemon-user -Token <enrollment-jwt> [-Server https://roomler.ai] `
#       [-Name $env:COMPUTERNAME]
#
#   (One-liner equivalent:
#     & ([scriptblock]::Create((irm https://roomler.ai/api/setup/install.ps1))) -Role daemon-user -Token <jwt>
#   )
#
# Roles (same vocabulary as the roomler-setup wizard):
#   daemon-user     perUser MSI -- Scheduled-Task autostart, no UAC. Default.
#   daemon-machine  perMachine MSI -- SCM service 'Roomler'. ELEVATED shell required.
#   daemon-system   perMachine MSI + SystemContext (pre-logon / lock screen /
#                   UAC control). ELEVATED shell required.
#   tunnel-client   the roomler CLI only ("reach others") -- zip + user PATH.
#
# Switches:
#   -DownloadOnly   resolve + download + verify, print what WOULD run,
#                   touch nothing else (safe on any box).
#   -NoEnroll       install without enrolling (no token needed).
#   -SkipDesktop    daemon roles: don't fetch roomler-desktop.exe.
#
# The enrollment token is single-use and is never echoed.

[CmdletBinding()]
param(
    [ValidateSet('daemon-user', 'daemon-machine', 'daemon-system', 'tunnel-client')]
    [string]$Role = 'daemon-user',
    [string]$Server = 'https://roomler.ai',
    [string]$Token = '',
    [string]$Name = $env:COMPUTERNAME,
    [switch]$DownloadOnly,
    [switch]$NoEnroll,
    [switch]$SkipDesktop
)

$ErrorActionPreference = 'Stop'
[Net.ServicePointManager]::SecurityProtocol = [Net.SecurityProtocolType]::Tls12

function Say([string]$msg)  { Write-Host "==> $msg" }
function Warn([string]$msg) { Write-Host "WARNING: $msg" -ForegroundColor Yellow }

function Test-Elevated {
    $id = [Security.Principal.WindowsIdentity]::GetCurrent()
    ([Security.Principal.WindowsPrincipal]$id).IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)
}

# Raw .NET SHA256 rather than Get-FileHash: the cmdlet depends on
# module autoloading (Microsoft.PowerShell.Utility), which constrained
# or oddly-configured hosts sometimes lack; the .NET path works on any
# PowerShell 5.1+ unconditionally.
function Get-Sha256Hex([string]$File) {
    $sha = [System.Security.Cryptography.SHA256]::Create()
    try {
        $fs = [System.IO.File]::OpenRead($File)
        try { $hash = $sha.ComputeHash($fs) } finally { $fs.Dispose() }
    } finally { $sha.Dispose() }
    ($hash | ForEach-Object { $_.ToString('x2') }) -join ''
}

# Verify a file against a "sha256:<hex>" digest (GitHub asset format).
# Soft-skips when the digest is absent (older releases lack it).
function Assert-Sha256([string]$File, [string]$Digest) {
    if (-not $Digest) {
        Warn ("no sha256 digest published for " + (Split-Path -Leaf $File) + " -- skipping verification")
        return
    }
    $want = ($Digest -replace '^sha256:', '').ToLower()
    $got = Get-Sha256Hex -File $File
    if ($got -ne $want) {
        throw "sha256 mismatch for ${File}: got $got, want $want"
    }
    Say ("sha256 verified: " + (Split-Path -Leaf $File))
}

$stage = Join-Path $env:TEMP ("roomler-install-" + [guid]::NewGuid().ToString('N').Substring(0, 8))
New-Item -ItemType Directory -Path $stage -Force | Out-Null

$machineRole = $Role -in @('daemon-machine', 'daemon-system')
if ($machineRole -and -not $DownloadOnly -and -not (Test-Elevated)) {
    throw "role '$Role' installs the perMachine MSI -- run this script from an ELEVATED PowerShell (Terminal (Admin))."
}

# Pre-rendered hint fragment for enroll commands (perMachine flavours
# write the machine-global config, which needs the flag + elevation).
$mgFlag = ''
if ($machineRole) { $mgFlag = ' --machine-global' }

Say "roomler install.ps1 -- role=$Role server=$Server"

# --- daemon roles: MSI (carries roomlerd + the roomler CLI) -----------------

function Install-Daemon {
    $flavour = 'permachine'
    if ($Role -eq 'daemon-user') { $flavour = 'peruser' }
    Say "resolving the $flavour MSI via $Server/api/agent/installer/$flavour/health"
    $health = Invoke-RestMethod -UseBasicParsing "$Server/api/agent/installer/$flavour/health?version=latest"
    $msi = Join-Path $stage $health.filename
    Say ("downloading " + $health.filename + " (tag " + $health.tag + ")")
    Invoke-WebRequest -UseBasicParsing ($Server + $health.uri) -OutFile $msi
    Assert-Sha256 -File $msi -Digest $health.digest

    $sysCtx = '0'
    if ($Role -eq 'daemon-system') { $sysCtx = '1' }
    $msiArgs = "/i `"$msi`" /qn /norestart"
    if ($machineRole) { $msiArgs = $msiArgs + " ENABLE_SYSTEM_CONTEXT=$sysCtx" }

    $installDir = Join-Path $env:ProgramFiles 'Roomler'
    if ($Role -eq 'daemon-user') { $installDir = Join-Path $env:LOCALAPPDATA 'Programs\Roomler' }
    $daemon = Join-Path $installDir 'roomlerd.exe'

    if ($DownloadOnly) {
        Say "download-only: would run: msiexec $msiArgs"
        Say "download-only: would enroll via: '$daemon' enroll --server $Server --token <token> --name '$Name'$mgFlag"
        if (-not $SkipDesktop) { Say "download-only: would place roomler-desktop.exe into $installDir" }
        return
    }

    Say "installing the MSI (quiet)"
    $proc = Start-Process -FilePath 'msiexec.exe' -ArgumentList $msiArgs -Wait -PassThru
    if ($proc.ExitCode -ne 0) { throw "msiexec exited $($proc.ExitCode)" }
    if (-not (Test-Path $daemon)) { throw "install finished but $daemon is missing" }
    Say "installed: $installDir (roomlerd.exe + roomler.exe on PATH for new shells)"

    if (-not $SkipDesktop) { Install-Desktop -InstallDir $installDir }

    if ($NoEnroll -or -not $Token) {
        if (-not $NoEnroll) { Warn "no -Token given -- skipping enrollment" }
        Say "enroll later with: & '$daemon' enroll --server $Server --token <agent-enrollment-jwt> --name '$Name'$mgFlag"
        return
    }

    Say "enrolling this machine as '$Name' against $Server (token is single-use, never echoed)"
    $enrollArgs = @('enroll', '--server', $Server, '--token', $Token, '--name', $Name)
    if ($machineRole) { $enrollArgs += '--machine-global' }
    & $daemon @enrollArgs
    if ($LASTEXITCODE -ne 0) { throw "enrollment failed (exit $LASTEXITCODE)" }

    # Kick the autostart so the daemon picks up the fresh config now
    # rather than at the next logon / service cycle. Best-effort.
    if ($Role -eq 'daemon-user') {
        Say "starting the Scheduled Task 'Roomler'"
        schtasks /Run /TN Roomler | Out-Null
    } else {
        Say "restarting the 'Roomler' service so it picks up the enrollment"
        try { Restart-Service -Name Roomler -Force } catch { Warn "Restart-Service Roomler: $_" }
    }
}

# The desktop companion is a standalone release EXE (not in the MSI).
# Placing it BESIDE roomlerd.exe is the supported layout -- the tray
# resolves the daemon as a sibling of its own EXE.
function Install-Desktop([string]$InstallDir) {
    Say "resolving roomler-desktop from $Server/api/agent/latest-release"
    try {
        $releases = Invoke-RestMethod -UseBasicParsing "$Server/api/agent/latest-release"
        $asset = $null
        foreach ($r in $releases) {
            if ($r.tag_name -notlike 'agent-v*' -or $r.draft) { continue }
            $asset = $r.assets | Where-Object {
                $_.name -like 'roomler-desktop-*-x86_64-pc-windows-msvc*.exe' -and $_.name -notlike '*.sha256'
            } | Select-Object -First 1
            if ($asset) { break }
        }
        if (-not $asset) { Warn "no roomler-desktop asset found in recent agent releases -- skipped"; return }
        $exe = Join-Path $stage 'roomler-desktop.exe'
        Say ("downloading " + $asset.name)
        Invoke-WebRequest -UseBasicParsing $asset.browser_download_url -OutFile $exe
        Assert-Sha256 -File $exe -Digest $asset.digest
        Copy-Item $exe (Join-Path $InstallDir 'roomler-desktop.exe') -Force
        Say "placed roomler-desktop.exe in $InstallDir"
    } catch {
        Warn "roomler-desktop install skipped: $_"
    }
}

# --- tunnel-client role: CLI zip + user PATH --------------------------------

function Install-TunnelClient {
    Say "resolving the CLI zip via $Server/api/tunnel/installer/windows-x86_64/health"
    $health = Invoke-RestMethod -UseBasicParsing "$Server/api/tunnel/installer/windows-x86_64/health?version=latest"
    $zip = Join-Path $stage $health.filename
    Say ("downloading " + $health.filename + " (tag " + $health.tag + ")")
    Invoke-WebRequest -UseBasicParsing ($Server + $health.uri) -OutFile $zip
    Assert-Sha256 -File $zip -Digest $health.digest

    # The roomler-setup wizard's canonical per-user install root -- a
    # script install lands where the wizard (and its detect) expects.
    $installDir = Join-Path $env:LOCALAPPDATA 'roomler\roomler-tunnel\Programs\roomler-tunnel'

    if ($DownloadOnly) {
        Say "download-only: would extract to $installDir, append it to the user PATH,"
        Say "download-only: then enroll via: roomler.exe enroll --server $Server --token <token> --name '$Name'"
        return
    }

    New-Item -ItemType Directory -Path $installDir -Force | Out-Null
    Expand-Archive -Path $zip -DestinationPath $installDir -Force
    # Archives ship BOTH names since the P3d rename; prefer 'roomler'.
    $cli = $null
    foreach ($n in @('roomler.exe', 'roomler-tunnel.exe')) {
        $found = Get-ChildItem -Path $installDir -Filter $n -Recurse | Select-Object -First 1
        if ($found) { $cli = $found.FullName; break }
    }
    if (-not $cli) { throw "no roomler.exe / roomler-tunnel.exe in the extracted archive" }
    $cliDir = Split-Path -Parent $cli
    Say "installed: $cli"

    $userPath = [Environment]::GetEnvironmentVariable('PATH', 'User')
    if (-not ($userPath -split ';' | Where-Object { $_ -eq $cliDir })) {
        [Environment]::SetEnvironmentVariable('PATH', ($userPath.TrimEnd(';') + ';' + $cliDir), 'User')
        Say "appended $cliDir to the user PATH (new shells)"
    }
    $env:PATH = "$env:PATH;$cliDir"

    if ($NoEnroll -or -not $Token) {
        if (-not $NoEnroll) { Warn "no -Token given -- skipping enrollment" }
        Say "enroll later with: & '$cli' enroll --server $Server --token <tunnel-enrollment-jwt> --name '$Name'"
        return
    }
    Say "enrolling this tunnel client as '$Name' against $Server (token is single-use, never echoed)"
    & $cli enroll --server $Server --token $Token --name $Name
    if ($LASTEXITCODE -ne 0) { throw "enrollment failed (exit $LASTEXITCODE)" }
}

# --- main -------------------------------------------------------------------

try {
    if ($Role -eq 'tunnel-client') { Install-TunnelClient } else { Install-Daemon }
    Say "done."
} finally {
    Remove-Item -Recurse -Force $stage -ErrorAction SilentlyContinue
}
