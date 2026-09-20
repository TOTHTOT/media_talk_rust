pub fn list_devices() {
    #[cfg(target_os = "linux")]
    {
        match ipcam_alsa::enumerate_capture_devices() {
            Ok(list) => {
                if list.is_empty() {
                    println!("(no ALSA capture devices found)");
                } else {
                    for d in list {
                        println!("{}  ({})", d.id, d.name);
                    }
                }
            }
            Err(e) => {
                eprintln!("ALSA enumeration failed: {e}");
                std::process::exit(1);
            }
        }
    }
    #[cfg(not(target_os = "linux"))]
    {
        println!("ALSA enumeration is only supported on Linux.");
    }
}
