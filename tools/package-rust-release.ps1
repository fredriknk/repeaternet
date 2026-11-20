[CmdletBinding()]
param(
    [string] $ExePath,
    [string] $OutputPath,
    [string] $CargoManifestPath,
    [string] $NoticeManifestPath,
    [switch] $AllowIncompleteNotices,
    [ValidatePattern('^[0-9A-Fa-f]{64}$')] [string] $ExpectedSha256
)

$ErrorActionPreference = 'Stop'
$repo = Split-Path -Parent $PSScriptRoot
if (-not $ExePath) { $ExePath = Join-Path $repo 'rust/target/release/repeaternet.exe' }
if (-not $CargoManifestPath) { $CargoManifestPath = Join-Path $repo 'rust/Cargo.toml' }
if (-not $OutputPath) { $OutputPath = Join-Path $repo 'dist/repeaternet-windows-x64.zip' }
if (-not $NoticeManifestPath) { $NoticeManifestPath = Join-Path $repo 'rust/notices/release-notices.json' }

function Find-RustTool([string] $Name) {
    $tool = Get-Command $Name -CommandType Application -ErrorAction SilentlyContinue | Select-Object -First 1
    if ($tool) { return $tool.Source }
    $rustupShim = Join-Path $env:USERPROFILE ".cargo/bin/$Name"
    if (Test-Path -LiteralPath $rustupShim -PathType Leaf) { return $rustupShim }
    throw "$Name is required on the packaging machine. Install the same toolchain and cache the locked crate sources."
}

