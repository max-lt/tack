// The prebuilt onnxruntime (ort, via fastembed) uses `@available`, which
// calls ___isPlatformVersionAtLeast from clang's runtime. rustc does not link
// that runtime; a release build with LTO drops the calling code, a plain
// `cargo install` does not and fails to link.
fn main() {
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("macos") {
        return;
    }

    let Ok(out) = std::process::Command::new("clang")
        .arg("-print-resource-dir")
        .output()
    else {
        return;
    };

    let dir = String::from_utf8_lossy(&out.stdout).trim().to_string();
    let lib = std::path::Path::new(&dir).join("lib/darwin/libclang_rt.osx.a");

    if lib.exists() {
        println!("cargo:rustc-link-arg={}", lib.display());
    }
}
