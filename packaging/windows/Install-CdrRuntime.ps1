# Install the pinned CDR backend beside a desktop/CLI build. No system installation or PATH edits.
[CmdletBinding()]
param(
    [string]$TargetDirectory = (Join-Path $PSScriptRoot '..\..\target\debug\cdr')
)
$ErrorActionPreference = 'Stop'
$ProgressPreference = 'SilentlyContinue'
$version = 'v0.2.1'
$archiveHash = '955a9b0bdb13e0e10801abd2de9efbccde1a584b6595ceb1df24630c81bebc4a'
$url = "https://github.com/WaltRiceJr/inkscape-extension-cdr/releases/download/$version/inkscape-cdr-extension-windows-x86_64.zip"
$destination = [IO.Path]::GetFullPath($TargetDirectory)
$work = Join-Path ([IO.Path]::GetTempPath()) ('vectorcraft-cdr-install-' + [Guid]::NewGuid().ToString('N'))
New-Item -ItemType Directory -Path $work | Out-Null
try {
    $archive = Join-Path $work 'runtime.zip'
    Invoke-WebRequest -Uri $url -OutFile $archive -UseBasicParsing
    if ((Get-FileHash -LiteralPath $archive -Algorithm SHA256).Hash.ToLowerInvariant() -ne $archiveHash) {
        throw 'CDR runtime SHA-256 verification failed; nothing was installed.'
    }
    Expand-Archive -LiteralPath $archive -DestinationPath (Join-Path $work 'unpacked')
    $source = Join-Path $work 'unpacked\inkscape-cdr-extension\bin'
    $files = @('cdr2svg.exe', 'libgcc_s_seh-1.dll', 'libicudt78.dll', 'libicuin78.dll',
        'libicuuc78.dll', 'liblcms2-2.dll', 'libstdc++-6.dll', 'libwinpthread-1.dll', 'zlib1.dll')
    foreach ($file in $files) {
        if (!(Test-Path -LiteralPath (Join-Path $source $file) -PathType Leaf)) {
            throw "Verified runtime is missing $file"
        }
    }
    New-Item -ItemType Directory -Path $destination -Force | Out-Null
    foreach ($file in $files) {
        Copy-Item -LiteralPath (Join-Path $source $file) -Destination (Join-Path $destination $file) -Force
    }
    @"
CDR converter: inkscape-extension-cdr $version
Archive: $url
SHA-256: $archiveHash
Corresponding source: https://github.com/WaltRiceJr/inkscape-extension-cdr/tree/$version
libcdr and librevenge: Mozilla Public License 2.0 (librevenge also offers LGPL 2.1).
ICU: Unicode License; lcms2: MIT; zlib: zlib license.
GCC runtime: GPL with GCC Runtime Library Exception; winpthreads: MIT/BSD.
This is the converter backend only. The Inkscape application and its extension scripts are not installed.
Before redistribution, retain corresponding component licenses and source offers; see docs/cdr-import.md.
"@ | Set-Content -LiteralPath (Join-Path $destination 'SOURCE.txt') -Encoding UTF8
    Write-Output "CDR runtime $version installed and verified: $destination"
} finally {
    Remove-Item -LiteralPath $work -Recurse -Force -ErrorAction SilentlyContinue
}
