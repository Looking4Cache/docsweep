//! Exposes the Bundle ID from `tauri.conf.json`, its single source, as a constant.

use std::fs;

fn main() {
    println!("cargo:rerun-if-changed=tauri.conf.json");
    let raw = fs::read_to_string("tauri.conf.json").expect("tauri.conf.json must be readable");
    let config: serde_json::Value =
        serde_json::from_str(&raw).expect("tauri.conf.json must be valid JSON");
    let identifier = config["identifier"]
        .as_str()
        .expect("tauri.conf.json must define an identifier");
    println!("cargo:rustc-env=DOCSWEEP_BUNDLE_ID={identifier}");

    tauri_build::build();
}
