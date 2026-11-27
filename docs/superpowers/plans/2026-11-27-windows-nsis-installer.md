# Windows NSIS 安装器 Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 在保留绿色 ZIP 的同时，为 Windows 构建当前用户安装版 setup.exe，并验证安装、独立启动、卸载及用户数据保留。

**Architecture:** 主程序在创建 Tokio runtime 前定位 EXE 同目录的随包 GStreamer，开始菜单因此可直接启动 GUI。现有绿色包目录作为 NSIS 唯一安装源，NSIS 负责程序文件、快捷方式与 HKCU 卸载项；独立验收脚本驱动真实安装和卸载，release.yml 复用同一验收。

**Tech Stack:** Rust 2024、Tokio、NSIS 3、PowerShell 5.1+/7、GitHub Actions windows-latest。

## Global Constraints

- 保留 `media_talk_rust-<version>-windows-x86_64.zip`。
- 新增 `media_talk_rust-<version>-windows-x86_64-setup.exe`，同一 Artifact 和 Release 发布。
- 仅当前用户安装，默认 `%LOCALAPPDATA%\Programs\MediaTalk`，不写 HKLM，不请求管理员权限。
- 卸载保留 `%APPDATA%\media_talk`，不提供删除用户数据选项。
- 快捷方式直接执行 `media_talk_rust.exe gui`，不得经过 BAT 弹控制台。
- 开发机 EXE 旁没有随包插件目录时不得改变 GStreamer 环境。
- 本期不做代码签名；RK3566 构建与安装行为不变。
- PowerShell 脚本保存为 UTF-8 with BOM，兼容本机 Windows PowerShell 5.1。

---

### Task 1: Windows 随包 GStreamer 自动定位

**Files:**
- Create: `crates/media_talk_rust/src/packaged_env.rs`
- Modify: `crates/media_talk_rust/src/main.rs:1-5,115-118`

**Interfaces:**
- Consumes: `std::env::current_exe()` 返回的主程序路径。
- Produces: `discover_bundled_gstreamer(exe_path: &Path) -> Option<BundledGstreamer>` 与 `configure_bundled_gstreamer()`；后者只在 Windows 主入口、Tokio runtime 创建前调用。

- [ ] **Step 1: 先写路径发现失败测试**

创建 `packaged_env.rs`，先只放测试；在 `main.rs` 顶部加 `#[cfg(windows)] mod packaged_env;`，暂时不要调用它。

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::path::PathBuf;

    fn test_root(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "media-talk-packaged-env-{name}-{}",
            std::process::id()
        ))
    }

    #[test]
    fn ignores_development_executable_without_bundle() {
        let root = test_root("development");
        fs::create_dir_all(&root).unwrap();
        assert_eq!(discover_bundled_gstreamer(&root.join("media_talk_rust.exe")), None);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn finds_plugins_and_scanner_next_to_executable() {
        let root = test_root("bundle");
        let plugins = root.join("lib").join("gstreamer-1.0");
        fs::create_dir_all(&plugins).unwrap();
        fs::write(root.join("gst-plugin-scanner.exe"), b"test").unwrap();
        let found = discover_bundled_gstreamer(&root.join("media_talk_rust.exe")).unwrap();
        assert_eq!(found.plugin_path, plugins);
        assert_eq!(found.scanner, Some(root.join("gst-plugin-scanner.exe")));
        fs::remove_dir_all(root).unwrap();
    }
}
```

- [ ] **Step 2: 运行测试并确认按预期失败**

Run: `cargo test -p media_talk_rust --bin media_talk_rust packaged_env`

Expected: FAIL，错误指出 `discover_bundled_gstreamer` 和 `BundledGstreamer` 尚未定义；不是编译环境错误。

- [ ] **Step 3: 实现最小路径发现和环境应用**

把以下实现放在 `packaged_env.rs` 测试模块之前：

```rust
use std::path::{Path, PathBuf};

#[derive(Debug, Eq, PartialEq)]
pub(crate) struct BundledGstreamer {
    pub(crate) plugin_path: PathBuf,
    pub(crate) scanner: Option<PathBuf>,
}

