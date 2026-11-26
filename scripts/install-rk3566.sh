#!/bin/sh
# media_talk_rust 设备端安装器 (RK3566 aarch64, glibc >= 2.31).
# 在解压后的包目录内以 root 执行:
#   tar xzf media_talk_rust-<ver>-rk3566-aarch64-glibc2.31.tar.gz
#   cd media_talk_rust-<ver>-rk3566-aarch64-glibc2.31
#   sudo sh install.sh
#
# 幂等: 重复执行 = 覆盖升级。布局与服务文件约定一致:
#   二进制  /usr/local/bin/media_talk_rust
#   运行库  /opt/mediatalk/lib (+ ld.so.conf.d 注册)
#   插件    /opt/mediatalk/lib/gstreamer-1.0 (GST_PLUGIN_PATH 由 service 注入)
#   数据    /usr/share/rime-data (合并), /usr/local/share/fonts (合并)
set -e

PREFIX=/opt/mediatalk
SRC="$(cd "$(dirname "$0")" && pwd)"

if [ "$(id -u)" != "0" ]; then
    echo "需要 root: sudo sh install.sh" >&2
    exit 1
fi

echo ">> 安装二进制与运行库到 $PREFIX"
install -d "$PREFIX/lib/gstreamer-1.0" "$PREFIX/libexec/gstreamer-1.0" \
           /usr/local/bin /usr/local/share/fonts
cp -a "$SRC/opt/mediatalk/." "$PREFIX/"
install -m 0755 "$SRC/opt/mediatalk/bin/media_talk_rust" /usr/local/bin/media_talk_rust

echo ">> 注册动态链接器路径"
echo "$PREFIX/lib" > /etc/ld.so.conf.d/mediatalk.conf
ldconfig

echo ">> 合并拼音词库与字体到系统目录"
[ -d "$SRC/opt/mediatalk/share/rime-data" ] && \
    cp -a "$SRC/opt/mediatalk/share/rime-data/." /usr/share/rime-data/
[ -d "$SRC/opt/mediatalk/share/fonts" ] && \
    cp -a "$SRC/opt/mediatalk/share/fonts/." /usr/local/share/fonts/ 2>/dev/null || true
command -v fc-cache >/dev/null && fc-cache -f >/dev/null 2>&1 || true

echo ">> 安装 systemd 服务"
if command -v systemctl >/dev/null 2>&1; then
    cp "$SRC/media_talk_rust.service" /etc/systemd/system/media_talk_rust.service
    # 服务以 mediatalk 用户运行, 缺则建 (与手动部署文档一致)
    if ! id mediatalk >/dev/null 2>&1; then
        useradd --system --home /var/lib/media_talk_rust --shell /usr/sbin/nologin mediatalk
    fi
    systemctl daemon-reload
    echo ">> 完成: systemctl enable --now media_talk_rust 启动"
    echo "   日志: journalctl -u media_talk_rust -f"
else
    echo "!! 无 systemctl (容器?), 跳过服务安装。手动运行:" >&2
    echo "   GST_PLUGIN_PATH=$PREFIX/lib/gstreamer-1.0 \\" >&2
    echo "   GST_PLUGIN_SCANNER=$PREFIX/libexec/gstreamer-1.0/gst-plugin-scanner \\" >&2
    echo "   /usr/local/bin/media_talk_rust serve --bind 0.0.0.0:8080" >&2
fi
