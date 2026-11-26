fn main() {
    println!("cargo:rustc-link-arg-cdylib=/DELAYLOAD:scarlett-probe-delayed.dll");
    println!("cargo:rustc-link-lib=delayimp");
}
