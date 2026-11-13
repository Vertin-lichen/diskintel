# ============================================================================
#  DiskIntel 安装包构建脚本（Inno Setup 6）
#
#  用法（在仓库任意位置）：
#    pwsh -File tools\build-installer.ps1
#    pwsh -File tools\build-installer.ps1 -Version 0.3.2
#    pwsh -File tools\build-installer.ps1 -DistDir "D:\other\DiskIntel-0.3.2"
#
#  产物：
#    dist\DiskIntel-<Version>-Setup.exe
#    dist\DiskIntel-<Version>-Setup.exe.sha256
#
#  依赖：
#   - Inno Setup 6（ISCC.exe；默认按用户范围安装在 %LOCALAPPDATA%\Programs\Inno Setup 6）
#   - 已组装好的发行目录（默认 dist\DiskIntel-<Version>，由 tools\package.ps1 生成）
#   - assets\icon.png（构建时用 System.Drawing 缩放出多尺寸 icon.ico）
# ============================================================================

[CmdletBinding()]
param(
    [string]$Version = '0.3.2',
    [string]$DistDir = '',
    [string]$OutputDir = '',
    [string]$IsccPath = ''
)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

$root = Split-Path -Parent $PSScriptRoot
$installerDir = Join-Path $root 'tools\installer'
$issPath = Join-Path $installerDir 'DiskIntel.iss'
$iconSource = Join-Path $root 'assets\icon.png'
$iconPath = Join-Path $installerDir 'icon.ico'

function Stop-WithMessage([string]$Message) {
    throw $Message
}

