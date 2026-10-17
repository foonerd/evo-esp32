// Copyright (c) 2026 Just a Nerd
// SPDX-License-Identifier: BUSL-1.1

//! `evo-esp32-catalogue check <catalogue.toml>` validates and summarises.
//! `evo-esp32-catalogue emit <catalogue.toml>` prints the Rust form.

use std::process::ExitCode;

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let (verb, path) = match args.as_slice() {
        [verb, path] => (verb.as_str(), path.as_str()),
        _ => {
            eprintln!("usage: evo-esp32-catalogue check|emit <catalogue.toml>");
            return ExitCode::from(2);
        }
    };
    let catalogue = match evo_esp32_catalogue::load(path) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("{e}");
            return ExitCode::FAILURE;
        }
    };
    match verb {
        "check" => {
            for r in &catalogue.racks {
                println!(
                    "rack {} ({:?}, {:?}), {} shelves",
                    r.name,
                    r.family,
                    r.kinds,
                    r.shelves.len()
                );
                for s in &r.shelves {
                    println!(
                        "  shelf {} shape {}  {}",
                        s.name, s.shape, s.description
                    );
                }
            }
            ExitCode::SUCCESS
        }
        "emit" => {
            print!("{}", evo_esp32_catalogue::emit_rust(&catalogue));
            ExitCode::SUCCESS
        }
        _ => {
            eprintln!("unknown verb `{verb}`; use check or emit");
            ExitCode::from(2)
        }
    }
}
