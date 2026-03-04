use std::env;

fn main() {
    let target_os = env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
    let is_apple = matches!(target_os.as_str(), "ios" | "macos" | "tvos" | "watchos");
    if !is_apple {
        return;
    }

    cc::Build::new()
        .file("src/apple_console_shim.m")
        .flag("-fobjc-arc")
        .compile("xlog_core_apple_console");
    println!("cargo:rustc-link-lib=framework=Foundation");
}
