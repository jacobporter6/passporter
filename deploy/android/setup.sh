#!/usr/bin/env bash
# Android toolchain for building Passporter's Tauri 2 app, under your user
# (no sudo).
#
#   deploy/android/setup.sh        # from anywhere inside the repo checkout
#
# What it installs, and where:
#   1. a JDK and the Tauri CLI (cargo-tauri), via mise, at the versions pinned
#      in mise.android.toml (mise loads it when MISE_ENV=android);
#   2. the Android command-line tools into $ANDROID_HOME/cmdline-tools/latest
#      (ANDROID_HOME is ~/.local/share/android-sdk, set by mise.android.toml),
#      checked against the SHA-256 pinned below;
#   3. the SDK packages pinned below, via sdkmanager, accepting non-interactively
#      the licences of those packages (Google's Android SDK licence);
#   4. the Rust Android targets for the toolchain pinned in rust-toolchain.toml.
#
# Safe to re-run: each step checks what is there first, so a second run
# downloads and changes nothing. docs/development/android.md says how to undo it.
set -euo pipefail

# --- Pinned versions (the JDK and the Tauri CLI are in mise.android.toml) ---
# Command-line tools 21.0 (build 15641748). The SHA-1 in Google's repository
# manifest (dl.google.com/android/repository/repository2-3.xml) matches this
# file. Not 22.0 or later: their sdkmanager hands off to a separate "Android
# CLI" that it downloads unpinned and that collects usage metrics.
CMDLINE_TOOLS_BUILD="15641748"
CMDLINE_TOOLS_SHA256="a66d5ef0238fc0162e9c1446602ce0dd41702d4dd7a94d2ce42d12b7f80baf7e"
# Tauri CLI 2.12.4's Android template compiles against SDK 37 (packaged as
# android-37.0). NDK r29 stable: Tauri's own default (29.0.13846066) is the
# r29 beta 3, and Tauri uses whatever NDK_HOME names.
# Keep NDK_VERSION in step with NDK_HOME in mise.android.toml.
# platform-tools (adb) is the one unpinned package: sdkmanager only offers the
# latest. Builds don't use it; it talks to a device.
NDK_VERSION="29.0.14206865"
SDK_PACKAGES=(
    "platform-tools"
    "platforms;android-37.0"
    "build-tools;37.0.0"
    "ndk;${NDK_VERSION}"
)
RUST_TARGETS=(
    aarch64-linux-android
    armv7-linux-androideabi
    i686-linux-android
    x86_64-linux-android
)
# -----------------------------------------------------------------------------

usage() { sed -n '2,/^set -euo/p' "${BASH_SOURCE[0]}" | sed -e '$d' -e 's/^# \{0,1\}//'; }
case "${1:-}" in
    "") ;;
    -h|--help) usage; exit 0 ;;
    *) echo "unknown option: $1 (see --help)" >&2; exit 2 ;;
esac

step() { printf '\n==> %s\n' "$*"; }
die() { echo "setup.sh: $*" >&2; exit 1; }

for tool in git mise rustup curl unzip sha256sum; do
    command -v "$tool" >/dev/null || die "$tool is required but not on PATH"
done

repo="$(git -C "$(dirname "${BASH_SOURCE[0]}")" rev-parse --show-toplevel)"
cd "$repo"

export MISE_ENV=android
step "JDK and Tauri CLI (mise.android.toml)"
mise install java "github:tauri-apps/tauri"
# ANDROID_HOME, NDK_HOME, JAVA_HOME and PATH exactly as mise sets them.
# Unset first, so values from the caller's shell can't pass for mise's.
unset ANDROID_HOME NDK_HOME JAVA_HOME
mise_env="$(mise env --shell bash)" || die "mise env failed"
eval "$mise_env"
[[ -n "${ANDROID_HOME:-}" && -n "${NDK_HOME:-}" && -n "${JAVA_HOME:-}" ]] \
    || die "mise.android.toml must set ANDROID_HOME and NDK_HOME, and pin java"
[[ "$NDK_HOME" == "$ANDROID_HOME/ndk/$NDK_VERSION" ]] \
    || die "NDK_HOME in mise.android.toml ($NDK_HOME) does not match NDK_VERSION $NDK_VERSION here"

step "Android command-line tools $CMDLINE_TOOLS_BUILD in $ANDROID_HOME"
tools_dir="$ANDROID_HOME/cmdline-tools/latest"
stamp="$tools_dir/.passporter-sha256"
sdkmanager="$tools_dir/bin/sdkmanager"
if [[ -f "$stamp" && "$(<"$stamp")" == "$CMDLINE_TOOLS_SHA256" && -x "$sdkmanager" ]]; then
    echo "already installed"
else
    work="$(mktemp -d)"
    trap 'rm -rf "$work"' EXIT
    zip="$work/cmdline-tools.zip"
    curl --fail --location --proto '=https' --tlsv1.2 --silent --show-error \
        --output "$zip" \
        "https://dl.google.com/android/repository/commandlinetools-linux-${CMDLINE_TOOLS_BUILD}_latest.zip"
    echo "$CMDLINE_TOOLS_SHA256  $zip" | sha256sum --check --quiet \
        || die "command-line tools download does not match the pinned SHA-256"
    unzip -q "$zip" -d "$work/unpacked"
    mkdir -p "$ANDROID_HOME/cmdline-tools"
    rm -rf "$tools_dir"
    mv "$work/unpacked/cmdline-tools" "$tools_dir"
    echo "$CMDLINE_TOOLS_SHA256" >"$stamp"
    echo "installed"
fi

step "SDK packages: ${SDK_PACKAGES[*]}"
missing=()
for pkg in "${SDK_PACKAGES[@]}"; do
    # sdkmanager installs "a;b;c" into a/b/c and writes package.xml there last.
    [[ -f "$ANDROID_HOME/${pkg//;//}/package.xml" ]] || missing+=("$pkg")
done
if ((${#missing[@]} == 0)); then
    echo "already installed"
else
    # Answers yes to sdkmanager's prompts, which are the licences of these
    # packages (Google's Android SDK licence); the human who runs this accepts.
    { yes || true; } | "$sdkmanager" --sdk_root="$ANDROID_HOME" --install "${missing[@]}"
    for pkg in "${missing[@]}"; do
        [[ -f "$ANDROID_HOME/${pkg//;//}/package.xml" ]] || die "sdkmanager did not install $pkg"
    done
fi

channel="$(sed -n 's/^channel = "\(.*\)"$/\1/p' rust-toolchain.toml)"
[[ -n "$channel" ]] || die "no channel in rust-toolchain.toml"
step "Rust targets for toolchain $channel"
# With no argument, rustup installs the toolchain rust-toolchain.toml names,
# with its components and targets.
installed="$(rustup target list --installed --toolchain "$channel" 2>/dev/null)" || {
    rustup toolchain install
    installed="$(rustup target list --installed --toolchain "$channel")"
}
missing=()
for target in "${RUST_TARGETS[@]}"; do
    grep -qx "$target" <<<"$installed" || missing+=("$target")
done
if ((${#missing[@]} == 0)); then
    echo "already installed"
else
    rustup target add --toolchain "$channel" "${missing[@]}"
fi

step "Done"
cat <<MSG
With MISE_ENV=android, mise sets these inside this repo (mise.android.toml):
  ANDROID_HOME=$ANDROID_HOME
  NDK_HOME=$NDK_HOME
  JAVA_HOME=$JAVA_HOME
Set MISE_ENV=android in the shell you build the app from, or load them once:
  eval "\$(MISE_ENV=android mise env --shell bash)"
Then check with: cargo tauri --version
MSG
