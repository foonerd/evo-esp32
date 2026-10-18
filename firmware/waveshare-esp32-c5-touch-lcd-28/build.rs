// Copyright (c) 2026 Just a Nerd
// SPDX-License-Identifier: BUSL-1.1

//! ESP-IDF's build environment for the linker, and the catalogue compiled
//! in: `catalogue.toml` becomes `catalogue()` in `OUT_DIR`.

fn main() {
    embuild::espidf::sysenv::output();
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
