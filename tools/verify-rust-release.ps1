[CmdletBinding()]
param(
    [string] $ExePath,
    [string] $TerrainFixture,
    [ValidateRange(5, 120)] [int] $TimeoutSeconds = 30,
    [switch] $RequirePeTool
)

$ErrorActionPreference = 'Stop'
$repo = Split-Path -Parent $PSScriptRoot
if (-not $ExePath) { $ExePath = Join-Path $repo 'rust/target/release/repeaternet.exe' }
if (-not $TerrainFixture) { $TerrainFixture = Join-Path $repo 'rust/crates/terrain/tests/fixtures/fallback.tif' }

function Assert-Release([bool] $Condition, [string] $Message) {
    if (-not $Condition) { throw $Message }
}

function Add-Check([string] $Status, $Detail) {
    $checks.Add([ordered]@{ check = $phase; status = $Status; detail = $Detail })
}

function Start-Candidate([string] $Name, [string] $Arguments) {
    # Restore the parent environment immediately after the child inherits it.
    $environment = @{
        RF_PLANNER_TOKEN = $token
        RF_PLANNER_DATA = $null
        RF_PLANNER_TRUSTED_PROXY = $null
        RF_PLANNER_CACHE_DIR = $cacheDirectory
        RF_PLANNER_MAX_ACTIVE_JOBS = '1'
        PATH = "$env:SystemRoot\System32;$env:SystemRoot"
        LOCALAPPDATA = $root
        APPDATA = $root
        TEMP = $root
        TMP = $root
    }
    foreach ($item in Get-ChildItem Env:) {
        if ($item.Name -match '^(PYTHON|CONDA|VIRTUAL_ENV|GDAL_|PROJ_)') {
            $environment[$item.Name] = $null
        }
    }
    $previous = @{}
    try {
        foreach ($variable in $environment.Keys) {
            $previous[$variable] = [Environment]::GetEnvironmentVariable($variable, 'Process')
            [Environment]::SetEnvironmentVariable($variable, $environment[$variable], 'Process')
        }
        $stdout = Join-Path $root "$Name.stdout.log"
        $stderr = Join-Path $root "$Name.stderr.log"
        $process = Start-Process -FilePath $candidate -WorkingDirectory $appDirectory `
            -ArgumentList $Arguments -WindowStyle Hidden -PassThru `
            -RedirectStandardOutput $stdout -RedirectStandardError $stderr
        $processes.Add($process)
        # Windows PowerShell 5.1 needs a retained handle to read a fast child's exit code.
        $null = $process.Handle
        [pscustomobject]@{ Process = $process; Stdout = $stdout; Stderr = $stderr }
    } finally {
        foreach ($variable in $previous.Keys) {
            [Environment]::SetEnvironmentVariable($variable, $previous[$variable], 'Process')
        }
    }
}

function Stop-Candidate($Child) {
    if (-not $Child.HasExited) { $Child.Kill() }
    Assert-Release ($Child.WaitForExit(5000)) 'The executable did not stop within 5 seconds.'
}

function Get-ListenUrl($Child) {
    $timer = [Diagnostics.Stopwatch]::StartNew()
    while ($timer.Elapsed.TotalSeconds -lt $TimeoutSeconds) {
        if ($Child.Process.HasExited) {
            throw "Server exited with $($Child.Process.ExitCode). $(Get-Content -LiteralPath $Child.Stderr -Raw)"
        }
        $log = Get-Content -LiteralPath $Child.Stdout -Raw
        if ($log -match '\blistening at (http://127\.0\.0\.1:[1-9][0-9]*/)(?:\s|$)') {
            return [uri] $Matches[1]
        }
        Start-Sleep -Milliseconds 100
    }
    throw "Server did not announce a loopback bind within $TimeoutSeconds seconds. $(Get-Content -LiteralPath $Child.Stderr -Raw)"
}

function New-ReleaseClient {
    $handler = [Net.Http.HttpClientHandler]::new()
    $handler.AllowAutoRedirect = $false
    $handler.UseProxy = $false
    $client = [Net.Http.HttpClient]::new($handler)
    $client.Timeout = [TimeSpan]::FromSeconds([Math]::Min(10, $TimeoutSeconds))
    $clients.Add($client)
    return $client
}

