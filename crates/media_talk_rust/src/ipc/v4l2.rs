pub fn list() {
    #[cfg(target_os = "linux")]
    {
        match v4l2_device_cap::list_capture_devices() {
            Ok(list) => {
                if list.is_empty() {
                    println!("(no V4L2 capture devices found)");
                } else {
                    for d in list {
                        println!(
                            "{} driver={} bus={:?} formats={}",
                            d.path,
                            d.driver,
                            d.bus_info,
                            d.formats.len()
                        );
                        for f in &d.formats {
                            println!("    fmt={:?} sizes={:?}", f.pixel_format, f.sizes);
                        }
                    }
                }
            }
            Err(e) => {
                eprintln!("V4L2 enumeration failed: {e}");
                std::process::exit(1);
            }
        }
    }
    #[cfg(not(target_os = "linux"))]
    {
        println!("V4L2 enumeration is only supported on Linux.");
    }
}
