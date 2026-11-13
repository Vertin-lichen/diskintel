# ============================================================================
#  DiskIntel 安装包自动化验收脚本（安装 / 卸载 / 删除快照分支）
#
#  用法：
#    pwsh -File tools\installer\Test-Installer.ps1
#    pwsh -File tools\installer\Test-Installer.ps1 -SetupPath dist\DiskIntel-0.3.1-Setup.exe
#    pwsh -File tools\installer\Test-Installer.ps1 -TestRoot D:\_installtest
#
#  安全约束：
#   - 必须以普通权限运行（管理员下卸载器会真的清计划任务；脚本会拒绝）
#   - 只使用 -TestRoot 下的临时目录，不碰 D:\disk-intel-data
#   - 测试前备份 %APPDATA%\DiskIntel，结束后恢复
#   - 测试目录每次新建 run-<时间戳>，不覆盖历史证据
# ============================================================================

[CmdletBinding()]
param(
    [string]$SetupPath = '',
    [string]$TestRoot = 'D:\_installtest'
)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

$root = Split-Path -Parent (Split-Path -Parent $PSScriptRoot)
$script:failures = 0
$script:results = New-Object System.Collections.ArrayList

function Add-Result([string]$Area, [string]$Name, [bool]$Ok, [string]$Detail) {
    $status = 'FAIL'
    if ($Ok) { $status = 'PASS' }
    $extra = ''
    if ($Detail -ne '') { $extra = '：' + $Detail }
    [void]$script:results.Add([pscustomobject]@{ Area = $Area; Status = $status; Name = $Name; Detail = $Detail })
    if (-not $Ok) { $script:failures = $script:failures + 1 }
    Write-Host ("[{0}] {1} / {2}{3}" -f $status, $Area, $Name, $extra)
}

function Wait-PathGone([string]$Path, [int]$TimeoutSeconds) {
    $deadline = (Get-Date).AddSeconds($TimeoutSeconds)
    while ((Get-Date) -lt $deadline) {
        if (-not (Test-Path -LiteralPath $Path)) { return $true }
        Start-Sleep -Milliseconds 400
    }
    return (-not (Test-Path -LiteralPath $Path))
}

function Get-ShortcutInfo([string]$Path) {
    $ws = New-Object -ComObject WScript.Shell
    $sc = $ws.CreateShortcut($Path)
    return [pscustomobject]@{ TargetPath = $sc.TargetPath; Arguments = $sc.Arguments; WorkingDirectory = $sc.WorkingDirectory }
}

function Get-UninstallKey() {
    $base = 'HKCU:\Software\Microsoft\Windows\CurrentVersion\Uninstall'
    if (-not (Test-Path $base)) { return $null }
    foreach ($k in (Get-ChildItem $base -ErrorAction SilentlyContinue)) {
        $p = Get-ItemProperty -LiteralPath $k.PSPath -ErrorAction SilentlyContinue
        if ($p -and $p.DisplayName -and $p.DisplayName -like '*磁盘情报系统*') { return $p }
    }
    return $null
}

# ---------------------------------------------------------------------------
# 0. 安全前置检查
# ---------------------------------------------------------------------------
$identity = [Security.Principal.WindowsIdentity]::GetCurrent()
$principal = New-Object Security.Principal.WindowsPrincipal($identity)
if ($principal.IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)) {
    throw '请用普通权限（非管理员）终端运行本测试：管理员模式下卸载器会真的清理系统计划任务。'
}
$running = Get-Process -Name 'DiskIntel', 'DiskIntelCLI' -ErrorAction SilentlyContinue
if ($running) {
    throw ('检测到正在运行的 DiskIntel 进程（PID: ' + (($running | ForEach-Object { $_.Id }) -join ', ') + '）。请先关闭，避免卸载测试误杀正在使用的程序。')
}
if ($TestRoot -like '*disk-intel-data*') {
    throw '测试根目录不能指向真实数据根 D:\disk-intel-data。'
}

