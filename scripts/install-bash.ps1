<#
.SYNOPSIS
安装随包 Git Bash（§11.2）：下载 Git for Windows Portable 并解压到 tpi.exe 同目录的 git\，
使 tpi 的 bash 工具无需系统安装 Git 即可使用。

.DESCRIPTION
1. 定位 tpi.exe（默认 %USERPROFILE%\.cargo\bin\tpi.exe；可 -InstallDir 覆盖）；
2. 从 git-for-windows 最新 release 下载 PortableGit（约 250MB）；
3. 自解压到 <目录>\git\；
4. 校验 <目录>\git\bin\bash.exe 存在。

.PARAMETER InstallDir
安装目标目录（默认 tpi.exe 所在目录；找不到时用当前目录）。

.PARAMETER Version
指定 Git 版本（如 2.49.0）；默认最新 release。

.EXAMPLE
.\scripts\install-bash.ps1
.EXAMPLE
.\scripts\install-bash.ps1 -InstallDir C:\tools\tpi -Version 2.49.0
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

# 获取 release 资产（最新或指定版本）。
$apiHeaders = @{ 'User-Agent' = 'tpi-installer' }
if ($Version) {
    $release = Invoke-RestMethod -Uri "https://api.github.com/repos/git-for-windows/git/releases/tags/v$Version" -Headers $apiHeaders
} else {
    $release = Invoke-RestMethod -Uri "https://api.github.com/repos/git-for-windows/git/releases/latest" -Headers $apiHeaders
}
$asset = $release.assets | Where-Object { $_.name -match '^PortableGit-.*-64-bit\.7z\.exe$' } | Select-Object -First 1
if (-not $asset) {
    throw "找不到 PortableGit 资产（release: $($release.tag_name)）"
}

Write-Host "下载 $($asset.name)（$([math]::Round($asset.size / 1MB, 1)) MB）..."
$tmp = Join-Path $env:TEMP $asset.name
Invoke-WebRequest -Uri $asset.browser_download_url -OutFile $tmp

$actualHash = (Get-FileHash -Path $tmp -Algorithm SHA256).Hash.ToLowerInvariant()
$expectedHash = $null
if ($release.body -match '(?i)PortableGit[^`r`n]*\.7z\.exe[^`r`n]*([0-9a-f]{64})') {
    $expectedHash = $Matches[1].ToLowerInvariant()
}
if ($expectedHash -and $actualHash -ne $expectedHash) {
    Remove-Item $tmp -Force
    throw "SHA-256 校验失败：期望 $expectedHash，实际 $actualHash"
}
if ($expectedHash) {
    Write-Host "SHA-256 校验通过。"
} else {
    Write-Host "未找到 release 中的 SHA-256，已下载但未校验（$actualHash）。"
}

New-Item -ItemType Directory -Force -Path $gitDir | Out-Null
Write-Host "解压到 $gitDir ..."
# PortableGit 是 7-Zip 自解压包：-o 输出目录，-y 覆盖确认。
& $tmp "-o$gitDir" -y
Remove-Item $tmp -Force

if (-not (Test-Path $bash)) {
    throw "解压后未找到 $bash（安装可能不完整）"
}
Write-Host "完成：$bash"
Write-Host "运行 tpi 后 bash 工具将自动使用随包 Git Bash。"
