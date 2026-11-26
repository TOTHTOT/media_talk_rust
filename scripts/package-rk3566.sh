#!/usr/bin/env bash
# 在 Dockerfile.rk3566 构建的容器内运行, 把 release 二进制 + 全部运行时依赖
# 打成自包含 tar 包 (客户机无需 apt 装 GStreamer/ALSA/librime 任何东西).
#
# 用法 (workflow 内):
#   docker run --rm -e RELEASE_VERSION=1.2.3 -v "$PWD/dist:/out" \
#       media-talk-rk3566 bash /src/scripts/package-rk3566.sh
#
# 包布局:
#   media_talk_rust-<ver>-rk3566-aarch64-glibc2.31/
#     install.sh            # 板端 root 执行, 见文件头注释
#     media_talk_rust.service  # 已注入 GST_PLUGIN_PATH 等环境
#     opt/mediatalk/bin/media_talk_rust
#     opt/mediatalk/lib/*.so*                      # ldd 闭包 (递归, 去重)
#     opt/mediatalk/lib/gstreamer-1.0/*.so         # 插件白名单
#     opt/mediatalk/libexec/gstreamer-1.0/gst-plugin-scanner
#     opt/mediatalk/share/rime-data/               # 拼音词库 (RIME_DATA_DIR 默认路径)
#     opt/mediatalk/share/fonts/                   # 中文字体 (GUI)
set -euo pipefail

VERSION="${RELEASE_VERSION:-dev}"
OUT_DIR="${OUT_DIR:-/out}"
ROOT="/tmp/pkgroot"
PKG_NAME="media_talk_rust-${VERSION}-rk3566-aarch64-glibc2.31"
LIBDIR="/usr/lib/aarch64-linux-gnu"
GST_PLUGIN_DIR="$LIBDIR/gstreamer-1.0"

# ---- gst 插件白名单 (逻辑名 → libgst<名>.so 候选, 存在才拷) -------------------
# 来源: ipcam-gst 代码里 ElementFactory::make 的元件清单
#   rtspsrc→rtsp, rtpbin→rtpmanager, rtph264/265depay→rtp, h264/265parse→videoparsersbad,
#   avdec_*→libav, appsink→app, webrtcsink→webrtc(+dtls/srtp/sctp), nicesrc/sink→nice,
#   alsasink→alsa, audioconvert/audioresample→同名, opusenc→opus,
#   videoconvert/videoscale→1.16 是两个插件, 1.20+ 合并为 videoconvertscale, 全列靠存在性过滤
PLUGINS="coreelements app alsa audioconvert audioresample opus \
    videoconvert videoscale videoconvertscale \
    rtsp rtp rtpmanager udp tcp audioparsers \
    videoparsersbad webrtc dtls srtp sctp nice libav"

# ---- ldd 闭包收集 -----------------------------------------------------------
# 板子系统自带的库不打包 (glibc 内核族 + nss 动态装载器族)
SKIP='^(ld-linux-aarch64\.so\.[0-9]+|libc\.so\.[0-9]+|libm\.so\.[0-9]+|libpthread\.so\.[0-9]+|libdl\.so\.[0-9]+|librt\.so\.[0-9]+|libresolv\.so\.[0-9]+|libnsl\.so\.[0-9]+|libutil\.so\.[0-9]+|libnss_[a-z0-9_]+\.so\.[0-9]+)$'
declare -A COPIED=()

