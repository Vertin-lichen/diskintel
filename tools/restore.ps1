<#
磁盘情报系统 · 从灾备恢复
用 pwsh 7 运行：
  只校验：pwsh -File restore.ps1 -BackupDir <备份目录> -VerifyOnly -OutDir x
  真恢复：pwsh -File restore.ps1 -BackupDir <备份目录> -OutDir D:\diskintel-恢复
恢复动作：校验 SHA256 → 从 bundle 克隆源码 → cargo build --release --offline → 冒烟自检
#>
[CmdletBinding()]
param(
    [Parameter(Mandatory = $true)][string] $BackupDir,
    [Parameter(Mandatory = $true)][string] $OutDir,
    [switch] $VerifyOnly,
    [switch] $SkipBuild
)

$ErrorActionPreference = 'Stop'
$BackupDir = (Resolve-Path -LiteralPath $BackupDir).Path
$problems = 0

Write-Host '[1/5] 校验 SHA256SUMS.txt'
$sumFile = Join-Path $BackupDir 'SHA256SUMS.txt'
if (-not (Test-Path -LiteralPath $sumFile)) { throw "缺少 $sumFile" }
$checked = 0
foreach ($line in Get-Content -LiteralPath $sumFile) {
    if ($line -notmatch '^([0-9A-Fa-f]{64})\s{2}(.+)$') { continue }
    $expect = $Matches[1].ToUpperInvariant()
    $rel = $Matches[2]
    $file = Join-Path $BackupDir $rel
    if (-not (Test-Path -LiteralPath $file)) { Write-Warning "缺失：$rel"; $problems++; continue }
    $actual = (Get-FileHash -LiteralPath $file -Algorithm SHA256).Hash
    if ($actual -ne $expect) { Write-Warning "哈希不符：$rel"; $problems++ }
    $checked++
}
Write-Host ("      校验 {0} 个文件，问题 {1} 个" -f $checked, $problems)
if ($problems -gt 0) { throw "备份损坏（$problems 处），请换一个副本再试" }
if ($VerifyOnly) { Write-Host '校验通过（未做恢复）'; return }

Write-Host '[2/5] 准备输出目录'
if (Test-Path -LiteralPath $OutDir) { throw "输出目录已存在：$OutDir（换一个，或先自行清理）" }
New-Item -ItemType Directory -Force -Path $OutDir | Out-Null

Write-Host '[3/5] 从 bundle 克隆源码'
$bundle = Join-Path $BackupDir 'diskintel-rs.bundle'
if (-not (Test-Path -LiteralPath $bundle)) { throw '缺少 diskintel-rs.bundle' }
& git clone $bundle (Join-Path $OutDir 'diskintel-rs')
if ($LASTEXITCODE -ne 0) { throw 'git clone 失败' }
$cnBundle = Join-Path $BackupDir 'diskintel-cn.bundle'
if (Test-Path -LiteralPath $cnBundle) {
    & git clone $cnBundle (Join-Path $OutDir '磁盘情报系统')
    if ($LASTEXITCODE -ne 0) { Write-Warning '磁盘情报系统 bundle 克隆失败（可忽略）' }
}

Write-Host '[4/5] 复制二进制产物'
$zip = Get-ChildItem $BackupDir -Filter 'DiskIntel-*-win64.zip' | Select-Object -First 1
if ($zip) { Copy-Item $zip.FullName $OutDir -Force }
$cli = Join-Path $BackupDir 'DiskIntelCLI.exe'
$src = Join-Path $OutDir 'diskintel-rs'
if (Test-Path -LiteralPath $cli) {
    New-Item -ItemType Directory -Force -Path (Join-Path $src 'dist') | Out-Null
    Copy-Item $cli (Join-Path $src 'dist') -Force
}
if (Test-Path -LiteralPath (Join-Path $BackupDir 'goz')) {
    Copy-Item (Join-Path $BackupDir 'goz') (Join-Path $src 'goz') -Recurse -Force
}

Write-Host '[5/5] 编译与冒烟自检'
if (-not $SkipBuild) {
    Push-Location $src
    try {
        & cargo build --release --offline
        if ($LASTEXITCODE -ne 0) { throw 'cargo build 失败（依赖未缓存时可去掉 --offline）' }
    } finally { Pop-Location }
    $exe = Join-Path $src 'target\release\diskintel-rs.exe'
    & $exe --probe
    & $exe --json status | Select-Object -First 1
}
Write-Host ''
Write-Host ("恢复完成：{0}" -f $OutDir)
