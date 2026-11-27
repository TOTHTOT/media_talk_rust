param(
    [string]$Version = "dev",
    [string]$SourceDir = "dist\media_talk_rust-$Version-windows-x86_64",
    [string]$OutputFile = "dist\media_talk_rust-$Version-windows-x86_64-setup.exe",
    [string]$Makensis = ""
)

$ErrorActionPreference = "Stop"
$source = (Resolve-Path $SourceDir).Path
foreach ($path in @("$source\media_talk_rust.exe", "$source\run.bat", "$source\lib\gstreamer-1.0")) {
    if (-not (Test-Path $path)) { throw "安装源不完整: $path" }
}

$command = Get-Command makensis.exe -ErrorAction SilentlyContinue
if (-not $Makensis -and $command) { $Makensis = $command.Source }
if (-not $Makensis) {
    $Makensis = @("${env:ProgramFiles(x86)}\NSIS\makensis.exe", "$env:ProgramFiles\NSIS\makensis.exe") |
        Where-Object { Test-Path $_ } | Select-Object -First 1
}
if (-not $Makensis -or -not (Test-Path $Makensis)) { throw "找不到 makensis.exe, 先安装 NSIS" }

$output = [IO.Path]::GetFullPath($OutputFile)
New-Item -ItemType Directory -Force (Split-Path $output) | Out-Null
$script = (Resolve-Path "scripts\windows-installer.nsi").Path
$args = @("/V4", "/DVERSION=$Version", "/DSOURCE_DIR=$source", "/DOUTPUT_FILE=$output", $script)
& $Makensis $args
if ($LASTEXITCODE -ne 0) { throw "makensis 失败: exit=$LASTEXITCODE" }
if (-not (Test-Path $output)) { throw "安装器未生成: $output" }
Write-Host "==> $output"
