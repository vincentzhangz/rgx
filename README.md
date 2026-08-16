# rgx

Fast indexed regex search, in Rust.

`rgx` builds a sparse n-gram index over your source tree once, then answers
regex queries in milliseconds — even on very large codebases. It implements
the sparse n-gram algorithm from Cursor's ["fast regex search"][paper] blog
post, rewritten from scratch against `regex`/`regex-syntax`.

[paper]: https://cursor.com/blog/fast-regex-search

## Why it's fast

The index stores postings for only the most selective n-grams of every file
(≤ 2n−2 grams per file). A query is decomposed into a covering set of n-grams
(≤ n−2, max length 16); the candidate set is computed with an intersection of
prefix grams and suffix grams, then verified exactly with `regex`. Only
candidates are read from disk, so search time scales with the number of
*matching* files — not the size of the tree.

- **Parallel by default**: Both query verification and index construction /
  updating use standard library `std::thread::scope` for multi-threaded
  concurrency without binding to any runtime.
- **High-throughput buffered I/O**: Index tables (`lookup.dat`, `postings.dat`,
  `files.dat`, `meta.dat`, `grams.dat`) are written via 64 KB `BufWriter`s and
  read via memory mapping (`mmap`).
- **Production file discovery**: Powered by the `ignore` crate (ripgrep ecosystem),
  supporting root and nested `.gitignore` files, glob negations (`!pattern`),
  `.git/info/exclude`, and custom `.rgxignore` files.
- Hash-only keys are sound: a hash collision only widens the candidate set;
  verification is always exact.
- The index is ASCII case-folded, so mixed-case literals prune and `-i`
  queries still verify correctly. Folding only widens the candidate set;
  `regex` always matches the original file bytes. See [docs/BENCHMARKS.md](docs/BENCHMARKS.md)
  for how to measure prune rate.
- Corrupt or truncated indexes are detected at load time and reported as an
  error (exit 2) instead of crashing. First builds write the index in place;
  rebuilds write to a staging directory and atomically swap it in.
- `--update` is incremental: unchanged files are never re-read or re-indexed.
  A per-file n-gram cache (`grams.dat`), keyed by `(mtime, size, content
  hash)`, lets a mostly-unchanged repo update in a fraction of the time.

## Usage

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

The index is stored in `.rgx/` at the search root and is built automatically
on first use. Hidden files and directories, default build artifacts (`node_modules`,
`target`, `vendor`, `dist`, `build`, etc.), `.gitignore` rules (including nested
and negated patterns), custom `.rgxignore` files, and binary files are skipped
while scanning.

Example:

```console
$ rgx "fn main" .
src/main.rs:4:fn main() {
```

JSON output reports submatch byte offsets:

```console
$ rgx --json "hello|world" . | head -1
{"path":"./src/main.rs","line_number":2,"line":"    let s = \"hello world\";","submatches":[{"start":12,"end":17},{"start":18,"end":23}]}
```

## Benchmarks

Benchmarked against `ripgrep` and `grep` using `./scripts/bench.sh` on a synthetic corpus of **10,000 files (421 MB corpus, 366 MB index)** on macOS (best-of-5 runs, 2 warmups):

### Search Latency & Memory Usage

