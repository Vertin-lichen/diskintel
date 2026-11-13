<#
磁盘情报系统 · 生成公开仓库导出目录（供 GitHub 发布）
用 pwsh 7 运行：
  pwsh -File tools\export-public.ps1 -OutDir D:\diskintel-public -Version 0.3.2
它会：
  1. 从当前 HEAD 导出干净源码树（不含 .git/target/dist/待删除）；
  2. 移除内部开发记录（进度/交接/并行任务汇总/机器专属清单）；
  3. 脱敏（个人路径、工具痕迹）；
  4. 重新 git init，以 GitHub noreply 身份做单次初始提交；
  5. 跑一遍公开前自检（密钥/私密信息），输出结果。
#>
[CmdletBinding()]
param(
    [Parameter(Mandatory = $true)][string] $OutDir,
    [string] $Version = '0.3.2'
)

$ErrorActionPreference = 'Stop'
$repo = Split-Path -Parent $PSScriptRoot
if (Test-Path -LiteralPath $OutDir) { throw "输出目录已存在：$OutDir（换一个或先移走）" }
New-Item -ItemType Directory -Force -Path $OutDir | Out-Null

Write-Host '[1/6] 导出 HEAD 源码树'
$tmpClone = $OutDir + '_clone_tmp'
if (Test-Path -LiteralPath $tmpClone) { Remove-Item -LiteralPath $tmpClone -Recurse -Force }
& git clone --quiet --no-hardlinks $repo $tmpClone
if ($LASTEXITCODE -ne 0) { throw 'git clone 失败' }
# 只保留工作树，丢弃 .git 与忽略产物
Remove-Item -LiteralPath (Join-Path $tmpClone '.git') -Recurse -Force
Get-ChildItem -LiteralPath $tmpClone -Force | ForEach-Object { Move-Item -LiteralPath $_.FullName -Destination $OutDir -Force }
Remove-Item -LiteralPath $tmpClone -Recurse -Force -ErrorAction SilentlyContinue

Write-Host '[2/6] 移除内部开发记录'
$drop = @(
    'docs\agents',
    'docs\进度-夜间开发-20261113.md',
    'docs\收尾-卸载与灾备-20261113.md',
    'docs\重启后检查清单-20261113.md',
    'docs\研究-并行任务交付汇总-20261113.md',
    'docs\方案C-分工与打包.md',
    'docs\实现记录-对齐Web版.md',
    'docs\实现记录-目录榜与快照切换.md',
    'docs\发版说明-0.1.0.md',
    'docs\研究-egui-037-UI手册-20261113.md',
    'OPENSOURCE_READY.md'
)
foreach ($rel in $drop) {
    $p = Join-Path $OutDir $rel
    if (Test-Path -LiteralPath $p) { Remove-Item -LiteralPath $p -Recurse -Force; Write-Host "      移除 $rel" }
}

Write-Host '[3/6] 脱敏'
$textFiles = Get-ChildItem $OutDir -Recurse -File -Include *.md,*.txt,*.rs,*.ps1,*.yml,*.iss,*.toml |
    Where-Object { $_.FullName -notmatch '\\\.git\\' }
$replacements = [ordered]@{
    'C:\Users\<user>'        = 'C:\Users\<user>'
    'C:/Users/<user>'        = 'C:/Users/<user>'
    'C:/Users/<user>'        = 'C:/Users/<user>'
    'C:/Users/<user>'        = 'c:/Users/<user>'
    'Users\<user>'           = 'Users\<user>'
    '<machine>'          = '<machine>'
    '沙箱'               = '沙箱'
    '开发环境会'                 = '开发环境会'
    '开发环境'                    = '开发环境'
    '并行任务'                  = '并行任务'
}
$changed = 0
foreach ($file in $textFiles) {
    $text = [System.IO.File]::ReadAllText($file.FullName, [System.Text.Encoding]::UTF8)
    $original = $text
    foreach ($key in $replacements.Keys) { $text = $text.Replace($key, $replacements[$key]) }
    if ($text -ne $original) {
        [System.IO.File]::WriteAllText($file.FullName, $text, [System.Text.UTF8Encoding]::new($false))
        $changed++
    }
}
Write-Host "      处理 $changed 个文件"

Write-Host '[4/6] 重新初始化 Git'
Push-Location $OutDir
try {
    & git init -q
    & git checkout -q -b master
    & git config user.name 'Vertin-lichen'
    & git config user.email 'Vertin-lichen@users.noreply.github.com'
    & git add -A
    & git -c commit.gpgsign=false commit -q -m "chore: DiskIntel $Version initial release"
    if ($LASTEXITCODE -ne 0) { throw 'git commit 失败' }
} finally { Pop-Location }

Write-Host '[5/6] 发布前自检'
$files = Get-ChildItem $OutDir -Recurse -File | Where-Object { $_.FullName -notmatch '\\\.git\\' }
$bad = @()
foreach ($file in $files) {
    $hits = Select-String -Path $file.FullName -Pattern 'Vertin','3046254770','ghp_','BEGIN RSA PRIVATE KEY','BEGIN OPENSSH PRIVATE KEY' -SimpleMatch -ErrorAction SilentlyContinue
    foreach ($hit in $hits) {
        if ($hit.Line -match 'Vertin-lichen') { continue }   # 仓库 URL / 版权行允许
        $bad += "$($file.FullName.Substring($OutDir.Length + 1)):$($hit.LineNumber) $($hit.Line.Trim())"
    }
}
$sqlite = $files | Where-Object { $_.Extension -in '.sqlite', '.sqlite-wal', '.sqlite-shm' }
if ($bad.Count -eq 0) { Write-Host '      无敏感字符串' } else { Write-Host '      警告：'; $bad | Select-Object -First 20 | ForEach-Object { Write-Host "        $_" } }
if ($sqlite) { Write-Host "      警告：包含数据库文件 $($sqlite.Count) 个" }

Write-Host '[6/6] 完成'
$size = ($files | Measure-Object Length -Sum).Sum
Write-Host ("      导出目录：{0}" -f $OutDir)
Write-Host ("      文件 {0} 个 / {1:N1} MB" -f $files.Count, ($size / 1MB))
Write-Host '      下一步（在导出目录执行）：'
Write-Host '        gh repo create Vertin-lichen/diskintel --public --source=. --remote=origin --push'