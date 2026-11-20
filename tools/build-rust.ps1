$ErrorActionPreference = 'Stop'

$cargo = Get-Command cargo -CommandType Application -ErrorAction SilentlyContinue
if ($cargo) {
    $cargoPath = $cargo.Source
} else {
    $cargoPath = Join-Path $env:USERPROFILE '.cargo/bin/cargo.exe'
    if (-not (Test-Path -LiteralPath $cargoPath -PathType Leaf)) {
        throw 'Cargo was not found. Install Rust via rustup with the MSVC toolchain.'
    }
}

$rustRoot = Join-Path (Split-Path -Parent $PSScriptRoot) 'rust'
$target = Join-Path $rustRoot 'target'
Push-Location -LiteralPath $rustRoot
try {
    & $cargoPath build --target-dir $target --locked --release --package repeaternet
    if ($LASTEXITCODE -ne 0) {
        exit $LASTEXITCODE
    }
} finally {
    Pop-Location
}

Write-Output (Join-Path $target 'release/repeaternet.exe')
