<#
磁盘情报系统 · 灾备备份（防杀软误杀 / 磁盘损坏 / 误删）
用 pwsh 7 运行：
  pwsh -File tools\backup.ps1 -Dest 'F:\diskintel-灾备' -Mirror 'I:\diskintel-灾备'
产物（每次一个时间戳目录）：
  diskintel-rs.bundle          完整 Git 历史（含所有提交，可离线克隆）
  diskintel-cn.bundle         Python 参考实现/建库器的 Git 历史
  DiskIntel-<ver>-win64.zip   已构建发行包
  DiskIntelCLI.exe            建库器（发行包依赖，不在 Git 里）
  goz\                        实时引擎二进制（可选）
  docs\                       两份仓库的文档快照（纯文本，便于直接翻阅）
  snapshots-manifest.txt      快照清单（名称/大小/修改时间，用于核对数据是否被动过）
  SHA256SUMS.txt              全部文件的校验和
  restore.ps1                 一键恢复脚本（会先校验哈希再克隆重建）
  恢复说明.md                 人工步骤
#>
[CmdletBinding()]
param(
    [string] $Dest = 'F:\diskintel-灾备',
    [string] $Mirror = '',
    [string] $Version = '0.3.0',
    [string] $GozDir = 'D:\soctftware_new\goz',
    [string] $DataRoot = 'D:\disk-intel-data'
)

$ErrorActionPreference = 'Stop'
$repo = Split-Path -Parent $PSScriptRoot
$projects = Split-Path -Parent $repo
$cnRepo = Join-Path $projects '磁盘情报系统'
$stamp = Get-Date -Format 'yyyyMMdd-HHmmss'
$work = Join-Path $Dest $stamp

function Write-Utf8([string]$Path, [string]$Text) {
    $dir = Split-Path -Parent $Path
    if (-not (Test-Path -LiteralPath $dir)) { New-Item -ItemType Directory -Force -Path $dir | Out-Null }
    [System.IO.File]::WriteAllText($Path, $Text, [System.Text.UTF8Encoding]::new($false))
}

New-Item -ItemType Directory -Force -Path (Join-Path $work 'docs') | Out-Null
Write-Host "[1/7] 目标目录 $work"

Write-Host '[2/7] 导出 Git bundle'
& git -C $repo bundle create (Join-Path $work 'diskintel-rs.bundle') --all | Out-Null
if ($LASTEXITCODE -ne 0) { throw 'diskintel-rs bundle 失败' }
if (Test-Path -LiteralPath (Join-Path $cnRepo '.git')) {
    & git -C $cnRepo bundle create (Join-Path $work 'diskintel-cn.bundle') --all | Out-Null
    if ($LASTEXITCODE -ne 0) { throw '磁盘情报系统 bundle 失败' }
}

Write-Host '[3/7] 复制发行包与建库器'
$zip = Join-Path $repo ("dist\DiskIntel-$Version-win64.zip")
if (Test-Path -LiteralPath $zip) { Copy-Item $zip $work -Force }
$cli = Join-Path $cnRepo 'dist\DiskIntelCLI.exe'
if (Test-Path -LiteralPath $cli) { Copy-Item $cli $work -Force }
if (Test-Path -LiteralPath $GozDir) {
    New-Item -ItemType Directory -Force -Path (Join-Path $work 'goz') | Out-Null
    Get-ChildItem $GozDir -File | Where-Object { $_.Extension -in '.exe', '.md', '.txt' } |
        Copy-Item -Destination (Join-Path $work 'goz') -Force
}

Write-Host '[4/7] 复制文档快照'
Copy-Item (Join-Path $repo 'docs') (Join-Path $work 'docs\diskintel-rs') -Recurse -Force
if (Test-Path -LiteralPath (Join-Path $cnRepo 'docs')) {
    Copy-Item (Join-Path $cnRepo 'docs') (Join-Path $work 'docs\磁盘情报系统') -Recurse -Force
}
foreach ($name in @('README.md', 'CHANGELOG.md')) {
    $file = Join-Path $repo $name
    if (Test-Path -LiteralPath $file) { Copy-Item $file (Join-Path $work 'docs\diskintel-rs') -Force }
}

Write-Host '[5/7] 记录快照清单（不复制快照本体）'
$lines = @('# 快照清单（制作备份时观察到的状态）', "# 数据根：$DataRoot", '')
$snapDir = Join-Path $DataRoot 'snapshots'
if (Test-Path -LiteralPath $snapDir) {
    Get-ChildItem $snapDir -File | Sort-Object Name | ForEach-Object {
        $lines += ('{0}    {1:N0}    {2}' -f $_.Name, $_.Length, $_.LastWriteTime.ToString('yyyy-MM-dd HH:mm:ss'))
    }
} else {
    $lines += '（快照目录不存在）'
}
Write-Utf8 (Join-Path $work 'snapshots-manifest.txt') ($lines -join [Environment]::NewLine)

Write-Host '[6/7] 校验和'
Copy-Item (Join-Path $PSScriptRoot 'restore.ps1') $work -Force
$restoreDoc = @'
# 恢复说明（人工步骤）

1. 先校验完整性（推荐每次都做）：
     pwsh -File restore.ps1 -BackupDir <本目录> -VerifyOnly -OutDir <临时目录>
2. 一键恢复并从源码重建：
     pwsh -File restore.ps1 -BackupDir <本目录> -OutDir D:\diskintel-恢复
   产物在 <OutDir>\diskintel-rs（源码）与 <OutDir>\DiskIntel-<ver>-win64.zip（发行包副本）。
3. 只想用发行包：解压 zip 里的 DiskIntel-<ver> 目录即可，无需编译。
4. 快照不受灾备影响：D:\disk-intel-data\snapshots 请另行备份（体积大，按需）。
5. 若关键文件被杀软隔离：先从隔离区恢复并加排除，再用本目录的 bundle 恢复源码。
'@
Write-Utf8 (Join-Path $work '恢复说明.md') $restoreDoc
$sums = Get-ChildItem $work -Recurse -File | Where-Object { $_.Name -ne 'SHA256SUMS.txt' } |
    Sort-Object FullName |
    ForEach-Object {
        $hash = (Get-FileHash -LiteralPath $_.FullName -Algorithm SHA256).Hash
        $rel = $_.FullName.Substring($work.Length + 1)
        "$hash  $rel"
    }
Write-Utf8 (Join-Path $work 'SHA256SUMS.txt') (($sums -join [Environment]::NewLine) + [Environment]::NewLine)

Write-Host '[7/7] 镜像副本'
if ($Mirror) {
    $mirrorDir = Join-Path $Mirror $stamp
    New-Item -ItemType Directory -Force -Path $mirrorDir | Out-Null
    Copy-Item (Join-Path $work '*') $mirrorDir -Recurse -Force
    Write-Host "      已镜像到 $mirrorDir"
}

$size = (Get-ChildItem $work -Recurse -File | Measure-Object Length -Sum).Sum
Write-Host ''
Write-Host ("备份完成：{0}" -f $work)
Write-Host ("体积 {0:N1} MB / {1} 个文件" -f ($size / 1MB), (Get-ChildItem $work -Recurse -File).Count)
if (Test-Path -LiteralPath $zip) {
    Write-Host ("zip SHA256 = {0}" -f (Get-FileHash -LiteralPath $zip -Algorithm SHA256).Hash)
}
