# Changelog

All notable changes to this project are documented here.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Fixed

- Index builds no longer fail on Windows CI when antivirus holds a freshly
  written staging file open: first builds write in place (no rename), and
  rebuilds retry the atomic directory swap with a long backoff.
- Removed the `--test-threads=1` workaround from the Windows CI matrix; the
  underlying rename/delete contention is now handled by the index writer.

### Added

- Initial `rgx` release: sparse n-gram indexed regex search over a source tree.
- Automatic index build on first use, stored in `.rgx/` at the search root.
- CLI flags: `-i/--ignore-case`, `--build`, `--update`, `--no-index`,
  `--stats`, `--time`, `--json`, `--follow`, `-h/--help`.
- JSON Lines output with submatch byte offsets.
- Scanner policies: hidden files/dirs, `.gitignore`, binary detection
  (extension + null-byte sniff), max file size, optional symlink following.
- Atomic index builds (staging directory + rename swap).
- Incremental `--update`: a per-file n-gram cache (`grams.dat`) keyed by
  `(mtime, size, 128-bit content hash)` means unchanged files are never
  re-read; missing or corrupt caches fall back to a full rebuild.
- Corruption-tolerant index loading: truncated or malformed indexes are
  reported as errors (exit 2) rather than panicking.
- Rust 2024 edition with MSRV 1.97.
- Testing via `cargo-nextest`, code coverage via `cargo-llvm-cov`, and a CI
  matrix for Linux, macOS, and Windows.
- `rgx::search`: a one-call programmatic search API returning `Match`es
  sorted by path then line.
- Usage examples on the `rgx-index` and `rgx-query` crate docs; every public
  type and field in `rgx-index` is now documented (`missing_docs` enabled).
- `crates/rgx-*` are consumable from Git (versioned path deps) for projects
  that want the library without the CLI.
- GitHub Release workflow: pushing a `v*` tag builds and attaches
  `x86_64`/`aarch64` Linux, macOS, and Windows binaries with checksums.
- `cargo release` automation: `make release` bumps the version, moves the
  changelog into a `[version]` section, and pushes the tag.