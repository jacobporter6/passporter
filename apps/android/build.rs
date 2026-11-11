//! Runs Tauri's build step for Android builds only. On the host the crate
//! is its plain-Rust core (see `Cargo.toml`), with nothing to generate.

fn main() {
    if std::env::var("CARGO_CFG_TARGET_OS").is_ok_and(|os| os == "android") {
        tauri_build::build();
    }
}
