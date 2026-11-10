// SPDX-License-Identifier: Apache-2.0 AND MIT
// x264 bindings derived from voxl's build.rs (MIT, Copyright (c) 2026 Tembo).
//
// 1. Generates Rust bindings for x264.h (bindgen, needs libclang).
// 2. Links x264 and libyuv.
// 3. Generates include/guac_extra_video.h from the C API (cbindgen).

use std::path::PathBuf;

fn main() {
    let dir = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").unwrap());
    let out = PathBuf::from(std::env::var("OUT_DIR").unwrap());

    let x264 = pkg_config::probe_library("x264").expect("x264 (install libx264-dev)");
    // libyuv ships no .pc file on Debian/Ubuntu: link it from the default path.
    if pkg_config::probe_library("libyuv").is_err() {
        println!("cargo:rustc-link-lib=yuv");
    }

    let mut b = bindgen::Builder::default()
        .header_contents("wrap.h", "#include <stdint.h>\n#include <x264.h>\n")
        .allowlist_function("x264_.*")
        .allowlist_type("x264_.*")
        .allowlist_var("X264_.*")
        .layout_tests(false);
    for p in &x264.include_paths {
        b = b.clang_arg(format!("-I{}", p.display()));
    }
    let mut code = b.generate().expect("bindgen x264").to_string();
    // x264.h renames x264_encoder_open after its build number (a macro bindgen
    // can't follow); alias whichever version this x264 has.
    let open = code
        .split(|c: char| !(c.is_alphanumeric() || c == '_'))
        .find(|w| {
            w.strip_prefix("x264_encoder_open_")
                .is_some_and(|n| n.chars().all(|c| c.is_ascii_digit()))
        })
        .expect("x264_encoder_open_<build> in x264.h")
        .to_string();
    code.push_str(&format!("\npub use {open} as x264_encoder_open;\n"));
    std::fs::write(out.join("x264.rs"), code).unwrap();

    let config = cbindgen::Config::from_file(dir.join("cbindgen.toml")).unwrap();
    cbindgen::Builder::new()
        .with_crate(&dir)
        .with_config(config)
        .generate()
        .expect("cbindgen failed")
        .write_to_file(dir.join("include/guac_extra_video.h"));

    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=src");
    println!("cargo:rerun-if-changed=cbindgen.toml");
    // Regenerate the header if it is missing (it is never copied into builds)
    println!("cargo:rerun-if-changed=include/guac_extra_video.h");
}