if ([string]::IsNullOrWhiteSpace($SetupPath)) {
    $candidates = @(Get-ChildItem (Join-Path $root 'dist') -Filter 'DiskIntel-*-Setup.exe' -ErrorAction SilentlyContinue | Sort-Object LastWriteTime -Descending)
    if ($candidates.Count -eq 0) {
        throw ('未找到安装包：' + (Join-Path $root 'dist\DiskIntel-*-Setup.exe') + '。请先运行 tools\build-installer.ps1。')
    }
    $SetupPath = $candidates[0].FullName
}
if (-not (Test-Path -LiteralPath $SetupPath -PathType Leaf)) {
    throw ('安装包不存在：' + $SetupPath)
}
if (-not [System.IO.Path]::IsPathRooted($SetupPath)) {
    $SetupPath = Join-Path $root $SetupPath
}
$SetupPath = [System.IO.Path]::GetFullPath($SetupPath)
$setupVersion = 'unknown'
$m = [regex]::Match((Split-Path -Leaf $SetupPath), 'DiskIntel-(\d+(?:\.\d+){1,3})-Setup\.exe')
if ($m.Success) { $setupVersion = $m.Groups[1].Value }

$stamp = Get-Date -Format 'yyyyMMdd-HHmmss'
$runDir = Join-Path $TestRoot ('run-' + $stamp)
New-Item -ItemType Directory -Force -Path $runDir | Out-Null
$installDir = Join-Path $runDir 'DiskIntel'
$dataRoot = Join-Path $runDir 'fakedata'
$snapDir = Join-Path $dataRoot 'snapshots'
New-Item -ItemType Directory -Force -Path $snapDir | Out-Null
$logFile = Join-Path $runDir 'test-result.log'

Start-Transcript -LiteralPath $logFile -Force | Out-Null

Write-Host '================================================================'
Write-Host 'DiskIntel 安装包验收测试'
Write-Host ('安装包：' + $SetupPath)
Write-Host ('版本：' + $setupVersion)
Write-Host ('测试目录：' + $runDir)
Write-Host '================================================================'

# 备份用户现有的 %APPDATA%\DiskIntel（卸载器会删除它）
$appdataDir = Join-Path $env:APPDATA 'DiskIntel'
$appdataBackup = Join-Path $runDir 'appdata-backup'
$hadAppdata = Test-Path -LiteralPath $appdataDir
if ($hadAppdata) {
    Copy-Item -LiteralPath $appdataDir -Destination $appdataBackup -Recurse -Force
    Write-Host ('[info] 已备份 %APPDATA%\DiskIntel -> ' + $appdataBackup)
}
else {
    Write-Host '[info] %APPDATA%\DiskIntel 不存在，无需备份'
}

function Restore-AppData {
    if ($hadAppdata) {
        if (Test-Path -LiteralPath $appdataDir) { Remove-Item -LiteralPath $appdataDir -Recurse -Force }
        Copy-Item -LiteralPath $appdataBackup -Destination $appdataDir -Recurse -Force
        Write-Host '[info] 已恢复 %APPDATA%\DiskIntel'
    }
    elseif (Test-Path -LiteralPath $appdataDir) {
        Remove-Item -LiteralPath $appdataDir -Recurse -Force
        Write-Host '[info] 已清理测试写入的 %APPDATA%\DiskIntel'
    }
}

function New-FakeSnapshots {
    Set-Content -LiteralPath (Join-Path $snapDir 'a.sqlite') -Value 'fake snapshot' -Encoding ascii
    Set-Content -LiteralPath (Join-Path $snapDir 'index.json') -Value '{}' -Encoding ascii
    Set-Content -LiteralPath (Join-Path $snapDir '.create.lock') -Value '' -Encoding ascii
    Set-Content -LiteralPath (Join-Path $snapDir 'notes.json') -Value '{}' -Encoding ascii
    Set-Content -LiteralPath (Join-Path $snapDir 'keep.txt') -Value 'must survive' -Encoding ascii
    Set-Content -LiteralPath (Join-Path $dataRoot 'keep-root.txt') -Value 'must survive' -Encoding ascii
}

$unins = Join-Path $installDir 'unins000.exe'

