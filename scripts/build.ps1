<#
.SYNOPSIS
    Full test suite + one-folder PyInstaller build of Bulk Download Manager.

.DESCRIPTION
    1. Verifies the .venv exists (scripts\setup.ps1 must have run).
    2. Runs the FULL pytest suite; any failure aborts the build with that
       exit code (never swallowed).
    3. Ensures pyinstaller (>= 6.0) is installed in the venv and reports it.
    4. Builds a one-folder (--onedir) no-console (--windowed) Windows GUI
       bundle with dev-only modules (tests/pytest/pip/setuptools) excluded.

    Output: dist\BulkDownloadManager\BulkDownloadManager.exe
    The produced BulkDownloadManager.spec at the repo root is a build
    artifact (git-ignored).

.USAGE
    powershell -NoProfile -ExecutionPolicy Bypass -File scripts\build.ps1
#>

$ErrorActionPreference = 'Stop'

$Root = Split-Path -Parent $PSScriptRoot
$VenvPython = Join-Path $Root '.venv\Scripts\python.exe'
$SpecName = 'BulkDownloadManager'
$ExePath = Join-Path $Root 'dist\BulkDownloadManager\BulkDownloadManager.exe'

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

Write-Step "Bulk Download Manager build (root: $Root)"

# ---------------------------------------------------------------------------
# Step 1: venv must exist
# ---------------------------------------------------------------------------
Write-Step "Checking virtual environment"
if (-not (Test-Path -LiteralPath $VenvPython)) {
    Write-Fail "Virtual environment not found: $VenvPython"
    Write-Host ""
    Write-Host "Run setup first:" -ForegroundColor Yellow
    Write-Host "    powershell -NoProfile -ExecutionPolicy Bypass -File scripts\setup.ps1" -ForegroundColor Yellow
    exit 1
}
Write-Ok $VenvPython

# ---------------------------------------------------------------------------
# Step 2: full test suite - gate the build on green tests
# ---------------------------------------------------------------------------
Write-Step "Running full test suite"
Push-Location $Root
try {
    # -o addopts= neutralizes the -q addopts from pyproject.toml so the
    # run is explicit and its exit code is trustworthy.
    & $VenvPython -m pytest tests -q -o addopts=
    $testCode = $LASTEXITCODE
} finally {
    Pop-Location
}
if ($testCode -ne 0) {
    Write-Fail "Test suite failed with exit code $testCode - build ABORTED."
    exit $testCode
}
Write-Ok "test suite passed"

# ---------------------------------------------------------------------------
# Step 3: PyInstaller availability
# ---------------------------------------------------------------------------
Write-Step "Checking PyInstaller"
$pyinstallerOk = $false
try {
    & $VenvPython -c "import importlib.util; raise SystemExit(0 if importlib.util.find_spec('PyInstaller') else 1)"
    $pyinstallerOk = ($LASTEXITCODE -eq 0)
} catch {
    $pyinstallerOk = $false
}

if (-not $pyinstallerOk) {
    Write-Warn2 "PyInstaller not installed - installing 'pyinstaller>=6.0'"
    & $VenvPython -m pip install 'pyinstaller>=6.0' --disable-pip-version-check
    if ($LASTEXITCODE -ne 0) {
        Write-Fail "Failed to install PyInstaller"
        exit 1
    }
}

# Version reporting is informational: if it fails for any reason (stderr
# noise under $ErrorActionPreference Stop, etc.) the real build step below
# still surfaces any genuine PyInstaller failure.
$pyinstallerVersion = 'unknown'
try {
    $raw = & $VenvPython -m PyInstaller --version
    if ($LASTEXITCODE -eq 0) { $pyinstallerVersion = ($raw | Select-Object -Last 1).Trim() }
} catch {
    $pyinstallerVersion = 'unknown (could not query)'
}
Write-Ok "PyInstaller $pyinstallerVersion"

# ---------------------------------------------------------------------------
# Step 4: build (one-folder, no console, GUI)
#
# Splatting an argument array instead of backtick line-continuations: robust
# to trailing whitespace and trivially reviewable/editable.
#
# Notes on flags:
#   --onedir + --windowed  -> single distribution folder, no console window.
#   --paths src            -> classic 'src' import root (run.py imports src.*).
#   No --add-data          -> the theme QSS is Python code (src\app\theme.py);
#                             there are no resource/data files under src\.
#   Qt6 needs no HiDPI manifest tweaks; shiboken6 ships with PySide6
#   automatically (collect-all not required for a regular PySide6 install).
#   Dev-only modules are explicitly excluded to slim the bundle.
# ---------------------------------------------------------------------------
Write-Step "Building one-folder GUI bundle (dist\BulkDownloadManager\)"
$pyinstallerArgs = @(
    '--noconfirm',
    '--clean',
    '--onedir',
    '--windowed',
    '--name', $SpecName,
    '--paths', 'src',
    '--distpath', 'dist',
    '--workpath', 'build',
    '--specpath', '.',
    '--exclude-module', 'tests',
    '--exclude-module', 'pytest',
    '--exclude-module', 'pip',
    '--exclude-module', 'setuptools',
    '--exclude-module', 'pkg_resources',
    '--exclude-module', 'wheel',
    'run.py'
)
Push-Location $Root
try {
    & $VenvPython -m PyInstaller @pyinstallerArgs
    if ($LASTEXITCODE -ne 0) {
        Write-Fail "PyInstaller exited with code $LASTEXITCODE - build ABORTED."
        exit $LASTEXITCODE
    }
} finally {
    Pop-Location
}
Write-Ok "PyInstaller build finished"

# ---------------------------------------------------------------------------
# Step 5: verify output
# ---------------------------------------------------------------------------
Write-Step "Verifying build output"
if (-not (Test-Path -LiteralPath $ExePath)) {
    Write-Fail "Expected executable not found: $ExePath"
    exit 1
}
$resolved = (Resolve-Path -LiteralPath $ExePath).Path
$sizeMb = [math]::Round((Get-Item -LiteralPath $resolved).Length / 1MB, 1)
Write-Ok ("Built: {0} ({1} MB)" -f $resolved, $sizeMb)

Write-Host ""
Write-Host "=============================================" -ForegroundColor Green
Write-Host " Build COMPLETE" -ForegroundColor Green
Write-Host "=============================================" -ForegroundColor Green
Write-Host ("  Executable : {0}" -f $resolved)
Write-Host ("  Distribution folder: {0}" -f (Join-Path $Root 'dist\BulkDownloadManager'))
Write-Host ""
Write-Host "Notes:"
Write-Host "  - Deploy the app by copying the whole 'dist\BulkDownloadManager' folder."
Write-Host "  - User data (DB, logs) always goes to %LOCALAPPDATA%\BulkDownloadManager,"
Write-Host "    never into the bundle - see src\utils\paths.py."
Write-Host ""
