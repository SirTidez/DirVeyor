#Requires -Version 5.1
# Offline integration tests. No real GitHub downloads, app execution, or PATH writes.
param([string]$PackagePath)
$ErrorActionPreference = 'Stop'
$root = Split-Path -Parent $PSScriptRoot
. (Join-Path $root 'install.ps1')
Add-Type -AssemblyName System.IO.Compression
Add-Type -AssemblyName System.IO.Compression.FileSystem
$testRoot = Join-Path ([IO.Path]::GetTempPath()) ('dirveyor-installer-tests-' + [Guid]::NewGuid().ToString('N'))
[void][IO.Directory]::CreateDirectory($testRoot)
$oldLocalData = $env:LOCALAPPDATA
$oldProcessPath = $env:Path
$oldUserPath = [Environment]::GetEnvironmentVariable('Path', 'User')
$env:LOCALAPPDATA = Join-Path $testRoot 'local'
$script:ApiFailure = $false
$script:BadChecksum = $false
$script:DownloadFailure = $false
$script:Passed = 0

function Assert-True($Condition, [string]$Message) {
    if (-not $Condition) { throw "Assertion failed: $Message" }
}
function Expect-Failure([scriptblock]$Action, [string]$MessagePattern) {
    $failed = $false
    try { & $Action } catch {
        $failed = $true
        Assert-True ($_.Exception.Message -like $MessagePattern) "unexpected error: $($_.Exception.Message)"
    }
    Assert-True $failed 'expected the operation to fail'
}
function New-TestRelease([string]$Version, [string]$Payload, [string]$ExtraEntry) {
    $script:AssetName = "dirveyor-$Version-windows-x86_64.zip"
    $script:Archive = Join-Path $testRoot ($script:AssetName + '.' + [Guid]::NewGuid().ToString('N'))
    $zip = [IO.Compression.ZipFile]::Open($script:Archive, [IO.Compression.ZipArchiveMode]::Create)
    try {
        $entries = @{ 'dirveyor.exe' = $Payload; 'LICENSE' = 'MIT'; 'README.md' = 'Package instructions' }
        if ($ExtraEntry) { $entries[$ExtraEntry] = 'unexpected' }
        foreach ($name in $entries.Keys) {
            $entry = $zip.CreateEntry($name)
            $writer = [IO.StreamWriter]::new($entry.Open(), [Text.UTF8Encoding]::new($false))
            try { $writer.Write($entries[$name]) } finally { $writer.Dispose() }
        }
    } finally { $zip.Dispose() }
    $script:Checksum = "$script:Archive.sha256"
    $hash = (Get-FileHash -LiteralPath $script:Archive -Algorithm SHA256).Hash
    [IO.File]::WriteAllText($script:Checksum, "$hash  $script:AssetName`n")
    $base = "https://github.com/SirTidez/DirVeyor/releases/download/v$Version"
    $script:MockRelease = [PSCustomObject]@{
        tag_name = "v$Version"; draft = $false; prerelease = $false
        assets = @(
            [PSCustomObject]@{ name = $script:AssetName; browser_download_url = "$base/$script:AssetName" },
            [PSCustomObject]@{ name = "$script:AssetName.sha256"; browser_download_url = "$base/$script:AssetName.sha256" }
        )
    }
}
function Invoke-RestMethod {
    param($Uri, $Headers, $TimeoutSec)
    Assert-True ($Uri -eq 'https://api.github.com/repos/SirTidez/DirVeyor/releases/latest') 'latest-release endpoint'
    if ($script:ApiFailure) { throw 'simulated API failure' }
    return $script:MockRelease
}
function Invoke-WebRequest {
    param($Uri, $Headers, $OutFile, $TimeoutSec, [switch]$UseBasicParsing)
    if ($script:DownloadFailure) { throw 'simulated interrupted download' }
    if ($Uri.EndsWith('.sha256')) {
        if ($script:BadChecksum) { [IO.File]::WriteAllText($OutFile, (('0' * 64) + "  $script:AssetName")) }
        else { [IO.File]::Copy($script:Checksum, $OutFile) }
    } else { [IO.File]::Copy($script:Archive, $OutFile) }
}
function Test-Case([string]$Name, [scriptblock]$Action) {
    & $Action
    $script:Passed++
    Write-Host "PASS: $Name"
}