function Invoke-ReleaseRequest {
    param(
        [Net.Http.HttpClient] $Client = $session,
        [string] $Method = 'GET',
        [Parameter(Mandatory)] [string] $Path,
        [int] $Status = 200,
        [Net.Http.HttpContent] $Content,
        [string] $Origin
    )
    $request = [Net.Http.HttpRequestMessage]::new([Net.Http.HttpMethod]::new($Method), [uri]::new($base, $Path))
    $response = $null
    try {
        $request.Content = $Content
        if ($Origin) { $request.Headers.Add('Origin', $Origin) }
        $response = $Client.SendAsync($request).GetAwaiter().GetResult()
        $type = [string] $response.Content.Headers.ContentType
        $body = ''
        $bytes = [byte[]] @()
        if ($type -eq 'image/png' -and [int] $response.StatusCode -eq $Status) {
            $bytes = $response.Content.ReadAsByteArrayAsync().GetAwaiter().GetResult()
        } else {
            $body = $response.Content.ReadAsStringAsync().GetAwaiter().GetResult()
        }
        if ([int] $response.StatusCode -ne $Status) {
            throw "$Method $Path returned $([int] $response.StatusCode), expected $Status. $($body.Substring(0, [Math]::Min(500, $body.Length)))"
        }
        $cookies = @()
        if ($response.Headers.Contains('Set-Cookie')) { $cookies = @($response.Headers.GetValues('Set-Cookie')) }
        [pscustomobject]@{
            Body = $body
            Bytes = $bytes
            Type = $type
            Location = [string] $response.Headers.Location
            Cookies = $cookies
        }
    } finally {
        if ($response) { $response.Dispose() }
        $request.Dispose()
    }
}

function New-TextBody([string] $Body, [string] $MediaType = 'application/json') {
    return [Net.Http.StringContent]::new($Body, [Text.Encoding]::UTF8, $MediaType)
}

function Assert-Cookie($Response, [string] $Name) {
    $header = @($Response.Cookies | Where-Object { $_ -match "^$Name=[^;]+;" })
    Assert-Release ($header.Count -eq 1) "Expected one $Name cookie."
    foreach ($attribute in @('Path=/', 'HttpOnly', 'SameSite=Strict')) {
        Assert-Release (($header[0] -split ';\s*') -contains $attribute) "$Name is missing $attribute."
    }
}

function Test-PeImports {
    $tool = Get-Command objdump.exe, dumpbin.exe, llvm-readobj.exe -CommandType Application -ErrorAction SilentlyContinue |
        Select-Object -First 1
    if (-not $tool) {
        if ($RequirePeTool) { throw 'Install objdump, dumpbin or llvm-readobj and place it on PATH.' }
        Add-Check 'skipped' 'No objdump, dumpbin or llvm-readobj on PATH.'
        return
    }

    switch ($tool.Name) {
        'objdump.exe'      { $toolArguments = @('-p', $candidate); $pattern = '^\s*DLL Name:\s*(\S+\.dll)\s*$' }
        'dumpbin.exe'      { $toolArguments = @('/DEPENDENTS', $candidate); $pattern = '^\s*(\S+\.dll)\s*$' }
        'llvm-readobj.exe' { $toolArguments = @('--coff-imports', $candidate); $pattern = '^\s*Name:\s*(\S+\.dll)\s*$' }
    }
    $output = & $tool.Source @toolArguments
    Assert-Release ($LASTEXITCODE -eq 0) "$($tool.Name) could not inspect the executable."
    $imports = @($output | ForEach-Object {
        if ($_ -match $pattern) { $Matches[1].ToLowerInvariant() }
    } | Sort-Object -Unique)
    Assert-Release ($imports.Count -gt 0) "$($tool.Name) reported no recognizable PE imports."
    $systemDlls = @('advapi32.dll', 'bcrypt.dll', 'bcryptprimitives.dll', 'crypt32.dll', 'dnsapi.dll',
        'iphlpapi.dll', 'kernel32.dll', 'kernelbase.dll', 'msvcrt.dll', 'ncrypt.dll', 'netapi32.dll',
        'normaliz.dll', 'ntdll.dll', 'ole32.dll', 'oleaut32.dll', 'psapi.dll', 'rpcrt4.dll', 'secur32.dll',
        'shell32.dll', 'shlwapi.dll', 'ucrtbase.dll', 'user32.dll', 'userenv.dll', 'version.dll', 'winhttp.dll',
        'winmm.dll', 'ws2_32.dll', 'wtsapi32.dll')
    $unexpected = @($imports | Where-Object { $_ -notin $systemDlls -and $_ -notmatch '^(api-ms-win-|ext-ms-win-).+\.dll$' })
    Assert-Release ($unexpected.Count -eq 0) "Review unexpected direct PE imports: $($unexpected -join ', ')."
    Add-Check 'passed' ([ordered]@{ tool = $tool.Name; imports = $imports })
}

