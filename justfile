# Passporter task runner. `just` lists recipes.

set shell := ["bash", "-euo", "pipefail", "-c"]

# Crates that must also compile for the browser (added as they land: protocol, web).
wasm_crates := "protocol web"

default:
    @just --list

# Format check, lints (host + wasm), tests. Must pass before you open a PR.
verify: fmt-check lint test

# The full gate (CI runs this): verify + release build + PWA size budget + supply-chain audit,
# plus the Android checks where the Android toolchain is installed.
gate: verify build web-size audit android-gate

fmt:
    cargo fmt --all

fmt-check:
    cargo fmt --all --check

lint: lint-host lint-wasm

lint-host:
    cargo clippy --workspace --all-targets --all-features --locked -- -D warnings

lint-wasm:
    #!/usr/bin/env bash
    set -euo pipefail
    for crate in {{wasm_crates}}; do
        cargo clippy --package "$crate" --all-targets --locked --target wasm32-unknown-unknown -- -D warnings
    done

test:
    cargo nextest run --workspace --all-features --locked --no-tests=warn

build:
    cargo build --workspace --release --locked

audit:
    cargo deny --locked check

# Release build of the PWA into crates/web/dist (trunk runs wasm-bindgen and wasm-opt).
# Panic locations name dependency and std sources by absolute path; mapping the
# cargo home and the toolchain to fixed prefixes makes the WASM, and so the size
# budget (`web-size`), the same on every machine.
web-build:
    #!/usr/bin/env bash
    set -euo pipefail
    remap=$(just _web-remap)
    eval "$remap"
    cd crates/web && trunk build --release --locked

# Fail if the PWA's gzipped WASM is over budget (400 KiB).
web-size: web-build
    crates/web/scripts/check-size.sh crates/web/dist

# Serve the PWA on 127.0.0.1:8080 with the production CSP (no autoreload).
web-serve:
    #!/usr/bin/env bash
    set -euo pipefail
    remap=$(just _web-remap)
    eval "$remap"
    cd crates/web && trunk serve --release --locked

# The path remap `web-build` and `web-serve` share, as an `export` line.
# Cargo ignores per-target flags when RUSTFLAGS is set, so that is refused.
_web-remap:
    #!/usr/bin/env bash
    set -euo pipefail
    if [[ -n "${RUSTFLAGS:-}${CARGO_ENCODED_RUSTFLAGS:-}" ]]; then
        echo "web: unset RUSTFLAGS: it would drop the path remap and change the WASM sizes" >&2
        exit 1
    fi
    printf 'export CARGO_TARGET_WASM32_UNKNOWN_UNKNOWN_RUSTFLAGS=%q\n' \
        "--remap-path-prefix=${CARGO_HOME:-$HOME/.cargo}=/cargo --remap-path-prefix=$(rustc --print sysroot)=/rustc"

# Documentation site (docs-site/, needs Node 22+): tests, build, link and render checks. Not part of `gate`.
docs:
    cd docs-site && npm ci && npm run check

# Opt-in headless browser smoke tests (docs/testing.md); not part of `gate`.
# Skips with a message when no Chromium is installed.
e2e:
    #!/usr/bin/env bash
    set -euo pipefail
    if [[ -z "${PASSPORTER_CHROMIUM:-}" ]] && ! command -v chromium chromium-browser google-chrome-stable google-chrome chrome >/dev/null 2>&1; then
        echo "e2e: SKIPPED: no Chromium found (install chromium, or set PASSPORTER_CHROMIUM)"
        exit 0
    fi
    just web-build
    cargo build --locked --package bridge --bin passporter-bridge
    PASSPORTER_BRIDGE_BIN="$PWD/target/debug/passporter-bridge" \
    PASSPORTER_WEB_DIST="$PWD/crates/web/dist" \
        cargo nextest run --locked --package e2e --run-ignored only --no-capture

# Record the demo video into target/demo (e2e/examples/demo.rs; needs Chromium and ffmpeg).
demo: web-build
    cargo build --locked --package bridge --bin passporter-bridge
    cargo run --locked --package e2e --example demo

# Install the Android toolchain under your user: JDK, SDK, NDK, Rust targets, Tauri CLI (docs/development/android.md).
android-setup:
    deploy/android/setup.sh

