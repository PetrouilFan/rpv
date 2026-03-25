fn main() {
    // Link FFmpeg libraries via pkg-config
    for lib in &["libavcodec", "libavutil", "libswscale"] {
        if let Ok(lib) = pkg_config::Config::new().probe(lib) {
            for path in &lib.include_paths {
                println!("cargo:include={}", path.display());
            }
        }
    }
}