| Pattern | Category | `rgx` Time | `ripgrep` Time | `grep` Time | `rgx` RSS | `ripgrep` RSS |
|---|---|---|---|---|---|---|
| `fn return` | Common | **60 ms** | 90 ms | 2,870 ms | **8.7 MB** | 20.8 MB |
| `impl.*struct` | Common | **80 ms** | 100 ms | 3,010 ms | **19.9 MB** | 20.9 MB |
| `match.*enum` | Common | **70 ms** | 100 ms | 3,030 ms | **18.8 MB** | 21.2 MB |
| `HashMap.*BTreeMap` | Selective | **70 ms** | 80 ms | 2,800 ms | **5.1 MB** | 21.5 MB |
| `async.*await` | Selective | **30 ms** | 80 ms | 2,910 ms | **5.5 MB** | 21.1 MB |
| `serialize.*derive` | Selective | **40 ms** | 80 ms | 2,910 ms | **6.0 MB** | 21.2 MB |
| `tokio.*spawn` | Selective | **30 ms** | 80 ms | 3,000 ms | **5.6 MB** | 21.6 MB |
| `SENTINEL_XYZZY` | Rare | **70 ms** | 80 ms | 2,730 ms | **5.0 MB** | 21.5 MB |
| `phant0m_thread` | Rare | **10 ms** | 80 ms | 2,920 ms | **6.1 MB** | 20.9 MB |
| `zwj_codepoint.*QUUX` | Rare | **10 ms** | 80 ms | 2,760 ms | **5.7 MB** | 21.3 MB |
| `nebula_vortex` | Rare | **10 ms** | 80 ms | 2,920 ms | **5.7 MB** | 21.3 MB |
| `hyperdrive_init` | Rare | **10 ms** | 80 ms | 2,870 ms | **5.8 MB** | 20.9 MB |

### Summary

- **Query Speed**: **490 ms** total across all benchmark patterns (**2.1× faster than `ripgrep`**, **70.9× faster than `grep`**).
- **Memory Footprint**: Peak RSS of **5.0 – 6.1 MB** for selective/rare queries (up to **4× less memory than `ripgrep`**).
- **Reproduce Locally**:
  ```console
  $ ./scripts/bench.sh
  ```

## Install

```console
$ cargo install --path crates/rgx
```

Requires Rust 1.97+ (edition 2024).

## Use as a library

All three crates are library-first; depend on them from Git (no crates.io
publish yet). `crates/rgx` bundles the index, query planner, matcher, and a
one-call search API; use `rgx-index` + `rgx-query` directly if you want the
pieces separately.

```toml
[dependencies]
rgx = { git = "https://github.com/vincentzhangz/rgx", branch = "main" }
# or, to use the layers individually:
# rgx-index = { git = "https://github.com/vincentzhangz/rgx", branch = "main" }
# rgx-query = { git = "https://github.com/vincentzhangz/rgx", branch = "main" }
```

```rust,no_run
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
# Ok::<(), Box<dyn std::error::Error>>(())
```

Search results are the same `Match` type the CLI prints: path, 1-based line
number, the line text, and submatch byte offsets when a match is JSON-typed.

## Development

```console
$ cargo test --workspace          # unit + integration tests
$ cargo nextest run --workspace   # parallel test runner (recommended)
$ cargo clippy --all-targets -- -D warnings
$ cargo fmt --check
$ ./scripts/coverage.sh           # HTML + LCOV report into target/coverage
$ ./scripts/bench.sh              # rgx vs ripgrep on a synthetic corpus
```

How to run that script, what to record, and how to report results in a
pull request is documented in [docs/BENCHMARKS.md](docs/BENCHMARKS.md).

Coverage is a developer aid and is never gated in CI.

## Workspace layout

- `crates/rgx` — CLI (`rgx::execute`), matcher, JSON output.
- `crates/rgx-index` — scanner (hidden/`.gitignore`/binary policies),
  n-gram algorithm, on-disk index format, atomic build.
- `crates/rgx-query` — query decomposition, candidate planning, set algebra.

## Index format

Five files under `.rgx/`, all little-endian, versioned by magic bytes:

| file         | contents                                                        |
|--------------|-----------------------------------------------------------------|
| `lookup.dat` | `"RGXLOOK2"` + sorted `(hash u64, postings-offset u64)` entries  |
| `postings.dat`| `"RGXPOST2"` + length-prefixed sorted `u32` file-id lists        |
| `files.dat`  | `"RGXFILS2"` + length-prefixed file paths (index == file id)     |
| `meta.dat`   | `"RGXMETA2"` + per-file `(path, mtime, size)` for change detection |
| `grams.dat`  | `"RGXGRAM2"` + per-file `(path, mtime, size, content-hash, grams)` cache |

## License

MIT. See [LICENSE](LICENSE).