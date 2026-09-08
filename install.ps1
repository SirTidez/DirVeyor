#Requires -Version 5.1
<#
.SYNOPSIS
Installs the latest stable DirVeyor Windows x64 release for the current user.
.EXAMPLE
irm https://raw.githubusercontent.com/SirTidez/DirVeyor/main/install.ps1 | iex
.EXAMPLE
.\install.ps1 -InstallDir "$env:LOCALAPPDATA\Programs\DirVeyor" -NoPath
#>
[CmdletBinding()]
param([string]$InstallDir, [switch]$NoPath, [switch]$Repair)

function Add-DirVeyorPathEntry {
    param([string]$CurrentPath, [string]$Directory)
    $alreadyPresent = @($CurrentPath -split ';' | Where-Object {
        $_ -and [Environment]::ExpandEnvironmentVariables($_).TrimEnd('\') -ieq $Directory.TrimEnd('\')
    }).Count -gt 0
    if ($alreadyPresent) { return $CurrentPath }
    if ([string]::IsNullOrWhiteSpace($CurrentPath)) { return $Directory }
    return "$($CurrentPath.TrimEnd(';'));$Directory"
}

function Install-DirVeyor {
    [CmdletBinding()]
    param([string]$InstallDir, [switch]$NoPath, [switch]$Repair)

    $ErrorActionPreference = 'Stop'
    if ([Environment]::OSVersion.Platform -ne [PlatformID]::Win32NT) {
        throw 'This installer supports Windows x64 only.'
    }
    $architecture = $env:PROCESSOR_ARCHITEW6432
    if (-not $architecture) { $architecture = $env:PROCESSOR_ARCHITECTURE }
    if ($architecture -ne 'AMD64') { throw 'DirVeyor currently provides Windows x64 releases only.' }
    $repository = 'SirTidez/DirVeyor'
    $localData = $env:LOCALAPPDATA
    if (-not $localData) { $localData = [Environment]::GetFolderPath('LocalApplicationData') }
    $recordPath = Join-Path $localData 'DirVeyor\installer\installation.json'
    $record = $null
    if ([IO.File]::Exists($recordPath)) {
        try {
            $candidate = [IO.File]::ReadAllText($recordPath) | ConvertFrom-Json
            if ($candidate.schemaVersion -eq 1 -and $candidate.repository -eq $repository -and
                [IO.Path]::IsPathRooted([string]$candidate.installDir)) { $record = $candidate }
        } catch { Write-Warning 'The installer record is damaged; falling back to installation discovery.' }
    }
    if (-not $InstallDir -and $record) { $InstallDir = [string]$record.installDir }
    if (-not $InstallDir) {
        $existing = Get-Command dirveyor.exe -CommandType Application -ErrorAction SilentlyContinue | Select-Object -First 1
        if ($existing) { $InstallDir = Split-Path -Parent $existing.Source }
    }
    if (-not $InstallDir) { $InstallDir = Join-Path $localData 'Programs\DirVeyor' }
    $InstallDir = [IO.Path]::GetFullPath($InstallDir)
    if ($record -and $record.installDir -ieq $InstallDir -and
        -not $PSBoundParameters.ContainsKey('NoPath') -and $record.addToPath -is [bool]) {
        $NoPath = -not $record.addToPath
    }
    if (-not $NoPath -and $InstallDir.Contains(';')) {
        throw 'An install path containing a semicolon cannot be added to PATH. Choose another path or use -NoPath.'
    }
    $headers = @{ 'User-Agent' = 'DirVeyor-Installer'; 'Accept' = 'application/vnd.github+json' }
    $previousTls = [Net.ServicePointManager]::SecurityProtocol
    $work = $null
    $stagedExe = $null
    $backupExe = $null
    $replacementSucceeded = $false
    try {
        [Net.ServicePointManager]::SecurityProtocol = $previousTls -bor [Net.SecurityProtocolType]::Tls12
        try {
            $release = Invoke-RestMethod -Uri "https://api.github.com/repos/$repository/releases/latest" -Headers $headers -TimeoutSec 30
        } catch {
            throw "Cannot find the latest stable DirVeyor release. Check your connection and https://github.com/$repository/releases. A stable release may not have been published yet. $($_.Exception.Message)"
        }
        $version = [string]$release.tag_name
        if ($release.draft -or $release.prerelease -or $version -notmatch '^v?\d+\.\d+\.\d+(?:-[0-9A-Za-z.-]+)?$') {
            throw 'GitHub returned an unsupported release. No files were installed.'
        }
        $version = $version -replace '^v', ''
        $assetName = "dirveyor-$version-windows-x86_64.zip"
        $archives = @($release.assets | Where-Object { $_.name -ceq $assetName })
        $checksums = @($release.assets | Where-Object { $_.name -ceq "$assetName.sha256" })
        if ($archives.Count -ne 1 -or $checksums.Count -ne 1) {
            throw "The release is missing its Windows x64 ZIP or checksum: $assetName. No files were installed."
        }
        foreach ($asset in @($archives[0], $checksums[0])) {
            $uri = [Uri]$asset.browser_download_url
            if ($uri.Scheme -ne 'https' -or $uri.Host -ne 'github.com' -or
                -not $uri.AbsolutePath.StartsWith("/$repository/releases/download/", [StringComparison]::Ordinal)) {
                throw 'Release asset URL does not belong to the expected GitHub repository.'
            }
        }
        $work = Join-Path ([IO.Path]::GetTempPath()) ('dirveyor-install-' + [Guid]::NewGuid().ToString('N'))
        [void][IO.Directory]::CreateDirectory($work)
        $archivePath = Join-Path $work $assetName
        $checksumPath = "$archivePath.sha256"
        $operation = if ($Repair) { 'Repairing' } elseif ([IO.File]::Exists((Join-Path $InstallDir 'dirveyor.exe'))) { 'Updating' } else { 'Installing' }
        Write-Host "$operation DirVeyor with the latest release ($version)..."
        Invoke-WebRequest -UseBasicParsing -Uri $archives[0].browser_download_url -Headers $headers -OutFile $archivePath -TimeoutSec 300
        Invoke-WebRequest -UseBasicParsing -Uri $checksums[0].browser_download_url -Headers $headers -OutFile $checksumPath -TimeoutSec 30
        $checksum = [IO.File]::ReadAllText($checksumPath).Trim()
        if ($checksum -notmatch ('\A([a-fA-F0-9]{64})[ \t]+\*?' + [Regex]::Escape($assetName) + '\z')) {
            throw 'The release checksum file is invalid. No files were installed.'
        }
        $expectedHash = $Matches[1]
        if ((Get-FileHash -LiteralPath $archivePath -Algorithm SHA256).Hash -ne $expectedHash) {
            throw 'SHA-256 checksum mismatch. No files were installed.'
        }
        Add-Type -AssemblyName System.IO.Compression
        Add-Type -AssemblyName System.IO.Compression.FileSystem
        $zip = [IO.Compression.ZipFile]::OpenRead($archivePath)
        $files = @{}
        try {
            # Only the three flat package entries are accepted. Do not extract
            # arbitrary archive paths, symlinks, duplicates, or nested content.
            foreach ($entry in $zip.Entries) {
                if ($entry.FullName -cnotin @('dirveyor.exe', 'LICENSE', 'README.md') -or
                    $files.ContainsKey($entry.FullName) -or $entry.Length -gt 128MB) {
                    throw 'The release archive contains unexpected or oversized entries.'
                }
                $destination = Join-Path $work $entry.FullName
                [IO.Compression.ZipFileExtensions]::ExtractToFile($entry, $destination, $false)
                $files[$entry.FullName] = $destination
            }
            if ($files.Count -ne 3) { throw 'The release archive is incomplete.' }
        } finally { $zip.Dispose() }
        $stream = [IO.File]::OpenRead($files['dirveyor.exe'])
        try {
            if ($stream.ReadByte() -ne 0x4D -or $stream.ReadByte() -ne 0x5A) {
                throw 'The package does not contain a Windows executable.'
            }
        } finally { $stream.Dispose() }

        # Stage on the destination volume before replacing the executable.
        # A failed replacement (e.g. an app that is still running) keeps the old file.
        [void][IO.Directory]::CreateDirectory($InstallDir)
        $suffix = [Guid]::NewGuid().ToString('N')
        $stagedExe = Join-Path $InstallDir ".dirveyor-$suffix.tmp"
        $backupExe = Join-Path $InstallDir ".dirveyor-$suffix.bak"
        $exePath = Join-Path $InstallDir 'dirveyor.exe'
        [IO.File]::Copy($files['dirveyor.exe'], $stagedExe)
        try {
            if ([IO.File]::Exists($exePath)) {
                [IO.File]::Replace($stagedExe, $exePath, $backupExe)
            } else { [IO.File]::Move($stagedExe, $exePath) }
            $replacementSucceeded = $true
        } catch {
            throw "Could not replace dirveyor.exe. Close DirVeyor and retry. $($_.Exception.Message)"
        }
        Copy-Item -LiteralPath $files['LICENSE'] -Destination (Join-Path $InstallDir 'DirVeyor-LICENSE.txt') -Force
        Copy-Item -LiteralPath $files['README.md'] -Destination (Join-Path $InstallDir 'DirVeyor-README.md') -Force
        [void][IO.Directory]::CreateDirectory((Split-Path -Parent $recordPath))
        $recordJson = @{ schemaVersion = 1; repository = $repository; installDir = $InstallDir;
            version = $version; addToPath = (-not $NoPath) } | ConvertTo-Json
        [IO.File]::WriteAllText($recordPath, $recordJson, [Text.UTF8Encoding]::new($false))
        if (-not $NoPath) {
            $userPath = [Environment]::GetEnvironmentVariable('Path', 'User')
            $newPath = Add-DirVeyorPathEntry -CurrentPath $userPath -Directory $InstallDir
            if ($newPath -cne $userPath) {
                [Environment]::SetEnvironmentVariable('Path', $newPath, 'User')
            }
            $env:Path = Add-DirVeyorPathEntry -CurrentPath $env:Path -Directory $InstallDir
        }
        Write-Host "Installed DirVeyor $version to $exePath"
        if ($NoPath) { Write-Host "Run: & '$exePath'" }
        else { Write-Host 'Run dirveyor in this terminal. Other terminals may need to be reopened.' }
    } finally {
        [Net.ServicePointManager]::SecurityProtocol = $previousTls
        if ($stagedExe -and [IO.File]::Exists($stagedExe)) { [IO.File]::Delete($stagedExe) }
        if ($backupExe -and [IO.File]::Exists($backupExe)) {
            if ($replacementSucceeded) { [IO.File]::Delete($backupExe) }
            elseif (-not [IO.File]::Exists($exePath)) { [IO.File]::Move($backupExe, $exePath) }
            else { Write-Warning "The previous executable was retained at $backupExe" }
        }
        if ($work -and [IO.Directory]::Exists($work)) {
            $tempRoot = [IO.Path]::GetFullPath([IO.Path]::GetTempPath()).TrimEnd('\') + '\'
            $resolved = [IO.Path]::GetFullPath($work)
            if ($resolved.StartsWith($tempRoot, [StringComparison]::OrdinalIgnoreCase) -and
                [IO.Path]::GetFileName($resolved) -like 'dirveyor-install-*') {
                Remove-Item -LiteralPath $resolved -Recurse -Force
            }
        }
    }
}

# Dot-sourcing loads the function for local tests without installing anything.
if ($MyInvocation.InvocationName -ne '.') { Install-DirVeyor @PSBoundParameters }