function Assert-SafeScratchDirectory([string] $Directory) {
    foreach ($entry in Get-ChildItem -LiteralPath $Directory -Force) {
        Assert-Release (-not ($entry.Attributes -band [IO.FileAttributes]::ReparsePoint)) "Scratch contains a reparse point: $($entry.FullName)"
        if ($entry.PSIsContainer) { Assert-SafeScratchDirectory $entry.FullName }
    }
}

$checks = [Collections.Generic.List[object]]::new()
$processes = [Collections.Generic.List[Diagnostics.Process]]::new()
$clients = [Collections.Generic.List[object]]::new()
$report = [ordered]@{ status = 'failed'; executable = $null; version = $null; sha256 = $null; checks = $checks }
$root = $null
$ownsRoot = $false
$phase = 'isolated executable'
try {
    Assert-Release ($env:OS -eq 'Windows_NT') 'Run the release verifier on Windows.'
    Add-Type -AssemblyName System.Net.Http
    $source = Get-Item -LiteralPath $ExePath
    Assert-Release (-not $source.PSIsContainer) 'ExePath must point to a prebuilt executable.'
    $report.executable = $source.FullName
    $tempDirectory = Get-Item -LiteralPath ([IO.Path]::GetTempPath()) -Force
    Assert-Release $tempDirectory.PSIsContainer 'The Windows temp path must be an existing directory.'
    for ($parent = $tempDirectory; $parent; $parent = $parent.Parent) {
        Assert-Release (-not ($parent.Attributes -band [IO.FileAttributes]::ReparsePoint)) "Temp path traverses a reparse point: $($parent.FullName)"
    }
    $temp = $tempDirectory.FullName
    # Start-Process resolves both redirect file paths as wildcard patterns in Windows PowerShell 5.1.
    Assert-Release ($temp -notmatch '[\[\]`]') 'Set TEMP and TMP to a directory without brackets or backticks for Start-Process log redirection.'
    $protected = @((Join-Path $repo 'web-data'))
    if ($env:LOCALAPPDATA) { $protected += Join-Path $env:LOCALAPPDATA 'RepeaterNet' }
    if ($env:RF_PLANNER_DATA) {
        $protected += $ExecutionContext.SessionState.Path.GetUnresolvedProviderPathFromPSPath($env:RF_PLANNER_DATA)
    }
    foreach ($path in $protected) {
        $prefix = [IO.Path]::GetFullPath($path).TrimEnd('\') + '\'
        Assert-Release (-not ($temp.TrimEnd('\') + '\').StartsWith($prefix, [StringComparison]::OrdinalIgnoreCase)) 'Use a Windows temp directory outside application data.'
    }
    $scratchName = 'repeaternet-verify-' + [guid]::NewGuid().ToString('N')
    $root = Join-Path $temp $scratchName
    [void] (New-Item -Path $root -ItemType Directory)
    $ownsRoot = $true
    $appDirectory = Join-Path $root 'app'
    [void] (New-Item -Path $appDirectory -ItemType Directory)
    $dataDirectory = Join-Path $root 'data'
    $cacheDirectory = Join-Path $root 'cache'
    $candidate = Join-Path $appDirectory 'repeaternet.exe'
    Copy-Item -LiteralPath $source.FullName -Destination $candidate
    $report.sha256 = (Get-FileHash -LiteralPath $candidate -Algorithm SHA256).Hash
    $token = [guid]::NewGuid().ToString('N')
    Add-Check 'passed' 'Executable copied alone. Data, cache, app and runtime temp paths are isolated.'

    $phase = 'version'
    $versionRun = Start-Candidate 'version' '--version'
    Assert-Release ($versionRun.Process.WaitForExit($TimeoutSeconds * 1000)) '--version timed out.'
    if ($versionRun.Process.ExitCode -ne 0) {
        throw "--version exit code: $($versionRun.Process.ExitCode). $(Get-Content -LiteralPath $versionRun.Stderr -Raw)"
    }
    $report.version = (Get-Content -LiteralPath $versionRun.Stdout -Raw).Trim()
    Assert-Release ($report.version -match '^repeaternet\s+\S+$') 'Unexpected --version output.'
    Add-Check 'passed' $report.version

    $phase = 'direct PE imports'
    Test-PeImports

    $phase = 'headless launch and health'
    $arguments = '--no-browser --host 127.0.0.1 --data "{0}" --port' -f $dataDirectory
    $server = Start-Candidate 'server' "$arguments 0"
    $base = Get-ListenUrl $server
    $origin = $base.GetLeftPart([UriPartial]::Authority)
    $session = New-ReleaseClient
    $health = (Invoke-ReleaseRequest -Path '/health').Body | ConvertFrom-Json
    Assert-Release ($health.status -eq 'ok' -and $health.runtime -eq 'rust') 'Unexpected health payload.'
    Add-Check 'passed' $health

    $phase = 'authentication and workspace cookie'
    $signIn = Invoke-ReleaseRequest -Path '/'
    Assert-Release ($signIn.Body -match 'action="/login"' -and $signIn.Cookies.Count -eq 0) 'Unauthenticated root should serve sign-in without issuing a workspace cookie.'
    $gatedAsset = Invoke-ReleaseRequest -Path '/assets/app.js'
    Assert-Release ($gatedAsset.Type -like 'text/html*' -and $gatedAsset.Body -match 'action="/login"') 'Unauthenticated script request should serve sign-in.'
    [void] (Invoke-ReleaseRequest -Path '/api/state' -Status 401)
    [void] (Invoke-ReleaseRequest -Method POST -Path '/login' -Status 401 -Origin $origin `
        -Content (New-TextBody 'token=wrong' 'application/x-www-form-urlencoded'))
    [void] (Invoke-ReleaseRequest -Method POST -Path '/login' -Status 403 -Origin 'https://invalid.example' `
        -Content (New-TextBody "token=$token" 'application/x-www-form-urlencoded'))
    $login = Invoke-ReleaseRequest -Method POST -Path '/login' -Status 303 -Origin $origin `
        -Content (New-TextBody "token=$token" 'application/x-www-form-urlencoded')
    Assert-Release ($login.Location -eq '/') 'Login did not redirect to the dashboard.'
    Assert-Cookie $login 'planner_access'
    $dashboard = Invoke-ReleaseRequest -Path '/'
    Assert-Cookie $dashboard 'planner_workspace'
    Assert-Release (($dashboard.Cookies[0] -split ';\s*') -contains 'Max-Age=31536000') 'Workspace cookie does not have the expected lifetime.'
    Add-Check 'passed' 'Sign-in, failed token, rejected foreign Origin, redirect and both cookie policies.'

    $phase = 'embedded dashboard assets'
    Assert-Release ($dashboard.Type -like 'text/html*' -and $dashboard.Body -match '/assets/app\.js') 'Unexpected dashboard HTML.'
    $assets = [ordered]@{
        '/assets/style.css' = 'text/css*'; '/assets/meshcore.css' = 'text/css*'; '/assets/coverage.css' = 'text/css*'
        '/assets/workspace.css' = 'text/css*'; '/leaflet/leaflet.css' = 'text/css*'; '/leaflet/leaflet.js' = '*javascript*'
        '/assets/main_view_navigation.js' = '*javascript*'; '/assets/app.js' = '*javascript*'
        '/assets/coverage_inspection.js' = '*javascript*'; '/leaflet/images/marker-icon.png' = 'image/png'
        '/leaflet/images/marker-icon-2x.png' = 'image/png'; '/leaflet/images/marker-shadow.png' = 'image/png'
    }
    foreach ($path in $assets.Keys) {
        $asset = Invoke-ReleaseRequest -Path $path
        Assert-Release ($asset.Type -like $assets[$path]) "Unexpected content type for $path."
        if ($asset.Type -eq 'image/png') {
            Assert-Release ($asset.Bytes.Length -gt 32 -and
                [BitConverter]::ToString($asset.Bytes, 0, 8) -eq '89-50-4E-47-0D-0A-1A-0A') "Invalid PNG asset at $path."
        } else {
            Assert-Release ($asset.Body.Length -gt 32) "Empty or truncated asset at $path."
        }
    }
    Add-Check 'passed' @($assets.Keys)

    $phase = 'create and autosave project'
    $projectName = 'Windows release smoke test'
    $created = (Invoke-ReleaseRequest -Method POST -Path '/api/projects' -Origin $origin `
        -Content (New-TextBody (@{ name = $projectName } | ConvertTo-Json -Compress))).Body | ConvertFrom-Json
    Assert-Release ($created.id -and $created.name -eq $projectName) 'Project creation returned no matching project.'
    # EPSG:25833 fixture coordinates (500050, 6650850) and (500290, 6650850), in WGS84 lat/lon.
    $plan = @{
        a = @(59.99496092062187, 15.000896279590481)
        b = @(59.994960821365005, 15.005198421612754)
        candidates = @{ infrastructure_policy = 'existing_only'; refine_radius_m = 0; parallel_workers = 2 }
    }
    $saved = (Invoke-ReleaseRequest -Method POST -Path '/api/projects/autosave' -Origin $origin `
        -Content (New-TextBody (@{ project_id = $created.id; plan = $plan } | ConvertTo-Json -Depth 5 -Compress))).Body | ConvertFrom-Json
    Assert-Release ($saved.project_id -eq $created.id) 'Autosave returned a different project ID.'
    Add-Check 'passed' $projectName

    $phase = 'upload terrain and calculate route'
    $fixture = Get-Item -LiteralPath $TerrainFixture
    Assert-Release (-not $fixture.PSIsContainer) 'TerrainFixture must be a GeoTIFF file.'
    $fixtureHash = (Get-FileHash -LiteralPath $fixture.FullName -Algorithm SHA256).Hash
    [void] (Invoke-ReleaseRequest -Path '/api/result' -Status 404)
    $multipart = [Net.Http.MultipartFormDataContent]::new()
    try {
        $part = [Net.Http.ByteArrayContent]::new([IO.File]::ReadAllBytes($fixture.FullName))
        $part.Headers.ContentType = [Net.Http.Headers.MediaTypeHeaderValue]::new('image/tiff')
        $multipart.Add($part, 'file', 'terrain.tif')
        [void] (Invoke-ReleaseRequest -Method POST -Path '/api/terrain/dtm' -Origin $origin -Content $multipart)
    } finally {
        $multipart.Dispose()
    }
    $job = (Invoke-ReleaseRequest -Method POST -Path '/api/optimize' -Origin $origin `
        -Content (New-TextBody ($plan | ConvertTo-Json -Depth 5 -Compress))).Body | ConvertFrom-Json
    Assert-Release ([bool] $job.job_id) 'Route submission returned no job ID.'
    $timer = [Diagnostics.Stopwatch]::StartNew()
    do {
        $state = (Invoke-ReleaseRequest -Path '/api/state').Body | ConvertFrom-Json
        if ($state.job.state -notin @('preparing', 'queued', 'running')) { break }
        Start-Sleep -Milliseconds 100
    } while ($timer.Elapsed.TotalSeconds -lt $TimeoutSeconds)
    Assert-Release ($state.job.state -eq 'complete') "Route did not complete: $($state.job | ConvertTo-Json -Depth 5 -Compress)"
    $route = (Invoke-ReleaseRequest -Path '/api/result').Body | ConvertFrom-Json
    Assert-Release ($route.job_id -eq $job.job_id -and $route.found -eq $true -and $route.links[0].profile.distances_m.Count -gt 3) 'No matching certified route with a sampled terrain profile.'
    $geojson = (Invoke-ReleaseRequest -Path '/api/export/geojson').Body | ConvertFrom-Json
    Assert-Release ($geojson.type -eq 'FeatureCollection' -and $geojson.features.Count -gt 0) 'Route export did not return a GeoJSON feature collection with features.'
    Add-Check 'passed' ([ordered]@{ fixture_sha256 = $fixtureHash; route_found = $true; profile_samples = $route.links[0].profile.distances_m.Count })

    $phase = 'occupied port'
    $conflictData = Join-Path $root 'conflict-data'
    $conflictArguments = '--no-browser --host 127.0.0.1 --data "{0}" --port {1}' -f $conflictData, $base.Port
    $conflict = Start-Candidate 'conflict' $conflictArguments
    Assert-Release ($conflict.Process.WaitForExit($TimeoutSeconds * 1000)) 'Second process did not exit when its port was occupied.'
    $bindError = Get-Content -LiteralPath $conflict.Stderr -Raw
    Assert-Release ($conflict.Process.ExitCode -ne 0 -and $bindError -match "Could not listen on 127\.0\.0\.1:$($base.Port)") "Expected a nonzero exit with a bind error. $bindError"
    [void] (Invoke-ReleaseRequest -Path '/health')
    Add-Check 'passed' ([ordered]@{ exit_code = $conflict.Process.ExitCode; port = $base.Port })

    $phase = 'restart and project persistence'
    Stop-Candidate $server.Process
    $restarted = Start-Candidate 'restart' "$arguments $($base.Port)"
    Assert-Release ((Get-ListenUrl $restarted).AbsoluteUri -eq $base.AbsoluteUri) 'Restart listened on a different origin.'
    $state = (Invoke-ReleaseRequest -Path '/api/state').Body | ConvertFrom-Json
    Assert-Release ($state.projects.active_id -eq $created.id -and $state.projects.active_name -eq $projectName) 'The same cookie session did not reopen its project.'
    Assert-Release ($state.terrain.dtm.Count -eq 1 -and $state.projects.previous_result.found -eq $true -and $state.projects.previous_result.stale -eq $false) 'Uploaded DTM or certified route summary did not survive restart.'
    $exported = (Invoke-ReleaseRequest -Path '/api/export/project').Body | ConvertFrom-Json
    foreach ($endpoint in @('a', 'b')) {
        $actual = $exported.$endpoint
        $expected = $plan[$endpoint]
        Assert-Release ($actual.Count -eq 2 -and [Math]::Abs($actual[0] - $expected[0]) -lt 1e-9 -and
            [Math]::Abs($actual[1] - $expected[1]) -lt 1e-9) "Saved endpoint $endpoint changed on restart."
    }
    $secondSession = New-ReleaseClient
    [void] (Invoke-ReleaseRequest -Client $secondSession -Path '/api/state' -Status 401)
    [void] (Invoke-ReleaseRequest -Client $secondSession -Method POST -Path '/login' -Status 303 -Origin $origin `
        -Content (New-TextBody "token=$token" 'application/x-www-form-urlencoded'))
    $secondDashboard = Invoke-ReleaseRequest -Client $secondSession -Path '/'
    Assert-Cookie $secondDashboard 'planner_workspace'
    $otherState = (Invoke-ReleaseRequest -Client $secondSession -Path '/api/state').Body | ConvertFrom-Json
    Assert-Release ($otherState.projects.active_id -and $otherState.projects.active_id -ne $created.id) 'A fresh workspace saw the first workspace project.'
    Add-Check 'passed' 'Forced stop and same-origin restart retained the plan, DTM and route summary. A fresh session got a separate workspace.'
    $report.status = 'passed'
} catch {
    Add-Check 'failed' $_.Exception.Message
} finally {
    $phase = 'cleanup'
    $cleanupErrors = @()
    foreach ($client in $clients) { $client.Dispose() }
    foreach ($process in $processes) {
        try { Stop-Candidate $process } catch { $cleanupErrors += $_.Exception.Message }
        finally { $process.Dispose() }
    }
    if ($ownsRoot -and $cleanupErrors.Count -eq 0) {
        try {
            $scratch = Get-Item -LiteralPath $root -Force
            $expected = [IO.Path]::GetFullPath((Join-Path $temp $scratchName))
            Assert-Release ($scratch.FullName -eq $expected -and $scratch.Parent.FullName.TrimEnd('\') -eq $temp.TrimEnd('\') -and
                $scratch.Name -match '^repeaternet-verify-[0-9a-f]{32}$' -and
                -not ($scratch.Attributes -band [IO.FileAttributes]::ReparsePoint)) 'Scratch path failed the cleanup containment check.'
            Assert-SafeScratchDirectory $scratch.FullName
            Remove-Item -LiteralPath $scratch.FullName -Recurse -Force
        } catch { $cleanupErrors += $_.Exception.Message }
    }
    if ($cleanupErrors.Count -gt 0) {
        $report.status = 'failed'
        Add-Check 'failed' ([ordered]@{ scratch = $root; errors = $cleanupErrors })
    } else {
        Add-Check 'passed' 'Stopped owned processes and removed owned scratch files.'
    }
}

$report | ConvertTo-Json -Depth 8
if ($report.status -ne 'passed') { exit 1 }
exit 0
