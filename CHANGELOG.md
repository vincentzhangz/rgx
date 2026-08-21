# Changelog

All notable changes to this project are documented here.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.2.0]

### Fixed

- **Case-folded index**: indexed content is now ASCII case-folded before
  n-gram hashing, and the query planner folds extracted literals the same
  way. Previously the index stored raw-case grams while queries folded,
  so any query containing uppercase letters (e.g. `HashMap.*BTreeMap`) and
  every `-i` query silently lost all index pruning and fell back to
  verifying every file. Results were always correct; pruning now works.
- **Absent patterns no longer scan the corpus**: `candidates()` now
  distinguishes "no constraint" from "provably impossible". A pattern whose
  covering n-grams exist nowhere in the index (e.g. a typo) is answered in
  microseconds instead of triggering a full verification scan.

### Changed

- **Index format v2** (`RGX*2` magic bytes). Indexes built by older
  versions are detected at open time and rebuilt transparently on first
  use; results are unchanged.
  - Posting lists are delta + LEB128 varint encoded (dense lists shrink
    5–10×).
  - N-gram hash keys are truncated to 40 bits: collisions only widen
    candidate sets and exact regex verification is unaffected, while
    `grams.dat` shrinks ~40% and `lookup.dat` entries drop from 16 to 10
    bytes.
  - Stored paths are root-relative, making indexes smaller and relocatable.
- A corrupt or outdated index is rebuilt automatically instead of exiting
  with code 2.

### Performance

- Build workers receive chunks of at least 64 files (up to 8× cores
  workers), so a few large files no longer idle the remaining threads.
- The scanner no longer opens every file twice: binary detection moved to
  content read time, and walk metadata is reused instead of re-statting.
- `--no-index` scans the tree once instead of twice.
- Lookup entry count is cached at open; posting decode slices bounds-checked
  reads once per list element instead of per access.

## [0.1.1]

### Fixed

- Dropped open `Index` memory-mapped handles before incremental rebuilds in unit tests, fixing transient `ERROR_ACCESS_DENIED` failures on Windows.

### Performance

- **Memory-mapped index tables**: All index files (`lookup.dat`, `postings.dat`, `files.dat`, and `meta.dat`) are now 100% memory-mapped on open, eliminating heap allocations for path strings and table metadata.
- **O(1) postings table header validation**: Index loading validates posting list bounds from header metadata in \(O(1)\), preventing untouched postings pages from being paged into physical RAM (RSS).
- **Compact 4-byte record offsets**: Stored compact 4-byte `Vec<u32>` record offsets for files and metadata tables, slicing `&Path` zero-copy directly from the mmap.
- **Packed 12-byte postings during build**: Introduced a packed 12-byte `Posting` struct (`#[repr(C, packed)]`) that reduces RAM usage by 25% during index builds and in-memory sorting.
- **Streaming caches & in-place deduplication**: `grams.dat` streams via `BufReader` into `Arc<[u64]>`, and `file_grams` deduplicates n-grams in place with `sort_unstable` and `dedup`.
- **Whole-content matcher fast-path**: Added an upfront `re.is_match(content)` check before line splitting, skipping line iterations entirely for false-positive candidates.
- **Candidate set algebra & read buffer reuse**: Added `intersect_sorted_into` and `union_sorted_into` in `rgx-query` and reused per-thread file read buffers across candidates in `rgx`.
- **Zero-allocation JSON streaming**: JSON Lines and submatch offsets are streamed directly to the writer without intermediate `String` or `Vec` formatting allocations.

## [0.1.0]

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