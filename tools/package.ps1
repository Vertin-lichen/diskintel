# DiskIntel 发行包组装脚本
#
# 用法（在 diskintel-rs 目录下）：
#   powershell -ExecutionPolicy Bypass -File tools\package.ps1 -Version 0.2.0
#
# 依赖：已执行 cargo build --release；同级的 磁盘情报系统\dist\DiskIntelCLI.exe 已构建；
#       goz 位于 $env:DISKINTEL_GOZ 或 D:\soctftware_new\goz\goz.exe。
# 产物：dist\DiskIntel-<Version>\ 与 dist\DiskIntel-<Version>-win64.zip

[CmdletBinding()]
param(
    [string] $Version = '0.3.2',
    [string] $GozDir = $(if ($env:DISKINTEL_GOZ) { Split-Path $env:DISKINTEL_GOZ } else { 'D:\soctftware_new\goz' })
)

$ErrorActionPreference = 'Stop'

$root = Split-Path -Parent $PSScriptRoot
$py = Join-Path (Split-Path -Parent $root) '磁盘情报系统'
$exe = Join-Path $root 'target\release\diskintel-rs.exe'
$cli = Join-Path $py 'dist\DiskIntelCLI.exe'
$dist = Join-Path $root 'dist'
$out = Join-Path $dist "DiskIntel-$Version"
$zip = Join-Path $dist "DiskIntel-$Version-win64.zip"

foreach ($required in @($exe, $cli)) {
    if (-not (Test-Path $required)) { throw "缺少产物：$required" }
}
foreach ($name in @('goz.exe', 'gozd.exe')) {
    if (-not (Test-Path (Join-Path $GozDir $name))) { throw "缺少 goz 组件：$name" }
}

if (Test-Path $out) { Remove-Item $out -Recurse -Force }
New-Item -ItemType Directory -Path (Join-Path $out 'goz') -Force | Out-Null
New-Item -ItemType Directory -Path (Join-Path $out 'tools') -Force | Out-Null

Copy-Item $exe (Join-Path $out 'DiskIntel.exe')
Copy-Item $cli $out
Copy-Item (Join-Path $GozDir 'goz.exe') (Join-Path $out 'goz')
Copy-Item (Join-Path $GozDir 'gozd.exe') (Join-Path $out 'goz')
Copy-Item (Join-Path $py 'tools\install-engine-bridge.ps1') (Join-Path $out 'tools')
Copy-Item (Join-Path $py 'tools\run-bridge-elevated.vbs') (Join-Path $out 'tools')
# 授权脚本会硬性检查同目录的这个隐藏启动器，缺了会直接抛异常。
Copy-Item (Join-Path $py 'tools\start-gozd-hidden.vbs') (Join-Path $out 'tools')
# USN 校验器可选（脚本找不到时只告警），但一并带上可减少一次警告。
if (Test-Path (Join-Path $py 'tools\verify_usn_elevated.py')) {
    Copy-Item (Join-Path $py 'tools\verify_usn_elevated.py') (Join-Path $out 'tools')
}
# 说明文档用纯 ASCII 文件名：Compress-Archive 会对非 ASCII 文件名做二次编码，
# 解压后变成乱码（实测「使用说明.txt」会落盘为不可读字节）。
if (Test-Path (Join-Path $root 'docs\使用说明.txt')) {
    Copy-Item (Join-Path $root 'docs\使用说明.txt') (Join-Path $out 'README.txt')
}

if (Test-Path $zip) { Remove-Item $zip -Force }
Compress-Archive -Path $out -DestinationPath $zip -CompressionLevel Optimal

$total = (Get-ChildItem $out -Recurse -File | Measure-Object Length -Sum).Sum
$hash = (Get-FileHash $zip -Algorithm SHA256).Hash.ToLower()

Write-Host ("目录：{0}  （{1:N1} MB，{2} 个文件）" -f $out, ($total / 1MB), (Get-ChildItem $out -Recurse -File).Count)
Write-Host ("压缩：{0}  （{1:N1} MB）" -f $zip, ((Get-Item $zip).Length / 1MB))
Write-Host ("SHA256：{0}" -f $hash)
