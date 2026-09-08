#Requires -Version 5.1
[CmdletBinding()]
param([string]$OutputDirectory)
$ErrorActionPreference = 'Stop'
$root = Split-Path -Parent $PSScriptRoot
if (-not $OutputDirectory) { $OutputDirectory = Join-Path $root 'dist' }
$OutputDirectory = [IO.Path]::GetFullPath($OutputDirectory)
Push-Location $root
try {
    $metadataText = & cargo metadata --no-deps --locked --format-version 1
    if ($LASTEXITCODE -ne 0) { throw 'Could not read Cargo package metadata.' }
    $metadata = $metadataText | ConvertFrom-Json
    $version = ($metadata.packages | Where-Object name -eq 'dirveyor').version
    if ($version -notmatch '^\d+\.\d+\.\d+(?:-[0-9A-Za-z.-]+)?$') { throw 'Invalid package version.' }
    & cargo build --release --locked --target x86_64-pc-windows-msvc -p dirveyor
    if ($LASTEXITCODE -ne 0) { throw 'Windows release build failed.' }
    [void][IO.Directory]::CreateDirectory($OutputDirectory)
    $name = "dirveyor-$version-windows-x86_64.zip"
    $zipPath = Join-Path $OutputDirectory $name
    $stagedZip = "$zipPath.$([Guid]::NewGuid().ToString('N')).tmp"
    Add-Type -AssemblyName System.IO.Compression
    Add-Type -AssemblyName System.IO.Compression.FileSystem
    $zip = [IO.Compression.ZipFile]::Open($stagedZip, [IO.Compression.ZipArchiveMode]::Create)
    try {
        $exe = Join-Path $metadata.target_directory 'x86_64-pc-windows-msvc\release\dirveyor.exe'
        [void][IO.Compression.ZipFileExtensions]::CreateEntryFromFile($zip, $exe, 'dirveyor.exe')
        [void][IO.Compression.ZipFileExtensions]::CreateEntryFromFile($zip, (Join-Path $root 'LICENSE'), 'LICENSE')
        [void][IO.Compression.ZipFileExtensions]::CreateEntryFromFile($zip, (Join-Path $root 'docs\WINDOWS_PACKAGE.md'), 'README.md')
    } finally { $zip.Dispose() }
    Move-Item -LiteralPath $stagedZip -Destination $zipPath -Force
    $hash = (Get-FileHash -LiteralPath $zipPath -Algorithm SHA256).Hash.ToLowerInvariant()
    [IO.File]::WriteAllText("$zipPath.sha256", "$hash  $name`n", [Text.UTF8Encoding]::new($false))
    Write-Host "Package: $zipPath"
    Write-Host "Checksum: $zipPath.sha256"
} finally {
    if ($stagedZip -and [IO.File]::Exists($stagedZip)) { [IO.File]::Delete($stagedZip) }
    Pop-Location
}
