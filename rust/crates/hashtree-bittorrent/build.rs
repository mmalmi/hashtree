fn main() {
    println!("cargo:rerun-if-changed=native");
    #[cfg(feature = "libtorrent")]
    {
        let library = pkg_config::Config::new()
            .range_version("2.1".."2.2")
            .probe("libtorrent-rasterbar")
            .expect(
                "the opt-in bittorrent feature requires libtorrent-rasterbar 2.1.x and pkg-config",
            );
        let mut build = cc::Build::new();
        build.cpp(true).std("c++17").files([
            "native/storage.cpp",
            "native/metadata.cpp",
            "native/session.cpp",
        ]);
        if library.defines.contains_key("TORRENT_USE_OPENSSL") {
            let openssl = pkg_config::Config::new()
                .cargo_metadata(false)
                .probe("openssl")
                .expect("this libtorrent build requires OpenSSL development headers");
            for include in openssl.include_paths {
                build.include(include);
            }
        }
        for include in library.include_paths {
            build.include(include);
        }
        for (key, value) in library.defines {
            build.define(&key, value.as_deref());
        }
        println!("cargo:rerun-if-env-changed=BOOST_ROOT");
        let boost = std::env::var_os("BOOST_ROOT")
            .map(std::path::PathBuf::from)
            .or_else(|| {
                if std::env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("macos") {
                    return None;
                }
                let output = std::process::Command::new("brew")
                    .args(["--prefix", "boost"])
                    .output()
                    .ok()?;
                output.status.success().then(|| {
                    std::path::PathBuf::from(String::from_utf8_lossy(&output.stdout).trim())
                })
            });
        if let Some(boost) = boost {
            build.include(boost.join("include"));
        }
        build.warnings(true).compile("hashtree_bittorrent");
    }
}
