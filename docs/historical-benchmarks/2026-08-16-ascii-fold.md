# 2026-08-16 — ASCII-fold index (`*2` magics)

Protocol: [../BENCHMARKS.md](../BENCHMARKS.md). Same host and 8,000-file
corpus as [2026-08-15-v0.1.1.md](2026-08-15-v0.1.1.md).

Host: Windows, 24 logical CPUs, rustc 1.97.1, ripgrep 15.2.0.
Tree: this PR (`file_grams` ASCII-fold, table magics `*2`).
Corpus: 8,000 synthetic files, 35.2 MiB source. Search stdout discarded.
Median of 5 query runs after the index was warm. One `--build` first.

## Prune (`rgx --stats --time`)

| Pattern | Candidates | Matches |
|---------|----------:|--------:|
| `needle_token_UNIQUE_8000` | **1** (was 8,000 on 0.1.1) | 1 |
| `sits here` | 1 | 1 |
| `hello` | 8,000 | 165,825 |
| `.` | (unconstrained) | all lines |

`load` for the UNIQUE query was 651 µs. `match` for UNIQUE and `sits here`
was 0 ms at `--time` resolution.

## Query wall vs ripgrep

Median of 5, stdout discarded:

| Pattern | rgx | ripgrep 15.2.0 |
|---------|----:|---------------:|
| `needle_token_UNIQUE_8000` | **12.4 ms** | 298 ms |
| `sits here` | **10.8 ms** | 290 ms |
| `hello` | 493 ms | 652 ms |
| `.` | 2,530 ms | 2,387 ms |

Rare mixed-case literals now prune. Common tokens and unconstrained `.`
still scan the tree; the index is not a ripgrep replacement there.

## Index

| | |
|--|--:|
| Build wall (`--build`, 8,000 re-read) | 117 s |
| Index on disk | 299 MiB (**8.50×** corpus) |
| n-grams / postings | 2,911,706 / 19,934,502 |

Most of the on-disk size is `grams.dat` (incremental-update cache).