# ---------------------------------------------------------------------------
#  System.Drawing：从 assets\icon.png 生成多尺寸 icon.ico（BMP 条目，16~256）
# ---------------------------------------------------------------------------
function New-MultiSizeIcon {
    param(
        [Parameter(Mandatory = $true)][string]$PngPath,
        [Parameter(Mandatory = $true)][string]$IcoPath,
        [int[]]$Sizes = @(16, 32, 48, 64, 128, 256)
    )

    Add-Type -AssemblyName System.Drawing

    $source = [System.Drawing.Image]::FromFile($PngPath)
    $entries = New-Object System.Collections.ArrayList
    try {
        foreach ($size in $Sizes) {
            $bmp = [System.Drawing.Bitmap]::new([int]$size, [int]$size, [System.Drawing.Imaging.PixelFormat]::Format32bppArgb)
            $pixels = $null
            $stride = 0
            try {
                $g = [System.Drawing.Graphics]::FromImage($bmp)
                try {
                    $g.CompositingMode = [System.Drawing.Drawing2D.CompositingMode]::SourceCopy
                    $g.InterpolationMode = [System.Drawing.Drawing2D.InterpolationMode]::HighQualityBicubic
                    $g.PixelOffsetMode = [System.Drawing.Drawing2D.PixelOffsetMode]::HighQuality
                    $g.SmoothingMode = [System.Drawing.Drawing2D.SmoothingMode]::HighQuality
                    $g.Clear([System.Drawing.Color]::Transparent)
                    $rect = [System.Drawing.Rectangle]::new(0, 0, [int]$size, [int]$size)
                    $g.DrawImage($source, $rect)
                }
                finally {
                    $g.Dispose()
                }

                $lockRect = [System.Drawing.Rectangle]::new(0, 0, [int]$size, [int]$size)
                $data = $bmp.LockBits($lockRect, [System.Drawing.Imaging.ImageLockMode]::ReadOnly, [System.Drawing.Imaging.PixelFormat]::Format32bppArgb)
                try {
                    $stride = [int]$data.Stride
                    if ($stride -ne ([int]$size * 4)) {
                        Stop-WithMessage ("图标缩放失败：像素步长异常（size={0} stride={1}）" -f $size, $stride)
                    }
                    $bytesPerImage = $stride * [int]$size
                    $pixels = [byte[]]::new($bytesPerImage)
                    [System.Runtime.InteropServices.Marshal]::Copy($data.Scan0, $pixels, 0, $bytesPerImage)
                }
                finally {
                    $bmp.UnlockBits($data)
                }
            }
            finally {
                $bmp.Dispose()
            }

            $ms = [System.IO.MemoryStream]::new()
            $bw = [System.IO.BinaryWriter]::new($ms)
            try {
                $bw.Write([uint32]40)                  # BITMAPINFOHEADER.biSize
                $bw.Write([int32]$size)                # biWidth
                $bw.Write([int32]($size * 2))          # biHeight：XOR 图 + AND 掩码
                $bw.Write([uint16]1)                   # biPlanes
                $bw.Write([uint16]32)                  # biBitCount
                $bw.Write([uint32]0)                   # BI_RGB
                $bw.Write([uint32]($size * $size * 4)) # biSizeImage
                $bw.Write([int32]0)
                $bw.Write([int32]0)
                $bw.Write([uint32]0)
                $bw.Write([uint32]0)
                for ($y = $size - 1; $y -ge 0; $y--) {
                    $bw.Write($pixels, ($y * $stride), ($size * 4))
                }
                $maskStride = [int]([math]::Ceiling($size / 32.0) * 4)
                $mask = [byte[]]::new($maskStride * $size)
                $bw.Write($mask, 0, $mask.Length)
                $bw.Flush()
                [void]$entries.Add([pscustomobject]@{ Size = [int]$size; Data = $ms.ToArray() })
            }
            finally {
                $bw.Dispose()
                $ms.Dispose()
            }
        }
    }
    finally {
        $source.Dispose()
    }

    $outBytes = $null
    $outStream = [System.IO.MemoryStream]::new()
    $writer = [System.IO.BinaryWriter]::new($outStream)
    try {
        $writer.Write([uint16]0)          # reserved
        $writer.Write([uint16]1)          # type = icon
        $writer.Write([uint16]$entries.Count)
        $offset = [int](6 + 16 * $entries.Count)
        foreach ($entry in $entries) {
            $dimValue = 0
            if ($entry.Size -lt 256) {
                $dimValue = $entry.Size
            }
            $writer.Write([byte]$dimValue)    # width（256 记 0）
            $writer.Write([byte]$dimValue)    # height
            $writer.Write([byte]0)            # color count
            $writer.Write([byte]0)            # reserved
            $writer.Write([uint16]1)          # planes
            $writer.Write([uint16]32)         # bit count
            $writer.Write([uint32]$entry.Data.Length)
            $writer.Write([uint32]$offset)
            $offset = $offset + $entry.Data.Length
        }
        foreach ($entry in $entries) {
            $writer.Write($entry.Data, 0, $entry.Data.Length)
        }
        $writer.Flush()
        $outBytes = $outStream.ToArray()
    }
    finally {
        $writer.Dispose()
    }

    [System.IO.File]::WriteAllBytes($IcoPath, $outBytes)

    $info = Get-Item -LiteralPath $IcoPath
    if ($info.Length -lt 1024) {
        Stop-WithMessage ("生成的 icon.ico 过小（{0} 字节），可能不合法。" -f $info.Length)
    }
    $sizeList = ($Sizes | ForEach-Object { [string]$_ + 'x' + [string]$_ }) -join ' / '
    Write-Host ("      icon.ico 已生成：{0} 字节，尺寸 {1}" -f $info.Length, $sizeList)
}

# ---------------------------------------------------------------------------
#  1. 参数与路径
# ---------------------------------------------------------------------------
if ($Version -notmatch '^\d+(\.\d+){1,3}$') {
    Stop-WithMessage ("版本号必须为 1-4 段数字（例如 0.3.2），因为它要写入 Setup.exe 的版本资源。当前收到：'{0}'" -f $Version)
}

if ([string]::IsNullOrWhiteSpace($DistDir)) {
    $DistDir = Join-Path $root ("dist\DiskIntel-" + $Version)
}
if (-not [System.IO.Path]::IsPathRooted($DistDir)) {
    $DistDir = Join-Path $root $DistDir
}
$DistDir = [System.IO.Path]::GetFullPath($DistDir)

if ([string]::IsNullOrWhiteSpace($OutputDir)) {
    $OutputDir = Join-Path $root 'dist'
}
if (-not [System.IO.Path]::IsPathRooted($OutputDir)) {
    $OutputDir = Join-Path $root $OutputDir
}
$OutputDir = [System.IO.Path]::GetFullPath($OutputDir)

