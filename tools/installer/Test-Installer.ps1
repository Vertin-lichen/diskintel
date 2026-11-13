# ============================================================================
#  DiskIntel 安装包自动化验收脚本（安装 / 卸载 / 双目录快照清理）
#
#  用法：
#    pwsh -File tools\installer\Test-Installer.ps1
#    pwsh -File tools\installer\Test-Installer.ps1 -SetupPath dist\DiskIntel-0.3.2-Setup.exe
#    pwsh -File tools\installer\Test-Installer.ps1 -TestRoot D:\_installtest
#
#  安全约束：
#   - 必须以普通权限运行（管理员下卸载器会真的清计划任务；脚本会拒绝）
#   - 只使用 -TestRoot 下的临时目录，不碰 D:\disk-intel-data
#   - 若本机已安装磁盘情报系统：备份卸载登记/开始菜单/桌面快捷方式/注册表值/%APPDATA%，
#     测试结束后原样恢复；真实程序目录不会被卸载
#   - 测试前备份 %APPDATA%\DiskIntel，结束后恢复
#   - 测试目录每次新建 run-<时间戳>，不覆盖历史证据
# ============================================================================

[CmdletBinding()]
param(
    [string]$SetupPath = '',
    [string]$TestRoot = 'D:\_installtest',
    [string]$CrossDriveRoot = 'E:\_installtest'
)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

$root = Split-Path -Parent (Split-Path -Parent $PSScriptRoot)
$script:failures = 0
$script:results = New-Object System.Collections.ArrayList
$script:testAttempted = $false

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
    if (-not (Test-Path -LiteralPath $base)) { return $null }
    foreach ($k in (Get-ChildItem -LiteralPath $base -ErrorAction SilentlyContinue)) {
        $p = Get-ItemProperty -LiteralPath $k.PSPath -ErrorAction SilentlyContinue
        if ($p -and $p.DisplayName -and $p.DisplayName -like '*磁盘情报系统*') { return $p }
    }
    return $null
}

function Get-RegValue([string]$Name) {
    $key = 'HKCU:\Software\DiskIntel'
    if (-not (Test-Path -LiteralPath $key)) { return $null }
    try {
        $item = Get-ItemProperty -LiteralPath $key -Name $Name -ErrorAction Stop
        return $item.$Name
    }
    catch {
        return $null
    }
}

function Set-RegValue([string]$Name, [string]$Value) {
    $key = 'HKCU:\Software\DiskIntel'
    if (-not (Test-Path -LiteralPath $key)) {
        New-Item -Path $key -Force | Out-Null
    }
    Set-ItemProperty -LiteralPath $key -Name $Name -Value $Value
}

function Remove-RegValue([string]$Name) {
    $key = 'HKCU:\Software\DiskIntel'
    if (-not (Test-Path -LiteralPath $key)) { return }
    Remove-ItemProperty -LiteralPath $key -Name $Name -Force -ErrorAction SilentlyContinue
}

function Backup-UninstallKeys([string]$BackupDir) {
    $files = New-Object System.Collections.ArrayList
    $base = 'HKCU:\Software\Microsoft\Windows\CurrentVersion\Uninstall'
    if (-not (Test-Path -LiteralPath $base)) { return @() }
    $index = 0
    foreach ($k in (Get-ChildItem -LiteralPath $base -ErrorAction SilentlyContinue)) {
        $p = Get-ItemProperty -LiteralPath $k.PSPath -ErrorAction SilentlyContinue
        if ($p -and $p.DisplayName -and $p.DisplayName -like '*磁盘情报系统*') {
            $index = $index + 1
            $file = Join-Path $BackupDir ('uninstall-original-' + $index + '.reg')
            & reg.exe export $k.Name $file /y | Out-Null
            if (Test-Path -LiteralPath $file) {
                [void]$files.Add($file)
            }
        }
    }
    return @($files)
}

