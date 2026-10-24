<#
.SYNOPSIS
    Launch Bulk Download Manager from the development virtual environment.

.DESCRIPTION
    Verifies that .venv\Scripts\python.exe exists (pointing the user at
    scripts\setup.ps1 otherwise), then runs run.py from the repository root,
    forwarding any extra arguments. The process exit code is propagated.

.USAGE
    powershell -NoProfile -ExecutionPolicy Bypass -File scripts\run.ps1 [extra args...]
#>

$ErrorActionPreference = 'Stop'

$Root = Split-Path -Parent $PSScriptRoot
$VenvPython = Join-Path $Root '.venv\Scripts\python.exe'
$Launcher = Join-Path $Root 'run.py'

if (-not (Test-Path -LiteralPath $VenvPython)) {
    Write-Host "[FAIL] Virtual environment not found: $VenvPython" -ForegroundColor Red
    Write-Host ""
    Write-Host "Run setup first:" -ForegroundColor Yellow
    Write-Host "    powershell -NoProfile -ExecutionPolicy Bypass -File scripts\setup.ps1" -ForegroundColor Yellow
    exit 1
}

if (-not (Test-Path -LiteralPath $Launcher)) {
    Write-Host "[FAIL] Entry point not found: $Launcher" -ForegroundColor Red
    exit 1
}

Push-Location $Root
try {
    & $VenvPython $Launcher @args
    $code = $LASTEXITCODE
} finally {
    Pop-Location
}

exit $code
