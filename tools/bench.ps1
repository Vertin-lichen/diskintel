<#
磁盘情报系统 · 运行数据采集（给 README 的性能表用）
用 pwsh 7 运行：
  pwsh -File tools\bench.ps1 -Exe D:\di-build\lead\release\diskintel-rs.exe -Runs 5
输出：控制台表格 + docs\bench-<时间>.md
#>
[CmdletBinding()]
param(
    [Parameter(Mandatory = $true)][string] $Exe,
    [int] $Runs = 5,
    [string] $Snapshot = '20261112-100008',
    [string] $OutDir = ''
)

$ErrorActionPreference = 'Stop'
if (-not (Test-Path -LiteralPath $Exe)) { throw "exe 不存在：$Exe" }
if (-not $OutDir) { $OutDir = Join-Path (Split-Path -Parent $PSScriptRoot) 'docs' }
New-Item -ItemType Directory -Force -Path $OutDir | Out-Null
$shots = Join-Path $env:TEMP 'diskintel-bench'
New-Item -ItemType Directory -Force -Path $shots | Out-Null

function Median([double[]] $values) {
    if ($values.Count -eq 0) { return 0 }
    $sorted = $values | Sort-Object
    $mid = [int]([math]::Floor($sorted.Count / 2))
    if ($sorted.Count % 2 -eq 1) { return $sorted[$mid] }
    return ($sorted[$mid - 1] + $sorted[$mid]) / 2
}

function Run-Shot([string[]] $extra) {
    $out = Join-Path $shots ("shot-" + [guid]::NewGuid().ToString('N') + ".png")
    $log = & $Exe --shot $out --frames 30 @extra 2>&1
    $first = ($log | Select-String -Pattern '首帧准备\s+([0-9.]+) ms').Matches
    $rss = ($log | Select-String -Pattern '\[rss\]\s+([0-9.]+) MB').Matches
    Remove-Item -LiteralPath $out -ErrorAction SilentlyContinue
    [pscustomobject]@{
        FirstFrameMs = if ($first.Count) { [double]$first[0].Groups[1].Value } else { 0 }
        RssMB        = if ($rss.Count) { [double]$rss[0].Groups[1].Value } else { 0 }
        Log          = $log
    }
}

Write-Host "采集启动首帧 / 内存（$Runs 轮）"
$searchFirst = @(); $spaceFirst = @(); $searchRss = @(); $spaceRss = @()
for ($i = 0; $i -lt $Runs; $i++) {
    $r = Run-Shot @('--snapshot', $Snapshot, '--view', 'search', '--query', '报告', '--source', 'snapshot', '--size', '1200x760')
    $searchFirst += $r.FirstFrameMs; $searchRss += $r.RssMB
    $r = Run-Shot @('--snapshot', $Snapshot, '--view', 'space', '--size', '1200x760')
    $spaceFirst += $r.FirstFrameMs; $spaceRss += $r.RssMB
}

Write-Host '采集查询耗时（snapshot 模式）'
$terms = @('dll', '报告', 'ext:docx', 'after:2026-01-01')
$queryRows = @()
foreach ($term in $terms) {
    $ms = @(); $total = 0
    for ($i = 0; $i -lt $Runs; $i++) {
        $json = & $Exe --json search $term --limit 200 --snapshot $Snapshot 2>$null | ConvertFrom-Json
        if ($json.ok) {
            $ms += [double]$json.meta.elapsed_ms
            $total = [int]$json.data.total
        }
    }
    $queryRows += [pscustomobject]@{ Term = $term; Hits = $total; MedianMs = [math]::Round((Median $ms), 1) }
}

Write-Host '采集对比耗时'
$diffMs = @()
for ($i = 0; $i -lt $Runs; $i++) {
    $log = & $Exe --shot (Join-Path $shots 'diff.png') --frames 4000 --snapshot $Snapshot --view diff --diff-old 20261109-212714 --diff-new 20261112-100008 2>&1
    $m = ($log | Select-String -Pattern '对比完成.*?([0-9.]+) ms').Matches
    if ($m.Count) { $diffMs += [double]$m[0].Groups[1].Value }
}
Remove-Item -LiteralPath (Join-Path $shots 'diff.png') -ErrorAction SilentlyContinue

$ver = (& $Exe --version 2>$null | Select-Object -First 1)
$now = Get-Date -Format 'yyyy-MM-dd HH:mm'
$lines = @(
    "# 运行数据（$now，$Runs 轮中位）",
    '',
    "| 指标 | 数值 |",
    '|---|---:|',
    ("| 冷启动首帧（检索视图） | {0} ms |" -f [math]::Round((Median $searchFirst), 0)),
    ("| 冷启动首帧（空间视图） | {0} ms |" -f [math]::Round((Median $spaceFirst), 0)),
    ("| 常驻内存（检索） | {0} MB |" -f [math]::Round((Median $searchRss), 0)),
    ("| 常驻内存（空间） | {0} MB |" -f [math]::Round((Median $spaceRss), 0)),
    ("| 对比两份 446 万行快照 | {0} ms |" -f [math]::Round((Median $diffMs), 0)),
    '',
    '| 查询 | 命中 | 耗时（snapshot） |',
    '|---|---:|---:|'
)
foreach ($row in $queryRows) { $lines += ("| {0} | {1} | {2} ms |" -f $row.Term, $row.Hits, $row.MedianMs) }
$marker = [System.IO.Path]::GetFileNameWithoutExtension($Exe)
$out = Join-Path $OutDir ('bench-' + (Get-Date -Format 'yyyyMMdd-HHmmss') + '.md')
[System.IO.File]::WriteAllText($out, ($lines -join [Environment]::NewLine) + [Environment]::NewLine, [System.Text.UTF8Encoding]::new($false))
Write-Host ''
Write-Host ($lines -join [Environment]::NewLine)
Write-Host ''
Write-Host "已写入 $out"
