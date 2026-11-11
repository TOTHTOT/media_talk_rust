// libnice 链接: Linux 走默认搜索路径 (-lnice, libnice-dev 提供);
// Windows 借 gstreamer msvc 捆绑包的 nice.lib, 用 pkg-config 问出 libdir.
fn main() {
    println!("cargo:rerun-if-env-changed=PKG_CONFIG_PATH");
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
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