function Get-LicenseFiles($Package) {
    $directory = Split-Path -Parent $Package.manifest_path
    $files = @{}
    foreach ($file in (Get-ChildItem -LiteralPath $directory -File -Recurse)) {
        $relative = $file.FullName.Substring($directory.Length + 1).Replace('\', '/')
        if ($Package.name -eq 'libsqlite3-sys' -and $relative.StartsWith('sqlcipher/') -and -not $usesSqlCipher) {
            continue
        }
        if ($file.Name -match '^(LICENSE|LICENCE|COPYING|COPYRIGHT|NOTICE|UNLICENSE)([._-]|$)') {
            $files[$relative] = $file.FullName
        }
    }
    if ($Package.license_file) {
        $declared = Get-Item -LiteralPath (Join-Path $directory $Package.license_file)
        $prefix = $directory + [IO.Path]::DirectorySeparatorChar
        if ($declared.PSIsContainer -or -not $declared.FullName.StartsWith($prefix, [StringComparison]::OrdinalIgnoreCase)) {
            throw "Declared license-file lies outside the crate source directory: $($Package.id)"
        }
        $files[$declared.FullName.Substring($prefix.Length).Replace('\', '/')] = $declared.FullName
    }
    return $files
}

function Get-CargoTree([string] $Edges) {
    $rows = & $cargo tree --manifest-path $manifest --locked --offline --package repeaternet --target $targetTriple --edges $Edges --prefix none --format '{p}|{f}' --color never
    if ($LASTEXITCODE -ne 0) { throw "Locked offline Cargo tree failed for edges $Edges." }
    $packages = @{}
    foreach ($row in $rows) {
        if ($row -notmatch '^([\w-]+) v([^ |]+)(?: \(.+\))?\|([^ ]*)(?: \(\*\))?$') {
            throw "Unexpected Cargo tree row: $row"
        }
        $key = "$($Matches[1])-$($Matches[2])"
        $record = $packagesByKey[$key]
        if (@($record).Count -ne 1 -or -not $record) {
            throw "Cannot match Cargo tree package unambiguously to metadata: $key"
        }
        $features = @($Matches[3] -split ',')
        if ($packages.ContainsKey($key)) { $features += $packages[$key] }
        $packages[$key] = $features
    }
    return $packages
}

function Add-NoticeFile([string] $Name, [string] $Path, [string] $ExpectedHash) {
    if ($noticeFiles.ContainsKey($Name)) { throw "Duplicate notice archive path: $Name" }
    if (-not (Test-Path -LiteralPath $Path -PathType Leaf) -or (Get-Item -LiteralPath $Path).Length -eq 0) {
        throw "Missing required notice file for ${Name}: $Path"
    }
    $hash = (Get-FileHash -LiteralPath $Path -Algorithm SHA256).Hash
    if ($ExpectedHash -and $hash -ne $ExpectedHash) { throw "Notice SHA-256 mismatch for ${Name}: $Path" }
    $noticeFiles[$Name] = @{ path = $Path; hash = $hash }
}

function New-ArchiveEntry([string] $Name) {
    $entry = $archive.CreateEntry($Name, [IO.Compression.CompressionLevel]::Optimal)
    $entry.LastWriteTime = [DateTimeOffset]::new(2000, 1, 1, 0, 0, 0, [TimeSpan]::Zero)
    return $entry
}

function Write-ArchiveText([string] $Name, [string] $Text) {
    $entry = New-ArchiveEntry $Name
    $writer = [IO.StreamWriter]::new($entry.Open(), [Text.UTF8Encoding]::new($false))
    try { $writer.Write(($Text -replace '\r?\n', "`n") + "`n") } finally { $writer.Dispose() }
}

function Write-ArchiveFile([string] $Name, [IO.Stream] $Source) {
    $entry = New-ArchiveEntry $Name
    $target = $entry.Open()
    try { $Source.CopyTo($target) } finally { $target.Dispose() }
}

if ($env:OS -ne 'Windows_NT') { throw 'Run the release packager on Windows.' }
Add-Type -AssemblyName System.IO.Compression
$source = Get-Item -LiteralPath $ExePath
if ($source.PSIsContainer) { throw 'ExePath must point to a prebuilt executable.' }

# Resolve the containing directory before creating any file. Never stage through a junction.
$output = $ExecutionContext.SessionState.Path.GetUnresolvedProviderPathFromPSPath($OutputPath)
if ([IO.Path]::GetExtension($output) -ne '.zip') { throw 'OutputPath must have a .zip extension.' }
$outputDirectory = Get-Item -LiteralPath ([IO.Path]::GetDirectoryName($output)) -Force
if (-not $outputDirectory.PSIsContainer) { throw 'The ZIP destination directory must already exist.' }
$protected = @((Join-Path $repo 'web-data'))
if ($env:LOCALAPPDATA) { $protected += Join-Path $env:LOCALAPPDATA 'RepeaterNet' }
if ($env:RF_PLANNER_DATA) {
    $protected += $ExecutionContext.SessionState.Path.GetUnresolvedProviderPathFromPSPath($env:RF_PLANNER_DATA)
}
foreach ($path in $protected) {
    $prefix = [IO.Path]::GetFullPath($path).TrimEnd('\') + '\'
    if ($output.StartsWith($prefix, [StringComparison]::OrdinalIgnoreCase)) { throw 'Choose a ZIP destination outside the application data directory.' }
}
for ($parent = $outputDirectory; $parent; $parent = $parent.Parent) {
    if ($parent.Attributes -band [IO.FileAttributes]::ReparsePoint) { throw "ZIP destination traverses a reparse point: $($parent.FullName)" }
}
if (Test-Path -LiteralPath $output) { throw "ZIP destination already exists: $output" }

$manifest = (Get-Item -LiteralPath $CargoManifestPath).FullName
$lock = Join-Path (Split-Path -Parent $manifest) 'Cargo.lock'
$lockHash = (Get-FileHash -LiteralPath $lock -Algorithm SHA256).Hash
$targetTriple = 'x86_64-pc-windows-msvc'
$cargo = Find-RustTool 'cargo.exe'
$rustc = Find-RustTool 'rustc.exe'

Push-Location -LiteralPath (Split-Path -Parent $manifest)
try {
    $toolchainVersion = & $rustc --version
    if ($LASTEXITCODE -ne 0) { throw 'Could not identify the Rust toolchain.' }
    $sysroot = & $rustc --print sysroot
    if ($LASTEXITCODE -ne 0) { throw 'Could not locate the Rust toolchain license notices.' }
    $rustNotices = Join-Path $sysroot 'share/doc/rust/COPYRIGHT-library.html'
    if (-not (Test-Path -LiteralPath $rustNotices -PathType Leaf)) {
        throw 'The Rust standard library copyright file is missing. Install the matching rust-docs component.'
    }

    $metadataJson = & $cargo metadata --manifest-path $manifest --locked --offline --format-version 1 --filter-platform $targetTriple
    if ($LASTEXITCODE -ne 0) { throw 'Locked offline Cargo metadata failed. Use the build workspace and prefetch its locked dependencies.' }
    $metadata = $metadataJson | ConvertFrom-Json
    $repeaternet = @($metadata.packages | Where-Object { $_.name -eq 'repeaternet' -and $_.id -in $metadata.workspace_members })
    if ($repeaternet.Count -ne 1) { throw 'The workspace must contain one repeaternet package.' }

    $packagesByKey = @{}
    foreach ($package in $metadata.packages) {
        $key = "$($package.name)-$($package.version)"
        $packagesByKey[$key] += @($package)
    }
    $buildTree = Get-CargoTree 'normal,build'
    $targetTree = Get-CargoTree 'normal,no-proc-macro'
} finally {
    Pop-Location
}
if ((Get-FileHash -LiteralPath $lock -Algorithm SHA256).Hash -ne $lockHash) { throw 'Cargo.lock changed while reading metadata.' }
$dependencies = @($metadata.packages | Where-Object { $_.source -and $buildTree.ContainsKey("$($_.name)-$($_.version)") } |
    Sort-Object -Property name, version, source)
if ($dependencies.Count -eq 0) { throw 'Cargo metadata returned no external dependencies.' }

$noticeManifest = Get-Item -LiteralPath $NoticeManifestPath
$noticeRoot = $noticeManifest.DirectoryName
$noticeInput = Get-Content -LiteralPath $noticeManifest.FullName -Raw -Encoding UTF8 | ConvertFrom-Json
if ($noticeInput.lock_sha256 -ne $lockHash) { throw 'Notice manifest was prepared for a different Cargo.lock.' }
if ($noticeInput.target -ne $targetTriple) { throw 'Notice manifest was prepared for a different target.' }
$usesSqlCipher = @($buildTree.Keys | Where-Object {
    $_ -like 'libsqlite3-sys-*' -and ($buildTree[$_] | Where-Object { $_ -match 'sqlcipher' })
}).Count -gt 0
$noticeFiles = @{}
Add-NoticeFile 'licenses/rust-standard-library/COPYRIGHT-library.html' $rustNotices
Add-NoticeFile 'licenses/release-notices.json' $noticeManifest.FullName
foreach ($entry in $noticeInput.supplements) {
    if ($entry.file -notmatch '^texts/[A-Za-z0-9._/-]+$' -or $entry.archive -notmatch '^licenses/[A-Za-z0-9._/-]+$' -or
        $entry.file -match '(^|/)\.\.?(/|$)' -or $entry.archive -match '(^|/)\.\.?(/|$)' -or
        $entry.sha256 -notmatch '^[0-9A-Fa-f]{64}$' -or -not $entry.source) {
        throw "Invalid supplemental notice record: $($entry.file)"
    }
    Add-NoticeFile $entry.archive (Join-Path $noticeRoot $entry.file) $entry.sha256
}

$inventory = [Collections.Generic.List[string]]::new()
$selectedLicenses = @{}
foreach ($selection in $noticeInput.license_selections) {
    $package = @($dependencies | Where-Object { "$($_.name)-$($_.version)" -eq $selection.crate })
    if ($package.Count -ne 1 -or $package[0].license -ne $selection.declared -or
        $selection.selected -ne 'Apache-2.0' -or
        $selection.declared -notin @('MIT OR Apache-2.0', 'MIT/Apache-2.0') -or
        -not $noticeFiles.ContainsKey("licenses/crates/$($selection.crate)/LICENSE-APACHE-2.0.txt")) {
        throw "Invalid or unsupported license selection: $($selection.crate)"
    }
    $selectedLicenses[$selection.crate] = $selection.selected
}
$knownGaps = @{}
foreach ($gap in $noticeInput.unresolved) { $knownGaps[$gap.crate] = $gap.reason }
$targetCount = 0
if (@($noticeInput.crates.PSObject.Properties).Count -ne $dependencies.Count) {
    throw 'Cargo tree count differs from the notice manifest.'
}
foreach ($package in $dependencies) {
    $key = "$($package.name)-$($package.version)"
    $record = $noticeInput.crates.PSObject.Properties[$key]
    if (-not $record) { throw "Crate missing from the notice manifest: $key" }
    $scope = if ($targetTree.ContainsKey($key)) { $targetCount++; 'target' } else { 'build/proc-macro' }
    $license = if ($package.license) { $package.license } else { '(see license file)' }
    $inventory.Add("$($package.name) $($package.version) | $scope | $license | $($package.source)")
    $destination = "licenses/crates/$key"
    $files = Get-LicenseFiles $package
    foreach ($required in $record.Value) {
        if (-not $files.ContainsKey($required)) { throw "Missing required notice file for ${key}: $required" }
    }
    foreach ($relative in ($files.Keys | Sort-Object)) {
        Add-NoticeFile "$destination/$relative" $files[$relative]
    }
    if (-not ($noticeFiles.Keys -like "$destination/*") -and -not $knownGaps.ContainsKey($key)) {
        throw "No license text found for $key."
    }
}
$missingTexts = @($knownGaps.Keys | Sort-Object | ForEach-Object { "${_}: $($knownGaps[$_])" })
if ($missingTexts.Count -and -not $AllowIncompleteNotices) {
    throw "Unresolved upstream notices. Use -AllowIncompleteNotices only to produce a notice audit ZIP.`n$($missingTexts -join "`n")"
}
if ($missingTexts.Count) { Write-Warning 'The ZIP will contain unresolved notice gaps.' }

$temporaryZip = Join-Path $outputDirectory.FullName ('.repeaternet-package-' + [guid]::NewGuid().ToString('N') + '.tmp')
$ownsTemporaryZip = $false
$exeStream = [IO.File]::Open($source.FullName, [IO.FileMode]::Open, [IO.FileAccess]::Read, [IO.FileShare]::Read)
try {
    # IMAGE_FILE_MACHINE_AMD64, independent of the optional import-inspection tool.
    $reader = [IO.BinaryReader]::new($exeStream, [Text.Encoding]::ASCII, $true)
    try {
        if ($exeStream.Length -lt 64 -or $reader.ReadUInt16() -ne 0x5A4D) { throw 'The input is not a Windows PE executable.' }
        $exeStream.Position = 0x3C
        $peOffset = $reader.ReadUInt32()
        if ($peOffset -gt ($exeStream.Length - 6)) { throw 'Invalid Windows PE header offset.' }
        $exeStream.Position = $peOffset
        if ($reader.ReadUInt32() -ne 0x4550 -or $reader.ReadUInt16() -ne 0x8664) { throw 'The package expects a Windows x64 executable.' }
    } finally { $reader.Dispose() }
    $exeStream.Position = 0
    $exeHash = (Get-FileHash -InputStream $exeStream -Algorithm SHA256).Hash
    if ($ExpectedSha256 -and $exeHash -ne $ExpectedSha256) { throw "Executable SHA-256 does not match the verified artifact. Actual: $exeHash" }
    $exeStream.Position = 0

    $zipStream = [IO.File]::Open($temporaryZip, [IO.FileMode]::CreateNew, [IO.FileAccess]::ReadWrite, [IO.FileShare]::None)
    $ownsTemporaryZip = $true
    try {
        $archive = [IO.Compression.ZipArchive]::new($zipStream, [IO.Compression.ZipArchiveMode]::Create, $true)
        try {
            Write-ArchiveFile 'repeaternet.exe' $exeStream
            Write-ArchiveText 'START-HERE.txt' @'
Extract the ZIP and double-click repeaternet.exe. RepeaterNet serves the
dashboard at http://127.0.0.1:8000/ and opens your default browser after binding.
Leave the server running while using the dashboard. From a terminal, press
Ctrl+C to stop it. To run headless from PowerShell in the extracted folder:

    .\repeaternet.exe --no-browser --host 127.0.0.1 --port 8000

Open the address printed by the server. If the port is occupied, close the
other listener or select an available port with --port. Keep the same address
and browser profile to reopen a workspace. Use --data with a quoted folder
path to select storage explicitly. Use --help for available options.

The dashboard assets are embedded. Python and Cargo are unnecessary to launch
the app. See THIRD-PARTY-NOTICES.txt for component notices and EPSG terms.
'@
            Write-ArchiveText 'SHA256SUMS.txt' "$exeHash  repeaternet.exe"
            $entryNames = [string[]] @($noticeFiles.Keys)
            [Array]::Sort($entryNames, [StringComparer]::Ordinal)
            foreach ($entryName in $entryNames) {
                $stream = [IO.File]::OpenRead($noticeFiles[$entryName].path)
                try { Write-ArchiveFile $entryName $stream } finally { $stream.Dispose() }
            }

            $missing = if ($missingTexts.Count) { $missingTexts -join "`n" } else { '(none recorded)' }
            Write-ArchiveText 'THIRD-PARTY-NOTICES.txt' @"
RepeaterNet Windows x64 third-party notices

Executable SHA-256: $exeHash
Inventory workspace package: repeaternet $($repeaternet[0].version)
Inventory Cargo.lock SHA-256: $lockHash
Inventory target: $targetTriple
Packaging toolchain: $toolchainVersion

The Cargo inventory uses the Windows x64 Cargo tree for the repeaternet package
with default features, normal and build edges. It excludes dev dependencies and
dependencies inactive for this build target and feature selection. The target
label means reachable through normal edges without passing through a proc macro.
The build/proc-macro label covers the remaining build graph. The target graph
is not a list of code proven linked into the executable.

Required notice files for this inventory are checked against the offline notice
manifest. See licenses/release-notices.json for source URLs, excerpt ranges and
supplemental SHA-256 hashes. Review the unresolved items and applicable upstream
terms before distribution. File validation does not establish legal completeness
or binary provenance.

Embedded coordinate reference definitions are derived from the International
Association of Oil & Gas Producers (IOGP) EPSG Geodetic Parameter Dataset
v12.029, as distributed with PyProj 3.8.0 and PROJ 9.8.1. IOGP maintains the
EPSG Dataset. Its Terms of Use apply to the source dataset and govern its use
and redistribution. See https://epsg.org/ and the IOGP EPSG Dataset Terms of Use
at https://iogp.georepository.com/terms-of-use.html.

Leaflet 1.9.4 is embedded in the dashboard and route reports. Its upstream
BSD 2-Clause license is included in licenses/leaflet-1.9.4/LICENSE.txt.
Upstream source: https://github.com/Leaflet/Leaflet/tree/v1.9.4

The SQLite amalgamation bundled by libsqlite3-sys is dedicated to the public
domain. See https://sqlite.org/copyright.html. The Rust standard library
copyright notices are included under licenses/rust-standard-library/.

Unresolved upstream notice items
$missing

Cargo dependencies (crate and version | scope | declared license | source)
$($inventory -join "`n")
"@
        } finally { $archive.Dispose() }
    } finally { $zipStream.Dispose() }

    $noticeFiles['repeaternet.exe'] = @{ hash = $exeHash }
    $zipStream = [IO.File]::OpenRead($temporaryZip)
    try {
        $archive = [IO.Compression.ZipArchive]::new($zipStream, [IO.Compression.ZipArchiveMode]::Read, $true)
        try {
            foreach ($entryName in $noticeFiles.Keys) {
                $entry = $archive.GetEntry($entryName)
                if (-not $entry) { throw "Missing required ZIP entry: $entryName" }
                $stream = $entry.Open()
                try { $actual = (Get-FileHash -InputStream $stream -Algorithm SHA256).Hash }
                finally { $stream.Dispose() }
                if ($actual -ne $noticeFiles[$entryName].hash) { throw "ZIP entry SHA-256 mismatch: $entryName" }
            }
        } finally { $archive.Dispose() }
    } finally { $zipStream.Dispose() }

    # Move fails if another process published to this path after the initial check.
    [IO.File]::Move($temporaryZip, $output)
    $ownsTemporaryZip = $false
    [ordered]@{
        status = if ($missingTexts.Count) { 'packaged_with_notice_gaps' } else { 'packaged' }
        archive = $output; executable_sha256 = $exeHash
        zip_sha256 = (Get-FileHash -LiteralPath $output -Algorithm SHA256).Hash
        inventory_lock_sha256 = $lockHash; dependency_count = $dependencies.Count
        target_dependency_count = $targetCount; build_only_dependency_count = $dependencies.Count - $targetCount
        unresolved_notice_items = $missingTexts
    } | ConvertTo-Json -Depth 4
} finally {
    $exeStream.Dispose()
    if ($ownsTemporaryZip) { Remove-Item -LiteralPath $temporaryZip -Force }
}