function Restore-UninstallKeys([string[]]$BackupFiles, [bool]$Attempted) {
    $base = 'HKCU:\Software\Microsoft\Windows\CurrentVersion\Uninstall'
    if (Test-Path -LiteralPath $base) {
        foreach ($k in (Get-ChildItem -LiteralPath $base -ErrorAction SilentlyContinue)) {
            $p = Get-ItemProperty -LiteralPath $k.PSPath -ErrorAction SilentlyContinue
            if ($p -and $p.DisplayName -and $p.DisplayName -like '*磁盘情报系统*') {
                Remove-Item -LiteralPath $k.PSPath -Recurse -Force -ErrorAction SilentlyContinue
            }
        }
    }
    if (@($BackupFiles).Count -gt 0) {
        foreach ($file in $BackupFiles) {
            if (Test-Path -LiteralPath $file) {
                & reg.exe import $file | Out-Null
            }
        }
        Write-Host '[info] 已恢复原有卸载登记'
    }
    elseif ($Attempted) {
        Write-Host '[info] 原本无卸载登记；已清理测试项'
    }
}

function Restore-StartMenu([string]$BackupDir, [string]$GroupDir, [bool]$Attempted) {
    if (Test-Path -LiteralPath $BackupDir) {
        if (Test-Path -LiteralPath $GroupDir) {
            Remove-Item -LiteralPath $GroupDir -Recurse -Force
        }
        Copy-Item -LiteralPath $BackupDir -Destination $GroupDir -Recurse -Force
        Write-Host '[info] 已恢复开始菜单快捷方式'
    }
    elseif ($Attempted -and (Test-Path -LiteralPath $GroupDir)) {
        Remove-Item -LiteralPath $GroupDir -Recurse -Force
    }
}

function Restore-Desktop([string]$BackupFile, [string]$LinkPath, [bool]$Attempted) {
    if (Test-Path -LiteralPath $BackupFile) {
        if (Test-Path -LiteralPath $LinkPath) {
            Remove-Item -LiteralPath $LinkPath -Force
        }
        Copy-Item -LiteralPath $BackupFile -Destination $LinkPath -Force
        Write-Host '[info] 已恢复桌面快捷方式'
    }
    elseif ($Attempted -and (Test-Path -LiteralPath $LinkPath)) {
        Remove-Item -LiteralPath $LinkPath -Force
    }
}

function New-FakeSnapshotSet([string]$Dir, [string]$Tag) {
    New-Item -ItemType Directory -Force -Path $Dir | Out-Null
    Set-Content -LiteralPath (Join-Path $Dir 'a.sqlite') -Value ('fake snapshot ' + $Tag) -Encoding ascii
    Set-Content -LiteralPath (Join-Path $Dir 'index.json') -Value '{}' -Encoding ascii
    Set-Content -LiteralPath (Join-Path $Dir '.create.lock') -Value '' -Encoding ascii
    Set-Content -LiteralPath (Join-Path $Dir 'notes.json') -Value '{}' -Encoding ascii
    Set-Content -LiteralPath (Join-Path $Dir 'keep.txt') -Value 'must survive' -Encoding ascii
}

# ---------------------------------------------------------------------------
# 0. 安全前置检查与路径
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
if (-not [System.IO.Path]::IsPathRooted($SetupPath)) {
    $SetupPath = Join-Path $root $SetupPath
}
if (-not (Test-Path -LiteralPath $SetupPath -PathType Leaf)) {
    throw ('安装包不存在：' + $SetupPath)
}
$SetupPath = [System.IO.Path]::GetFullPath($SetupPath)
$setupVersion = 'unknown'
$m = [regex]::Match((Split-Path -Leaf $SetupPath), 'DiskIntel-(\d+(?:\.\d+){1,3})-Setup\.exe')
if ($m.Success) { $setupVersion = $m.Groups[1].Value }