pub(crate) fn discover_bundled_gstreamer(exe_path: &Path) -> Option<BundledGstreamer> {
    let root = exe_path.parent()?;
    let plugin_path = root.join("lib").join("gstreamer-1.0");
    if !plugin_path.is_dir() {
        return None;
    }
    let scanner = root.join("gst-plugin-scanner.exe");
    Some(BundledGstreamer {
        plugin_path,
        scanner: scanner.is_file().then_some(scanner),
    })
}

pub(crate) fn configure_bundled_gstreamer() {
    let Ok(exe) = std::env::current_exe() else {
        return;
    };
    let Some(bundle) = discover_bundled_gstreamer(&exe) else {
        return;
    };
    // SAFETY: main 在创建 Tokio runtime、任何 worker 或应用线程之前调用本函数。
    unsafe {
        std::env::set_var("GST_PLUGIN_PATH", bundle.plugin_path);
        if let Some(scanner) = bundle.scanner {
            std::env::set_var("GST_PLUGIN_SCANNER", scanner);
        }
    }
}
```

- [ ] **Step 4: 在 runtime 创建前接入主入口**

把 `#[tokio::main] async fn main()` 改成同步入口，原函数体原样移动到 `async_main()`。这样满足 Rust 2024 对环境修改必须在多线程 runtime 前的约束：

```rust
fn main() -> anyhow::Result<()> {
    #[cfg(windows)]
    packaged_env::configure_bundled_gstreamer();
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?
        .block_on(async_main())
}

async fn async_main() -> anyhow::Result<()> {
    init_tracing();
    let cli = Cli::parse();

    match cli.command {
        Commands::Discover { timeout_secs, json, username, password } => {
            ipc::discover::run(timeout_secs, json, username, password).await?
        }
        Commands::Serve {
            bind, discovery_timeout_secs, username, password, rtsp_url, audio_out,
        } => {
            let shutdown = ipc::shutdown::install();
            ipc::serve::run(
                bind,
                discovery_timeout_secs,
                username,
                password,
                rtsp_url,
                audio_out,
                shutdown,
            )
            .await?
        }
        Commands::DecodeBench { path, max_frames } => {
            ipc::decode_bench::run(&path, max_frames).await?
        }
        Commands::Audio { action } => match action {
            AudioAction::ListDevices => ipc::audio::list_devices(),
        },
        Commands::V4l2 { action } => match action {
            V4l2Action::List => ipc::v4l2::list(),
        },
        Commands::Gui {
            server,
            username,
            password,
            expires,
            video_src,
            audio_src,
            max_width,
            local_ip,
            video_rtp_port,
            audio_rtp_port,
            ice,
            stun,
        } => {
            call_ui::run(call_ui::GuiSipOverrides {
                server,
                username,
                password,
                expires,
                local_ip,
                video_src,
                audio_src,
                max_width,
                ice,
                stun,
                video_rtp_port,
                audio_rtp_port,
            })?;
        }
    }
    Ok(())
}
```

实现时必须保留原 `match cli.command` 的全部分支，不得用上面注释替代实际代码。

- [ ] **Step 5: 运行单元测试和完整编译**

Run:

```bash
cargo test -p media_talk_rust --bin media_talk_rust packaged_env
cargo build --release -p media_talk_rust --locked
```

Expected: 两个路径测试 PASS；release 编译 exit 0。开发机构建没有同目录 `lib/gstreamer-1.0`，行为不变。

- [ ] **Step 6: 提交**

```bash
git add crates/media_talk_rust/src/main.rs crates/media_talk_rust/src/packaged_env.rs
git commit -m "[新增] Windows 主程序自动定位随包 GStreamer"
```

### Task 2: NSIS 安装器与真实安装卸载验收

**Files:**
- Create: `scripts/windows-installer.nsi`
- Create: `scripts/package-windows-installer.ps1`
- Create: `scripts/verify-windows-installer.ps1`
- Test: `scripts/verify-windows-installer.ps1`

**Interfaces:**
- Consumes: `dist/media_talk_rust-<version>-windows-x86_64/` 绿色包目录。
- Produces: `dist/media_talk_rust-<version>-windows-x86_64-setup.exe`；验收脚本参数为 `-SetupPath <path>`。

- [ ] **Step 1: 先运行验收，确认安装器尚不存在时失败**

创建验收脚本后再运行：

