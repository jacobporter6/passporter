# Contributing to Passporter

Thanks for your interest! Passporter is a personal project shared as is.
Issues and pull requests are welcome; responses are best effort.

## Getting set up

1. Install [rustup](https://rustup.rs). The toolchain pinned in
   `rust-toolchain.toml` (with the `wasm32-unknown-unknown` target) installs
   itself on first use.
2. Install the tools in `mise.toml` (`just`, `cargo-nextest`, `cargo-deny`,
   `trunk`): with [mise](https://mise.jdx.dev), run `mise install`; or
   install each with `cargo install --locked`.
3. Optional: a shared compile cache. Install `sccache` and set
   `RUSTC_WRAPPER=sccache`.

```sh
just verify      # fmt check, clippy (host + wasm, pedantic, -D warnings), tests
just gate        # verify + release build + PWA size budget + cargo deny (what CI runs)
just e2e         # headless browser smoke tests (needs Chromium; skips without it)
```

You don't need herdr, Claude Code or a phone to run the tests: the
integration tests use `FakeHerdr` and a fake summarizer. See
[docs/testing.md](docs/testing.md).

## Making a change

- Read [docs/design.md](docs/design.md) and, for anything touching auth,
  actions or rendering, [docs/security.md](docs/security.md).
- Keep changes small and focused. New behaviour needs tests: unit tests next
  to the code (`#[cfg(test)]`), integration tests in the crate's `tests/`.
- The lints are strict on purpose: no `unsafe`, no `unwrap()` outside tests,
  clippy pedantic, doc comments on public items, errors with context
  (`thiserror` in libraries).
- Inbound data goes through `crates/protocol` types with strict serde
  (`deny_unknown_fields`).
- Don't add dependencies casually. Every new crate must pass `cargo deny`
  (`just audit`); say in the PR why it is worth it.
- Never commit secrets, real keys, or real transcripts beyond small,
  scrubbed, hand-checked excerpts.
- Use [Conventional Commits](https://www.conventionalcommits.org/)
  (`feat(bridge): …`, `fix(web): …`).
- In the PR, describe what changed and why, how you tested it, and confirm
  `just verify` passes.

## AI-assisted contributions

Most of Passporter was written by coding agents under human review, and
AI-assisted contributions are welcome under the same bar as any other: you
are responsible for what you submit, it must pass the gate, and you should
understand it well enough to answer review questions. [AGENTS.md](AGENTS.md)
is the brief for agents working in this repo.

## Security issues

Please don't open public issues for vulnerabilities. See
[SECURITY.md](SECURITY.md).

## Conduct

This project follows the [Contributor Covenant](CODE_OF_CONDUCT.md).

## License

By contributing, you agree that your contributions are dual licensed under
the MIT and Apache-2.0 licenses, as described in the [README](README.md#license).
