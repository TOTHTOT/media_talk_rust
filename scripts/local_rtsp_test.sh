#!/usr/bin/env bash
# 本地 RTSP 测试源一键启动: mediamtx + ffmpeg 循环推流
#
# 用途: 监控页/web 预览的本地联调。ingest 只收 rtsp:// (见 crates/ipcam-gst/src/ingest/config.rs),
# 所以测本地视频必须起一个真 RTSP 服务, 本脚本把两条命令合成一条。
#
# 用法:
#   scripts/local_rtsp_test.sh [视频文件]     # 默认 assets/oceans.mp4
#   Ctrl+C 停止 (mediamtx 和 ffmpeg 一起收掉)
#
# 然后在 GUI 设置页添加摄像头 rtsp://127.0.0.1:8554/<文件名去掉扩展名>
# 注意: ffmpeg 推流的 AAC 音轨是 MPEG4-GENERIC 封装, ingest 只配 LATM,
#       web 支路会无声 (监控本地模式忽略音频不受影响) —— 见 docs/pitfalls.md

set -euo pipefail
cd "$(dirname "$0")/.."

VERSION="v1.15.5"
DIR="tools/mediamtx"
MMTX="$DIR/mediamtx.exe"
VIDEO="${1:-assets/oceans.mp4}"
STREAM="$(basename "$VIDEO" | sed 's/\.[^.]*$//')"
URL="rtsp://127.0.0.1:8554/$STREAM"

# 1. mediamtx 不存在则下载 (GitHub release, windows amd64 zip)
if [ ! -f "$MMTX" ]; then
    echo ">> mediamtx 不存在, 下载 $VERSION 到 $DIR ..."
    mkdir -p "$DIR"
    curl -fSL "https://github.com/bluenviron/mediamtx/releases/download/$VERSION/mediamtx_${VERSION}_windows_amd64.zip" -o "$DIR/mediamtx.zip"
    unzip -o "$DIR/mediamtx.zip" -d "$DIR"
    rm "$DIR/mediamtx.zip"
fi

[ -f "$VIDEO" ] || { echo "!! 视频文件不存在: $VIDEO"; exit 1; }

# 2. 起 mediamtx (后台)
echo ">> 启动 mediamtx ..."
"$MMTX" "$DIR/mediamtx.yml" >/dev/null 2>&1 &
MMTX_PID=$!

# 3. 等 8554 端口起来 (最多 5s)
for _ in $(seq 1 50); do
    (echo > /dev/tcp/127.0.0.1/8554) 2>/dev/null && break
    sleep 0.1
done

# 4. ffmpeg 循环推流 (后台)
echo ">> 推流 $VIDEO -> $URL"
ffmpeg -loglevel warning -re -stream_loop -1 -i "$VIDEO" -c copy -f rtsp "$URL" &
FF_PID=$!

trap 'kill $MMTX_PID $FF_PID 2>/dev/null || true' EXIT INT TERM

echo ""
echo "就绪. GUI 设置页添加摄像头: $URL"
echo "运行: cargo run --release --bin media_talk_rust gui"
echo "Ctrl+C 停止服务"
wait