if (-not (Test-Path -LiteralPath $issPath -PathType Leaf)) {
    Stop-WithMessage ("缺少 Inno Setup 脚本：{0}" -f $issPath)
}
if (-not (Test-Path -LiteralPath $iconSource -PathType Leaf)) {
    Stop-WithMessage ("缺少图标源文件：{0}（安装包图标由 assets\icon.png 生成）" -f $iconSource)
}

# ---------------------------------------------------------------------------
#  2. 校验发行目录
# ---------------------------------------------------------------------------
if (-not (Test-Path -LiteralPath $DistDir -PathType Container)) {
    Stop-WithMessage ('发行目录不存在：' + $DistDir + [Environment]::NewLine +
        '请先运行 tools\package.ps1 -Version ' + $Version + '（需要 cargo build --release 与 Python 版 DiskIntelCLI.exe），或用 -DistDir 指向已有的 DiskIntel-<版本> 目录。')
}

$requiredFiles = @(
    'DiskIntel.exe',
    'DiskIntelCLI.exe',
    'README.txt',
    'goz\goz.exe',
    'goz\gozd.exe'
)
$missing = @()
foreach ($rel in $requiredFiles) {
    $full = Join-Path $DistDir $rel
    if (-not (Test-Path -LiteralPath $full -PathType Leaf)) {
        $missing += $rel
    }
}
$toolsDir = Join-Path $DistDir 'tools'
if (-not (Test-Path -LiteralPath $toolsDir -PathType Container)) {
    $missing += 'tools\'
}
$repoUninstallPs1 = Join-Path $root 'tools\uninstall.ps1'
$repoUninstallCmd = Join-Path $root 'tools\uninstall-elevated.cmd'
if (-not (Test-Path -LiteralPath $repoUninstallPs1 -PathType Leaf)) {
    $missing += 'tools\uninstall.ps1（仓库内，需打包进 {app}\tools）'
}
if (-not (Test-Path -LiteralPath $repoUninstallCmd -PathType Leaf)) {
    $missing += 'tools\uninstall-elevated.cmd（仓库内，需打包进 {app}\tools）'
}
if ($missing.Count -gt 0) {
    Stop-WithMessage ('发行目录不完整：' + $DistDir + [Environment]::NewLine +
        '缺少：' + ($missing -join '，') + [Environment]::NewLine +
        '建议：运行 tools\package.ps1 -Version ' + $Version + ' 重新组装发行包，或用 -DistDir 指向完整目录。')
}

# 可选 LICENSE：仓库根或发行目录下有就带进安装包
$licenseCandidates = @(
    (Join-Path $root 'LICENSE'),
    (Join-Path $root 'LICENSE.txt'),
    (Join-Path $DistDir 'LICENSE'),
    (Join-Path $DistDir 'LICENSE.txt')
)
$licenseFile = ''
foreach ($candidate in $licenseCandidates) {
    if (Test-Path -LiteralPath $candidate -PathType Leaf) {
        $licenseFile = $candidate
        break
    }
}

# ---------------------------------------------------------------------------
#  3. 查找 ISCC / 生成图标 / 编译
# ---------------------------------------------------------------------------
$isccCandidates = @()
if (-not [string]::IsNullOrWhiteSpace($IsccPath)) {
    $isccCandidates += $IsccPath
}
if ($env:LOCALAPPDATA) {
    $isccCandidates += (Join-Path $env:LOCALAPPDATA 'Programs\Inno Setup 6\ISCC.exe')
}
$isccCandidates += 'C:\Program Files (x86)\Inno Setup 6\ISCC.exe'
$isccCandidates += 'C:\Program Files\Inno Setup 6\ISCC.exe'
$isccCmd = Get-Command iscc.exe -ErrorAction SilentlyContinue
if ($isccCmd) {
    $isccCandidates += $isccCmd.Source
}
$iscc = ''
foreach ($candidate in $isccCandidates) {
    if ($candidate -and (Test-Path -LiteralPath $candidate -PathType Leaf)) {
        $iscc = $candidate
        break
    }
}
if ($iscc -eq '') {
    Stop-WithMessage ("未找到 Inno Setup 6 编译器 ISCC.exe。" + [Environment]::NewLine +
        "请安装 Inno Setup 6（winget install JRSoftware.InnoSetup），或用 -IsccPath 指定，例如：" + [Environment]::NewLine +
        '  -IsccPath "C:\Users\<你>\AppData\Local\Programs\Inno Setup 6\ISCC.exe"')
}
Write-Host ("[1/4] 编译器：{0}" -f $iscc)

