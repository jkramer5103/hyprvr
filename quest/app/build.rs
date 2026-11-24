fn main() {
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("android") {
        println!("cargo:rustc-link-lib=EGL");
        println!("cargo:rustc-link-lib=GLESv3");
        println!("cargo:rustc-link-lib=mediandk");
        println!("cargo:rustc-link-lib=nativewindow");
        println!("cargo:rustc-link-lib=aaudio");
    }
}
