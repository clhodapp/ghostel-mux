// Generate Rust declarations for libghostty-vt from the headers of the
// library this crate links, so types and constants always match it.
use std::env;
use std::path::PathBuf;

fn main() {
    let lib = pkg_config::Config::new()
        .probe("libghostty-vt")
        .expect("libghostty-vt must be findable through pkg-config");

    let mut builder = bindgen::Builder::default()
        .header("src/ghostty.h")
        .allowlist_function("ghostty_.*")
        .allowlist_type("Ghostty.*")
        .allowlist_var("GHOSTTY_.*")
        .prepend_enum_name(false)
        .parse_callbacks(Box::new(bindgen::CargoCallbacks::new()));
    for path in &lib.include_paths {
        builder = builder.clang_arg(format!("-I{}", path.display()));
    }

    let out = PathBuf::from(env::var("OUT_DIR").unwrap()).join("ghostty.rs");
    builder
        .generate()
        .expect("bindgen failed on libghostty-vt headers")
        .write_to_file(out)
        .expect("could not write bindings");
}
