<#
.SYNOPSIS
一键安装 TPI：编译安装 tpi + 下载随包 Git Bash（bash 工具无需系统安装 Git）。

.DESCRIPTION
1. `cargo install --path . --locked`（装到 %USERPROFILE%\.cargo\bin\tpi.exe）；
2. 调用 install-bash.ps1，把 Git for Windows Portable 解压到 tpi.exe 同目录的 git\。

.PARAMETER SkipBash
跳过 Git Bash 安装（已有 Git Bash 或用 WSL 时可选）。

.PARAMETER BashVersion
指定 Git 版本（如 2.49.0）；默认最新 release。

.EXAMPLE
.\scripts\install.ps1
.EXAMPLE
.\scripts\install.ps1 -SkipBash
.EXAMPLE
.\scripts\install.ps1 -BashVersion 2.49.0
#>
[CmdletBinding()]
param(
    [switch]$SkipBash,
    [string]$BashVersion
)

$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest

$projectDir = Split-Path -Parent $PSScriptRoot

Write-Host "== 1/2: 编译安装 tpi =="
Push-Location $projectDir
try {
    cargo install --path . --locked
    if ($LASTEXITCODE -ne 0) { throw "cargo install 失败（exit $LASTEXITCODE）" }
}
finally {
    Pop-Location
}

if ($SkipBash) {
    Write-Host "已跳过 Git Bash 安装。tpi 装好，bash 工具需要系统 Git Bash 或随包 bash。"
    exit 0
}

# 定位 cargo 安装目录（tpi.exe 所在），传给 install-bash.ps1。
$cargoHome = if ($env:CARGO_HOME) { $env:CARGO_HOME } else { Join-Path $HOME '.cargo' }
$binDir = Join-Path $cargoHome 'bin'

Write-Host "== 2/2: 安装随包 Git Bash =="
$bashScript = Join-Path $PSScriptRoot 'install-bash.ps1'
# 数组 splatting 按位置绑定，不能用于命名参数；显式传参。
if ($BashVersion) {
    & $bashScript -InstallDir $binDir -Version $BashVersion
} else {
    & $bashScript -InstallDir $binDir
}
if ($LASTEXITCODE -ne 0) { throw "install-bash.ps1 失败（exit $LASTEXITCODE）" }

Write-Host ""
Write-Host "安装完成："
Write-Host "  tpi.exe   -> $(Join-Path $binDir 'tpi.exe')"
Write-Host "  bash.exe  -> $(Join-Path $binDir 'git\bin\bash.exe')"
Write-Host "运行 tpi 即可使用（bash 工具将自动使用随包 Git Bash）。"