function Invoke-Setup([string[]]$Arguments) {
    $p = Start-Process -FilePath $SetupPath -ArgumentList $Arguments -Wait -PassThru
    return $p.ExitCode
}

try {
    # -----------------------------------------------------------------------
    # 1. 安装 + 校验
    # -----------------------------------------------------------------------
    Write-Host ''
    Write-Host '--- [1/3] 静默安装（带桌面图标任务）---'
    New-FakeSnapshots
    $installLog = Join-Path $runDir 'setup-install.log'
    $installArgs = @(
        '/VERYSILENT', '/SUPPRESSMSGBOXES', '/NORESTART',
        ('/DIR=' + $installDir),
        ('/DATAROOT=' + $dataRoot),
        '/TASKS=desktopicon',
        ('/LOG=' + $installLog)
    )
    $exit = Invoke-Setup $installArgs
    Add-Result '安装' 'Setup 退出码' ($exit -eq 0) ('exit=' + $exit)

    foreach ($rel in @('DiskIntel.exe', 'DiskIntelCLI.exe', 'README.txt', 'goz\goz.exe', 'goz\gozd.exe', 'tools\install-engine-bridge.ps1', 'tools\uninstall.ps1', 'tools\uninstall-elevated.cmd')) {
        $full = Join-Path $installDir $rel
        Add-Result '安装' ('文件 ' + $rel) (Test-Path -LiteralPath $full -PathType Leaf) $full
    }
    $unins = Join-Path $installDir 'unins000.exe'
    Add-Result '安装' '卸载程序 unins000.exe 存在' (Test-Path -LiteralPath $unins -PathType Leaf) $unins

    $groupDir = Join-Path $env:APPDATA 'Microsoft\Windows\Start Menu\Programs\磁盘情报系统'
    Add-Result '安装' '开始菜单组存在' (Test-Path -LiteralPath $groupDir -PathType Container) $groupDir
    $smProgram = Join-Path $groupDir '磁盘情报系统.lnk'
    $smOpen = Join-Path $groupDir '打开数据目录.lnk'
    $smUnins = Join-Path $groupDir '卸载 磁盘情报系统.lnk'
    Add-Result '安装' '开始菜单-程序快捷方式' (Test-Path -LiteralPath $smProgram -PathType Leaf) $smProgram
    Add-Result '安装' '开始菜单-打开数据目录' (Test-Path -LiteralPath $smOpen -PathType Leaf) $smOpen
    Add-Result '安装' '开始菜单-卸载快捷方式' (Test-Path -LiteralPath $smUnins -PathType Leaf) $smUnins
    $desktopLnk = Join-Path ([Environment]::GetFolderPath('Desktop')) '磁盘情报系统.lnk'
    Add-Result '安装' '桌面快捷方式（/TASKS=desktopicon）' (Test-Path -LiteralPath $desktopLnk -PathType Leaf) $desktopLnk

    if (Test-Path -LiteralPath $smProgram -PathType Leaf) {
        $sc = Get-ShortcutInfo $smProgram
        $argsOk = ($sc.TargetPath -eq (Join-Path $installDir 'DiskIntel.exe')) -and ($sc.Arguments -like ('*--data-root*' + $dataRoot + '*'))
        Add-Result '安装' '程序快捷方式带 --data-root 参数' $argsOk ('target=' + $sc.TargetPath + ' args=' + $sc.Arguments)
    }
    if (Test-Path -LiteralPath $smOpen -PathType Leaf) {
        $scOpen = Get-ShortcutInfo $smOpen
        Add-Result '安装' '打开数据目录快捷方式指向所选目录' (($scOpen.TargetPath -like '*explorer.exe') -and ($scOpen.Arguments -like ('*' + $dataRoot + '*'))) ('args=' + $scOpen.Arguments)
    }

    $reg = Get-ItemProperty -LiteralPath 'HKCU:\Software\DiskIntel' -ErrorAction SilentlyContinue
    $regRoot = ''
    if ($reg -and ($reg.PSObject.Properties.Name -contains 'DataRoot')) { $regRoot = $reg.DataRoot }
    Add-Result '安装' 'HKCU\Software\DiskIntel\DataRoot' ($regRoot -eq $dataRoot) ('value=' + $regRoot)

    $uk = Get-UninstallKey
    Add-Result '安装' '卸载登记项存在' ($uk -ne $null) $(if ($uk) { $uk.PSPath } else { '' })
    if ($uk) {
        Add-Result '安装' '卸载登记 DisplayName' ($uk.DisplayName -like '*磁盘情报系统*') $uk.DisplayName
        Add-Result '安装' '卸载登记 DisplayVersion' ($uk.DisplayVersion -eq $setupVersion) ($uk.DisplayVersion + ' / 期望 ' + $setupVersion)
        Add-Result '安装' '卸载登记 UninstallString' ($uk.UninstallString -like '*unins000.exe*') $uk.UninstallString
    }

    Add-Result '安装' '安装时未删除快照' (Test-Path -LiteralPath (Join-Path $snapDir 'a.sqlite') -PathType Leaf) 'a.sqlite 保留'

    # -----------------------------------------------------------------------
    # 2. 默认卸载（不勾选删除快照）
    # -----------------------------------------------------------------------
    Write-Host ''
    Write-Host '--- [2/3] 静默卸载（默认：保留快照）---'
    $uninstallLog1 = Join-Path $runDir 'uninstall-default.log'
    $p = Start-Process -FilePath $unins -ArgumentList @('/VERYSILENT', '/SUPPRESSMSGBOXES', '/NORESTART', ('/LOG=' + $uninstallLog1)) -Wait -PassThru
    Add-Result '卸载' '卸载器退出码' ($p.ExitCode -eq 0) ('exit=' + $p.ExitCode)
    Add-Result '卸载' '程序目录已删除' (Wait-PathGone $installDir 20) $installDir
    Add-Result '卸载' '开始菜单组已删除' (-not (Test-Path -LiteralPath $groupDir)) $groupDir
    Add-Result '卸载' '桌面快捷方式已删除' (-not (Test-Path -LiteralPath $desktopLnk)) $desktopLnk
    Add-Result '卸载' '%APPDATA%\DiskIntel 已删除' (-not (Test-Path -LiteralPath $appdataDir)) $appdataDir
    $regAfter = $null
    if (Test-Path -LiteralPath 'HKCU:\Software\DiskIntel') { $regAfter = Get-ItemProperty -LiteralPath 'HKCU:\Software\DiskIntel' -ErrorAction SilentlyContinue }
    Add-Result '卸载' 'HKCU DataRoot 已清理' ($regAfter -eq $null) 'Software\DiskIntel'
    Add-Result '卸载' '卸载登记项已删除' ((Get-UninstallKey) -eq $null) ''
    Add-Result '卸载' '默认保留 a.sqlite' (Test-Path -LiteralPath (Join-Path $snapDir 'a.sqlite') -PathType Leaf) 'a.sqlite 仍在'
    Add-Result '卸载' '默认保留 index.json' (Test-Path -LiteralPath (Join-Path $snapDir 'index.json') -PathType Leaf) 'index.json 仍在'
    Add-Result '卸载' '默认保留 .create.lock' (Test-Path -LiteralPath (Join-Path $snapDir '.create.lock') -PathType Leaf) '.create.lock 仍在'
    Add-Result '卸载' '默认保留 notes.json' (Test-Path -LiteralPath (Join-Path $snapDir 'notes.json') -PathType Leaf) 'notes.json 仍在'
    Add-Result '卸载' '默认保留快照目录控制文件' (Test-Path -LiteralPath (Join-Path $snapDir 'keep.txt') -PathType Leaf) 'keep.txt 仍在'

    # -----------------------------------------------------------------------
    # 3. 勾选删除快照（脚本化等价 /DELSNAPSHOTS=1）
    # -----------------------------------------------------------------------
    Write-Host ''
    Write-Host '--- [3/3] 重新安装后静默卸载（/DELSNAPSHOTS=1，删除快照）---'
    $installLog2 = Join-Path $runDir 'setup-install2.log'
    $exit2 = Invoke-Setup @('/VERYSILENT', '/SUPPRESSMSGBOXES', '/NORESTART', ('/DIR=' + $installDir), ('/DATAROOT=' + $dataRoot), ('/LOG=' + $installLog2))
    Add-Result '重装' 'Setup 退出码' ($exit2 -eq 0) ('exit=' + $exit2)
    Add-Result '重装' '默认无桌面快捷方式' (-not (Test-Path -LiteralPath $desktopLnk)) '未指定 /TASKS'
    Add-Result '重装' '卸载程序存在' (Test-Path -LiteralPath $unins -PathType Leaf) $unins

    $uninstallLog2 = Join-Path $runDir 'uninstall-delete-snapshots.log'
    $p2 = Start-Process -FilePath $unins -ArgumentList @('/VERYSILENT', '/SUPPRESSMSGBOXES', '/NORESTART', '/DELSNAPSHOTS=1', ('/LOG=' + $uninstallLog2)) -Wait -PassThru
    Add-Result '删除快照' '卸载器退出码' ($p2.ExitCode -eq 0) ('exit=' + $p2.ExitCode)
    Add-Result '删除快照' '程序目录已删除' (Wait-PathGone $installDir 20) $installDir
    Add-Result '删除快照' 'a.sqlite 已删除' (-not (Test-Path -LiteralPath (Join-Path $snapDir 'a.sqlite') -PathType Leaf)) 'a.sqlite'
    Add-Result '删除快照' 'index.json 已删除' (-not (Test-Path -LiteralPath (Join-Path $snapDir 'index.json') -PathType Leaf)) 'index.json'
    Add-Result '删除快照' '.create.lock 已删除' (-not (Test-Path -LiteralPath (Join-Path $snapDir '.create.lock') -PathType Leaf)) '.create.lock'
    Add-Result '删除快照' 'notes.json 已删除' (-not (Test-Path -LiteralPath (Join-Path $snapDir 'notes.json') -PathType Leaf)) 'notes.json'
    Add-Result '删除快照' '非快照文件 keep.txt 保留' (Test-Path -LiteralPath (Join-Path $snapDir 'keep.txt') -PathType Leaf) 'keep.txt'
    Add-Result '删除快照' '数据根控制文件保留' (Test-Path -LiteralPath (Join-Path $dataRoot 'keep-root.txt') -PathType Leaf) 'keep-root.txt'
    Add-Result '删除快照' '快照目录本身保留' (Test-Path -LiteralPath $snapDir -PathType Container) $snapDir

    $logContent = ''
    if (Test-Path -LiteralPath $uninstallLog2) { $logContent = Get-Content -LiteralPath $uninstallLog2 -Raw }
    Add-Result '卸载脚本' '调用了 tools\uninstall.ps1 并记录退出码' ($logContent -like '*引擎桥卸载脚本退出码*') '见 uninstall-delete-snapshots.log'
    Add-Result '卸载脚本' 'taskkill 结束进程逻辑执行过' ($logContent -like '*taskkill DiskIntel.exe*') '见日志'
}
finally {
    # 清理：如果测试中途失败留下安装，尽量先静默卸载（不删快照）
    if (Test-Path -LiteralPath $unins -PathType Leaf) {
        try {
            Start-Process -FilePath $unins -ArgumentList @('/VERYSILENT', '/SUPPRESSMSGBOXES', '/NORESTART') -Wait | Out-Null
        }
        catch {}
    }
    Restore-AppData
    Stop-Transcript | Out-Null
}

# ---------------------------------------------------------------------------
# 汇总
# ---------------------------------------------------------------------------
$total = $script:results.Count
$passed = $total - $script:failures
Write-Host ''
Write-Host '================================================================'
Write-Host ('验收结果：' + $passed + ' / ' + $total + ' 通过，失败 ' + $script:failures)
Write-Host ('证据目录：' + $runDir)
Write-Host ('完整日志：' + $logFile)
Write-Host '================================================================'
if ($script:failures -gt 0) {
    Write-Host '失败项：'
    $script:results | Where-Object { $_.Status -eq 'FAIL' } | ForEach-Object { Write-Host ('  - ' + $_.Area + ' / ' + $_.Name + '：' + $_.Detail) }
    exit 1
}
exit 0
