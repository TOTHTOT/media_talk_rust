// libnice 链接: Linux 走默认搜索路径 (-lnice, libnice-dev 提供);
// Windows 借 gstreamer msvc 捆绑包的 nice.lib; macOS 走 Homebrew keg-only
// 目录. Windows/macOS 都用 pkg-config 问出 libdir, 避免链接器找不到 -lnice.
fn main() {
    println!("cargo:rerun-if-env-changed=PKG_CONFIG_PATH");
    let target_os = std::env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
    if matches!(target_os.as_str(), "windows" | "macos") {
        let out = std::process::Command::new("pkg-config")
            .args(["--variable=libdir", "nice"])
            .output();
        if let Ok(out) = out {
            if out.status.success() {
                let dir = String::from_utf8_lossy(&out.stdout);
                println!("cargo:rustc-link-search=native={}", dir.trim());
            }
        }
    }
}
