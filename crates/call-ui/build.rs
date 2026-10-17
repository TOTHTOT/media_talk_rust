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
    assert!(ok, "下载失败: {url} ({hint})");
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
    let version = lines.next().expect("icons.txt 第一行须为 lucide-static 版本号");
    let dir = Path::new("ui/icons");
    std::fs::create_dir_all(dir).expect("create ui/icons failed");
    for name in lines {
        let file = dir.join(format!("{name}.svg"));
        if file.exists() {
            continue;
        }
        let url =
            format!("https://cdn.jsdelivr.net/npm/lucide-static@{version}/icons/{name}.svg");
        download(&url, &file, "离线构建请先手动放图标到 ui/icons/");
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
                "离线构建请先手动解压 material 组件集到 crate 根目录",
            );
        }
        let file = std::fs::File::open(&zip_path).expect("open material zip failed");
        zip::ZipArchive::new(file)
            .expect("material zip 损坏")
            .extract(".")
            .expect("解压 material 组件集失败");
        println!("cargo:warning=fetched material components {VERSION}");
    }
}

fn main() {
    fetch_icons();
    fetch_material();
    slint_build::compile("ui/main.slint").expect("slint compile failed");
}