$stamp = Get-Date -Format 'yyyyMMdd-HHmmss'
$runDir = Join-Path $TestRoot ('run-' + $stamp)
$backupDir = Join-Path $runDir 'backup'
New-Item -ItemType Directory -Force -Path $runDir, $backupDir | Out-Null
$installDir = Join-Path $runDir 'DiskIntel'
$dataRoot = Join-Path $runDir 'fakedata'
$snapDir = Join-Path $dataRoot 'snapshots'
New-Item -ItemType Directory -Force -Path $snapDir | Out-Null
$logFile = Join-Path $runDir 'test-result.log'

$snapA = Join-Path $TestRoot 'snapA'
$snapASnapshots = Join-Path $snapA 'snapshots'
$snapB = Join-Path $CrossDriveRoot 'snapB'
$crossRoot = [System.IO.Path]::GetPathRoot($CrossDriveRoot)
if ([string]::IsNullOrWhiteSpace($crossRoot) -or (-not (Test-Path -LiteralPath $crossRoot))) {
    $snapB = Join-Path $TestRoot 'snapB'
    Write-Host ('[info] 跨盘目录不可用，snapB 改到：' + $snapB)
}
New-Item -ItemType Directory -Force -Path $snapASnapshots, $snapB | Out-Null

$unins = Join-Path $installDir 'unins000.exe'
$regKey = 'HKCU:\Software\DiskIntel'
$groupDir = Join-Path $env:APPDATA 'Microsoft\Windows\Start Menu\Programs\磁盘情报系统'
$desktopLnk = Join-Path ([Environment]::GetFolderPath('Desktop')) '磁盘情报系统.lnk'
$appdataDir = Join-Path $env:APPDATA 'DiskIntel'