```powershell
powershell -ExecutionPolicy Bypass -File scripts/verify-windows-installer.ps1 `
  -SetupPath dist/media_talk_rust-dev-windows-x86_64-setup.exe
```

Expected: FAIL，明确提示 setup.exe 不存在；不得修改现有安装或用户数据。

验收脚本完整实现如下：

```powershell
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
```

- [ ] **Step 2: 编写 NSIS 脚本**

`windows-installer.nsi` 完整内容：

```nsi
!include "MUI2.nsh"

!ifndef VERSION
!define VERSION "dev"
!endif
!ifndef SOURCE_DIR
!error "SOURCE_DIR is required"
!endif
!ifndef OUTPUT_FILE
!error "OUTPUT_FILE is required"
!endif

Name "MediaTalk"
OutFile "${OUTPUT_FILE}"
Unicode true
RequestExecutionLevel user
InstallDir "$LOCALAPPDATA\Programs\MediaTalk"
SetCompressor /SOLID lzma
BrandingText "MediaTalk ${VERSION}"

!define MUI_ABORTWARNING
!define MUI_FINISHPAGE_RUN "$INSTDIR\media_talk_rust.exe"
!define MUI_FINISHPAGE_RUN_PARAMETERS "gui"
!insertmacro MUI_PAGE_DIRECTORY
!insertmacro MUI_PAGE_INSTFILES
!insertmacro MUI_PAGE_FINISH
!insertmacro MUI_UNPAGE_CONFIRM
!insertmacro MUI_UNPAGE_INSTFILES
!insertmacro MUI_LANGUAGE "SimpChinese"

Section "Install"
    SetShellVarContext current
    SetOutPath "$INSTDIR"
    File /r "${SOURCE_DIR}\*"
    WriteUninstaller "$INSTDIR\Uninstall.exe"

    WriteRegStr HKCU "Software\Microsoft\Windows\CurrentVersion\Uninstall\MediaTalk" "DisplayName" "MediaTalk"
    WriteRegStr HKCU "Software\Microsoft\Windows\CurrentVersion\Uninstall\MediaTalk" "DisplayVersion" "${VERSION}"
    WriteRegStr HKCU "Software\Microsoft\Windows\CurrentVersion\Uninstall\MediaTalk" "Publisher" "MediaTalk"
    WriteRegStr HKCU "Software\Microsoft\Windows\CurrentVersion\Uninstall\MediaTalk" "InstallLocation" "$INSTDIR"
    WriteRegStr HKCU "Software\Microsoft\Windows\CurrentVersion\Uninstall\MediaTalk" "UninstallString" '"$INSTDIR\Uninstall.exe"'
    WriteRegStr HKCU "Software\Microsoft\Windows\CurrentVersion\Uninstall\MediaTalk" "QuietUninstallString" '"$INSTDIR\Uninstall.exe" /S'
    WriteRegDWORD HKCU "Software\Microsoft\Windows\CurrentVersion\Uninstall\MediaTalk" "NoModify" 1
    WriteRegDWORD HKCU "Software\Microsoft\Windows\CurrentVersion\Uninstall\MediaTalk" "NoRepair" 1

    CreateDirectory "$SMPROGRAMS\MediaTalk"
    CreateShortCut "$SMPROGRAMS\MediaTalk\MediaTalk.lnk" "$INSTDIR\media_talk_rust.exe" "gui" "$INSTDIR\media_talk_rust.exe" 0
    CreateShortCut "$SMPROGRAMS\MediaTalk\Uninstall MediaTalk.lnk" "$INSTDIR\Uninstall.exe"
SectionEnd

Section "Uninstall"
    SetShellVarContext current
    Delete "$SMPROGRAMS\MediaTalk\MediaTalk.lnk"
    Delete "$SMPROGRAMS\MediaTalk\Uninstall MediaTalk.lnk"
    RMDir "$SMPROGRAMS\MediaTalk"
    DeleteRegKey HKCU "Software\Microsoft\Windows\CurrentVersion\Uninstall\MediaTalk"
    RMDir /r "$INSTDIR"
