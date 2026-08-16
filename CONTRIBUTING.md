# Contributing

Thanks for your interest in `rgx`. This document covers how to contribute
code, report bugs, and keep the project consistent.

## Development setup

- Rust 1.97+ (edition 2024). Install with `rustup`.
- Recommended: [cargo-nextest](https://nexte.st) for the test runner.

## Getting started

```console
$ cargo build
$ cargo test --workspace
$ cargo nextest run --workspace
$ cargo clippy --all-targets -- -D warnings
$ cargo fmt --check
```

## Code style

- Keep the dependency footprint minimal: `regex`, `regex-syntax`, `memmap2`
  are the only allowed external crates. New dependencies need a strong
  justification in the PR.
- No `//` comments in code. Use `///` doc comments for public items and
  `//!` module docs where they add value.
- Format with `cargo fmt`, and ensure `cargo clippy --all-targets -- -D warnings`
  is clean.
- `rgx-index` deliberately uses `unsafe` (mmap). Keep the unsafe surface tiny
  and contained; never add new unsafe blocks without a reviewer.

## Testing

- Unit tests live next to the code they cover.
- End-to-end CLI tests live in `crates/rgx/tests/cli.rs` and drive
  `rgx::execute` in-process with buffer writers (fast, and covered by
  coverage).
- Tests must be self-contained: create unique temp dirs per test, never rely
  on shared mutable state or network access.
- Run the full suite with nextest before opening a PR.

## Pull requests

- Run the full checks above before opening a PR.
- Add a CHANGELOG entry under `Unreleased`.
- Keep commits focused; a small PR that lands is better than a large one that
  stalls.

### Benchmarks

PRs that change index or query behaviour, or that claim a speed or memory
improvement, must include a **Benchmark** section that links to
[docs/BENCHMARKS.md](docs/BENCHMARKS.md) and reports a run of that
protocol (host, corpus, prune `candidates` for a mixed-case literal via
`rgx --stats --time`, wall time vs `ripgrep` when available).
`scripts/bench.sh` covers timing vs ripgrep; mixed-case prune is a
one-hit fixture until the generator emits `needle_token_UNIQUE_*`.

Do not paste a wall-clock table with no method. Numbers from one machine
go stale; the protocol is what the next reviewer re-runs.

Docs-only or typo PRs do not need a bench. Improvements to the protocol
itself (CPU, RSS, prune rate, corpus recipe) are welcome — land them in
`docs/BENCHMARKS.md` and `scripts/bench.sh` in the same PR that uses them.

## Releasing

Releases are cut with [cargo-release](https://github.com/cargo-bins/cargo-release),
which bumps the workspace version, moves the `Unreleased` changelog into a
`[version]` section, commits, tags `v<version>`, and pushes. The GitHub
Release workflow then builds and attaches binaries for Linux, macOS, and
Windows.

```console
$ cargo install cargo-release
$ cargo release patch --workspace --dry-run   # preview: no changes made
$ cargo release patch --workspace             # bump, changelog, tag, push
```

Use `minor` or `major` for non-patch bumps. The `pre-release-replacements`
rule in `[workspace.metadata.release]` only fires for stable releases, so
pre-releases keep their notes under `Unreleased`. After the tag push, the
workflow at `.github/workflows/release.yml` publishes the release; check the
Release on GitHub and the CI run.

## Bugs and feature requests

Open an issue with the pattern, the search root layout, and the `--time`
output if it is a performance problem. For crashes, include the panic message
and a minimal reproducer.