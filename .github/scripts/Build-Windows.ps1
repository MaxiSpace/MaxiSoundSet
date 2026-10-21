[CmdletBinding()]
param()

$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest
$taskRepoRoot = (Resolve-Path (Join-Path $PSScriptRoot '../..')).Path
$taskTarget = 'x86_64-pc-windows-msvc'
$taskOutput = Join-Path $taskRepoRoot 'artifacts/windows-x64'

function Invoke-CheckedCargo {
    param([string[]]$CargoArguments)
    & cargo @CargoArguments
    if ($LASTEXITCODE -ne 0) {
        throw "cargo $($CargoArguments -join ' ') failed with exit code $LASTEXITCODE"
    }
}

# Run inside each crate so Cargo loads its static CRT and stack settings.
Push-Location (Join-Path $taskRepoRoot 'MaxiSoundSet-Rust')
try {
    & ./Build.ps1 -Mode Release -Target $taskTarget
    Invoke-CheckedCargo -CargoArguments @('test', '--locked', '--target', $taskTarget, '--', '--test-threads=1')
} finally {
    Pop-Location
}

# The installer includes the canonical app executable at compile time.
Push-Location (Join-Path $taskRepoRoot 'MaxiSoundSet-Installer')
try {
    Invoke-CheckedCargo -CargoArguments @('build', '--release', '--locked', '--target', $taskTarget)
    Invoke-CheckedCargo -CargoArguments @('test', '--locked', '--target', $taskTarget, '--', '--test-threads=1')
    $taskTargetDir = if ($env:CARGO_TARGET_DIR) {
        $env:CARGO_TARGET_DIR
    } else {
        Join-Path (Get-Location).Path 'target'
    }
    $taskInstaller = Join-Path $taskTargetDir "$taskTarget/release/maxi-soundset-setup.exe"
    New-Item -ItemType Directory -Path $taskOutput -Force | Out-Null
    Copy-Item -LiteralPath $taskInstaller -Destination (Join-Path $taskOutput 'MaxiSoundSet-Setup.exe') -Force
} finally {
    Pop-Location
}

Copy-Item -LiteralPath (Join-Path $taskRepoRoot 'MaxiSoundSet-Rust/MaxiSoundSet.exe') -Destination $taskOutput -Force
$taskChecksums = foreach ($taskName in @('MaxiSoundSet.exe', 'MaxiSoundSet-Setup.exe')) {
    $taskHash = Get-FileHash -LiteralPath (Join-Path $taskOutput $taskName) -Algorithm SHA256
    '{0}  {1}' -f $taskHash.Hash.ToLowerInvariant(), $taskName
}
$taskChecksums | Set-Content -LiteralPath (Join-Path $taskOutput 'SHA256SUMS.txt') -Encoding utf8