SectionEnd
```

- [ ] **Step 3: 编写安装器构建脚本**

`package-windows-installer.ps1` 完整内容，保存为 UTF-8 with BOM：

```powershell
param(
    [string]$Version = "dev",
    [string]$SourceDir = "dist\media_talk_rust-$Version-windows-x86_64",
    [string]$OutputFile = "dist\media_talk_rust-$Version-windows-x86_64-setup.exe"
)

$ErrorActionPreference = "Stop"
$source = (Resolve-Path $SourceDir).Path
foreach ($path in @("$source\media_talk_rust.exe", "$source\lib\gstreamer-1.0")) {
    if (-not (Test-Path $path)) { throw "安装源不完整: $path" }
}

$command = Get-Command makensis.exe -ErrorAction SilentlyContinue
$makensis = if ($command) { $command.Source } else {
    @("${env:ProgramFiles(x86)}\NSIS\makensis.exe", "$env:ProgramFiles\NSIS\makensis.exe") |
        Where-Object { Test-Path $_ } | Select-Object -First 1
}
if (-not $makensis) { throw "找不到 makensis.exe, 先安装 NSIS" }

$output = [IO.Path]::GetFullPath($OutputFile)
New-Item -ItemType Directory -Force (Split-Path $output) | Out-Null
$script = (Resolve-Path "scripts\windows-installer.nsi").Path
$args = @("/V4", "/DVERSION=$Version", "/DSOURCE_DIR=$source",
          "/DOUTPUT_FILE=$output", $script)
& $makensis $args
if ($LASTEXITCODE -ne 0) { throw "makensis 失败: exit=$LASTEXITCODE" }
if (-not (Test-Path $output)) { throw "安装器未生成: $output" }
Write-Host "==> $output"
```

- [ ] **Step 4: 生成并验收安装器**

若本机没有 NSIS，经用户同意后安装：`choco install nsis --yes --no-progress`。

Run:

```powershell
powershell -ExecutionPolicy Bypass -File scripts/package-windows.ps1 -Version dev
powershell -ExecutionPolicy Bypass -File scripts/package-windows-installer.ps1 -Version dev
powershell -ExecutionPolicy Bypass -File scripts/verify-windows-installer.ps1 `
  -SetupPath dist/media_talk_rust-dev-windows-x86_64-setup.exe
```

Expected: `makensis` exit 0；真实安装、`--version`、卸载和 marker 保留全部输出 OK。若本机已有 MediaTalk 安装则不要覆盖，转到 CI 验收。

- [ ] **Step 5: 提交**

```bash
git add scripts/windows-installer.nsi scripts/package-windows-installer.ps1 scripts/verify-windows-installer.ps1
git commit -m "[新增] Windows NSIS 当前用户安装器"
```

### Task 3: 发布流接入 setup.exe 与文档

**Files:**
- Modify: `.github/workflows/release.yml:90-133`
- Modify: `docs/cross-build.md:266-286`
- Test: `.github/workflows/release.yml`

**Interfaces:**
- Consumes: Task 2 的 `package-windows-installer.ps1` 和 `verify-windows-installer.ps1`。
- Produces: 同一 Windows Artifact 内含 ZIP 与 setup.exe；tag Release 自动附带二者。

- [ ] **Step 1: 修改 workflow，让上传路径尚未包含安装器时检查失败**

先运行静态断言，确认当前发布流还没有安装器步骤：

```bash
python -c 'import yaml; d=yaml.safe_load(open(".github/workflows/release.yml", encoding="utf-8")); s=str(d["jobs"]["windows"]["steps"]); assert "package-windows-installer.ps1" not in s'
```

Expected: exit 0，证明测试能区分接入前后。实现后同一断言应改为检查步骤存在和上传路径包含 `dist/*-setup.exe`。

- [ ] **Step 2: 接入 NSIS 构建和验收**

在现有 `Package` 步骤后、`upload-artifact` 前加入：

```yaml
      - name: Install NSIS
        run: choco install nsis --yes --no-progress

      - name: Build installer (setup.exe)
        shell: pwsh
        run: scripts/package-windows-installer.ps1 -Version "${{ needs.version.outputs.version }}"

      - name: Verify installer (install + launch + uninstall)
        shell: pwsh
        run: scripts/verify-windows-installer.ps1 -SetupPath "dist/media_talk_rust-${{ needs.version.outputs.version }}-windows-x86_64-setup.exe"
```

