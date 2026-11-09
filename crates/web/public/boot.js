// Loads the WASM app. An external module because the CSP forbids inline
// scripts; trunk's preload links (see Trunk.toml) carry the hashed URLs.
// All app logic lives in Rust.
const url = (kind) => document.querySelector(`link[data-app="${kind}"]`).href;
const { default: init } = await import(url("js"));
await init({ module_or_path: url("wasm") });
