# pe-dependent-load-flags.ps1 -- read DependentLoadFlags and the static imports
# from a built EXE's own bytes and assert them.
#
#   pwsh scripts/signing/pe-dependent-load-flags.ps1 -Path roomler-setup.exe -Expect 0x800 -NotImported secur32.dll
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
# The flag covers the EXE's own imports, not an import's imports: secur32.dll's
# SSPICLI.DLL came from the EXE's folder with the flag set, before main can run.
# So build.rs delay-loads secur32.dll, and -NotImported fails a file whose import
# table still names it. A delay-loaded DLL has its own table and does not count.
#
# Run by CI's Windows job on the debug EXE, by release-setup.yml on the EXE it
# signs, and by 90-verify-release.ps1 on the published one.

param(
    [Parameter(Mandatory)][string]$Path,
    [string]$Expect = '',
    [string[]]$NotImported = @()
)

$ErrorActionPreference = 'Stop'

function Read-Pe {
    param([Parameter(Mandatory)][string]$File)
    $b = [IO.File]::ReadAllBytes($File)
    if ($b.Length -lt 0x40 -or [BitConverter]::ToUInt16($b, 0) -ne 0x5A4D) { throw "$File is not a PE file" }
    $pe = [BitConverter]::ToUInt32($b, 0x3C)
    if ([BitConverter]::ToUInt32($b, $pe) -ne 0x00004550) { throw "$File has no PE signature" }
    $optional = $pe + 24
    if ([BitConverter]::ToUInt16($b, $optional) -ne 0x20B) { throw "$File is not a PE32+ image" }
    return @{
        Bytes        = $b
        Optional     = $optional
        OptionalSize = [BitConverter]::ToUInt16($b, $pe + 20)
        Sections     = [BitConverter]::ToUInt16($b, $pe + 6)
    }
}

# The file offset of an address in the image, or -1 if no section holds it.
function Get-FileOffset {
    param([Parameter(Mandatory)]$Image, [Parameter(Mandatory)][uint32]$Rva)
    $b = $Image.Bytes
    for ($i = 0; $i -lt $Image.Sections; $i++) {
        $s = $Image.Optional + $Image.OptionalSize + 40 * $i
        $virtualSize = [BitConverter]::ToUInt32($b, $s + 8)
        $virtualAddress = [BitConverter]::ToUInt32($b, $s + 12)
        $rawSize = [BitConverter]::ToUInt32($b, $s + 16)
        $rawAddress = [BitConverter]::ToUInt32($b, $s + 20)
        if ($Rva -ge $virtualAddress -and $Rva -lt $virtualAddress + [Math]::Max($virtualSize, $rawSize)) {
            return $rawAddress + ($Rva - $virtualAddress)
        }
    }
    return -1
}

# PE32+: the data directories start at 112 in the optional header.
function Get-DirectoryRva {
    param([Parameter(Mandatory)]$Image, [Parameter(Mandatory)][int]$Number)
    return [BitConverter]::ToUInt32($Image.Bytes, $Image.Optional + 112 + $Number * 8)
}

function Get-DependentLoadFlags {
    param([Parameter(Mandatory)]$Image, [Parameter(Mandatory)][string]$File)
    $rva = Get-DirectoryRva -Image $Image -Number 10
    if ($rva -eq 0) { return 0 }
    $offset = Get-FileOffset -Image $Image -Rva $rva
    if ($offset -lt 0) { throw "$File has a load configuration in no section" }
    if ([BitConverter]::ToUInt32($Image.Bytes, $offset) -lt 0x50) { return 0 }
    return [BitConverter]::ToUInt16($Image.Bytes, $offset + 0x4E)
}

# The names in the import table, which the loader resolves before main runs.
function Get-StaticImports {
    param([Parameter(Mandatory)]$Image, [Parameter(Mandatory)][string]$File)
    $b = $Image.Bytes
    $names = @()
    $rva = Get-DirectoryRva -Image $Image -Number 1
    if ($rva -eq 0) { return $names }
    $entry = Get-FileOffset -Image $Image -Rva $rva
    if ($entry -lt 0) { throw "$File has an import table in no section" }
    while ($true) {
        $nameRva = [BitConverter]::ToUInt32($b, $entry + 12)
        if ($nameRva -eq 0) { break }
        $start = Get-FileOffset -Image $Image -Rva $nameRva
        if ($start -lt 0) { throw "$File has an import name in no section" }
        $end = [Array]::IndexOf($b, [byte]0, $start)
        $names += [Text.Encoding]::ASCII.GetString($b, $start, $end - $start)
        $entry += 20
    }
    return $names
}

$image = Read-Pe -File $Path
$flags = Get-DependentLoadFlags -Image $image -File $Path
$shown = '0x{0:X}' -f $flags
if ($Expect -eq '') {
    Write-Host "$Path DependentLoadFlags = $shown"
} elseif ($flags -ne [Convert]::ToInt32($Expect, 16)) {
    Write-Host "$Path DependentLoadFlags = $shown, expected $Expect" -ForegroundColor Red
    exit 1
} else {
    Write-Host "OK  $Path DependentLoadFlags = $shown" -ForegroundColor Green
}

$imports = Get-StaticImports -Image $image -File $Path
# One quoted comma-separated argument: a `pwsh <script>` call binds no array.
$NotImported = @($NotImported | ForEach-Object { $_ -split ',' } | Where-Object { $_ })
$found = @($NotImported | Where-Object { $imports -contains $_ })
if ($found.Count -gt 0) {
    Write-Host "$Path has $($found -join ', ') in its import table: it must be delay-loaded" -ForegroundColor Red
    exit 1
}
if ($NotImported.Count -gt 0) {
    Write-Host "OK  $Path does not load $($NotImported -join ', ') before main" -ForegroundColor Green
}
exit 0
