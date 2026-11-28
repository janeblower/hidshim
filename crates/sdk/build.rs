use std::path::PathBuf;

fn main() {
    println!("cargo:rerun-if-env-changed=HIDSHIM_DRIVER_DIR");
    if std::env::var_os("CARGO_FEATURE_INSTALL").is_none() {
        return;
    }
    // Outside this repository, as in a git dependency, point at an unpacked
    // hidshim-drivers.zip, or hidshim-drivers-arm64.zip for ARM64.
    let folder = match std::env::var("CARGO_CFG_TARGET_ARCH").as_deref() {
        Ok("aarch64") => "xusb-arm64",
        _ => "xusb",
    };
    let dir = std::env::var_os("HIDSHIM_DRIVER_DIR").map_or_else(
        || {
            PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                .join("../../target/drivers")
                .join(folder)
        },
        PathBuf::from,
    );
    for file in [
        "hidshimxusb.dll",
        "hidshimxusb.inf",
        "hidshimkbdctl.dll",
        "hidshimkbdctl.inf",
        "hidshimhid.dll",
        "hidshimhid.inf",
    ] {
        let path = dir.join(file);
        println!("cargo:rerun-if-changed={}", path.display());
        if !path.exists() {
            panic!(
                "{} is missing: run drivers/build.ps1 first, or set HIDSHIM_DRIVER_DIR",
                path.display()
            );
        }
    }
    println!(
        "cargo:rustc-env=HIDSHIM_DRIVER_DIR={}",
        dir.canonicalize().unwrap().display()
    );
}
