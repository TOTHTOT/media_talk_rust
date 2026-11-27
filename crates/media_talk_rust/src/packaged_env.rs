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
        assert_eq!(
            discover_bundled_gstreamer(&root.join("media_talk_rust.exe")),
            None
        );
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
