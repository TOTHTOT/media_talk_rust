use std::path::{Path, PathBuf};
use std::process::Command;

fn download(url: &str, dest: &Path, hint: &str) {
    let ok = Command::new("curl")
        .args(["-fsSL", "--max-time", "60", "-o"])
        .arg(dest)
        .arg(url)
        .status()
        .map(|s| s.success())
        .unwrap_or(false);
    assert!(ok, "download failed: {url} ({hint})");
}

/// 按 ui/icons.txt 清单把 lucide 图标下载到 ui/icons/ (已存在则跳过).
/// 清单第一行是 lucide-static 版本号, 其余每行一个图标名.
/// 图标不入库, 首次构建或加新图标时需要联网; jsdelivr 不通可换镜像源.
fn fetch_icons() {
    let manifest = Path::new("ui/icons.txt");
    println!("cargo:rerun-if-changed={}", manifest.display());
    let text = std::fs::read_to_string(manifest).expect("read ui/icons.txt failed");
    let mut lines = text
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with('#'));
    let version = lines
        .next()
        .expect("first line of icons.txt must be the lucide-static version");
    let dir = Path::new("ui/icons");
    std::fs::create_dir_all(dir).expect("create ui/icons failed");
    for name in lines {
        let file = dir.join(format!("{name}.svg"));
        if file.exists() {
            continue;
        }
        let url = format!("https://cdn.jsdelivr.net/npm/lucide-static@{version}/icons/{name}.svg");
        download(
            &url,
            &file,
            "offline build: place icons in ui/icons/ manually",
        );
        println!("cargo:warning=fetched icon {name}.svg @ lucide-static {version}");
    }
}

/// 下载解压 Slint 官方 Material 组件集 (不入库, 已存在则跳过).
/// ui/*.slint 用相对路径 ../material-1.1.0/material.slint 引用.
fn fetch_material() {
    const VERSION: &str = "1.1.0";
    let entry = PathBuf::from(format!("material-{VERSION}/material.slint"));
    if !entry.exists() {
        let zip_path = std::env::temp_dir().join(format!("material-{VERSION}.zip"));
        if !zip_path.exists() {
            download(
                &format!("https://material.slint.dev/zip/material-{VERSION}.zip"),
                &zip_path,
                "offline build: extract the material components into the crate root manually",
            );
        }
        let file = std::fs::File::open(&zip_path).expect("open material zip failed");
        zip::ZipArchive::new(file)
            .expect("corrupt material zip")
            .extract(".")
            .expect("failed to extract material components");
        println!("cargo:warning=fetched material components {VERSION}");
    }
    patch_material_text_field(Path::new(&format!(
        "material-{VERSION}/ui/components/text_field.slint"
    )));
}

/// 给 vendored material 的 TextField 注入 set-cursor-to-end 透传函数.
/// 背景: KbTextField 上屏是程序化写 text, slint 的 TextInput 不会自动
/// 移动光标, 需在写后显式归位; 但 material 的 TextField 不透出内层
/// TextInput 的 set-selection-offsets. material 目录不入库 (构建时解压),
/// 手改本地副本会在干净机器上丢失 → 这里在解压后打补丁, 幂等 (已注入
/// 则跳过), 锚点是 key_released 回调声明 (material 1.1.0 实测).
fn patch_material_text_field(path: &Path) {
    println!("cargo:rerun-if-changed={}", path.display());
    let Ok(src) = std::fs::read_to_string(path) else {
        println!("cargo:warning=material text_field.slint not found, skip cursor patch");
        return;
    };
    if src.contains("set-cursor-to-end") {
        return; // 已注入 (幂等)
    }
    let anchor = "    callback key_released(event: KeyEvent) -> EventResult;";
    let Some(_idx) = src.find(anchor) else {
        println!(
            "cargo:warning=material TextField anchor missing (version drift?), cursor patch not applied"
        );
        return;
    };
    let patch = format!(
        "{anchor}\n\n    // 屏幕键盘 (KbTextField) 追加式上屏后调用: 程序化写 text 不移动光标,\n    // 需显式移到末尾. 透传内层 TextInput 的 set-selection-offsets (字节\n    // 偏移; slint 字符串无 length 内建, 用 character-count*4+4 作上界 —\n    // UTF-8 每字符至多 4 字节 — 实现内 safe_byte_offset 会钳到串尾).\n    public function set-cursor-to-end() {{\n        input.set-selection-offsets(root.text.character-count * 4 + 4, root.text.character-count * 4 + 4);\n    }}"
    );
    let patched = src.replacen(anchor, &patch, 1);
    if std::fs::write(path, patched).is_err() {
        println!("cargo:warning=failed to write cursor patch");
    }
}

fn main() {
    fetch_icons();
    fetch_material();
    // i18n: 打包 lang/<语言>/LC_MESSAGES/call-ui.po, 运行时按系统 locale 自动选择,
    // UI 侧可用 slint::select_bundled_translation 切换; 文案以中文为源串, zh.po 留空做回退
    for po in [
        "lang/zh/LC_MESSAGES/call-ui.po",
        "lang/en/LC_MESSAGES/call-ui.po",
    ] {
        println!("cargo:rerun-if-changed={po}");
    }
    let config = slint_build::CompilerConfiguration::new()
        .with_bundled_translations("lang")
        // 不用默认 msgctxt (组件名), po 里不写 context, 同串合并翻译
        .with_default_translation_context(slint_build::DefaultTranslationContext::None);
    slint_build::compile_with_config("ui/main.slint", config).expect("slint compile failed");
}
