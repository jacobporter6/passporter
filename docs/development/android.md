---
description: Setting up the toolchain for Passporter's Android app. One script installs a JDK, the Android SDK and NDK, the Rust Android targets and the Tauri CLI under your user, at pinned versions.
sidebar:
  order: 2
sources:
  - deploy/android/setup.sh
  - mise.toml
  - justfile
---

# Android toolchain

The Android app (planned, [design §3](../design.md)) is a Tauri 2 shell
around the PWA. Building it needs a JDK, the Android SDK and NDK, the Rust
Android targets and the Tauri CLI. One script installs all of them under
your user, with no sudo and no Android Studio.

## Set up from scratch

You need `mise`, `rustup`, `curl`, `unzip` and `sha256sum`, and about 3.5 GB
of disk. From the repo checkout:

```sh
mise trust          # once, so mise reads the repo's mise.toml
just android-setup  # or: deploy/android/setup.sh
```

The script accepts Google's
[Android SDK licence](https://developer.android.com/studio/terms) on your
behalf, so read it first. It is safe to re-run: each step checks what is
already there, and a second run changes nothing.

With mise activated in your shell, `cd` into the repo sets `ANDROID_HOME`,
`NDK_HOME` and `JAVA_HOME` and puts `java` and `cargo-tauri` on `PATH`.
Without activation, load them into the current shell:

```sh
eval "$(mise env --shell bash)"
cargo tauri --version
```

## What it installs, and where

| What | Version | Where | Pinned in |
| --- | --- | --- | --- |
| JDK (Temurin) | 21 | `~/.local/share/mise/installs/java/` (`JAVA_HOME`) | `mise.toml` |
| Tauri CLI (`cargo-tauri`) | 2.12.4 | `~/.local/share/mise/installs/` | `mise.toml` |
| Android command-line tools | 21.0, SHA-256 checked | `~/.local/share/android-sdk/cmdline-tools/latest` | `setup.sh` |
| `platform-tools` (adb) | latest at install | `~/.local/share/android-sdk/platform-tools` | `setup.sh` |
| SDK platform | `android-37.0` | `~/.local/share/android-sdk/platforms` | `setup.sh` |
| Build tools | 37.0.0 | `~/.local/share/android-sdk/build-tools` | `setup.sh` |
| NDK | r29 (29.0.14206865) | `~/.local/share/android-sdk/ndk/` (`NDK_HOME`) | `setup.sh`, `mise.toml` |
| Rust targets | `aarch64`, `armv7`, `i686`, `x86_64` Android | rustup, for the toolchain in `rust-toolchain.toml` | `setup.sh` |

`ANDROID_HOME` is `~/.local/share/android-sdk`, set in `mise.toml`'s
`[env]`. The NDK version appears in both `setup.sh` and `mise.toml`; the
script stops if they disagree. The Rust targets go through rustup rather
than `rust-toolchain.toml`, so people who never build the app don't
download them.

Gradle and the Android Gradle plugin are not installed here: the app's
Gradle wrapper fetches them into `~/.gradle` on its first build. A debug
build also creates the debug signing key `~/.android/debug.keystore`.

## Upgrading

Change the pins and re-run the script. For a new NDK, change `NDK_VERSION`
in `deploy/android/setup.sh` and `NDK_HOME` in `mise.toml` together. For
new command-line tools, take the build number and SHA-1 from Google's
[repository manifest](https://dl.google.com/android/repository/repository2-3.xml),
check the download against the SHA-1, and pin its SHA-256.

## Uninstall

```sh
rm -rf ~/.local/share/android-sdk
mise uninstall java github:tauri-apps/tauri   # then drop both lines from mise.toml
rustup target remove aarch64-linux-android armv7-linux-androideabi i686-linux-android x86_64-linux-android
```

Builds may also have left `~/.gradle` (Gradle caches) and `~/.android`
(sdkmanager settings and the debug keystore). Remove them too if nothing
else on the machine uses them.