# 备份本机原有状态（真实安装 + 注册表 + %APPDATA%）
$origDataRoot = Get-RegValue 'DataRoot'
$origSnapshotDir = Get-RegValue 'SnapshotDir'
$existingGroupBackup = Join-Path $backupDir 'start-menu-group'
$existingDesktopBackup = Join-Path $backupDir 'desktop-shortcut.lnk'
if (Test-Path -LiteralPath $groupDir) {
    Copy-Item -LiteralPath $groupDir -Destination $existingGroupBackup -Recurse -Force
}
if (Test-Path -LiteralPath $desktopLnk) {
    Copy-Item -LiteralPath $desktopLnk -Destination $existingDesktopBackup -Force
}
$existingUninstallRegs = @(Backup-UninstallKeys -BackupDir $backupDir)
$hadExistingInstall = ((@($existingUninstallRegs).Count -gt 0) -or (Test-Path -LiteralPath $groupDir))
if ($hadExistingInstall -and (@($existingUninstallRegs).Count -eq 0)) {
    throw '检测到本机已安装磁盘情报系统，但未能备份卸载登记（reg export 失败）。为避免破坏真实安装，测试中止。'
}
$appdataBackup = Join-Path $backupDir 'appdata-DiskIntel'
$hadAppdata = Test-Path -LiteralPath $appdataDir
if ($hadAppdata) {
    Copy-Item -LiteralPath $appdataDir -Destination $appdataBackup -Recurse -Force
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

function Invoke-Setup([string[]]$Arguments) {
    $script:testAttempted = $true
    $p = Start-Process -FilePath $SetupPath -ArgumentList $Arguments -Wait -PassThru
    return $p.ExitCode
}

function Invoke-Uninstall([string[]]$Arguments) {
    $p = Start-Process -FilePath $unins -ArgumentList $Arguments -Wait -PassThru
    return $p.ExitCode
}

Start-Transcript -LiteralPath $logFile -Force | Out-Null

Write-Host '================================================================'
Write-Host 'DiskIntel 安装包验收测试'
Write-Host ('安装包：' + $SetupPath)
Write-Host ('版本：' + $setupVersion)
Write-Host ('测试目录：' + $runDir)
Write-Host ('双目录场景：' + $snapASnapshots + '  与  ' + $snapB)
if ($hadExistingInstall) {
    Write-Host '[info] 检测到本机已安装磁盘情报系统：测试期间会覆盖安装登记/快捷方式，结束后从备份恢复；真实程序目录不动'
}
Write-Host '================================================================'

try {
    # -----------------------------------------------------------------------
    # 1. 安装 + 校验（含桌面图标任务）
    # -----------------------------------------------------------------------
    Write-Host ''
    Write-Host '--- [1/6] 静默安装（带桌面图标任务）---'
    New-FakeSnapshotSet $snapDir 'dataprimary'
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
    Add-Result '安装' '卸载程序 unins000.exe 存在' (Test-Path -LiteralPath $unins -PathType Leaf) $unins

    Add-Result '安装' '开始菜单组存在' (Test-Path -LiteralPath $groupDir -PathType Container) $groupDir
    $smProgram = Join-Path $groupDir '磁盘情报系统.lnk'
    $smOpen = Join-Path $groupDir '打开数据目录.lnk'
    $smUnins = Join-Path $groupDir '卸载 磁盘情报系统.lnk'
    Add-Result '安装' '开始菜单-程序快捷方式' (Test-Path -LiteralPath $smProgram -PathType Leaf) $smProgram
    Add-Result '安装' '开始菜单-打开数据目录' (Test-Path -LiteralPath $smOpen -PathType Leaf) $smOpen
    Add-Result '安装' '开始菜单-卸载快捷方式' (Test-Path -LiteralPath $smUnins -PathType Leaf) $smUnins
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

    $regRoot = Get-RegValue 'DataRoot'
    Add-Result '安装' 'HKCU\Software\DiskIntel\DataRoot' ($regRoot -eq $dataRoot) ('value=' + $regRoot)
    Add-Result '安装' '安装未写 SnapshotDir（由程序写）' ((Get-RegValue 'SnapshotDir') -eq $null) ''

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
    Write-Host '--- [2/6] 静默卸载（默认：保留快照）---'
    $uninstallLog1 = Join-Path $runDir 'uninstall-default.log'
    $exit = Invoke-Uninstall @('/VERYSILENT', '/SUPPRESSMSGBOXES', '/NORESTART', ('/LOG=' + $uninstallLog1))
    Add-Result '卸载' '卸载器退出码' ($exit -eq 0) ('exit=' + $exit)
    Add-Result '卸载' '程序目录已删除' (Wait-PathGone $installDir 20) $installDir
    Add-Result '卸载' '开始菜单组已删除' (-not (Test-Path -LiteralPath $groupDir)) $groupDir
    Add-Result '卸载' '桌面快捷方式已删除' (-not (Test-Path -LiteralPath $desktopLnk)) $desktopLnk
    Add-Result '卸载' '%APPDATA%\DiskIntel 已删除' (-not (Test-Path -LiteralPath $appdataDir)) $appdataDir
    Add-Result '卸载' 'HKCU DataRoot 已清理' ((Get-RegValue 'DataRoot') -eq $null) 'Software\DiskIntel'
    Add-Result '卸载' 'HKCU SnapshotDir 已清理' ((Get-RegValue 'SnapshotDir') -eq $null) ''
    Add-Result '卸载' '卸载登记项已删除' ((Get-UninstallKey) -eq $null) ''
    Add-Result '卸载' '默认保留 a.sqlite' (Test-Path -LiteralPath (Join-Path $snapDir 'a.sqlite') -PathType Leaf) 'a.sqlite 仍在'
    Add-Result '卸载' '默认保留 index.json' (Test-Path -LiteralPath (Join-Path $snapDir 'index.json') -PathType Leaf) 'index.json 仍在'
    Add-Result '卸载' '默认保留 .create.lock' (Test-Path -LiteralPath (Join-Path $snapDir '.create.lock') -PathType Leaf) '.create.lock 仍在'
    Add-Result '卸载' '默认保留 notes.json' (Test-Path -LiteralPath (Join-Path $snapDir 'notes.json') -PathType Leaf) 'notes.json 仍在'
    Add-Result '卸载' '默认保留快照目录控制文件' (Test-Path -LiteralPath (Join-Path $snapDir 'keep.txt') -PathType Leaf) 'keep.txt 仍在'

    # -----------------------------------------------------------------------
    # 3. 勾选删除快照（/DELSNAPSHOTS=1，仅默认 DataRoot\snapshots）
    # -----------------------------------------------------------------------
    Write-Host ''
    Write-Host '--- [3/6] 重装后 /DELSNAPSHOTS=1 卸载（默认目录）---'
    $installLog2 = Join-Path $runDir 'setup-install2.log'
    $exit = Invoke-Setup @('/VERYSILENT', '/SUPPRESSMSGBOXES', '/NORESTART', ('/DIR=' + $installDir), ('/DATAROOT=' + $dataRoot), ('/LOG=' + $installLog2))
    Add-Result '重装' 'Setup 退出码' ($exit -eq 0) ('exit=' + $exit)
    Add-Result '重装' '默认无桌面快捷方式' (-not (Test-Path -LiteralPath $desktopLnk)) '未指定 /TASKS'
    Add-Result '重装' '卸载程序存在' (Test-Path -LiteralPath $unins -PathType Leaf) $unins

    $uninstallLog2 = Join-Path $runDir 'uninstall-delete-snapshots.log'
    $exit = Invoke-Uninstall @('/VERYSILENT', '/SUPPRESSMSGBOXES', '/NORESTART', '/DELSNAPSHOTS=1', ('/LOG=' + $uninstallLog2))
    Add-Result '删除快照' '卸载器退出码' ($exit -eq 0) ('exit=' + $exit)
    Add-Result '删除快照' '程序目录已删除' (Wait-PathGone $installDir 20) $installDir
    Add-Result '删除快照' 'a.sqlite 已删除' (-not (Test-Path -LiteralPath (Join-Path $snapDir 'a.sqlite') -PathType Leaf)) 'a.sqlite'
    Add-Result '删除快照' 'index.json 已删除' (-not (Test-Path -LiteralPath (Join-Path $snapDir 'index.json') -PathType Leaf)) 'index.json'
    Add-Result '删除快照' '.create.lock 已删除' (-not (Test-Path -LiteralPath (Join-Path $snapDir '.create.lock') -PathType Leaf)) '.create.lock'
    Add-Result '删除快照' 'notes.json 已删除' (-not (Test-Path -LiteralPath (Join-Path $snapDir 'notes.json') -PathType Leaf)) 'notes.json'
    Add-Result '删除快照' '非快照文件 keep.txt 保留' (Test-Path -LiteralPath (Join-Path $snapDir 'keep.txt') -PathType Leaf) 'keep.txt'
    Add-Result '删除快照' '快照目录本身保留' (Test-Path -LiteralPath $snapDir -PathType Container) $snapDir
    $logContent = ''
    if (Test-Path -LiteralPath $uninstallLog2) { $logContent = Get-Content -LiteralPath $uninstallLog2 -Raw }
    Add-Result '卸载脚本' '调用了 tools\uninstall.ps1 并记录退出码' ($logContent -like '*引擎桥卸载脚本退出码*') '见 uninstall-delete-snapshots.log'
    Add-Result '卸载脚本' 'taskkill 结束进程逻辑执行过' ($logContent -like '*taskkill DiskIntel.exe*') '见日志'

    # -----------------------------------------------------------------------
    # 4. 双快照目录：默认卸载保留（DataRoot\snapshots + 注册表 SnapshotDir）
    # -----------------------------------------------------------------------
    Write-Host ''
    Write-Host '--- [4/6] 双目录（C 盘程序 + E 盘快照）：默认卸载保留 ---'
    New-FakeSnapshotSet $snapASnapshots 'snapA'
    New-FakeSnapshotSet $snapB 'snapB'
    $installLog3 = Join-Path $runDir 'setup-install3.log'
    $exit = Invoke-Setup @('/VERYSILENT', '/SUPPRESSMSGBOXES', '/NORESTART', ('/DIR=' + $installDir), ('/DATAROOT=' + $snapA), ('/LOG=' + $installLog3))
    Add-Result '双目录' '安装退出码' ($exit -eq 0) ('exit=' + $exit)
    Add-Result '双目录' '安装后 SnapshotDir 仍为空（安装包不写）' ((Get-RegValue 'SnapshotDir') -eq $null) ''
    Set-RegValue 'SnapshotDir' $snapB
    Add-Result '双目录' '测试写入 SnapshotDir 成功' ((Get-RegValue 'SnapshotDir') -eq $snapB) ('value=' + (Get-RegValue 'SnapshotDir'))

    $uninstallLog3 = Join-Path $runDir 'uninstall-dual-preserve.log'
    $exit = Invoke-Uninstall @('/VERYSILENT', '/SUPPRESSMSGBOXES', '/NORESTART', ('/LOG=' + $uninstallLog3))
    Add-Result '双目录' '默认卸载退出码' ($exit -eq 0) ('exit=' + $exit)
    Add-Result '双目录' '程序目录已删除' (Wait-PathGone $installDir 20) $installDir
    Add-Result '双目录' 'snapA a.sqlite 保留' (Test-Path -LiteralPath (Join-Path $snapASnapshots 'a.sqlite') -PathType Leaf) ''
    Add-Result '双目录' 'snapA index.json 保留' (Test-Path -LiteralPath (Join-Path $snapASnapshots 'index.json') -PathType Leaf) ''
    Add-Result '双目录' 'snapA keep.txt 保留' (Test-Path -LiteralPath (Join-Path $snapASnapshots 'keep.txt') -PathType Leaf) ''
    Add-Result '双目录' 'snapB a.sqlite 保留' (Test-Path -LiteralPath (Join-Path $snapB 'a.sqlite') -PathType Leaf) ''
    Add-Result '双目录' 'snapB index.json 保留' (Test-Path -LiteralPath (Join-Path $snapB 'index.json') -PathType Leaf) ''
    Add-Result '双目录' 'snapB keep.txt 保留' (Test-Path -LiteralPath (Join-Path $snapB 'keep.txt') -PathType Leaf) ''
    Add-Result '双目录' '卸载后 SnapshotDir 注册表已清理' ((Get-RegValue 'SnapshotDir') -eq $null) ''

    # -----------------------------------------------------------------------
    # 5. 双快照目录：/DELSNAPSHOTS=1 同时清理两个目录
    # -----------------------------------------------------------------------
    Write-Host ''
    Write-Host '--- [5/6] 双目录：/DELSNAPSHOTS=1 同时清理 ---'
    New-FakeSnapshotSet $snapASnapshots 'snapA2'
    New-FakeSnapshotSet $snapB 'snapB2'
    $installLog4 = Join-Path $runDir 'setup-install4.log'
    $exit = Invoke-Setup @('/VERYSILENT', '/SUPPRESSMSGBOXES', '/NORESTART', ('/DIR=' + $installDir), ('/DATAROOT=' + $snapA), ('/LOG=' + $installLog4))
    Add-Result '双目录删除' '安装退出码' ($exit -eq 0) ('exit=' + $exit)
    Set-RegValue 'SnapshotDir' ($snapB + '\')
    Add-Result '双目录删除' 'SnapshotDir 为带尾斜杠的自定义目录' ((Get-RegValue 'SnapshotDir') -eq ($snapB + '\')) ('value=' + (Get-RegValue 'SnapshotDir'))

    $uninstallLog4 = Join-Path $runDir 'uninstall-dual-delete.log'
    $exit = Invoke-Uninstall @('/VERYSILENT', '/SUPPRESSMSGBOXES', '/NORESTART', '/DELSNAPSHOTS=1', ('/LOG=' + $uninstallLog4))
    Add-Result '双目录删除' '卸载器退出码' ($exit -eq 0) ('exit=' + $exit)
    Add-Result '双目录删除' '程序目录已删除' (Wait-PathGone $installDir 20) $installDir
    Add-Result '双目录删除' 'snapA a.sqlite 已删除' (-not (Test-Path -LiteralPath (Join-Path $snapASnapshots 'a.sqlite') -PathType Leaf)) ''
    Add-Result '双目录删除' 'snapA index.json 已删除' (-not (Test-Path -LiteralPath (Join-Path $snapASnapshots 'index.json') -PathType Leaf)) ''
    Add-Result '双目录删除' 'snapA keep.txt 保留' (Test-Path -LiteralPath (Join-Path $snapASnapshots 'keep.txt') -PathType Leaf) ''
    Add-Result '双目录删除' 'snapA 目录本身保留' (Test-Path -LiteralPath $snapASnapshots -PathType Container) ''
    Add-Result '双目录删除' 'snapB a.sqlite 已删除' (-not (Test-Path -LiteralPath (Join-Path $snapB 'a.sqlite') -PathType Leaf)) ''
    Add-Result '双目录删除' 'snapB index.json 已删除' (-not (Test-Path -LiteralPath (Join-Path $snapB 'index.json') -PathType Leaf)) ''
    Add-Result '双目录删除' 'snapB keep.txt 保留' (Test-Path -LiteralPath (Join-Path $snapB 'keep.txt') -PathType Leaf) ''
    Add-Result '双目录删除' 'snapB 目录本身保留' (Test-Path -LiteralPath $snapB -PathType Container) ''
    Add-Result '双目录删除' '卸载后 SnapshotDir 注册表已清理' ((Get-RegValue 'SnapshotDir') -eq $null) ''
    $log4 = ''
    if (Test-Path -LiteralPath $uninstallLog4) { $log4 = Get-Content -LiteralPath $uninstallLog4 -Raw }
    Add-Result '双目录删除' '日志记录 2 个快照目标目录' ($log4 -like '*卸载快照目标目录数：2*') '见 uninstall-dual-delete.log'
    Add-Result '双目录删除' '日志包含 snapA 清理记录' ($log4 -like ('*' + $snapASnapshots + '*')) ''
    Add-Result '双目录删除' '日志包含 snapB 清理记录' ($log4 -like ('*' + $snapB + '*')) ''
    $cleanCount = 0
    if ($log4 -ne '') { $cleanCount = ([regex]::Matches($log4, '快照目录清理：')).Count }
    Add-Result '双目录删除' '两个目录各清理一次（无重复）' ($cleanCount -eq 2) ('清理记录数=' + $cleanCount)
    Add-Result '双目录删除' '删除文件总数 8' ($log4 -like '*快照删除文件总数：8*') '4 个/目录'

    # -----------------------------------------------------------------------
    # 6. 去重：SnapshotDir 与 DataRoot\snapshots 相同
    # -----------------------------------------------------------------------
    Write-Host ''
    Write-Host '--- [6/6] 去重：SnapshotDir == DataRoot\snapshots ---'
    New-FakeSnapshotSet $snapASnapshots 'snapA3'
    New-FakeSnapshotSet $snapB 'snapB3'
    $installLog5 = Join-Path $runDir 'setup-install5.log'
    $exit = Invoke-Setup @('/VERYSILENT', '/SUPPRESSMSGBOXES', '/NORESTART', ('/DIR=' + $installDir), ('/DATAROOT=' + $snapA), ('/LOG=' + $installLog5))
    Add-Result '去重' '安装退出码' ($exit -eq 0) ('exit=' + $exit)
    Set-RegValue 'SnapshotDir' $snapASnapshots
    Add-Result '去重' 'SnapshotDir 指向默认快照目录' ((Get-RegValue 'SnapshotDir') -eq $snapASnapshots) ''

    $uninstallLog5 = Join-Path $runDir 'uninstall-dedupe.log'
    $exit = Invoke-Uninstall @('/VERYSILENT', '/SUPPRESSMSGBOXES', '/NORESTART', '/DELSNAPSHOTS=1', ('/LOG=' + $uninstallLog5))
    Add-Result '去重' '卸载器退出码' ($exit -eq 0) ('exit=' + $exit)
    Add-Result '去重' '程序目录已删除' (Wait-PathGone $installDir 20) $installDir
    Add-Result '去重' 'snapA a.sqlite 已删除' (-not (Test-Path -LiteralPath (Join-Path $snapASnapshots 'a.sqlite') -PathType Leaf)) ''
    Add-Result '去重' 'snapA index.json 已删除' (-not (Test-Path -LiteralPath (Join-Path $snapASnapshots 'index.json') -PathType Leaf)) ''
    Add-Result '去重' 'snapA keep.txt 保留' (Test-Path -LiteralPath (Join-Path $snapASnapshots 'keep.txt') -PathType Leaf) ''
    Add-Result '去重' 'snapB 未被清理（不重复删除）' ((Test-Path -LiteralPath (Join-Path $snapB 'a.sqlite') -PathType Leaf) -and (Test-Path -LiteralPath (Join-Path $snapB 'index.json') -PathType Leaf)) ''
    Add-Result '去重' '卸载后 SnapshotDir 注册表已清理' ((Get-RegValue 'SnapshotDir') -eq $null) ''
    $log5 = ''
    if (Test-Path -LiteralPath $uninstallLog5) { $log5 = Get-Content -LiteralPath $uninstallLog5 -Raw }
    Add-Result '去重' '日志记录只清理 1 个目标目录' ($log5 -like '*卸载快照目标目录数：1*') '见 uninstall-dedupe.log'
    Add-Result '去重' '日志记录已去重' (($log5 -like '*已去重*') -or ($log5 -like '*相同*')) ''
    Add-Result '去重' '删除文件总数 4（只清一次）' ($log5 -like '*快照删除文件总数：4*') ''
}
finally {
    if (Test-Path -LiteralPath $unins -PathType Leaf) {
        try {
            Start-Process -FilePath $unins -ArgumentList @('/VERYSILENT', '/SUPPRESSMSGBOXES', '/NORESTART') -Wait | Out-Null
        }
        catch {}
    }

    if ($null -ne $origDataRoot) { Set-RegValue 'DataRoot' $origDataRoot } else { Remove-RegValue 'DataRoot' }
    if ($null -ne $origSnapshotDir) { Set-RegValue 'SnapshotDir' $origSnapshotDir } else { Remove-RegValue 'SnapshotDir' }
    if (($null -eq $origDataRoot) -and ($null -eq $origSnapshotDir) -and (Test-Path -LiteralPath $regKey)) {
        $props = @((Get-Item -LiteralPath $regKey).Property)
        if ($props.Count -eq 0) {
            Remove-Item -LiteralPath $regKey -Force
        }
    }

    Restore-UninstallKeys -BackupFiles $existingUninstallRegs -Attempted $script:testAttempted
    Restore-StartMenu -BackupDir $existingGroupBackup -GroupDir $groupDir -Attempted $script:testAttempted
    Restore-Desktop -BackupFile $existingDesktopBackup -LinkPath $desktopLnk -Attempted $script:testAttempted
    Restore-AppData
    Stop-Transcript | Out-Null
}

$total = $script:results.Count
$passed = $total - $script:failures
Write-Host ''
Write-Host '================================================================'
Write-Host ('验收结果：' + $passed + ' / ' + $total + ' 通过，失败 ' + $script:failures)
Write-Host ('证据目录：' + $runDir)
Write-Host ('完整日志：' + $logFile)
Write-Host ('双目录证据：' + $snapA + '  与  ' + $snapB + '（保留，未删除）')
Write-Host '================================================================'
if ($script:failures -gt 0) {
    Write-Host '失败项：'
    $script:results | Where-Object { $_.Status -eq 'FAIL' } | ForEach-Object { Write-Host ('  - ' + $_.Area + ' / ' + $_.Name + '：' + $_.Detail) }
    exit 1
}
exit 0
