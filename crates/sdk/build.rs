use std::path::Path;

fn main() {
    if std::env::var_os("CARGO_FEATURE_INSTALL").is_none() {
        return;
    }
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/drivers/xusb");
    for file in ["hidshimxusb.dll", "hidshimxusb.inf"] {
        let path = dir.join(file);
        println!("cargo:rerun-if-changed={}", path.display());
        if !path.exists() {
            panic!("{} is missing: run drivers/build.ps1 first", path.display());
        }
    }
    println!(
        "cargo:rustc-env=HIDSHIM_DRIVER_DIR={}",
        dir.canonicalize().unwrap().display()
    );
}
