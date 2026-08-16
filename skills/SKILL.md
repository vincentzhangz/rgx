---
name: rgx
description: Fast indexed regex search in Rust. Use when building, testing, benchmarking, or using the rgx CLI or its library crates (rgx, rgx-index, rgx-query). Covers the CLI flags, exit codes, the .rgx index, the programmatic search API, and the workspace dev workflow. Triggers on: rgx, regex search, indexed search, ngram index, grep, search API, how to build/test/clippy this repo, coverage, bench.
---

# rgx

Fast indexed regex search, in Rust. `rgx` builds a sparse n-gram index over a
source tree once, then answers regex queries in milliseconds. Implements
Cursor's ["fast regex search"][paper] paper (rewritten against
`regex`/`regex-syntax`).

[paper]: https://cursor.com/blog/fast-regex-search

## How it works

- The index stores postings for only the most selective n-grams of every file
  (≤ 2n−2 grams per file). Queries decompose into a covering n-gram plan
  (≤ n−2, max length 16); candidates are computed by intersecting prefix and
  suffix gram postings, then verified exactly with `regex`.
- Search time scales with the number of matching files, not the tree size.
- Hash collisions only widen the candidate set; verification is always exact.
- The index is ASCII case-folded, so mixed-case literals prune and `-i`
  still verifies correctly. Folding only widens candidates; `regex` matches
  original bytes.
- `--update` is incremental: unchanged files are never re-read (per-file cache
  keyed by mtime/size/content-hash in `grams.dat`). First build writes in
  place; rebuilds stage then atomically swap.

## CLI

```
rgx [OPTIONS] <PATTERN> [PATH]

OPTIONS:
    -h, --help         Print help
    -i, --ignore-case  Case-insensitive search
        --build        (Re)build the index before searching
        --update       Incrementally update the index (only changed files
                       are re-read; falls back to a full rebuild if needed)
        --no-index     Search without using the index (brute force)
        --stats        Print index statistics
        --time         Print timing breakdown
        --json         Emit JSON Lines with submatch byte offsets
        --follow       Follow symbolic links while scanning

EXIT CODES:
    0   matches found
    1   no matches
    2   error
```

- The index lives in `.rgx/` at the search root and is built automatically on
  first use. `--build` forces a rebuild; `--update` re-reads only changed files.
- `--no-index` brute-forces every file (no `.rgx/` needed).
- Scan skips: hidden files/dirs, `node_modules`, `.gitignore`d paths, and
  binary files. `--follow` follows symlinks.
- Output format (non-JSON): `path:line:content`, sorted by path then line.
- JSON output: JSON Lines with `path`, 1-based `line_number`, `line`, and
  `submatches` (byte offsets):
  `{"path":"./src/main.rs","line_number":2,"line":"...","submatches":[{"start":12,"end":17}]}`

### Examples

```console
$ rgx "fn main" .
$ rgx -i --json "hello|world" src
$ rgx --build "unused" .        # (re)build the index
$ rgx --update "foo" .          # incremental index update
$ rgx --no-index "foo" .        # skip the index entirely
$ rgx --stats --time "foo" .    # diagnostics go to stderr
```

## Library API

All three crates are library-first. Depend from Git (no crates.io publish yet):

```toml
[dependencies]
rgx = { git = "https://github.com/vincentzhangz/rgx", branch = "main" }
# or the layers individually:
# rgx-index = { git = "https://github.com/vincentzhangz/rgx", branch = "main" }
# rgx-query = { git = "https://github.com/vincentzhangz/rgx", branch = "main" }
```

### `rgx` (bundled search)

```rust
use rgx::search;
use rgx_index::{Index, ScanOptions, build_index};

let root = std::path::Path::new("/path/to/code");
let index_dir = root.join(".rgx");
let mut progress = |p: &str| println!("indexing {p}");
build_index(root, &index_dir, &ScanOptions::default(), &mut progress)?;

let index = Index::open(&index_dir)?;
let matches = search(&index, "fn [a-z_]+\\(", false)?;
for m in &matches {
    println!("{}:{}: {}", m.path, m.line, m.text.trim());
}
```

