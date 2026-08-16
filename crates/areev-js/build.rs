fn main() {
    napi_build::setup();
    // macOS: napi cdylibs resolve Node-API (`napi_*`) symbols at load time from
    // the host node process — mirror crates/areev-py/build.rs so a bare
    // `cargo build` links cleanly too (napi build normally injects this itself).
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("macos") {
        println!("cargo:rustc-link-arg=-undefined");
        println!("cargo:rustc-link-arg=dynamic_lookup");
    }
}