把上传步骤改为：

```yaml
      - uses: actions/upload-artifact@v4
        with:
          name: media_talk_rust-${{ needs.version.outputs.version }}-windows-x86_64
          path: |
            dist/*.zip
            dist/*-setup.exe
          if-no-files-found: error
```

Release job 的 `gh release create "$GITHUB_REF_NAME" dist/*` 保持不变，因此自动上传两个 Windows 文件。

- [ ] **Step 3: 更新打包文档**

把 Windows 小节补成以下明确说明，并更新 GitHub Actions 产物列表：

````markdown
绿色 ZIP 与安装版共用同一份运行库目录。安装 NSIS 后生成当前用户安装版：

```powershell
powershell -File scripts/package-windows-installer.ps1 -Version 1.2.3
```

输出 `media_talk_rust-1.2.3-windows-x86_64-setup.exe`。默认安装到
`%LOCALAPPDATA%\Programs\MediaTalk`，无需管理员权限；开始菜单直接启动 GUI。
卸载保留 `%APPDATA%\media_talk`。CI 使用
`scripts/verify-windows-installer.ps1` 做真实安装、启动和卸载验收。
````

GitHub Actions 小节同时列出：

```text
media_talk_rust-<ver>-windows-x86_64.zip
media_talk_rust-<ver>-windows-x86_64-setup.exe
```

- [ ] **Step 4: 本地静态验证**

Run:

```bash
bash -n scripts/package-rk3566.sh
python -c 'import yaml; d=yaml.safe_load(open(".github/workflows/release.yml", encoding="utf-8")); s=str(d["jobs"]["windows"]["steps"]); assert "package-windows-installer.ps1" in s; assert "verify-windows-installer.ps1" in s; assert "dist/*-setup.exe" in s; print("release installer wiring OK")'
git diff --check
```

Expected: 全部 exit 0，YAML 可解析且包含构建、验收、上传三项。

- [ ] **Step 5: 提交**

```bash
git add .github/workflows/release.yml docs/cross-build.md
git commit -m "[新增] 发布 Windows setup.exe 并验收安装卸载"
```

### Task 4: 端到端发布验证

**Files:**
- Verify: `.github/workflows/release.yml`
- Verify: 下载后的 `media_talk_rust-dev-windows-x86_64-setup.exe`
- Verify: 下载后的 `media_talk_rust-dev-windows-x86_64.zip`

**Interfaces:**
- Consumes: Task 1-3 全部提交。
- Produces: 可交付的 ZIP 与 setup.exe；不改变产物接口。

- [ ] **Step 1: 推送已审核实现并触发 CI**

经用户确认推送后执行：

```bash
git push origin master
gh workflow run release.yml --ref master -f version=dev
```

记录新 run id 和 head SHA，确保运行的是最终实现提交，而不是旧 run。

- [ ] **Step 2: 监听并检查每个 Windows 步骤**

```bash
gh run watch <run-id> --exit-status --interval 60
```

Expected: RK3566 与 Windows 均 success；Windows 日志必须显示 NSIS 编译成功、安装后 `--version`、卸载成功和用户数据 marker 保留。

- [ ] **Step 3: 下载最终 Artifact 并检查文件**

```bash
gh run download <run-id> -n media_talk_rust-dev-windows-x86_64 -D dist/ci-installer-check
```

Expected: 同一目录存在 ZIP 和 setup.exe；setup.exe 文件头以 `MZ` 开头且非零大小。用哈希记录两个文件，避免误验旧的 `dev` 产物。

- [ ] **Step 4: 人工验收图形安装流程**

在 Windows 运行下载的 setup.exe，确认：无管理员提权、默认目录正确、开始菜单 `MediaTalk` 可直接打开 GUI、无额外控制台窗口。再从开始菜单卸载，确认 `%APPDATA%\media_talk\media_talk.db` 保留。

Expected: 全部符合设计。若代码签名缺失导致 SmartScreen 提示，记录为已知的未签名行为，不将其误报为签名验证通过。

- [ ] **Step 5: 汇总交付**

汇报 CI URL、最终提交 SHA、ZIP/setup.exe 大小和哈希、自动与人工验收结果；若未做人工 GUI 验收，明确标注该项未验证，不得宣称全部完成。
