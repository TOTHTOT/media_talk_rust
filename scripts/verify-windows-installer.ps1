param(
    [Parameter(Mandatory = $true)]
    [string]$SetupPath
)

$ErrorActionPreference = "Stop"

function Assert-True([bool]$Condition, [string]$Message) {
    if (-not $Condition) { throw $Message }
}

function Invoke-Nsis([string]$Path) {
    $p = Start-Process $Path -ArgumentList "/S" -Wait -PassThru
    Assert-True ($p.ExitCode -eq 0) "NSIS 进程失败: $Path, exit=$($p.ExitCode)"
}

$setup = (Resolve-Path $SetupPath).Path
$installDir = Join-Path $env:LOCALAPPDATA "Programs\MediaTalk"
$regPath = "HKCU:\Software\Microsoft\Windows\CurrentVersion\Uninstall\MediaTalk"
$menuDir = Join-Path ([Environment]::GetFolderPath("Programs")) "MediaTalk"
$appShortcut = Join-Path $menuDir "MediaTalk.lnk"
$uninstallShortcut = Join-Path $menuDir "Uninstall MediaTalk.lnk"
$dataDir = Join-Path $env:APPDATA "media_talk"
$marker = Join-Path $dataDir "installer-test-marker.txt"
$createdDataDir = -not (Test-Path $dataDir)
$uninstalled = $false

Assert-True (-not (Test-Path $installDir)) "验收需要干净环境: $installDir 已存在"
Assert-True (-not (Test-Path $regPath)) "验收需要干净环境: $regPath 已存在"
Assert-True (-not (Test-Path $marker)) "验收标记已存在: $marker"

New-Item -ItemType Directory -Force $dataDir | Out-Null
Set-Content -LiteralPath $marker -Value "preserve-user-data" -Encoding ascii

try {
    Invoke-Nsis $setup
    foreach ($file in @("media_talk_rust.exe", "gst-plugin-scanner.exe", "Uninstall.exe",
                        "lib\gstreamer-1.0\gstcoreelements.dll")) {
        Assert-True (Test-Path (Join-Path $installDir $file)) "安装后缺少 $file"
    }
    Assert-True (Test-Path $appShortcut) "开始菜单程序快捷方式不存在"
    Assert-True (Test-Path $uninstallShortcut) "开始菜单卸载快捷方式不存在"
    Assert-True (Test-Path $regPath) "HKCU 卸载注册项不存在"
    $registration = Get-ItemProperty $regPath
    Assert-True ($registration.DisplayName -eq "MediaTalk") "卸载显示名称不正确"

    $env:PATH = "$installDir;$env:SystemRoot\System32"
    $env:GST_PLUGIN_SYSTEM_PATH = ""
    & (Join-Path $installDir "media_talk_rust.exe") --version
    Assert-True ($LASTEXITCODE -eq 0) "安装后的主程序无法独立启动"

    Invoke-Nsis (Join-Path $installDir "Uninstall.exe")
    $deadline = [DateTime]::UtcNow.AddSeconds(30)
    while ((Test-Path $installDir) -and [DateTime]::UtcNow -lt $deadline) {
        Start-Sleep -Milliseconds 500
    }
    Assert-True (-not (Test-Path $installDir)) "卸载后程序目录仍存在"
    Assert-True (-not (Test-Path $menuDir)) "卸载后开始菜单目录仍存在"
    Assert-True (-not (Test-Path $regPath)) "卸载后注册项仍存在"
    Assert-True ((Get-Content $marker -Raw).Trim() -eq "preserve-user-data") "用户数据被修改"
    $uninstalled = $true
    Write-Host "安装、独立启动、卸载、用户数据保留: OK"
} finally {
    if (-not $uninstalled -and (Test-Path (Join-Path $installDir "Uninstall.exe"))) {
        Start-Process (Join-Path $installDir "Uninstall.exe") -ArgumentList "/S" -Wait
    }
    Remove-Item $marker -Force -ErrorAction SilentlyContinue
    if ($createdDataDir) {
        Remove-Item $dataDir -Force -ErrorAction SilentlyContinue
    }
}
