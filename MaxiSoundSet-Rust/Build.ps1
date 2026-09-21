param(
    [ValidateSet('Release', 'Debug', 'Test')][string]$Mode = 'Release',
    [string]$Target = 'x86_64-pc-windows-msvc'
)
$ErrorActionPreference = 'Stop'
if (-not (Get-Command cargo -ErrorAction SilentlyContinue)) {
    throw 'Install Rust stable 1.98 or later from https://rustup.rs and the Windows C++ build tools first.'
}
Push-Location $PSScriptRoot
try {
    if ($Mode -eq 'Test') {
        cargo test --locked --target $Target -- --test-threads=1
    } else {
        $taskArguments = @('build', '--locked', '--target', $Target)
        $taskProfile = 'debug'
        if ($Mode -eq 'Release') { $taskArguments += '--release'; $taskProfile = 'release' }
        cargo @taskArguments
        if ($LASTEXITCODE -eq 0) {
            if (Get-Process -Name 'MaxiSoundSet' -ErrorAction SilentlyContinue) {
                throw 'Close MAXI SOUNDSET with Exit app before replacing MaxiSoundSet.exe.'
            }
            $taskTargetDir = if ($env:CARGO_TARGET_DIR) { $env:CARGO_TARGET_DIR } else { Join-Path $PSScriptRoot 'target' }
            Copy-Item -LiteralPath (Join-Path $taskTargetDir "$Target\$taskProfile\maxi-sound-set.exe") -Destination (Join-Path $PSScriptRoot 'MaxiSoundSet.exe') -Force
        }
    }
    if ($LASTEXITCODE -ne 0) { throw "Cargo failed with exit code $LASTEXITCODE" }
} finally { Pop-Location }
