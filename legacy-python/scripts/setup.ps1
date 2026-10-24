<#
.SYNOPSIS
    Idempotent development-environment setup for Bulk Download Manager.

.DESCRIPTION
    1. Locates a system Python (py launcher, then `python`) with version >= 3.12.
    2. Creates the .venv if missing (recreating a stale/invalid one), else reuses it.
    3. Upgrades pip inside the venv and installs requirements.txt.

    Works from any working directory: all paths are resolved from $PSScriptRoot.

.USAGE
    powershell -NoProfile -ExecutionPolicy Bypass -File scripts\setup.ps1
#>

$ErrorActionPreference = 'Stop'

# ---------------------------------------------------------------------------
# Path bootstrap (works from any cwd)
# ---------------------------------------------------------------------------
$Root = Split-Path -Parent $PSScriptRoot
$VenvDir = Join-Path $Root '.venv'
$VenvPython = Join-Path $Root '.venv\Scripts\python.exe'
$Requirements = Join-Path $Root 'requirements.txt'

$MinVersion = [version]'3.12.0'

# ---------------------------------------------------------------------------
# Colored step reporting helpers
# ---------------------------------------------------------------------------
function Write-Step([string]$Message) {
    Write-Host ""
    Write-Host "==> $Message" -ForegroundColor Cyan
}
function Write-Ok([string]$Message) {
    Write-Host "    [OK]  $Message" -ForegroundColor Green
}
function Write-Warn2([string]$Message) {
    Write-Host "    [WARN] $Message" -ForegroundColor Yellow
}
function Write-Fail([string]$Message) {
    Write-Host "    [FAIL] $Message" -ForegroundColor Red
}

# ---------------------------------------------------------------------------
# Python discovery / version helpers
# ---------------------------------------------------------------------------

# Returns a System.Version for a python executable, or $null on any failure.
function Get-PythonVersion([string]$Exe) {
    try {
        $raw = & $Exe -c 'import sys; print("%d.%d.%d" % sys.version_info[:3])'
        if ($LASTEXITCODE -ne 0) { return $null }
        return [version]$raw.Trim()
    } catch {
        return $null
    }
}

# Finds a usable system Python >= 3.12: prefers the `py` launcher, then `python`.
# Returns a hashtable @{ Exe = <string>; Version = <version> } or $null.
function Find-SystemPython {
    $candidates = @()
    $pyLauncher = Get-Command 'py' -ErrorAction SilentlyContinue
    if ($pyLauncher) { $candidates += 'py' }
    $pythonExe = Get-Command 'python' -ErrorAction SilentlyContinue
    if ($pythonExe) { $candidates += $pythonExe.Source }

    foreach ($candidate in $candidates) {
        $version = Get-PythonVersion $candidate
        if ($version -and $version -ge $MinVersion) {
            return @{ Exe = $candidate; Version = $version }
        }
    }
    return $null
}

# Validates the venv interpreter: returns its version, or $null when missing/broken.
function Test-VenvInterpreter {
    if (-not (Test-Path -LiteralPath $VenvPython)) { return $null }
    return Get-PythonVersion $VenvPython
}

Write-Step "Bulk Download Manager setup (root: $Root)"

# ---------------------------------------------------------------------------
# Step 0: requirements must exist
# ---------------------------------------------------------------------------
Write-Step "Checking requirements.txt"
if (-not (Test-Path -LiteralPath $Requirements)) {
    Write-Fail "requirements.txt not found at: $Requirements"
    Write-Host "Setup FAILED - the repository appears incomplete." -ForegroundColor Red
    exit 1
}
Write-Ok "requirements.txt present"

# ---------------------------------------------------------------------------
# Step 1: venv - reuse if valid, recreate if stale/broken, create if missing
# ---------------------------------------------------------------------------
Write-Step "Checking virtual environment (.venv)"

