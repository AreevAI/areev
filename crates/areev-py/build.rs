fn main() {
    // macOS Python extension modules resolve Py* symbols at import time
    // from the host interpreter (what maturin does automatically).
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("macos") {
        println!("cargo:rustc-link-arg=-undefined");
        println!("cargo:rustc-link-arg=dynamic_lookup");
    }
}
