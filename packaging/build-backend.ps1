#requires -Version 5.1
<#
.SYNOPSIS
    Freeze the nanobot backend into a distributable directory (PyInstaller onedir).

.DESCRIPTION
    Output: packaging/dist/nanobot-backend/  (contains nanobot-backend.exe + deps)

    Why onedir instead of onefile:
      - onefile is a major antivirus false-positive magnet on Windows
        (self-extracting behaviour).
      - onefile re-extracts to temp on every launch, which visibly slows cold start.
    The installer ships the whole directory, so the user never notices.

    IMPORTANT: keep this file ASCII-only.
    Windows PowerShell 5.1 reads .ps1 as ANSI/GBK unless the file has a UTF-8 BOM.
    Non-ASCII bytes then corrupt string literals and even line-continuation
    backticks, producing bizarre parser errors that point at the wrong line.

.PARAMETER Clean
    Delete dist/build first for a clean rebuild.

.PARAMETER SkipPyInstallerInstall
    Reuse an already installed PyInstaller.

.PARAMETER SkipSmoke
    Build only, no smoke test.

.PARAMETER KeepRunning
    Leave the smoke-test gateway running for manual inspection.

.NOTES
    Needs the Clash proxy (HTTP_PROXY=http://127.0.0.1:7897) to install PyInstaller.
#>
[CmdletBinding()]
param(
    [string] $Root = (Split-Path -Parent $PSScriptRoot),
    [switch] $Clean,
    [switch] $SkipPyInstallerInstall,
    [switch] $SkipSmoke,
    [switch] $KeepRunning
)

$ErrorActionPreference = 'Stop'

$Upstream   = Join-Path $Root 'nanobot-0.3.5'
$Packaging  = Join-Path $Root 'packaging'
$DistDir    = Join-Path $Packaging 'dist'
$WorkDir    = Join-Path $Packaging 'build'
$VenvPython = Join-Path $Upstream '.venv\Scripts\python.exe'
$Entry      = Join-Path $Packaging 'backend_entry.py'
$Spec       = Join-Path $Packaging 'snapbot-backend.spec'
$OutExe     = Join-Path $DistDir 'nanobot-backend\nanobot-backend.exe'

# Isolated smoke-test environment. Never touches the real ~/.nanobot.
$SmokeDir   = Join-Path $env:TEMP 'snapbot-freeze-smoke'
$HealthPort = 28901
$WebuiPort  = 28902

function Write-Step([string] $m) { Write-Host "==> $m" -ForegroundColor Cyan }
function Write-Warn([string] $m) { Write-Host "!!  $m" -ForegroundColor Yellow }

# ---------------------------------------------------------------- preflight
if (-not (Test-Path $VenvPython)) {
    throw "venv not found: $VenvPython`n    run scripts/restore-upstream.ps1 first"
}
if (-not (Test-Path $Entry)) { throw "entry script not found: $Entry" }
if (-not (Test-Path $Spec))  { throw "spec not found: $Spec" }

# ---------------------------------------------------------------- proxy
if (-not $env:HTTP_PROXY) {
    $proxy = 'http://127.0.0.1:7897'
    Write-Warn "HTTP_PROXY not set; using $proxy for this run (needed to install PyInstaller)"
    $env:HTTP_PROXY = $proxy; $env:HTTPS_PROXY = $proxy; $env:ALL_PROXY = $proxy
}

# ---------------------------------------------------------------- PyInstaller
if (-not $SkipPyInstallerInstall) {
    $hasPyInstaller = $false
    $prev = $ErrorActionPreference
    $ErrorActionPreference = 'Continue'
    try {
        & $VenvPython -c "import PyInstaller" *> $null
        $hasPyInstaller = ($LASTEXITCODE -eq 0)
    } catch {
        $hasPyInstaller = $false
    } finally {
        $ErrorActionPreference = $prev
    }
    if (-not $hasPyInstaller) {
        Write-Step "Installing PyInstaller into the venv"
        # Same trap as the PyInstaller run below: uv writes its progress
        # ("Using Python ... environment at: ...") to STDERR, and under
        # $ErrorActionPreference='Stop' any native stderr output becomes a
        # NativeCommandError that TERMINATES the script -- so the install
        # "fails" before it even starts. Relax the preference locally and
        # judge success by exit code only.
        $prevInstall = $ErrorActionPreference
        $ErrorActionPreference = 'Continue'
        try {
            & uv pip install --python $VenvPython pyinstaller
            $pipExit = $LASTEXITCODE
        } finally {
            $ErrorActionPreference = $prevInstall
        }
        if ($pipExit -ne 0) { throw "PyInstaller install failed (exit $pipExit)" }
    } else {
        Write-Host "PyInstaller already available."
    }
}

# ---------------------------------------------------------------- clean
if ($Clean) {
    foreach ($d in @($DistDir, $WorkDir)) {
        if (Test-Path $d) { Write-Step "Removing $d"; Remove-Item $d -Recurse -Force }
    }
}

# ------------------------------------------------- clean Tauri staging copy
# Why this matters: `cargo tauri build` copies bundle.resources INTO
# desktop/src-tauri/target/release/nanobot-backend and does NOT prune files
# that are no longer in the source. Vite emits content-hashed chunk names, so
# every WebUI rebuild produces NEW asset filenames and leaves the previous
# ones behind -- the installer then ships both, including dead chunks that may
# still contain removed features. Symptom: staged/assets has MORE files than
# the source dist (e.g. 473 vs 422), and index.html points at the new chunk
# while the old one lingers on disk.
# Fix: always drop the staging copy so the next tauri build packs a clean tree.
$Staging = Join-Path $Root 'desktop\src-tauri\target\release\nanobot-backend'
if (Test-Path $Staging) {
    Write-Step "Removing stale Tauri staging copy: $Staging"
    Remove-Item -LiteralPath $Staging -Recurse -Force
}

# ---------------------------------------------------------------- build
Write-Step "Running PyInstaller"
# PyInstaller writes its progress log to STDERR. Under
# $ErrorActionPreference='Stop', any native stderr output becomes a
# NativeCommandError that TERMINATES the script -- which looks like
# "the build died halfway for no reason". So relax it here, capture both
# streams into a log file ourselves, and judge success by exit code only.
$pyiLog = Join-Path $env:TEMP 'snapbot-pyinstaller.log'
if (Test-Path $pyiLog) { Remove-Item $pyiLog -Force }

$prevEap = $ErrorActionPreference
$ErrorActionPreference = 'Continue'
try {
    & $VenvPython -m PyInstaller $Spec --distpath $DistDir --workpath $WorkDir --noconfirm *> $pyiLog
    $pyiExit = $LASTEXITCODE
} finally {
    $ErrorActionPreference = $prevEap
}

Write-Host "    PyInstaller exit code: $pyiExit"
Write-Host "    log: $pyiLog"
if ($pyiExit -ne 0) {
    Write-Host "---- PyInstaller log tail ----"
    if (Test-Path $pyiLog) { Get-Content $pyiLog -Tail 40 }
    throw "PyInstaller build failed (exit $pyiExit)"
}

if (-not (Test-Path $OutExe)) { throw "build finished but output missing: $OutExe" }

$size = [math]::Round(((Get-ChildItem (Split-Path $OutExe) -Recurse -File |
    Measure-Object Length -Sum).Sum / 1MB), 1)
Write-Host "Output : $OutExe"
Write-Host "Size   : $size MB"

if ($SkipSmoke) { Write-Host "Smoke test skipped."; return }

# ---------------------------------------------------------------- smoke 1
Write-Step "Smoke 1/2: does the frozen CLI start?"
$versionOut = Join-Path $env:TEMP 'snapbot-freeze-version.out.txt'
$versionErr = Join-Path $env:TEMP 'snapbot-freeze-version.err.txt'
$vp = Start-Process -FilePath $OutExe -ArgumentList @('--version') `
    -RedirectStandardOutput $versionOut -RedirectStandardError $versionErr `
    -PassThru -NoNewWindow -Wait
if ($vp.ExitCode -ne 0) {
    Write-Host "---- stdout ----"
    if (Test-Path $versionOut) { Get-Content $versionOut }
    Write-Host "---- stderr ----"
    if (Test-Path $versionErr) { Get-Content $versionErr }
    throw "frozen --version failed with exit code $($vp.ExitCode)"
}
$version = (Get-Content $versionOut -Raw).Trim()
Write-Host "    $version"

# ---------------------------------------------------------------- smoke 2
Write-Step "Smoke 2/2: does it run as a gateway and answer /health?"
if (Test-Path $SmokeDir) { Remove-Item $SmokeDir -Recurse -Force }
New-Item -ItemType Directory -Force -Path $SmokeDir | Out-Null

# Isolation trick: point --config at a temp dir. Upstream set_config_path()
# derives the DATA dir from the config file's PARENT
# (nanobot/config/paths.py: get_data_dir() = get_config_path().parent),
# so the real ~/.nanobot stays untouched and the test is repeatable.
# There is no NANOBOT_DATA_DIR env var upstream -- do not rely on one.
# Ports are deliberately unusual so they cannot collide with a running gateway.
$config = @{
    channels = @{
        websocket = @{
            enabled = $true
            host    = '127.0.0.1'
            port    = $WebuiPort
            token   = 'smoke-test-secret'
        }
    }
    gateway  = @{ host = '127.0.0.1'; port = $HealthPort }
} | ConvertTo-Json -Depth 6

$configPath = Join-Path $SmokeDir 'config.json'
# Must be BOM-less UTF-8: upstream rejects a BOM with
# "Unexpected UTF-8 BOM (decode using utf-8-sig)".
# PowerShell 5.1's `Set-Content -Encoding UTF8` ADDS a BOM -- classic trap.
$utf8NoBom = New-Object System.Text.UTF8Encoding($false)
[System.IO.File]::WriteAllText($configPath, $config, $utf8NoBom)

$env:NANOBOT_SKIP_WEBUI_BUILD = '1'
$env:DEEPSEEK_API_KEY = 'smoke-test-key'

$logOut = Join-Path $SmokeDir 'gateway.out.log'
$logErr = Join-Path $SmokeDir 'gateway.err.log'
$proc = Start-Process -FilePath $OutExe `
    -ArgumentList @('gateway', '--foreground', '--port', $HealthPort, '--config', $configPath) `
    -RedirectStandardOutput $logOut -RedirectStandardError $logErr `
    -PassThru -NoNewWindow

$healthy = $false
$lastBody = ''
for ($i = 1; $i -le 30; $i++) {
    Start-Sleep -Seconds 2
    if ($proc.HasExited) { break }
    try {
        $resp = Invoke-WebRequest "http://127.0.0.1:$HealthPort/health" -TimeoutSec 4 -UseBasicParsing
        if ($resp.Content -match '"status"') { $healthy = $true; $lastBody = $resp.Content; break }
    } catch { }
}

if ($healthy) {
    Write-Host "    /health responded: $($lastBody.Trim())" -ForegroundColor Green
} else {
    Write-Warn "health endpoint never became ready. Process exited: $($proc.HasExited)"
    Write-Host "---- stdout tail ----"
    if (Test-Path $logOut) { Get-Content $logOut -Tail 20 }
    Write-Host "---- stderr tail ----"
    if (Test-Path $logErr) { Get-Content $logErr -Tail 30 }
}

if (-not $KeepRunning -and -not $proc.HasExited) {
    # /T so the gateway's own children (channels, cron) die too
    & taskkill /PID $proc.Id /T /F 2>&1 | Out-Null
    Start-Sleep -Seconds 2
}

if ($healthy) {
    Write-Host ""
    Write-Host "Frozen backend works." -ForegroundColor Green
    Write-Host "    output dir : $(Split-Path $OutExe)"
    Write-Host "    smoke env  : $SmokeDir"
    if ($KeepRunning) {
        Write-Host "    gateway still running (pid $($proc.Id), health :$HealthPort, webui :$WebuiPort)"
        Write-Host "    stop it with: taskkill /PID $($proc.Id) /T /F"
    }
} else {
    throw "frozen backend smoke test FAILED (see log tails above)"
}
