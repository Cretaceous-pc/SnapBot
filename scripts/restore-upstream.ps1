#requires -Version 5.1
<#
.SYNOPSIS
    从原档 zip 恢复上游 nanobot 源码检出，并完成 editable 安装。

.DESCRIPTION
    上游 nanobot-0.3.5/ 不入库（527MB / 47406 文件）。此脚本从 原档/ 里的
    zip 解压恢复，然后建 venv 并 editable 安装。

    解压后的源码**不包含 .venv 与 node_modules**，所以恢复后必须重新安装：
      - Python 依赖（uv pip install -e .）
      - 前端产物（首次跑 webui 时自动 bun install + vite build）

.NOTES
    必须设 Clash 代理，否则 pythonhosted / npm registry 会卡死。
    作者环境实证：直连超 3 分钟，走代理秒级完成。
#>
[CmdletBinding()]
param(
    [string] $Root = (Split-Path -Parent $PSScriptRoot),
    [switch] $SkipInstall,
    [switch] $Force
)

$ErrorActionPreference = 'Stop'

$UpstreamDir = Join-Path $Root 'nanobot-0.3.5'
$ArchiveDir  = Join-Path $Root '原档'
$Archive     = Join-Path $ArchiveDir 'nanobot-0.3.5.zip'
$VenvPython  = Join-Path $UpstreamDir '.venv\Scripts\python.exe'

function Write-Step([string] $Message) { Write-Host "==> $Message" -ForegroundColor Cyan }
function Write-Warn([string] $Message) { Write-Host "!!  $Message" -ForegroundColor Yellow }

# ---- 前置检查 ----------------------------------------------------------
if (-not (Test-Path $Archive)) {
    throw "找不到原档: $Archive`n    请把上游 nanobot-0.3.5.zip 放到 原档/ 下。"
}

if ((Test-Path $UpstreamDir) -and -not $Force) {
    $count = (Get-ChildItem $UpstreamDir -Recurse -Force -File -ErrorAction SilentlyContinue).Count
    if ($count -gt 1000) {
        Write-Host "上游检出已存在（$count 个文件），跳过解压。"
        Write-Host "    要强制重来请加 -Force（会先删除现有目录）。"
    } else {
        Write-Warn "目录存在但文件数异常（$count），视为残留，重新解压。"
        $Force = $true
    }
}

# ---- 解压 --------------------------------------------------------------
if (-not (Test-Path $UpstreamDir) -or $Force) {
    if (Test-Path $UpstreamDir) {
        Write-Step "删除现有 $UpstreamDir"
        Remove-Item $UpstreamDir -Recurse -Force
    }
    Write-Step "解压 $Archive"
    Expand-Archive -LiteralPath $Archive -DestinationPath $Root -Force
    if (-not (Test-Path (Join-Path $UpstreamDir 'pyproject.toml'))) {
        throw "解压后没有找到 $UpstreamDir\pyproject.toml，原档结构与预期不符。"
    }
    Write-Host "    解压完成。"
}

# ---- 代理 --------------------------------------------------------------
if (-not $env:HTTP_PROXY) {
    $proxy = 'http://127.0.0.1:7897'
    Write-Warn "未检测到 HTTP_PROXY，本次设为 Clash 默认值 $proxy（装依赖必需）"
    $env:HTTP_PROXY  = $proxy
    $env:HTTPS_PROXY = $proxy
    $env:ALL_PROXY   = $proxy
}

# ---- venv + editable 安装 ---------------------------------------------
if ($SkipInstall) {
    Write-Host "已指定 -SkipInstall，跳过依赖安装。"
    exit 0
}

if (-not (Get-Command uv -ErrorAction SilentlyContinue)) {
    throw "找不到 uv。请先安装：https://docs.astral.sh/uv/"
}

if (-not (Test-Path $VenvPython)) {
    Write-Step "创建 venv（Python 3.13，避开过新的 3.14）"
    uv venv --python 3.13 (Join-Path $UpstreamDir '.venv')
} else {
    Write-Host "venv 已存在，跳过创建。"
}

Write-Step "editable 安装 nanobot"
uv pip install --python $VenvPython -e $UpstreamDir

Write-Step "验证"
& $VenvPython -c "import nanobot; print('nanobot', nanobot.__version__, '->', nanobot.__file__)"

Write-Host ""
Write-Host "恢复完成。启动：" -ForegroundColor Green
Write-Host "    cd `"$UpstreamDir`""
Write-Host "    .\.venv\Scripts\nanobot.exe webui --yes"
Write-Host ""
Write-Host "注意：前端产物（nanobot\web\dist）不在原档里，首次跑 webui 会自动"
Write-Host "      bun install + vite build（约 90 秒，需代理与 bun）。"
