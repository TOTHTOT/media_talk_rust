use std::path::Path;
use std::process::Command;

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
        let ok = Command::new("curl")
            .args(["-fsSL", "--max-time", "30", "-o"])
            .arg(&file)
            .arg(&url)
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
        assert!(ok, "下载图标失败: {url} (离线构建请先手动放 {name}.svg 到 ui/icons/)");
        println!("cargo:warning=fetched icon {name}.svg @ lucide-static {version}");
    }
}

fn main() {
    fetch_icons();
    slint_build::compile("ui/main.slint").expect("slint compile failed");
}