try {
    Test-Case 'PATH updates preserve entries and avoid duplicate expanded paths' {
        $directory = Join-Path $env:LOCALAPPDATA 'Programs\DirVeyor'
        Assert-True ((Add-DirVeyorPathEntry '' $directory) -eq $directory) 'empty PATH'
        Assert-True ((Add-DirVeyorPathEntry 'C:\Tools;' $directory) -eq "C:\Tools;$directory") 'preserves existing entries'
        $expanded = 'C:\Tools;%LOCALAPPDATA%\Programs\DirVeyor\'
        Assert-True ((Add-DirVeyorPathEntry $expanded $directory) -ceq $expanded) 'expanded duplicate'
        Assert-True ((Add-DirVeyorPathEntry $directory.ToUpperInvariant() $directory) -ceq $directory.ToUpperInvariant()) 'case-insensitive duplicate'
    }
    $custom = Join-Path $testRoot 'custom install'
    $exe = Join-Path $custom 'dirveyor.exe'
    $settings = Join-Path $custom 'user-settings.json'
    New-TestRelease '0.1.0' 'MZ-version-one'
    Test-Case 'fresh installation and checksum validation' {
        Install-DirVeyor -InstallDir $custom -NoPath
        Assert-True ([IO.File]::ReadAllText($exe) -eq 'MZ-version-one') 'installed payload'
        Assert-True ([IO.File]::Exists((Join-Path $custom 'DirVeyor-LICENSE.txt'))) 'installed license'
        [IO.File]::WriteAllText($settings, 'preserve me')
    }
    New-TestRelease '0.2.0' 'MZ-version-two'
    Test-Case 'update remembers custom directory and NoPath preference' {
        Install-DirVeyor
        Assert-True ([IO.File]::ReadAllText($exe) -eq 'MZ-version-two') 'updated payload'
        Assert-True ($env:Path -eq $oldProcessPath) 'process PATH unchanged'
        Assert-True ([Environment]::GetEnvironmentVariable('Path', 'User') -eq $oldUserPath) 'user PATH unchanged'
    }
    Test-Case 'same-version repair restores corruption and missing support files' {
        [IO.File]::WriteAllText($exe, 'damaged')
        [IO.File]::Delete((Join-Path $custom 'DirVeyor-LICENSE.txt'))
        Install-DirVeyor -Repair
        Assert-True ([IO.File]::ReadAllText($exe) -eq 'MZ-version-two') 'repaired executable'
        Assert-True ([IO.File]::Exists((Join-Path $custom 'DirVeyor-LICENSE.txt'))) 'repaired license'
        Assert-True ([IO.File]::ReadAllText($settings) -eq 'preserve me') 'user data preserved'
    }
    Test-Case 'repair restores a missing executable using the install record' {
        [IO.File]::Delete($exe)
        Install-DirVeyor -Repair
        Assert-True ([IO.File]::ReadAllText($exe) -eq 'MZ-version-two') 'restored missing executable'
    }
    Test-Case 'checksum mismatch leaves installation intact' {
        $script:BadChecksum = $true
        try { Expect-Failure { Install-DirVeyor } '*checksum mismatch*' }
        finally { $script:BadChecksum = $false }
        Assert-True ([IO.File]::ReadAllText($exe) -eq 'MZ-version-two') 'old executable retained'
    }
    Test-Case 'missing release assets leave installation intact' {
        $assets = $script:MockRelease.assets
        $script:MockRelease.assets = @()
        try { Expect-Failure { Install-DirVeyor } '*missing its Windows x64 ZIP or checksum*' }
        finally { $script:MockRelease.assets = $assets }
    }
    Test-Case 'unavailable release and interrupted downloads leave installation intact' {
        $script:ApiFailure = $true
        try { Expect-Failure { Install-DirVeyor } '*Cannot find the latest stable*' }
        finally { $script:ApiFailure = $false }
        $script:DownloadFailure = $true
        try { Expect-Failure { Install-DirVeyor } '*interrupted download*' }
        finally { $script:DownloadFailure = $false }
        Assert-True ([IO.File]::ReadAllText($exe) -eq 'MZ-version-two') 'old executable retained'
    }
    Test-Case 'unexpected archive paths are rejected before replacement' {
        New-TestRelease '0.2.0' 'MZ-malicious-package' '../outside.txt'
        Expect-Failure { Install-DirVeyor } '*unexpected or oversized entries*'
        Assert-True ([IO.File]::ReadAllText($exe) -eq 'MZ-version-two') 'old executable retained'
    }
    New-TestRelease '0.2.0' 'MZ-version-two'
    Test-Case 'locked executable fails without losing the previous version' {
        $locked = [IO.File]::Open($exe, [IO.FileMode]::Open, [IO.FileAccess]::Read, [IO.FileShare]::Read)
        try { Expect-Failure { Install-DirVeyor } '*Could not replace dirveyor.exe*' }
        finally { $locked.Dispose() }
        Assert-True ([IO.File]::ReadAllText($exe) -eq 'MZ-version-two') 'locked executable retained'
    }
    Test-Case 'download-and-execute entry point performs an idempotent update' {
        Invoke-Expression ([IO.File]::ReadAllText((Join-Path $root 'install.ps1')))
        Assert-True ([IO.File]::ReadAllText($exe) -eq 'MZ-version-two') 'entry point installed'
        Assert-True ([IO.File]::ReadAllText($settings) -eq 'preserve me') 'user data preserved'
    }
    if ($PackagePath) {
        Test-Case 'real release package installs with matching executable bytes' {
            $PackagePath = (Resolve-Path -LiteralPath $PackagePath).Path
            $packageName = [IO.Path]::GetFileName($PackagePath)
            Assert-True ($packageName -match '^dirveyor-(\d+\.\d+\.\d+(?:-[0-9A-Za-z.-]+)?)-windows-x86_64\.zip$') 'package filename'
            New-TestRelease $Matches[1] 'MZ-placeholder'
            $script:Archive = $PackagePath
            $script:Checksum = "$PackagePath.sha256"
            Install-DirVeyor -Repair
            $package = [IO.Compression.ZipFile]::OpenRead($PackagePath)
            try {
                $expectedExe = Join-Path $testRoot 'expected-dirveyor.exe'
                [IO.Compression.ZipFileExtensions]::ExtractToFile($package.GetEntry('dirveyor.exe'), $expectedExe, $false)
                Assert-True ((Get-FileHash -LiteralPath $expectedExe).Hash -eq (Get-FileHash -LiteralPath $exe).Hash) 'installed executable matches package'
            } finally { $package.Dispose() }
        }
    }
    Write-Host "$script:Passed installer tests passed."
} finally {
    $env:LOCALAPPDATA = $oldLocalData
    $env:Path = $oldProcessPath
    $resolved = [IO.Path]::GetFullPath($testRoot)
    $tempRoot = [IO.Path]::GetFullPath([IO.Path]::GetTempPath()).TrimEnd('\') + '\'
    if (-not $resolved.StartsWith($tempRoot, [StringComparison]::OrdinalIgnoreCase) -or
        [IO.Path]::GetFileName($resolved) -notlike 'dirveyor-installer-tests-*') {
        throw 'Refusing cleanup outside the test temporary directory.'
    }
    Remove-Item -LiteralPath $resolved -Recurse -Force
}
