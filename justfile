# Passporter task runner. `just` lists recipes.

set shell := ["bash", "-euo", "pipefail", "-c"]

# Crates that must also compile for the browser (added as they land: protocol, web).
wasm_crates := "protocol web"

default:
    @just --list

# Format check, lints (host + wasm), tests. Must pass before you open a PR.
verify: fmt-check lint test

# The full gate (CI runs this): verify + release build + PWA size budget + supply-chain audit.
gate: verify build web-size audit

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
web-build:
    cd crates/web && trunk build --release --locked

# Fail if the PWA's gzipped WASM is over budget (400 KiB).
web-size: web-build
    crates/web/scripts/check-size.sh crates/web/dist

# Serve the PWA on 127.0.0.1:8080 with the production CSP (no autoreload).
web-serve:
    cd crates/web && trunk serve --release --locked

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
