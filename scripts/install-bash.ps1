<#
.SYNOPSIS
安装随包 Git Bash（§11.2）：下载 Git for Windows Portable 并解压到 tpi.exe 同目录的 git\，
使 tpi 的 bash 工具无需系统安装 Git 即可使用。

.DESCRIPTION
资产从 `github.com/.../releases/download/...` 直连下载，并使用 GitHub release
资产页（API 作为回退）返回的 digest 校验；无法取得可信 SHA-256 时拒绝执行。流程：

1. 定位 tpi.exe（默认 %USERPROFILE%\.cargo\bin\tpi.exe；可 -InstallDir 覆盖）；
2. 确定 release tag：-Version 显式指定，否则 HEAD 请求 `releases/latest`
   跟随重定向，从最终 URL 提取最新 tag（网页重定向不受 API 限流）；
3. 构造 PortableGit 资产名直连下载（asset 名带 patch 号，如 tag
   `v2.55.0.windows.3` → `PortableGit-2.55.0.3-64-bit.7z.exe`；旧命名无 patch，
   自动探测 fallback）；
4. 从 GitHub release 资产页读取精确资产的 SHA-256（或使用 -Sha256 显式值）；
5. 自解压到 <目录>\git\，校验 <目录>\git\bin\bash.exe 存在。

.PARAMETER InstallDir
安装目标目录（默认 tpi.exe 所在目录；找不到时用当前目录）。

.PARAMETER Version
指定 Git 版本 tag（推荐完整形式，如 v2.55.0.windows.3）；默认最新 release。
注意 git-for-windows 的 tag 一律带 patch（vX.Y.Z.windows.N），直接指定时请用完整 tag。

.PARAMETER Sha256
PortableGit 资产的预期 SHA-256。通常无需指定；GitHub API 被限流时可作为安全回退。

.EXAMPLE
.\scripts\install-bash.ps1
.EXAMPLE
.\scripts\install-bash.ps1 -InstallDir C:\tools\tpi -Version v2.55.0.windows.3
#>
[CmdletBinding()]
param(
    [string]$InstallDir,
    [string]$Version,
    [string]$Sha256
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
if ($tag -notmatch '^v\d+\.\d+\.\d+(?:\.windows\.\d+)?$') {
    throw "Git for Windows tag 格式无效: $tag"
}
if ($Sha256 -and $Sha256 -notmatch '^[a-fA-F0-9]{64}$') {
    throw "Sha256 必须是 64 位十六进制字符串"
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

$expectedHash = if ($Sha256) { $Sha256.ToLowerInvariant() } else { $null }
if (-not $expectedHash) {
    try {
        $expandedUrl = "https://github.com/git-for-windows/git/releases/expanded_assets/$tag"
        $expanded = Invoke-WebRequest -Uri $expandedUrl -UseBasicParsing -ErrorAction Stop
        $assetPath = "/git-for-windows/git/releases/download/$tag/$assetName"
        # 摘要必须位于精确资产链接所在的同一个 <li> 内，不能误配其他资产的 hash。
        $pattern = '(?s)href="' + [regex]::Escape($assetPath) + `
            '"(?:(?!</li>).)*?sha256:([a-fA-F0-9]{64})'
        $match = [regex]::Match($expanded.Content, $pattern)
        if (-not $match.Success) { throw "资产页未返回 SHA-256 digest" }
        $expectedHash = $match.Groups[1].Value.ToLowerInvariant()
    }
    catch {
        Write-Verbose "release 资产页摘要读取失败：$($_.Exception.Message)；尝试 API。"
    }
}
if (-not $expectedHash) {
    try {
        $headers = @{
            Accept = 'application/vnd.github+json'
            'User-Agent' = 'tpi-installer'
            'X-GitHub-Api-Version' = '2022-11-28'
        }
        $release = Invoke-RestMethod `
            -Uri "https://api.github.com/repos/git-for-windows/git/releases/tags/$tag" `
            -Headers $headers -ErrorAction Stop
        $asset = @($release.assets | Where-Object { $_.name -eq $assetName })
        if ($asset.Count -ne 1) {
            throw "release API 中未唯一找到资产 $assetName"
        }
        if ($asset[0].digest -notmatch '^sha256:([a-fA-F0-9]{64})$') {
            throw "release API 未返回 SHA-256 digest"
        }
        $expectedHash = $Matches[1].ToLowerInvariant()
    }
    catch {
        throw "无法取得可信 SHA-256：$($_.Exception.Message)。请稍后重试或用 -Sha256 显式提供。"
    }
}

Write-Host "下载 $assetName ..."
$tmp = Join-Path ([System.IO.Path]::GetTempPath()) ("tpi-" + [guid]::NewGuid().ToString('N') + '.7z.exe')
try {
    Invoke-WebRequest -Uri $downloadUrl -OutFile $tmp -UseBasicParsing
    $actualHash = (Get-FileHash -Path $tmp -Algorithm SHA256).Hash.ToLowerInvariant()
    if ($actualHash -ne $expectedHash) {
        throw "SHA-256 校验失败：期望 $expectedHash，实际 $actualHash"
    }
    Write-Host "SHA-256 校验通过。"

    New-Item -ItemType Directory -Force -Path $gitDir | Out-Null
    Write-Host "解压到 $gitDir ..."
    # PortableGit 是 7-Zip 自解压包：-o 输出目录，-y 覆盖确认。
    & $tmp "-o$gitDir" -y
    if ($LASTEXITCODE -ne 0) {
        throw "PortableGit 解压失败（exit $LASTEXITCODE）"
    }
}
finally {
    if (Test-Path -LiteralPath $tmp) {
        try { Remove-Item -LiteralPath $tmp -Force -ErrorAction Stop }
        catch { Write-Host "警告：未能删除临时文件 $tmp（可稍后手动删除）" }
    }
}

if (-not (Test-Path $bash)) {
    throw "解压后未找到 $bash（安装可能不完整）"
}
Write-Host "完成：$bash"
Write-Host "运行 tpi 后 bash 工具将自动使用随包 Git Bash。"
