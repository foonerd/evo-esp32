// Copyright (c) 2026 Just a Nerd
// SPDX-License-Identifier: BUSL-1.1

//! Compile the catalogue in: `catalogue.toml` becomes `catalogue()` in
//! `OUT_DIR`, as a firmware distribution's build does.

fn main() {
    println!("cargo:rerun-if-changed=catalogue.toml");
    let out =
        std::path::PathBuf::from(std::env::var("OUT_DIR").expect("OUT_DIR"));
    if let Err(e) = evo_esp32_catalogue::emit_file(
        "catalogue.toml",
        out.join("catalogue.rs"),
    ) {
        panic!("catalogue.toml: {e}");
    }
}
