//! Tauri's plugin build step, for Android builds only: it generates the
//! command permissions and hands the Kotlin library (`android/`) to the
//! app's Gradle project. On the host there is nothing to build.

/// The commands the PWA may call (`permissions/default.toml` grants them).
/// The shell's own call, `webviewFeatures`, is not one: without a
/// permission, no page can reach it.
const COMMANDS: &[&str] = &["status", "pair", "session", "forget"];

fn main() {
    if std::env::var("CARGO_CFG_TARGET_OS").is_ok_and(|os| os == "android") {
        tauri_plugin::Builder::new(COMMANDS)
            .android_path("android")
            .build();
    }
}
