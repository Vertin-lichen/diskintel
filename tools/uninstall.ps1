<#
磁盘情报系统 · 卸载（仅引擎桥部分需要管理员）
背景：tools\install-engine-bridge.ps1 当初注册了两个计划任务：
  "DiskIntel gozd"       登录时静默拉起提权的 gozd，可被普通权限进程按需触发
  "DiskIntel gozd stop"  按需杀掉 gozd
这两条是磁盘上唯一的“系统级安装痕迹”（无服务、无注册表、无自启动项）。
删除它们需要一次管理员权限。

用法（二选一）：
  1. 双击本目录下的 uninstall-elevated.cmd（会弹一次 UAC）
  2. 管理员 PowerShell 里：pwsh -File tools\uninstall.ps1
只看不删（不需要管理员）：
  pwsh -File tools\uninstall.ps1 -DryRun
#>
[CmdletBinding()]
param(
    [switch] $DryRun,
    [switch] $KeepGozd   # 保留 gozd 进程，只删任务
)

$ErrorActionPreference = 'Continue'
$startTask = 'DiskIntel gozd'
$stopTask = 'DiskIntel gozd stop'

function Get-AdminState {
    $id = [Security.Principal.WindowsIdentity]::GetCurrent()
    $p = New-Object Security.Principal.WindowsPrincipal($id)
    return $p.IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)
}

$admin = Get-AdminState
Write-Host ("[环境] 管理员权限：{0}    DryRun：{1}" -f $admin, [bool]$DryRun)

$tasks = Get-ScheduledTask -ErrorAction SilentlyContinue |
    Where-Object { $_.TaskName -in @($startTask, $stopTask) }
if (-not $tasks) {
    Write-Host '[1/4] 未发现 DiskIntel 相关计划任务（已干净）'
} else {
    Write-Host '[1/4] 发现计划任务：'
    $tasks | Select-Object TaskName, State, @{n='RunLevel'; e={ $_.Principal.RunLevel }} |
        Format-Table -AutoSize | Out-Host
}

if ($DryRun) {
    Write-Host '[DryRun] 将执行：停止 gozd（除非 -KeepGozd）、注销上述任务'
    Write-Host '[DryRun] 未做任何修改'
    return
}

if (-not $admin) {
    Write-Warning '需要管理员权限：请双击 uninstall-elevated.cmd，或以管理员身份运行本脚本。'
    exit 1
}

Write-Host '[2/4] 停止 gozd'
if ($KeepGozd) {
    Write-Host '      -KeepGozd 指定，跳过'
} else {
    $bridged = $false
    if (Get-ScheduledTask -TaskName $stopTask -ErrorAction SilentlyContinue) {
        schtasks /Run /TN $stopTask | Out-Null
        $bridged = $true
        Start-Sleep -Seconds 2
    }
    Get-Process gozd -ErrorAction SilentlyContinue | ForEach-Object {
        try { Stop-Process -Id $_.Id -Force -ErrorAction Stop } catch { Write-Warning "停止 gozd(PID $($_.Id)) 失败：$_" }
    }
    $left = Get-Process gozd -ErrorAction SilentlyContinue
    if ($left) { Write-Warning 'gozd 仍在运行，请手动结束' } else { Write-Host ('      gozd 已停止（桥触发={0}）' -f $bridged) }
}

Write-Host '[3/4] 注销计划任务'
foreach ($name in @($stopTask, $startTask)) {
    $task = Get-ScheduledTask -TaskName $name -ErrorAction SilentlyContinue
    if (-not $task) { Write-Host "      $name ：不存在，跳过"; continue }
    try {
        Unregister-ScheduledTask -TaskName $name -Confirm:$false -ErrorAction Stop
        Write-Host "      $name ：已删除"
    } catch {
        Write-Warning "      $name ：删除失败（$_）"
    }
}

Write-Host '[4/4] 验证'
$rest = Get-ScheduledTask -ErrorAction SilentlyContinue | Where-Object { $_.TaskName -in @($startTask, $stopTask) }
if ($rest) {
    Write-Warning '仍有任务残留，请重试'
    $rest | Select-Object TaskName, State | Format-Table -AutoSize | Out-Host
    exit 1
}
Write-Host '      计划任务：干净'
$gozd = Get-Process gozd -ErrorAction SilentlyContinue
Write-Host ('      gozd 进程：{0}' -f $(if ($gozd) { '仍在运行' } else { '无' }))
Write-Host ''
Write-Host '引擎桥卸载完成。程序本体是可执行文件 + 数据目录，随文件夹删除即可。'