- `rgx::search(&Index, pattern: &str, ignore_case: bool) -> Result<Vec<Match>, regex::Error>`
  — the one-call entry point; results sorted by path then line.
- `rgx::execute(args, out: &mut dyn Write, err: &mut dyn Write) -> i32` — the
  full CLI as a callable function (matching results → `out`, diagnostics →
  `err`). `rgx::run()` is the thin binary wrapper; `rgx::Config` is the parsed
  CLI config; `rgx::json_escape` escapes for JSON output.
- `Match` has `path`, 1-based `line`, `line_text`, and `submatches` byte
  offsets when JSON-typed.
- `rgx-index` and `rgx-query` are re-exported transitively via the `rgx` crate.

### `rgx-index`

- `build_index(root: &Path, index_dir: &Path, opts: &ScanOptions, progress: &mut impl FnMut(&str)) -> Result<BuildStats>`
- `update_index(...) -> Result<BuildStats>` — incremental update.
- `Index::open(index_dir) -> Result<Index>` — load (mmap'd). Corrupt/truncated
  indexes are detected and reported as errors, not panics.
- `Index::file_count()`, `ngram_count()`, `posting_count()`, `file_path(id)`.
- `ScanOptions { follow_symlinks, ..Default::default() }`.
- `display_root(&Path) -> PathBuf`, `DEFAULT_MAX_FILE_SIZE`, `MIN_NGRAM_LENGTH`,
  `DEFAULT_MAX_NGRAM_LENGTH`.

### `rgx-query`

- `decompose(pattern: &str, fold_case: bool) -> QueryPlan` — covering
  n-gram plan. Pass `true` when querying the on-disk index.
- `candidates(index: &Index, &plan) -> Option<Vec<u32>>` — pruned file ids;
  `None` when the pattern has no useful literals (caller then scans all files).
- `QueryPlan`, `Branch`, `intersect_sorted`, `union_sorted`.

## Development workflow

```console
$ cargo build --workspace
$ cargo test --workspace              # unit + integration tests
$ cargo nextest run --workspace       # parallel test runner (recommended)
$ cargo clippy --all-targets -- -D warnings
$ cargo fmt --check
$ make build|test|nextest|clippy|fmt|fmt-check|coverage|bench|install
```

- `scripts/bench.sh` — rgx vs ripgrep on a synthetic corpus (env: `BIN`, `RG`,
  `CORPUS`, `N_FILES`, `SEED`). Needs `python3`; release binary built on demand.
- `scripts/coverage.sh` — HTML + LCOV into `target/coverage` and
  `target/coverage.lcov`. Requires `cargo llvm-cov`. Developer aid only; never
  gated in CI.
- Requires Rust 1.97+ (edition 2024). CI runs on push/PR via
  `.github/workflows/ci.yml`.
- Do not add new dependencies casually — the workspace deps are just
  `memmap2`, `regex`, `regex-syntax` (workspace pins in `Cargo.toml`).

### Release

Tags `v*` trigger `.github/workflows/release.yml`: `create-gh-release-action`
creates the GitHub Release from `CHANGELOG.md`, then `upload-rust-binary-action`
builds and attaches binaries (linux/macos/windows, x86_64 + aarch64) with
sha256 checksums. Or run `make release-dry-run` / `make release`
(`cargo release 0.1.0 --workspace`).

## Workspace layout

- `crates/rgx` — CLI (`rgx::execute`), matcher, JSON output.
- `crates/rgx-index` — scanner (hidden/`.gitignore`/binary policies),
  n-gram algorithm, on-disk index format, atomic build.
- `crates/rgx-query` — query decomposition, candidate planning, set algebra.

Index files under `.rgx/` (little-endian, magic-versioned): `lookup.dat`
(sorted `(hash, offset)`), `postings.dat` (length-prefixed u32 lists),
`files.dat` (paths, id == index), `meta.dat` (mtime/size), `grams.dat`
(per-file n-gram cache for incremental updates).