$venvVersion = Test-VenvInterpreter
if ($venvVersion) {
    if ($venvVersion -ge $MinVersion) {
        Write-Ok ("venv interpreter is Python {0} (>= 3.12) - reusing" -f $venvVersion)
    } else {
        Write-Warn2 ("venv interpreter is Python {0} (< 3.12) - recreating" -f $venvVersion)
        Remove-Item -LiteralPath $VenvDir -Recurse -Force
        Write-Ok "stale .venv removed"
        $venvVersion = $null
    }
} elseif (Test-Path -LiteralPath $VenvDir) {
    Write-Warn2 ".venv exists but its python interpreter is broken/missing - recreating"
    Remove-Item -LiteralPath $VenvDir -Recurse -Force
    Write-Ok "broken .venv removed"
    $venvVersion = $null
}

if (-not $venvVersion) {
    Write-Step "Locating a system Python >= 3.12"
    $system = Find-SystemPython
    if ($null -eq $system) {
        Write-Fail "No Python >= 3.12 found on PATH (checked 'py' launcher and 'python')."
        Write-Host ""
        Write-Host "ACTION REQUIRED:" -ForegroundColor Red
        Write-Host "  Install Python 3.12 or newer from https://www.python.org/downloads/" -ForegroundColor Red
        Write-Host "  (tick 'Add python.exe to PATH' in the installer), then re-run:" -ForegroundColor Red
        Write-Host "    powershell -NoProfile -ExecutionPolicy Bypass -File scripts\setup.ps1" -ForegroundColor Red
        exit 1
    }
    Write-Ok ("system Python {0} at '{1}'" -f $system.Version, $system.Exe)

    Write-Step "Creating .venv"
    & $system.Exe -m venv $VenvDir
    if ($LASTEXITCODE -ne 0) {
        Write-Fail "Failed to create virtual environment at: $VenvDir"
        Write-Host ""
        Write-Host "ACTION REQUIRED: delete '$VenvDir' manually if it is corrupt, then re-run scripts\setup.ps1" -ForegroundColor Red
        exit 1
    }
    Write-Ok ".venv created"
} else {
    Write-Step "Locating a system Python >= 3.12"
    Write-Ok "skipped - venv already valid"
}

# Re-validate the venv interpreter after creation, belt and braces.
$venvVersion = Test-VenvInterpreter
if ((-not $venvVersion) -or ($venvVersion -lt $MinVersion)) {
    Write-Fail ".venv\Scripts\python.exe is missing or below Python 3.12 after creation."
    Write-Host ""
    Write-Host "ACTION REQUIRED: delete '$VenvDir' and re-run scripts\setup.ps1" -ForegroundColor Red
    exit 1
}

# ---------------------------------------------------------------------------
# Step 2: pip upgrade
# ---------------------------------------------------------------------------
Write-Step "Upgrading pip inside the venv"
& $VenvPython -m pip install --upgrade pip --quiet --disable-pip-version-check
if ($LASTEXITCODE -ne 0) {
    Write-Warn2 "pip upgrade failed (non-fatal) - continuing with the bundled pip"
} else {
    $pipVersion = (& $VenvPython -m pip --version).Trim()
    Write-Ok $pipVersion
}

# ---------------------------------------------------------------------------
# Step 3: install requirements
# ---------------------------------------------------------------------------
Write-Step "Installing requirements"
& $VenvPython -m pip install -r $Requirements --disable-pip-version-check
if ($LASTEXITCODE -ne 0) {
    Write-Fail "pip install -r requirements.txt failed"
    Write-Host ""
    Write-Host "ACTION REQUIRED: check your network connection, or pin conflicts in:" -ForegroundColor Red
    Write-Host "  $Requirements" -ForegroundColor Red
    Write-Host "Then re-run scripts\setup.ps1." -ForegroundColor Red
    exit 1
}
Write-Ok "all requirements installed"

# ---------------------------------------------------------------------------
# Summary
# ---------------------------------------------------------------------------
Write-Host ""
Write-Host "=============================================" -ForegroundColor Green
Write-Host " Setup COMPLETE" -ForegroundColor Green
Write-Host "=============================================" -ForegroundColor Green
Write-Host ("  venv   : {0}" -f $VenvPython)
Write-Host ("  python : {0}" -f $venvVersion)
Write-Host ""
Write-Host "Run the app with:"
Write-Host "    powershell -NoProfile -ExecutionPolicy Bypass -File scripts\run.ps1"
Write-Host ""
