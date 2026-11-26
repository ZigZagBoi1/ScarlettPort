//! The Agility SDK opt-in (`D3D12SDKVersion`, `D3D12SDKPath` in `main.rs`) only works when the
//! executable exports both. lld-link exports `#[no_mangle]` statics of a program by itself;
//! MSVC's link.exe does not, and without them Windows 10 uses its inbox D3D12, which refuses the
//! root signatures the translator makes (heap directly indexed). Named here for every linker.
fn main() {
    let target = std::env::var("TARGET").unwrap_or_default();
    if target.contains("windows-msvc") {
        println!("cargo:rustc-link-arg-bins=/EXPORT:D3D12SDKVersion,DATA");
        println!("cargo:rustc-link-arg-bins=/EXPORT:D3D12SDKPath,DATA");
    }
}
