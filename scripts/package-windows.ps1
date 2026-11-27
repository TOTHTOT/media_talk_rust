# media_talk_rust Windows 打包: exe + GStreamer MSVC 运行库 + 插件白名单.
# 在 CI (windows-latest) 或本机 (装过 GStreamer MSVC 包) 运行:
#   powershell -File scripts/package-windows.ps1 -Version 1.2.3
#
# 产出 dist/media_talk_rust-<ver>-windows-x86_64.zip:
#   media_talk_rust.exe
#   *.dll                          # gstreamer bin 全量运行库 (crt-static, 无 vcredist 依赖)
#   gst-plugin-scanner.exe         # 插件扫描器 (若安装器提供)
#   lib/gstreamer-1.0/libgst*.dll  # 插件白名单 (与板端同一份逻辑名单)
#   run.bat                        # 设置 GST_PLUGIN_PATH 后启动
#   THIRD-PARTY-NOTICES.txt        # GStreamer LGPL 归属声明 (分发必需)
param(
    [string]$Version = "dev",
    [string]$GStreamerRoot = "C:\gstreamer\1.0\msvc_x86_64"
)

$ErrorActionPreference = "Stop"

$gstBin = Join-Path $GStreamerRoot "bin"
$gstPluginSrc = Join-Path $GStreamerRoot "lib\gstreamer-1.0"
$exe = "target\release\media_talk_rust.exe"
$dst = "dist\media_talk_rust-$Version-windows-x86_64"

if (-not (Test-Path $exe)) { throw "找不到 $exe, 先 cargo build --release" }
if (-not (Test-Path $gstBin)) { throw "找不到 $gstBin, 先装 GStreamer MSVC 包" }

# 公共插件与 scripts/package-rk3566.sh 一致, 另加 Windows 采集/输出插件
$plugins = @(
    "coreelements", "app", "autodetect", "audioconvert", "audioresample", "opus",
    "videoconvert", "videoscale", "videoconvertscale",
    "rtsp", "rtp", "rtpmanager", "udp", "tcp", "audioparsers",
    "videoparsersbad", "webrtc", "dtls", "srtp", "sctp", "nice", "libav",
    "alaw", "mulaw", "mpegtsmux", "playback", "typefindfunctions", "isomp4", "x264",
    "winks", "wasapi", "wasapi2", "directsound", "d3d11"
)
$required = @(
    "coreelements", "app", "autodetect", "audioconvert", "audioresample", "opus",
    "rtsp", "rtp", "rtpmanager", "udp", "tcp", "audioparsers",
    "videoparsersbad", "webrtc", "dtls", "srtp", "sctp", "nice", "libav",
    "alaw", "mulaw", "mpegtsmux", "playback", "typefindfunctions", "isomp4", "x264",
    "winks", "wasapi", "directsound", "d3d11"
)

Remove-Item -Recurse -Force $dst -ErrorAction SilentlyContinue
New-Item -ItemType Directory -Force $dst, "$dst\lib\gstreamer-1.0" | Out-Null

Copy-Item $exe "$dst\media_talk_rust.exe"

# 运行库: gstreamer bin 下全部 DLL (含 glib/gio/gst 栈及 nice/srtp 等依赖),
# 宁可多带不可漏 — 漏一个客户机就是启动失败, 全量约 100MB, zip 后 30-40MB
Copy-Item "$gstBin\*.dll" $dst

# 插件扫描器: 注册表生成需要, 部分安装器放在 bin, 部分在 libexec
foreach ($c in @("$gstBin\gst-plugin-scanner.exe",
                (Join-Path $GStreamerRoot "libexec\gstreamer-1.0\gst-plugin-scanner.exe"))) {
    if (Test-Path $c) { Copy-Item $c $dst; break }
}

# 插件白名单: Windows 文件名带不带 lib 前缀取决于打包者, 两种都试
$copied = @()
foreach ($p in $plugins) {
    foreach ($n in @("libgst$p.dll", "gst$p.dll")) {
        $src = Join-Path $gstPluginSrc $n
        if (Test-Path $src) { Copy-Item $src "$dst\lib\gstreamer-1.0\"; $copied += $p; break }
    }
}
Write-Host "plugins bundled: $($copied -join ' ')"
$missing = @($required | Where-Object { $copied -notcontains $_ })
if ($missing.Count) { throw "关键 GStreamer 插件缺失: $($missing -join ' ')" }
if (-not ($copied | Where-Object { $_ -in @("videoconvert", "videoscale", "videoconvertscale") })) {
    throw "关键 GStreamer 插件缺失: videoconvert/videoscale/videoconvertscale"
}

# 启动包装: 插件目录相对包定位, 客户机不用设任何环境变量
@"
@echo off
rem GStreamer 插件与运行库随包携带, 无需安装 GStreamer
set "GST_PLUGIN_PATH=%~dp0lib\gstreamer-1.0"
"%~dp0media_talk_rust.exe" %*
"@ | Set-Content -Encoding ascii "$dst\run.bat"

@"
media_talk_rust $Version 第三方组件归属

本包携带 GStreamer (https://gstreamer.freedesktop.org) 及其依赖
(GLib, libnice, libsrtp, opus, FFmpeg 等) 的预编译二进制,
按 LGPL-2.1-or-later (GStreamer/GLib/libnice) / BSD 类 (opus, libsrtp) /
LGPL/GPL-2.1+-with-linking-exception (FFmpeg, 经 gst-libav) 许可证发布。
你享有这些许可证赋予的权利; 若需对应源代码, 访问上述项目官网或联系发行方。
"@ | Set-Content -Encoding utf8 "$dst\THIRD-PARTY-NOTICES.txt"

New-Item -ItemType Directory -Force dist | Out-Null
$zip = "dist\media_talk_rust-$Version-windows-x86_64.zip"
Remove-Item -Force $zip -ErrorAction SilentlyContinue
Compress-Archive -Path $dst -DestinationPath $zip
Write-Host "==> $zip"
