#!/bin/sh
# Bundle gate (smoothness budget), run by `just web-size`:
#
# 1. The wasm-bindgen CLI trunk uses (Trunk.toml [tools] wasm_bindgen) must
#    equal the wasm-bindgen crate version in Cargo.lock; a mismatch makes
#    trunk fail or emit broken glue.
# 2. The app's WASM and its JS glue, the service worker's push wake WASM
#    (src/bin/notice.rs), the on-demand dictation WASM (src/bin/voice.rs),
#    the on-demand pairing WASM (src/bin/pair.rs), the on-demand extras
#    WASM (src/bin/extras.rs: rarely used sheets and pages) and the
#    on-demand terminal WASM (src/bin/terminal.rs: the command confirm and
#    watched terminal of terminal mode) must be under budget
#    after gzip -9, the compression every browser accepts. Brotli (what
#    the bridge should serve) is reported too and is always smaller.
#
# usage: check-size.sh [dist-dir]
# env:   WASM_BUDGET, JS_BUDGET, NOTICE_BUDGET, VOICE_BUDGET, PAIR_BUDGET,
#        EXTRAS_BUDGET, TERMINAL_BUDGET (bytes, gzipped)
set -eu

here=$(cd "$(dirname "$0")" && pwd)
crate="$here/.."
dist="${1:-$crate/dist}"
wasm_budget="${WASM_BUDGET:-409600}" # 400 KiB
js_budget="${JS_BUDGET:-20480}"      # 20 KiB
notice_budget="${NOTICE_BUDGET:-81920}" # 80 KiB
voice_budget="${VOICE_BUDGET:-102400}"  # 100 KiB
pair_budget="${PAIR_BUDGET:-122880}"    # 120 KiB: its own Leptos runtime
extras_budget="${EXTRAS_BUDGET:-163840}" # 160 KiB: its own Leptos runtime
terminal_budget="${TERMINAL_BUDGET:-106496}" # 104 KiB: its own Leptos runtime

fail=0

# --- 1. wasm-bindgen versions -------------------------------------------
lock="$crate/../../Cargo.lock"
locked=$(awk '/^name = "wasm-bindgen"$/ { getline; gsub(/version = |"/, ""); print; exit }' "$lock")
pinned=$(sed -n 's/^wasm_bindgen = "\(.*\)"/\1/p' "$crate/Trunk.toml")
if [ -z "$locked" ] || [ "$locked" != "$pinned" ]; then
  echo "check-size: FAIL: Trunk.toml wasm_bindgen = \"$pinned\" but Cargo.lock has wasm-bindgen $locked" >&2
  fail=1
else
  echo "check-size: ok: wasm-bindgen $locked (Cargo.lock) matches Trunk.toml"
fi

# --- 2. sizes -----------------------------------------------------------
wasm=$(find "$dist" -maxdepth 1 -name 'web-*_bg.wasm' | head -n 1)
js=$(find "$dist" -maxdepth 1 -name 'web-*.js' | head -n 1)
notice="$dist/notice_bg.wasm"
voice="$dist/voice_bg.wasm"
pair="$dist/pair_bg.wasm"
extras="$dist/extras_bg.wasm"
terminal="$dist/terminal_bg.wasm"
[ -n "$wasm" ] && [ -n "$js" ] && [ -f "$notice" ] && [ -f "$voice" ] && [ -f "$pair" ] && [ -f "$extras" ] && [ -f "$terminal" ] || { echo "check-size: no build in $dist (run just web-build)" >&2; exit 1; }

check() { # file budget
  raw=$(wc -c <"$1")
  gz=$(gzip -9 -c "$1" | wc -c)
  br="n/a"
  command -v brotli >/dev/null 2>&1 && br="$(brotli -q 11 -c "$1" | wc -c) B"
  printf '%-32s raw %8d B   gzip %7d B / %7d B (%3d%%)   brotli %s\n' \
    "$(basename "$1")" "$raw" "$gz" "$2" $((gz * 100 / $2)) "$br"
  if [ "$gz" -gt "$2" ]; then
    echo "check-size: FAIL: $(basename "$1") is $gz B gzipped, budget $2 B" >&2
    fail=1
  fi
}
check "$wasm" "$wasm_budget"
check "$js" "$js_budget"
check "$notice" "$notice_budget"
check "$voice" "$voice_budget"
check "$pair" "$pair_budget"
check "$extras" "$extras_budget"
check "$terminal" "$terminal_budget"

[ "$fail" -eq 0 ] && echo "check-size: ok"
exit "$fail"
