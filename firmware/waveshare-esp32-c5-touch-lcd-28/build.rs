// Copyright (c) 2026 Just a Nerd
// SPDX-License-Identifier: BUSL-1.1

//! ESP-IDF's build environment for the linker, the catalogue compiled in
//! (`catalogue.toml` becomes `catalogue()` in `OUT_DIR`), and the build
//! id: `git describe` of the tree, stamped into the binary so the boot log
//! names the commit it was built from and `image.sh` can check the image
//! is this tree's before anyone flashes it.

fn main() {
    embuild::espidf::sysenv::output();
    println!("cargo:rerun-if-changed=catalogue.toml");
    println!("cargo:rerun-if-changed=../../.git/HEAD");
    println!("cargo:rerun-if-changed=../../.git/index");
    let id = std::process::Command::new("git")
        .args(["describe", "--always", "--dirty", "--tags"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "unknown".into());
    println!("cargo:rustc-env=EVO_BUILD_ID=build:{id}");
    let out =
        std::path::PathBuf::from(std::env::var("OUT_DIR").expect("OUT_DIR"));
    if let Err(e) = evo_esp32_catalogue::emit_file(
        "catalogue.toml",
        out.join("catalogue.rs"),
    ) {
        panic!("catalogue.toml: {e}");
    }
}
