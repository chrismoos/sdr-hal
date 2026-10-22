use std::env;
use std::path::PathBuf;

fn main() {
    // docs.rs has no native SDR libraries, so it documents pregenerated bindings.
    if env::var_os("DOCS_RS").is_some() {
        let out_dir = PathBuf::from(env::var_os("OUT_DIR").unwrap());
        std::fs::copy("docs-rs-bindings.rs", out_dir.join("bindgen.rs"))
            .expect("Failed to copy docs.rs bindings");
        return;
    }
    let lib =
        pkg_config::probe_library("LimeSuite").expect("Could not find LimeSuite via pkg-config");

    let include_path = lib
        .include_paths
        .first()
        .expect("no include path for LimeSuite headers");

    let header = include_path.join("lime").join("LimeSuite.h");
    if !header.exists() {
        let alt = include_path.join("LimeSuite.h");
        if alt.exists() {
            generate_bindings(&alt, include_path);
            return;
        }
        panic!("Could not find LimeSuite.h at {:?} or {:?}", header, alt);
    }
    generate_bindings(&header, include_path);
}

fn generate_bindings(header: &std::path::Path, include_path: &std::path::Path) {
    let out_dir = PathBuf::from(env::var_os("OUT_DIR").unwrap());
    let out_path = out_dir.join("bindgen.rs");

    let bindings = bindgen::builder()
        .header(header.to_string_lossy().to_string())
        .clang_arg(format!("-I{}", include_path.to_string_lossy()))
        .allowlist_function("^LMS_.+")
        .allowlist_type("^lms_.+")
        .allowlist_var("^LMS_.+")
        .default_enum_style(bindgen::EnumVariation::ModuleConsts)
        .generate()
        .expect("Failed to generate LimeSuite bindings");

    bindings
        .write_to_file(out_path)
        .expect("Failed to write bindings to file");
}
