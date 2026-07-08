fn main() {
    #[cfg(all(target_os = "linux", feature = "hw-decode"))]
    {
        let header = "/usr/include/rockchip/rk_mpi.h";
        println!("MPP bindgen stub: would generate bindings from {}", header);
    }
    #[cfg(not(all(target_os = "linux", feature = "hw-decode")))]
    {
        eprintln!("mpp-bindgen requires target_os = \"linux\" and feature = \"hw-decode\"");
        std::process::exit(1);
    }
}
