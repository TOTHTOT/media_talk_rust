# 交叉编译: 用 Docker 给泰山派 (RK3566) 出 aarch64 二进制

目标板: 泰山派 RK3566 (aarch64, Ubuntu/Debian 系 rootfs, glibc 2.31).
产物: `media_talk_rust` (release, `--features hw-decode`).

## 为什么不用 cargo zigbuild

zigbuild 只解决 C 链接器和 glibc 版本号问题, **不带 sysroot**.
本项目无条件依赖 GStreamer (ipcam-gst), `glib-sys` 的 build script
要跑 pkg-config 找 aarch64 版的 GLib/GStreamer 开发库, 裸 zigbuild
直接报 `pkg-config has not been configured to support cross-compilation`.
要救 zigbuild 得手工凑一份 arm64 sysroot (deb 解包 + PKG_CONFIG_SYSROOT_DIR),
在 Windows 上路径问题一堆, 不值.

另外 `hw-decode` 目前还是 stub (`hardware-decode/src/lib.rs` 的 MPP FFI
未绑定, submit 返回 NotImplemented), 构建期**不需要** Rockchip MPP
头文件, 唯一的外部 C 依赖就是 GStreamer + ALSA.

## 方案: ARM 主机 + Docker 原生编译

构建机是 Mac M4 mini (ARM). Docker 跑 linux/arm64 容器在 Apple Silicon
上是**原生执行**, 没有 QEMU 模拟损耗 — 等于找了台 aarch64 Linux 机器
直接 cargo build, 不需要任何交叉工具链.

镜像选 `arm64v8/ubuntu:20.04`: glibc 恰好是 2.31, 和目标
`aarch64-unknown-linux-gnu.2.31` 天然对齐, 不用 zigbuild 压版本.

Dockerfile 在仓库根目录: [Dockerfile.rk3566](../Dockerfile.rk3566).

### 完整流程 (Mac 侧)

```bash
# 0. Docker Desktop 要处于运行状态 (CLI 不在默认 PATH):
export PATH="/Applications/Docker.app/Contents/Resources/bin:$PATH"

# 1. 基镜像 (国内拉不动 Docker Hub, 走 daocloud 镜像再本地 tag):
docker pull docker.m.daocloud.io/arm64v8/ubuntu:20.04
docker tag docker.m.daocloud.io/arm64v8/ubuntu:20.04 arm64v8/ubuntu:20.04

# 2. 构建 (源码整个 COPY 进镜像; 镜像内已换国内源, 见下节):
docker build -f Dockerfile.rk3566 -t media-talk-rk3566 .

# 3. 导出产物:
docker run --rm -v "$PWD/out:/out" media-talk-rk3566 cp /media_talk_rust /out/
```

产物: `./out/media_talk_rust` (ELF aarch64, stripped).

### Dockerfile 里的国内镜像三连

容器在国内网络构建, 三处都要换源, 缺一不可:

| 环节 | 默认 | 换成 | 坑 |
| --- | --- | --- | --- |
| 基镜像 | Docker Hub | `docker.m.daocloud.io` | 直连 auth.docker.io 超时 |
| apt | ports.ubuntu.com | `http://` 清华 ubuntu-ports | **必须 http**, 基础镜像没 ca-certificates, https 必败; arm64 的包在 ubuntu-ports 不是 ubuntu |
| rustup + crates.io | sh.rustup.rs / static.crates.io | rsproxy.cn | rustup-init.sh 也从 rsproxy 拉 |

### 验证产物

```bash
file out/media_talk_rust
# ELF 64-bit LSB pie executable, ARM aarch64, ... for GNU/Linux 3.7.0, stripped
ldd out/media_talk_rust   # 在容器里跑; 期望依赖见下节
```

## GUI (call-ui) 构建

`gui` 子命令 (Slint 通话界面) 在板子上走 KMS 直显, 不需要桌面环境.

### 构建期系统包

slint 的 `backend-linuxkms-noseat` 需要以下 dev 包 (已加进
Dockerfile.rk3566 的 apt 段; 手工搭构建环境时照这份装):

```bash
sudo apt install -y libgbm-dev libxkbcommon-dev libudev-dev libinput-dev
```