collect_deps() { # $1 = 已拷贝文件的目标目录, 其余参数 = 要分析的 elf 文件
    local dest="$1"; shift
    # 注意必须用进程替换喂 while, 不能管道: 管道会让 while 进子 shell,
    # COPIED 去重表和递归拷贝全丢
    while read -r lib; do
        local base
        base="$(basename "$lib")"
        [[ "$base" =~ $SKIP ]] && continue
        [[ -n "${COPIED[$base]:-}" ]] && continue
        COPIED[$base]=1
        cp -L "$lib" "$dest/$base"
        # 新拷进来的库继续展开自己的依赖 (迭代到不动点)
        collect_deps "$dest" "$dest/$base"
    done < <(ldd "$@" 2>/dev/null | awk '
        $1 ~ /^\// { print $1; next }          # 裸路径行: /lib/x.so (0x...)
        $2 == "=>" && $3 ~ /^\// { print $3 }  # 名字 => 路径 行
    ')
}

# ---- 组装 -------------------------------------------------------------------
rm -rf "$ROOT"
mkdir -p "$ROOT/$PKG_NAME/opt/mediatalk/bin" \
         "$ROOT/$PKG_NAME/opt/mediatalk/lib/gstreamer-1.0" \
         "$ROOT/$PKG_NAME/opt/mediatalk/libexec/gstreamer-1.0" \
         "$ROOT/$PKG_NAME/opt/mediatalk/share"

cp /media_talk_rust "$ROOT/$PKG_NAME/opt/mediatalk/bin/"
collect_deps "$ROOT/$PKG_NAME/opt/mediatalk/lib" /media_talk_rust

# 插件白名单: 存在才拷, 拷完递归收集插件自身的依赖 (libavcodec/libnice/libsrtp...)
found=""
for p in $PLUGINS; do
    src="$GST_PLUGIN_DIR/libgst$p.so"
    if [ -f "$src" ]; then
        cp -L "$src" "$ROOT/$PKG_NAME/opt/mediatalk/lib/gstreamer-1.0/"
        collect_deps "$ROOT/$PKG_NAME/opt/mediatalk/lib" "$src"
        found="$found $p"
    fi
done
echo "plugins bundled:$found"

# gst-plugin-scanner: Debian 系在 multiarch 插件目录下, 其他发行版在 libexec
scanner=""
for c in "$GST_PLUGIN_DIR/gst-plugin-scanner" \
         "/usr/libexec/gstreamer-1.0/gst-plugin-scanner"; do
    [ -f "$c" ] && scanner="$c" && break
done
[ -n "$scanner" ] || { echo "gst-plugin-scanner not found" >&2; exit 1; }
cp -L "$scanner" "$ROOT/$PKG_NAME/opt/mediatalk/libexec/gstreamer-1.0/"

# rime 拼音数据 (shared_data_dir 默认 /usr/share/rime-data, install.sh 合并到该路径)
if [ -d /usr/share/rime-data ]; then
    cp -a /usr/share/rime-data "$ROOT/$PKG_NAME/opt/mediatalk/share/rime-data"
else
    echo "warning: /usr/share/rime-data 不存在, 包内不含拼音词库 (中文输入将降级英文)" >&2
fi

# 中文字体 (Dockerfile 的 fonts-wqy-zenhei; 精简 rootfs 的板子 GUI 必需)
mkdir -p "$ROOT/$PKG_NAME/opt/mediatalk/share/fonts"
find /usr/share/fonts -name '*.tt[cf]' -o -name '*.otf' 2>/dev/null \
    | while read -r f; do cp -L "$f" "$ROOT/$PKG_NAME/opt/mediatalk/share/fonts/"; done

# service 文件: 注入运行环境定位 (仓库原文件保持手装场景不变, 只改包内副本)
sed -e 's|^Environment=RUST_LOG=info$|Environment=RUST_LOG=info\nEnvironment=GST_PLUGIN_PATH=/opt/mediatalk/lib/gstreamer-1.0\nEnvironment=GST_PLUGIN_SCANNER=/opt/mediatalk/libexec/gstreamer-1.0/gst-plugin-scanner\nEnvironment=GST_REGISTRY=/var/lib/media_talk_rust/registry.bin|' \
    /src/media_talk_rust.service > "$ROOT/$PKG_NAME/media_talk_rust.service"

cp /src/scripts/install-rk3566.sh "$ROOT/$PKG_NAME/install.sh"

# ---- 出包 -------------------------------------------------------------------
mkdir -p "$OUT_DIR"
tar -C "$ROOT" -czf "$OUT_DIR/$PKG_NAME.tar.gz" "$PKG_NAME"
echo "==> $OUT_DIR/$PKG_NAME.tar.gz"
du -sh "$OUT_DIR/$PKG_NAME.tar.gz"