# Android APK (apps/android) into target/android/: `debug` (default) or `release`, signed with the PASSPORTER_ANDROID_* env vars.
android-build profile="debug" abi="aarch64":
    #!/usr/bin/env bash
    set -euo pipefail
    export MISE_ENV=android
    eval "$(mise env --shell bash)"
    if ! command -v cargo-tauri >/dev/null || [[ ! -d "${ANDROID_HOME:-}/build-tools/37.0.0" ]]; then
        echo "android: no Android toolchain; run just android-setup (docs/development/android.md)" >&2
        exit 1
    fi
    case "{{profile}}" in
        debug) flags=(--debug) ;;
        release)
            missing=()
            for var in PASSPORTER_ANDROID_KEYSTORE PASSPORTER_ANDROID_KEYSTORE_PASSWORD \
                    PASSPORTER_ANDROID_KEY_ALIAS PASSPORTER_ANDROID_KEY_PASSWORD; do
                [[ -n "${!var:-}" ]] || missing+=("$var")
            done
            if (( ${#missing[@]} )); then
                echo "android-build release: set ${missing[*]} (docs/development/android.md, Release builds)" >&2
                exit 1
            fi
            if [[ ! -f "$PASSPORTER_ANDROID_KEYSTORE" ]]; then
                echo "android-build release: no keystore at PASSPORTER_ANDROID_KEYSTORE ($PASSPORTER_ANDROID_KEYSTORE)" >&2
                exit 1
            fi
            flags=() ;;
        *) echo "android-build: the profile is debug or release, not {{profile}}" >&2; exit 2 ;;
    esac
    cargo metadata --locked --format-version 1 >/dev/null
    out=apps/android/gen/android/app/build/outputs/apk/universal/{{profile}}
    # A fresh package: AGP rewrites an existing APK in place and leaves gaps in it.
    rm -rf "$out"
    (cd apps/android && cargo tauri android build --ci --apk --target {{abi}} "${flags[@]}")
    mkdir -p target/android
    apk=target/android/passporter-{{profile}}.apk
    cp "$out/app-universal-{{profile}}.apk" "$apk"
    "$ANDROID_HOME/build-tools/37.0.0/apksigner" verify "$apk"
    echo "$apk ($(du -h "$apk" | cut -f1))"

# Android checks: clippy for the Android target, the shell's and plugins' Rust tests, Gradle's JVM unit tests.
android-test:
    #!/usr/bin/env bash
    set -euo pipefail
    export MISE_ENV=android
    eval "$(mise env --shell bash)"
    if ! command -v cargo-tauri >/dev/null || [[ ! -d "${ANDROID_HOME:-}/build-tools/37.0.0" ]]; then
        echo "android: no Android toolchain; run just android-setup (docs/development/android.md)" >&2
        exit 1
    fi
    # The app's JVM tests compile against Kotlin files that only a Tauri build
    # generates: a fresh checkout builds the debug APK once first.
    if [[ ! -f apps/android/gen/android/app/src/main/java/dev/passporter/app/generated/TauriActivity.kt ]]; then
        just android-build
    fi
    # tauri-build writes the Gradle include files the JVM tests need when it knows the project.
    TAURI_ANDROID_PROJECT_PATH="$PWD/apps/android/gen/android" TAURI_ANDROID_PACKAGE_UNESCAPED=dev.passporter.app \
        cargo clippy --package passporter-android --all-targets --locked --target aarch64-linux-android -- -D warnings
    cargo clippy --package tauri-plugin-device --package tauri-plugin-recorder --all-targets --locked --target aarch64-linux-android -- -D warnings
    cargo nextest run --package passporter-android --package tauri-plugin-device --locked
    cd apps/android/gen/android && ./gradlew --console=plain testUniversalDebugUnitTest :tauri-plugin-device:testDebugUnitTest :tauri-plugin-recorder:testDebugUnitTest

# `android-test` where the Android toolchain is installed (`gate` runs this);
# elsewhere, CI included, skipped with a message, like `e2e` without Chromium.
android-gate:
    #!/usr/bin/env bash
    set -euo pipefail
    # Without the toolchain, mise may not even resolve the Android env.
    env=$(MISE_ENV=android mise env --shell bash 2>/dev/null) || env=""
    eval "$env"
    if ! command -v cargo-tauri >/dev/null || [[ ! -d "${ANDROID_HOME:-}/build-tools/37.0.0" ]]; then
        echo "android: SKIPPED: no Android toolchain (just android-setup, docs/development/android.md)"
        exit 0
    fi
    just android-test

# Reclaim stale build output across worktrees (see crates/xtask).
prune *args:
    cargo xtask prune {{args}}

# Shared compile-cache hit rate and size (when building with sccache).
cache-stats:
    sccache --show-stats

corpus := env_var('HOME') / ".local/share/passporter/corpus"

# Scrub raw transcript dumps into the corpus (docs/corpus.md); release build, debug takes minutes.
scrub-corpus raw=(corpus / "raw") out=(corpus / "scrubbed"):
    cargo run --release --quiet --locked -p xtask -- scrub-corpus --raw {{raw}} --out {{out}}