用 noseat 而不是 `backend-linuxkms`: Ubuntu 20.04 (focal) 仓库里没有
`libseat-dev` (最早 jammy 才有), 完整版后端在 focal 上装不齐依赖.
noseat 不走 libseat, 代价是进程要自己拿 DRM master — root 或
systemd unit 里 `AmbientCapabilities=CAP_SYS_ADMIN`; 板上本来就是
root 部署, 无额外成本.

### feature 开关

- `call-ui` crate: feature `kms` = `slint/backend-linuxkms-noseat` +
  `slint/renderer-software` (见 `crates/call-ui/Cargo.toml`)
- `media_talk_rust` 转发: `kms = ["call-ui/kms"]`, 构建命令
  `cargo build --release -p media_talk_rust --features hw-decode,kms`
  (Dockerfile 已带; 不能用 `--features call-ui/kms` 直接跨包指定)

### 运行时: SLINT_BACKEND 选择渲染器

```bash
SLINT_BACKEND=linuxkms-software ./media_talk_rust gui -u 9999 --server 192.168.11.17:5062
```

- `linuxkms-software` (优先): 纯 CPU 渲染, 不依赖 GPU/EGL, RK3566 最稳
- `linuxkms-femtovg` (备选): GPU 加速, 需要 GPU/EGL 驱动可用; 板子
  Mali 驱动不齐时会起不来
- 运行时还需要板上有 gbm/xkbcommon/udev/input 的运行库
  (装对应 `-dev` 包的系统一般已带; 精简 rootfs 需补
  `libgbm1 libxkbcommon0 libudev1 libinput10`)

### 退路: winit + weston

linuxkms 后端在 slint 里仍是实验特性, 若编译或运行不过, 退路是板子装
最小 weston, 然后 `SLINT_BACKEND=winit-femtovg` 跑 (winit 后端是默认
feature, 不需要 kms 编译开关). 这里只记方案, 不实现.

## 板端运行时依赖

二进制是动态链接, 泰山派上只需运行库 (不用 dev 包):

```bash
sudo apt install -y libgstreamer1.0-0 \
    gstreamer1.0-plugins-base gstreamer1.0-plugins-good \
    gstreamer1.0-plugins-bad gstreamer1.0-plugins-ugly \
    gstreamer1.0-libav gstreamer1.0-alsa libasound2
```

缺元件时程序以 `GstStreamError::Init` 报错并指明元件名, 可用
`gst-inspect-1.0 <元件名>` 逐个核对 (README 运行时依赖一节有清单).

## 部署 (README §3 的泰山派版)

```bash
scp out/media_talk_rust root@<泰山派>:/usr/local/bin/
scp media_talk_rust.service root@<泰山派>:/etc/systemd/system/
ssh root@<泰山派> '
  useradd --system --home /var/lib/media_talk_rust --shell /usr/sbin/nologin mediatalk
  systemctl daemon-reload
  systemctl enable --now media_talk_rust
  journalctl -u media_talk_rust -f
'
```

## 附: Windows Git Bash 调远程 Mac 的坑

从本仓库的 Windows 开发机用 paramiko 驱动 Mac 时, **单独的绝对路径参数**
会被 MSYS 路径转换吃掉: `put local /Users/yyy/file` 里的远程路径会被改写成
`C:/Program Files/Git/Users/yyy/file`, 服务端报 ENOENT. 绕法: 把路径写进
Python 脚本字符串里 (不参与参数转换), 或给命令加 `MSYS_NO_PATHCONV=1` 前缀.

## GitHub Actions (推荐长期方案)

同一套 Dockerfile 可以直接搬进 CI, 见
[rk3566.yml](../.github/workflows/rk3566.yml): 跑在 `ubuntu-24.04-arm`
**原生 ARM64 runner** 上 (公开仓库免费), 容器编译 + 导出 + 传 Artifacts,
手动触发或打 `v*` tag 时运行. 打 tag 触发时会额外用 `gh release create`
把二进制挂到同名 Release 上; 手动触发只留 Artifacts.

和 Mac 构建的差异:

- runner 直连 Docker Hub, 基镜像不用 daocloud 预拉
- Dockerfile 里的国内源 (tuna/rsproxy) 在 CI 上也能用, 不拆两套
- glibc 版本由容器里的 ubuntu:20.04 保证, 与 runner 宿主 (24.04,
  glibc 2.39) 无关 — 这正是用容器而不是直接在 runner 上 cargo build
  的原因
- 私有仓库的 arm runner 计费, 届时退回 Mac 构建即可
