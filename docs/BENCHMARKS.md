# Benchmark protocol

This document is the durable record of **how** to measure `rgx`. Numbers
from any one machine rot; the method does not. Pull requests that change
index or query behaviour, or that claim a speed or memory improvement,
must link here and report results produced by this protocol (or improve
the protocol in the same PR).

## Why a protocol, not a scoreboard

Wall-clock milliseconds copied into a PR description go stale the next
time someone runs the same script on a different OS, disk, or CPU. What
stays useful is:

- which binary flags were used
- which corpus recipe
- which patterns (selective vs unselective, mixed-case vs lowercase)
- which metrics (wall, CPU, working set, private bytes, prune rate)
- how many iterations, and whether stdout was discarded

Paste a short table of **your** run in the PR if it helps the reviewer.
Do not treat those figures as the project's published performance.

## Quick start

```console
$ cargo build --release
$ ./scripts/bench.sh
```

`scripts/bench.sh` builds a synthetic corpus (default 10,000 files),
warms the index, and compares `rgx` to `ripgrep` (and `grep` when
present). Override `N_FILES`, `ITERS`, `WARMUP`, `CORPUS`, `BIN`, and
`RG` as documented in the script header. The script's corpus does **not**
yet emit a mixed-case unique token; prune rate for that class is a
separate `--stats --time` run (see *Prune rate* below).

On Windows, run it from Git Bash or WSL. Peak working set and private
bytes are not what `/usr/bin/time -p` reports; see *Memory* below.

## Required measurements for index/query PRs

Run against a **release** binary (`cargo build --release`). Discard
search stdout so printing does not dominate. Take the median of at least
5 query runs after 1 warmup; builds may use 3 runs.

### 1. Prune rate (correctness first)

For every pattern you claim is "indexed", print `--stats --time` and
record `candidates` vs `index: N files`. Mixed-case prune is this
fixture measurement, not `scripts/bench.sh`, until the generator emits
`needle_token_UNIQUE_*`.

```console
$ rgx --stats --time 'needle_token_UNIQUE_1' /path/to/fixture
```

The fixture must contain that mixed-case token in **one** file. Expect
`1 candidates`.

| Pattern class | Example | Expectation |
|---------------|---------|-------------|
| Lowercase unique literal | `sits here` | 1 candidate on a corpus with one hit |
| Mixed-case unique literal | `needle_token_UNIQUE_1` | **1 candidate**, not the whole tree |
| Common token | `hello` / `fn` | may be most or all files; say so |
| No useful literals | `.` | all files (plan is unconstrained) |

If a mixed-case identifier scans every file, pruning is broken even when
the match list is correct. Folding is ASCII-only.

### 2. Time

Record both:

- **Wall** time (process start to exit)
- **CPU** time (user + kernel), when the platform can provide it

Wall time on a spawn-per-query CLI includes index load. `--time` splits
`load` vs `match`. A change that only moves load from 30 ms to 0.6 ms
should say that, not "search is 50× faster."

### 3. Memory

Distinguish:

- **Working set** — physical pages, including mmap'd index files that
  were faulted in
- **Private bytes** — heap/stack, excluding file-backed maps

`Index::open` must not walk every posting list (that faults the whole
index into the working set). Query private bytes should follow the
candidate set, not the corpus size. Build RSS is a separate number;
do not quote it as "query uses 4 MB."

### 4. Index size

`du` of `.rgx/` vs the corpus. Note `grams.dat` (incremental-update
cache) separately from `lookup.dat` + `postings.dat`.

## Pattern set (minimum)

Reuse these, plus any pattern your change cares about:

```
hello
fn main
needle_token_UNIQUE_<N>
sits here
.
```

`needle_token_UNIQUE_<N>` must appear in **one** file, with that mixed
case, if you are checking the ASCII-fold invariant.

## Reporting in a pull request

In the PR body, add a **Benchmark** section that:

1. Links to this file (`docs/BENCHMARKS.md`).
2. States host OS, CPU count, `rustc` version, and `rgx` commit.
3. States corpus (`scripts/bench.sh` defaults, or file count + recipe).
4. Includes prune `candidates` for the mixed-case literal.
5. Includes wall (and CPU/RSS if you measured them) for at least one
   selective and one unselective pattern, vs `ripgrep` when available.

Improvements to this protocol — Windows RSS sampling, a checked-in
corpus generator that emits a mixed-case unique, JSON output from
`bench.sh` — are welcome in the same PR that uses them.

## Out of scope

Do not commit `.rgx/` directories, generated corpora, or machine-local
CSV dumps. `.gitignore` already excludes `/.rgx/` and `/rgx-corpus/`.
