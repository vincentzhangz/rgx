# Benchmarks

`rgx` is a fast indexed regex search: it builds a sparse n-gram index over a
source tree once, then answers regex queries in milliseconds. This document
records how it compares against `ripgrep` (`rg`) and `grep` on a synthetic
corpus, and how to reproduce the numbers.

All three tools return **identical match counts** for every pattern (see
[Match verification](#match-verification)); the difference is purely speed
and memory.

## Environment

| tool      | version                              |
|-----------|--------------------------------------|
| rgx       | 0.2.0 (`dev`, release build)          |
| ripgrep   | 15.2.0                               |
| grep      | BSD grep 2.6.0-FreeBSD               |
| OS        | macOS 26.6.2 (Darwin, arm64)         |
| Date      | 2026-08-21                           |

Absolute numbers are machine-specific; the *ratios* are what matter.

## Methodology

- **Corpus**: 10,000 synthetic source files (215 MB), generated with seed 42.
  Text mixes common keywords (~every file), medium-frequency identifiers
  (~20% of files), and rare tokens (~2% of files) to exercise different
  selectivity regimes.
- **Timing**: best-of-5 wall-clock runs after 2 warmup rounds, measured with
  `/usr/bin/time`. CPU time and peak RSS are taken from the fastest run.
- **Index**: built once before querying with `rgx --build`; queries run with
  the index present (the default). `ripgrep`/`grep` run unindexed by design —
  they have no index to consult.

## Search latency (ms)

| Pattern | Category | `rgx` | `ripgrep` | `grep` |
|---|---|---:|---:|---:|
| `fn return` | Common | **60** | 90 | 1,710 |
| `impl.*struct` | Common | **80** | 110 | 1,850 |
| `match.*enum` | Common | **80** | 120 | 1,890 |
| `HashMap.*BTreeMap` | Selective | **30** | 90 | 1,660 |
| `async.*await` | Selective | **30** | 80 | 1,670 |
| `serialize.*derive` | Selective | **40** | 80 | 1,890 |
| `tokio.*spawn` | Selective | **30** | 80 | 1,860 |
| `SENTINEL_XYZZY` | Rare | **10** | 80 | 1,660 |
| `phant0m_thread` | Rare | **10** | 80 | 1,810 |
| `zwj_codepoint.*QUUX` | Rare | **0** | 80 | 1,680 |
| `nebula_vortex` | Rare | **10** | 80 | 1,880 |
| `hyperdrive_init` | Rare | **20** | 90 | 1,670 |

"Selective"/"rare" patterns are where the index pays off most: the candidate
set shrinks to only the files that can contain a match, so verification work
scales with matches, not corpus size.

## Resource usage (fastest run)

| Pattern | `rgx` CPU ms | `rg` CPU ms | `grep` CPU ms | `rgx` RSS MB | `rg` RSS MB | `grep` RSS MB |
|---|---:|---:|---:|---:|---:|---:|
| `fn return` | 120 | 180 | 3,420 | 8.1 | 21.2 | 5.0 |
| `impl.*struct` | 160 | 220 | 3,700 | 18.5 | 21.0 | 5.0 |
| `match.*enum` | 160 | 240 | 3,780 | 18.3 | 20.8 | 5.0 |
| `HashMap.*BTreeMap` | 60 | 180 | 3,320 | 5.2 | 21.1 | 5.0 |
| `async.*await` | 60 | 160 | 3,340 | 4.6 | 21.0 | 5.0 |
| `serialize.*derive` | 80 | 160 | 3,780 | 5.0 | 21.2 | 5.0 |
| `tokio.*spawn` | 60 | 160 | 3,720 | 4.8 | 20.8 | 5.0 |
| `SENTINEL_XYZZY` | 20 | 160 | 3,320 | 4.8 | 21.2 | 5.0 |
| `phant0m_thread` | 20 | 160 | 3,620 | 4.9 | 21.5 | 5.0 |
| `zwj_codepoint.*QUUX` | 0 | 160 | 3,360 | 5.0 | 21.3 | 5.0 |
| `nebula_vortex` | 20 | 160 | 3,760 | 5.1 | 21.2 | 5.0 |
| `hyperdrive_init` | 40 | 180 | 3,340 | 5.1 | 21.2 | 5.0 |

Peak RSS stays at **4.6 – 18.5 MB** because the index is memory-mapped and
only candidate pages are touched.

## Match verification

Match counts agree across all three tools for every pattern:

| Pattern | `rgx` | `ripgrep` | `grep` | |
|---|---:|---:|---:|---|
| `fn return` | 13,269 | 13,269 | 13,269 | ✓ |
| `impl.*struct` | 46,606 | 46,606 | 46,606 | ✓ |
| `match.*enum` | 45,935 | 45,935 | 45,935 | ✓ |
| `HashMap.*BTreeMap` | 0 | 0 | 0 | ✓ |
| `async.*await` | 0 | 0 | 0 | ✓ |
| `serialize.*derive` | 0 | 0 | 0 | ✓ |
| `tokio.*spawn` | 0 | 0 | 0 | ✓ |
| `SENTINEL_XYZZY` | 2,398 | 2,398 | 2,398 | ✓ |
| `phant0m_thread` | 2,288 | 2,288 | 2,288 | ✓ |
| `zwj_codepoint.*QUUX` | 0 | 0 | 0 | ✓ |
| `nebula_vortex` | 2,282 | 2,282 | 2,282 | ✓ |
| `hyperdrive_init` | 2,457 | 2,457 | 2,457 | ✓ |

(The zero-match "selective" patterns require two rare identifiers on the same
line, which this corpus rarely produces; all tools agree.)

## Index size

| Format | Corpus | Index | Ratio |
|---|---:|---:|---:|
| v1 (pre-0.2) | 215 MB | ~356 MB | 1.66× corpus |
| **v2 (current)** | 215 MB | **156 MB** | **0.73× corpus** |

The v2 format combines delta + LEB128 varint posting lists, 40-bit n-gram
hash keys, and root-relative paths. See the [CHANGELOG](CHANGELOG.md) and the
`crates/rgx-index/src/index.rs` module docs for format details.

## Summary

| Metric | Result |
|---|---|
| Total query time (12 patterns) | **400 ms** vs ripgrep's 1,060 ms (**2.6× faster**) and grep's 21,230 ms (**53× faster**) |
| Peak RSS (selective/rare queries) | **~5 MB**, up to ~4× less than ripgrep |
| Index size | **156 MB** for a 215 MB corpus |

## Reproducing

```console
$ cargo build --release
$ ./scripts/bench.sh                 # generates corpus, runs all three tools
```

Useful environment variables:

| Variable | Default | Purpose |
|---|---|---|
| `N_FILES` | `10000` | Corpus size in files |
| `SEED` | `42` | Corpus generator seed |
| `ITERS` / `WARMUP` | `5` / `2` | Best-of-N runs / warmup rounds |
| `BIN`, `RG`, `GREP` | auto | Override tool paths |
| `CSV`, `JSON_OUT` | off | Machine-readable output paths |

Example:

```console
$ N_FILES=2000 ITERS=3 JSON_OUT=/tmp/bench.json ./scripts/bench.sh
```
