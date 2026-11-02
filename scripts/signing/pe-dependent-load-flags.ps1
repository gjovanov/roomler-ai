# pe-dependent-load-flags.ps1 -- read DependentLoadFlags from a built EXE's own
# bytes and assert it.
#
#   pwsh scripts/signing/pe-dependent-load-flags.ps1 -Path roomler-setup.exe -Expect 0x800
#
# FR-129 C: the wizard EXE sits in the person's Downloads and goes on to run
# elevated, so its own imports must come from System32 alone. build.rs asks the
# linker for that (/DEPENDENTLOADFLAG:0x800, LOAD_LIBRARY_SEARCH_SYSTEM32), and
# a string in build.rs proves nothing about the file: a linker that drops the
# flag, a build script that stops running or a target it does not match all
# leave the source as it was. This reads the field the loader reads, which is
# IMAGE_LOAD_CONFIG_DIRECTORY64.DependentLoadFlags at offset 0x4E. A file with no
# load configuration, or one too short to hold the field, reads as 0.
#
# Run by CI's Windows job on the debug EXE, by release-setup.yml on the EXE it
# signs, and by 90-verify-release.ps1 on the published one.

param(
    [Parameter(Mandatory)][string]$Path,
    [string]$Expect = ''
)

$ErrorActionPreference = 'Stop'

function Get-DependentLoadFlags {
    param([Parameter(Mandatory)][string]$File)
    $b = [IO.File]::ReadAllBytes($File)
    if ($b.Length -lt 0x40 -or [BitConverter]::ToUInt16($b, 0) -ne 0x5A4D) { throw "$File is not a PE file" }
    $pe = [BitConverter]::ToUInt32($b, 0x3C)
    if ([BitConverter]::ToUInt32($b, $pe) -ne 0x00004550) { throw "$File has no PE signature" }
    $sections = [BitConverter]::ToUInt16($b, $pe + 6)
    $optional = $pe + 24
    $optionalSize = [BitConverter]::ToUInt16($b, $pe + 20)
    if ([BitConverter]::ToUInt16($b, $optional) -ne 0x20B) { throw "$File is not a PE32+ image" }
    # PE32+: the data directories start at 112, and Load Config is number 10.
    $directory = $optional + 112 + 10 * 8
    $rva = [BitConverter]::ToUInt32($b, $directory)
    if ($rva -eq 0) { return 0 }
    $offset = -1
    for ($i = 0; $i -lt $sections; $i++) {
        $s = $optional + $optionalSize + 40 * $i
        $virtualSize = [BitConverter]::ToUInt32($b, $s + 8)
        $virtualAddress = [BitConverter]::ToUInt32($b, $s + 12)
        $rawSize = [BitConverter]::ToUInt32($b, $s + 16)
        $rawAddress = [BitConverter]::ToUInt32($b, $s + 20)
        if ($rva -ge $virtualAddress -and $rva -lt $virtualAddress + [Math]::Max($virtualSize, $rawSize)) {
            $offset = $rawAddress + ($rva - $virtualAddress)
            break
        }
    }
    if ($offset -lt 0) { throw "$File has a load configuration in no section" }
    if ([BitConverter]::ToUInt32($b, $offset) -lt 0x50) { return 0 }
    return [BitConverter]::ToUInt16($b, $offset + 0x4E)
}

$flags = Get-DependentLoadFlags -File $Path
$shown = '0x{0:X}' -f $flags
if ($Expect -eq '') {
    Write-Host "$Path DependentLoadFlags = $shown"
    exit 0
}
if ($flags -ne [Convert]::ToInt32($Expect, 16)) {
    Write-Host "$Path DependentLoadFlags = $shown, expected $Expect" -ForegroundColor Red
    exit 1
}
Write-Host "OK  $Path DependentLoadFlags = $shown" -ForegroundColor Green
