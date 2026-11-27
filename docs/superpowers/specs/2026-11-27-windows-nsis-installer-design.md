# Windows NSIS 安装器设计

## 背景与目标

当前 Windows 发布流只生成自包含 ZIP。用户需要无需管理员权限的
`setup.exe`，同时保留绿色 ZIP。安装器必须随现有 GitHub Actions 构建，
并验证安装、启动和卸载，而不是只确认安装器文件生成成功。

选择 NSIS：可直接生成单个 EXE，当前用户安装与卸载脚本简单，CI 可通过
Chocolatey 安装。Inno Setup 对本需求没有额外收益；WiX 主要产出 MSI，
若要 `setup.exe` 还需 bootstrapper，复杂度不必要。

## 产物与构建流程

保留现有产物：

```text
media_talk_rust-<version>-windows-x86_64.zip
```

新增产物：

```text
media_talk_rust-<version>-windows-x86_64-setup.exe
```

`release.yml` 的 Windows job 在现有编译和绿色包生成后：

1. 安装 NSIS。
2. 调用安装器构建脚本，输入版本号、绿色包目录和输出文件路径。
3. 运行安装器验收。
4. 将 `dist/*.zip` 与 `dist/*-setup.exe` 上传到同一个 Artifact。

tag 发布时，现有 Release job 会同时附带 ZIP 和 setup.exe，无需另设发布流。

新增文件：

- `scripts/windows-installer.nsi`：安装、快捷方式、卸载和注册表行为。
- `scripts/package-windows-installer.ps1`：定位 `makensis.exe`，校验输入目录，
  传递版本、源目录和输出路径并检查编译返回值。

## 安装与卸载行为

- 仅当前用户安装：`RequestExecutionLevel user`，不请求管理员权限。
- 默认安装目录：`%LOCALAPPDATA%\Programs\MediaTalk`。
- 安装内容复用绿色包目录，包括主程序、全部 DLL、GStreamer 插件、scanner、
  第三方许可说明和卸载器。
- 开始菜单创建 `MediaTalk` 快捷方式，目标为安装目录中的
  `media_talk_rust.exe gui`；同时创建卸载快捷方式。
- 卸载注册项写入
  `HKCU\Software\Microsoft\Windows\CurrentVersion\Uninstall\MediaTalk`，
  包含显示名称、版本、安装目录和卸载命令。
- 重复安装使用同一目录覆盖升级。文件被运行中的程序占用时，NSIS 默认重试/
  中止流程向用户报告，不强制杀进程。
- 卸载删除程序目录、快捷方式和卸载注册项。
- 卸载保留 `%APPDATA%\media_talk`，不提供删除用户数据选项。
- 不修改系统级 GStreamer 环境，不安装全局 GStreamer，不写 HKLM。
- 本期不做代码签名；没有签名证书时 SmartScreen 可能提示未知发布者。

## 随包运行环境定位

开始菜单快捷方式不能依赖 `run.bat`，否则会额外出现控制台窗口。Windows
主程序在初始化 GStreamer 前检查自身目录：

- 若 EXE 同级存在 `lib/gstreamer-1.0`，将 `GST_PLUGIN_PATH` 指向该目录。
- 若 EXE 同级存在 `gst-plugin-scanner.exe`，将 `GST_PLUGIN_SCANNER` 指向它。
- 开发构建的 EXE 旁没有上述目录时不改环境，继续使用开发机配置。
- `run.bat` 保留，行为不变；绿色 ZIP 直接双击 EXE 也能使用随包插件。

该初始化只处理随包定位，不覆盖应用数据路径，不启用便携模式。

## 错误处理

- 安装器构建脚本在绿色包目录、主程序、插件目录或 `makensis.exe` 缺失时
  立即失败。
- NSIS 编译非零返回码使打包步骤失败，不上传残缺安装器。
- CI 验收任一步失败都阻止 Artifact 上传。
- 安装或卸载进程的非零退出码、关键文件缺失、卸载注册项缺失均视为失败。

## 验证

在本机先编译 NSIS 脚本并检查产物。GitHub Actions 在干净 runner 上执行：

1. `setup.exe /S` 静默安装，等待进程退出。
2. 检查主程序、插件、scanner、开始菜单快捷方式和 HKCU 卸载注册项。
3. 将进程 `PATH` 限为安装目录与 System32，清空系统插件搜索路径，执行安装后的
   `media_talk_rust.exe --version`，确认安装版可独立启动。
4. 执行卸载器 `/S` 并等待完成。
5. 检查程序目录、快捷方式和卸载注册项已删除。
6. 安装前在 `%APPDATA%\media_talk` 创建验收标记文件，卸载后确认该文件仍存在，
   证明卸载器不会删除用户数据。

最终仍需人工在 Windows 上运行一次非静默安装，确认安装向导、开始菜单启动
GUI 和卸载入口。RK3566 构建与安装行为不变。