New-Item -ItemType Directory -Force -Path $installerDir | Out-Null
New-Item -ItemType Directory -Force -Path $OutputDir | Out-Null
Write-Host ("[2/4] 生成图标：{0}" -f $iconPath)
New-MultiSizeIcon -PngPath $iconSource -IcoPath $iconPath

# 确保 iss 以 UTF-8 BOM 保存：Inno 官方推荐 Unicode 脚本带 BOM，避免编辑器保存后中文变乱码
$issBytes = [System.IO.File]::ReadAllBytes($issPath)
$hasBom = ($issBytes.Length -ge 3 -and $issBytes[0] -eq 0xEF -and $issBytes[1] -eq 0xBB -and $issBytes[2] -eq 0xBF)
if (-not $hasBom) {
    $bom = [byte[]](0xEF, 0xBB, 0xBF)
    $withBom = New-Object byte[] ($bom.Length + $issBytes.Length)
    [System.Array]::Copy($bom, 0, $withBom, 0, $bom.Length)
    [System.Array]::Copy($issBytes, 0, $withBom, $bom.Length, $issBytes.Length)
    [System.IO.File]::WriteAllBytes($issPath, $withBom)
    Write-Host '      已为 DiskIntel.iss 补写 UTF-8 BOM'
}

$isccArgs = @(
    '/Q',
    ('/DAppVersion=' + $Version),
    ('/DDistDir=' + $DistDir),
    ('/O' + $OutputDir)
)
if ($licenseFile -ne '') {
    $isccArgs += ('/DLicenseFile=' + $licenseFile)
    Write-Host ("      附带 LICENSE：{0}" -f $licenseFile)
}
else {
    Write-Host '      未发现 LICENSE，安装包不包含许可证文件'
}
$isccArgs += $issPath

Write-Host ("[3/4] 编译安装包：{0}" -f $issPath)
$compileOutput = & $iscc @isccArgs 2>&1
$compileExit = $LASTEXITCODE
if ($compileOutput) {
    $compileOutput | ForEach-Object { Write-Host ('      ' + [string]$_) }
}
if ($compileExit -ne 0) {
    Stop-WithMessage ('ISCC 编译失败（exit ' + $compileExit + '）。' + [Environment]::NewLine +
        '请检查上面的编译错误；脚本：' + $issPath)
}

$setupPath = Join-Path $OutputDir ('DiskIntel-' + $Version + '-Setup.exe')
if (-not (Test-Path -LiteralPath $setupPath -PathType Leaf)) {
    Stop-WithMessage ("ISCC 返回成功但未找到产物：{0}（检查 iss 的 OutputDir / OutputBaseFilename）" -f $setupPath)
}

$setupItem = Get-Item -LiteralPath $setupPath
$hash = (Get-FileHash -LiteralPath $setupPath -Algorithm SHA256).Hash.ToLowerInvariant()
$hashPath = $setupPath + '.sha256'
$hashLine = $hash + '  ' + (Split-Path -Leaf $setupPath)
Set-Content -LiteralPath $hashPath -Value $hashLine -Encoding ascii

Write-Host ''
Write-Host '=== 安装包构建完成 ==='
Write-Host ("产物：{0}" -f $setupPath)
Write-Host ("大小：{0:N2} MB（{1} 字节）" -f ($setupItem.Length / 1MB), $setupItem.Length)
Write-Host ("SHA256：{0}" -f $hash)
Write-Host ("校验文件：{0}" -f $hashPath)
Write-Host ("版本：{0}" -f $Version)
Write-Host '卸载程序：Inno Setup 自动生成 {app}\unins000.exe（安装后在开始菜单与「应用和功能」里可卸载）'
Write-Host ("[4/4] 完成")
