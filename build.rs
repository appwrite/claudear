use std::path::Path;
use std::process::Command;

fn main() {
    let dashboard_dist = Path::new("dashboard/dist");

    // Ensure the directory exists so rust-embed doesn't fail at compile time.
    // When building without a pre-built dashboard, this creates an empty dir
    // and the binary will run in API-only mode.
    if !dashboard_dist.exists() {
        std::fs::create_dir_all(dashboard_dist).expect("Failed to create dashboard/dist directory");
        println!("cargo:warning=dashboard/dist not found — created empty directory. Dashboard will not be embedded.");
    } else if !dashboard_dist.join("index.html").exists() {
        println!(
            "cargo:warning=dashboard/dist/index.html not found. Dashboard will not be embedded."
        );
    }

    println!("cargo:rerun-if-changed=dashboard/dist");

    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("macos") {
        link_clang_runtime();
    }
}

// onnxruntime's CoreML provider calls `@available`, which resolves to
// `__isPlatformVersionAtLeast` in clang's builtins. rustc links with
// `-nodefaultlibs`, so the builtins archive has to be named explicitly.
fn link_clang_runtime() {
    let Ok(output) = Command::new("clang").arg("--print-runtime-dir").output() else {
        return;
    };
    let directory = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if !output.status.success() || !Path::new(&directory).join("libclang_rt.osx.a").exists() {
        return;
    }
    println!("cargo:rustc-link-search=native={directory}");
    println!("cargo:rustc-link-lib=static=clang_rt.osx");
}
