<#
.SYNOPSIS
安装随包 Git Bash（§11.2）：下载 Git for Windows Portable 并解压到 tpi.exe 同目录的 git\，
使 tpi 的 bash 工具无需系统安装 Git 即可使用。

.DESCRIPTION
全程直连 `github.com/.../releases/download/...`，不调用 GitHub REST API
（api.github.com 未认证限流 60 次/小时/IP，曾导致安装失败）。流程：

1. 定位 tpi.exe（默认 %USERPROFILE%\.cargo\bin\tpi.exe；可 -InstallDir 覆盖）；
2. 确定 release tag：-Version 显式指定，否则 HEAD 请求 `releases/latest`
   跟随重定向，从最终 URL 提取最新 tag（网页重定向不受 API 限流）；
3. 构造 PortableGit 资产名直连下载（asset 名带 patch 号，如 tag
   `v2.55.0.windows.3` → `PortableGit-2.55.0.3-64-bit.7z.exe`；旧命名无 patch，
   自动探测 fallback）；
4. 抓取 release 页面 HTML 提取 SHA-256 校验（提取失败则警告但不中断，保持
   原降级语义）；
5. 自解压到 <目录>\git\，校验 <目录>\git\bin\bash.exe 存在。

.PARAMETER InstallDir
安装目标目录（默认 tpi.exe 所在目录；找不到时用当前目录）。

.PARAMETER Version
指定 Git 版本（如 2.49.0 或 v2.49.0.windows.3）；默认最新 release。
注意 git-for-windows 的 tag 一律带 patch（vX.Y.Z.windows.N），直接指定时请用完整 tag。

.EXAMPLE
.\scripts\install-bash.ps1
.EXAMPLE
.\scripts\install-bash.ps1 -InstallDir C:\tools\tpi -Version v2.55.0.windows.3
#>
[CmdletBinding()]
param(
    [string]$InstallDir,
    [string]$Version
)

$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest

# 定位安装目录：默认 tpi.exe 所在目录。
if (-not $InstallDir) {
    $cmd = Get-Command tpi -ErrorAction SilentlyContinue
    if ($cmd) {
        $InstallDir = Split-Path -Parent $cmd.Source
    } else {
        $InstallDir = (Get-Location).Path
    }
}
$gitDir = Join-Path $InstallDir 'git'
$bash = Join-Path $gitDir 'bin\bash.exe'

if (Test-Path $bash) {
    Write-Host "已存在 Git Bash: $bash"
    exit 0
}

Write-Host "安装目标: $gitDir"

# ---- 确定 release tag（直连，不走 GitHub REST API）----

function Get-LatestTag {
    # HEAD 跟随 releases/latest 重定向，从最终 URL 提取 tag（§11.2）。
    try {
        $resp = Invoke-WebRequest -Uri 'https://github.com/git-for-windows/git/releases/latest' `
            -Method Head -UseBasicParsing -ErrorAction Stop
        $finalUrl = $resp.BaseResponse.ResponseUri.AbsoluteUri
        if ($finalUrl -match '/releases/tag/([^/?#]+)') {
            return $Matches[1]
        }
        throw "重定向目标未包含 tag: $finalUrl"
    }
    catch {
        throw "无法确定最新 Git for Windows 版本：$($_.Exception.Message)（可用 -Version 显式指定）"
    }
}

$tag = if ($Version) {
    if ($Version -match '^v') { $Version } else { 'v' + $Version }
} else {
    Get-LatestTag
}
Write-Host "release tag: $tag"

# ---- 构造资产名并直连下载 ----
# asset 命名规则：tag `vX.Y.Z.windows.N` → `PortableGit-X.Y.Z.N-64-bit.7z.exe`
# （2.45+ 带 patch）；旧版本无 patch（`PortableGit-X.Y.Z-64-bit.7z.exe`）。逐个 HEAD 探测。

$assetNames = @()
if ($tag -match '^v(\d+\.\d+\.\d+)\.windows\.(\d+)$') {
    $assetNames += "PortableGit-$($Matches[1]).$($Matches[2])-64-bit.7z.exe"
}
$assetNames += "PortableGit-$($tag.TrimStart('v'))-64-bit.7z.exe"

$downloadUrl = $null
$assetName = $null
foreach ($name in $assetNames) {
    $url = "https://github.com/git-for-windows/git/releases/download/$tag/$name"
    try {
        Invoke-WebRequest -Uri $url -Method Head -UseBasicParsing -ErrorAction Stop | Out-Null
        $assetName = $name
        $downloadUrl = $url
        break
    }
    catch {
        Write-Verbose "资产不存在（$name），尝试下一个候选。"
    }
}
if (-not $downloadUrl) {
    throw "找不到 PortableGit 资产（tag: $tag，候选: $($assetNames -join ', ')）。请用 -Version 指定完整 tag。"
}

Write-Host "下载 $assetName（直连 releases/download，不经过 GitHub API）..."
$tmp = Join-Path $env:TEMP $assetName
Invoke-WebRequest -Uri $downloadUrl -OutFile $tmp -UseBasicParsing

# ---- SHA-256 校验：抓 release 页面 HTML 提取（网页不受 API 限流）----
$expectedHash = $null
try {
    $html = Invoke-WebRequest -Uri "https://github.com/git-for-windows/git/releases/tag/$tag" `
        -UseBasicParsing -ErrorAction Stop
    $pattern = '(?s)' + [regex]::Escape($assetName) + '.{0,400}?([a-f0-9]{64})'
    $m = [regex]::Match($html.Content, $pattern)
    if ($m.Success) {
        $expectedHash = $m.Groups[1].Value.ToLowerInvariant()
    }
}
catch {
    Write-Verbose "release 页面抓取失败：$($_.Exception.Message)"
}

$actualHash = (Get-FileHash -Path $tmp -Algorithm SHA256).Hash.ToLowerInvariant()
if ($expectedHash -and $actualHash -ne $expectedHash) {
    Remove-Item $tmp -Force
    throw "SHA-256 校验失败：期望 $expectedHash，实际 $actualHash"
}
if ($expectedHash) {
    Write-Host "SHA-256 校验通过。"
} else {
    Write-Host "未能在 release 页面提取到 SHA-256，已下载但未校验（$actualHash）。"
}

New-Item -ItemType Directory -Force -Path $gitDir | Out-Null
Write-Host "解压到 $gitDir ..."
# PortableGit 是 7-Zip 自解压包：-o 输出目录，-y 覆盖确认。
& $tmp "-o$gitDir" -y
# 自解压包的子进程可能短暂占用自身句柄，重试删除（最长 60s）；仍失败仅警告（安装已完成）。
$removed = $false
for ($i = 0; $i -lt 30; $i++) {
    Start-Sleep -Milliseconds 2000
    try {
        Remove-Item $tmp -Force -ErrorAction Stop
        $removed = $true
        break
    }
    catch {
        Write-Verbose "删除临时文件重试 $($i + 1)：$($_.Exception.Message)"
    }
}
if (-not $removed) {
    Write-Host "警告：未能删除临时文件 $tmp（可稍后手动删除）"
}

if (-not (Test-Path $bash)) {
    throw "解压后未找到 $bash（安装可能不完整）"
}
Write-Host "完成：$bash"
Write-Host "运行 tpi 后 bash 工具将自动使用随包 Git Bash。"
