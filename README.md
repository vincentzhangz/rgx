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
- **Compact format**: Posting lists are delta + varint encoded, n-gram keys
  are truncated to 40 bits (collisions only widen candidate sets — exact
  regex verification is unaffected), and paths are stored root-relative.
  Index tables (`lookup.dat`, `postings.dat`, `files.dat`, `meta.dat`,
  `grams.dat`) are written via 64 KB `BufWriter`s and read via memory
  mapping (`mmap`).
- **Production file discovery**: Powered by the `ignore` crate (ripgrep ecosystem),
  supporting root and nested `.gitignore` files, glob negations (`!pattern`),
  `.git/info/exclude`, and custom `.rgxignore` files.
- Hash-only keys are sound: a hash collision only widens the candidate set;
  verification is always exact.
- The index is ASCII case-folded, so case-insensitive queries (`-i`) prune
  correctly.
- Corrupt or outdated indexes are detected at load time and rebuilt
  transparently instead of crashing. First builds write the index in place;
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

Benchmarked against `ripgrep` and `grep` on a synthetic corpus of 10,000
files (215 MB corpus, 156 MB index), best-of-5 runs:

- **Query speed**: **2.6× faster than `ripgrep`**, **53× faster than `grep`**
  across 12 common/selective/rare patterns.
- **Memory**: peak RSS of ~5 MB for selective/rare queries (up to 4× less
  than `ripgrep`) — the index is memory-mapped and only candidate pages are
  touched.
- **Correctness**: match counts verified identical to both tools on every
  pattern.

Full tables, methodology and environment: [benchmark.md](benchmark.md).

Reproduce locally:

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
    println!("{}:{}: {}", m.path, m.line, m.line_text.trim());
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

Coverage is a developer aid and is never gated in CI.

## Workspace layout

- `crates/rgx` — CLI (`rgx::execute`), matcher, JSON output.
- `crates/rgx-index` — scanner (hidden/`.gitignore`/binary policies),
  n-gram algorithm, on-disk index format, atomic build.
- `crates/rgx-query` — query decomposition, candidate planning, set algebra.

## Index format

Five files under `.rgx/`, all little-endian, versioned by magic bytes
(`RGX*2`; older formats are rebuilt automatically):

| file         | contents                                                        |
|--------------|-----------------------------------------------------------------|
| `lookup.dat` | header + sorted `(u40 hash, u40 postings-offset)` entries (10 B) |
| `postings.dat`| header + delta-varint sorted `u32` file-id lists               |
| `files.dat`  | header + indexed root + length-prefixed root-relative paths     |
| `meta.dat`   | header + per-file `(path, mtime, size)` for change detection     |
| `grams.dat`  | header + varint per-file `(path, mtime, size, content-hash, grams)` cache for incremental updates |

## License

MIT. See [LICENSE](LICENSE).