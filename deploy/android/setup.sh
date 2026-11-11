#!/usr/bin/env bash
# Android toolchain for building Passporter's Tauri 2 app, under your user
# (no sudo).
#
#   deploy/android/setup.sh        # from anywhere inside the repo checkout
#
# What it installs, and where:
#   1. a JDK and the Tauri CLI (cargo-tauri), via mise, at the versions pinned
#      in mise.toml (`java`, `github:tauri-apps/tauri`);
#   2. the Android command-line tools into $ANDROID_HOME/cmdline-tools/latest
#      (ANDROID_HOME is ~/.local/share/android-sdk, set by mise.toml [env]),
#      checked against the SHA-256 pinned below;
#   3. the SDK packages pinned below, via sdkmanager, accepting Google's
#      Android SDK licences non-interactively;
#   4. the Rust Android targets for the toolchain pinned in rust-toolchain.toml.
#
# Safe to re-run: each step checks what is there first, so a second run
# downloads and changes nothing. docs/development/android.md says how to undo it.
set -euo pipefail

# ---- Pinned versions (the JDK and the Tauri CLI are pinned in mise.toml) ----
# Command-line tools 21.0 (build 15641748). The SHA-1 in Google's repository
# manifest (dl.google.com/android/repository/repository2-3.xml) matches this
# file. Not 22.0 or later: their sdkmanager hands off to a separate "Android
# CLI" that it downloads unpinned and that collects usage metrics.
CMDLINE_TOOLS_BUILD="15641748"
CMDLINE_TOOLS_SHA256="a66d5ef0238fc0162e9c1446602ce0dd41702d4dd7a94d2ce42d12b7f80baf7e"
# Tauri CLI 2.12.4's Android template compiles against SDK 37 (packaged as
# android-37.0). NDK r29 stable: Tauri's own default (29.0.13846066) is the
# r29 beta 3, and Tauri uses whatever NDK_HOME names.
# Keep NDK_VERSION in step with NDK_HOME in mise.toml.
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

for tool in mise rustup curl unzip sha256sum; do
    command -v "$tool" >/dev/null || die "$tool is required but not on PATH"
done

repo="$(git -C "$(dirname "${BASH_SOURCE[0]}")" rev-parse --show-toplevel)"
cd "$repo"

step "JDK and Tauri CLI (mise.toml)"
mise install java "github:tauri-apps/tauri"
# ANDROID_HOME, NDK_HOME, JAVA_HOME and PATH exactly as the repo's mise sets them.
eval "$(mise env --shell bash)"
[[ -n "${ANDROID_HOME:-}" && -n "${NDK_HOME:-}" && -n "${JAVA_HOME:-}" ]] \
    || die "mise.toml must set ANDROID_HOME and NDK_HOME, and pin java"
[[ "$NDK_HOME" == "$ANDROID_HOME/ndk/$NDK_VERSION" ]] \
    || die "NDK_HOME in mise.toml ($NDK_HOME) does not match NDK_VERSION $NDK_VERSION here"

step "Android command-line tools $CMDLINE_TOOLS_BUILD in $ANDROID_HOME"
tools_dir="$ANDROID_HOME/cmdline-tools/latest"
stamp="$tools_dir/.passporter-sha256"
if [[ -f "$stamp" && "$(<"$stamp")" == "$CMDLINE_TOOLS_SHA256" ]]; then
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
sdkmanager="$tools_dir/bin/sdkmanager"

step "SDK packages: ${SDK_PACKAGES[*]}"
missing=()
for pkg in "${SDK_PACKAGES[@]}"; do
    # sdkmanager installs "a;b;c" into a/b/c and writes package.xml there last.
    [[ -f "$ANDROID_HOME/${pkg//;//}/package.xml" ]] || missing+=("$pkg")
done
if ((${#missing[@]} == 0)); then
    echo "already installed"
else
    # The human who runs this accepts Google's Android SDK licence terms.
    { yes || true; } | "$sdkmanager" --sdk_root="$ANDROID_HOME" --licenses >/dev/null
    "$sdkmanager" --sdk_root="$ANDROID_HOME" --install "${missing[@]}"
fi

channel="$(sed -n 's/^channel = "\(.*\)"$/\1/p' rust-toolchain.toml)"
[[ -n "$channel" ]] || die "no channel in rust-toolchain.toml"
step "Rust targets for toolchain $channel"
grep -q "^$channel-" <<<"$(rustup toolchain list)" || rustup toolchain install "$channel" --profile minimal
installed="$(rustup target list --installed --toolchain "$channel")"
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
Inside this repo, mise sets these for you (mise.toml [env] and the pinned java):
  ANDROID_HOME=$ANDROID_HOME
  NDK_HOME=$NDK_HOME
  JAVA_HOME=$JAVA_HOME
Without mise activation in your shell, load them with:
  eval "\$(mise env --shell bash)"
Then check with: cargo tauri --version
MSG
