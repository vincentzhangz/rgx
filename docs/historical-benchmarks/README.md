# Historical benchmark runs

These files are **dated machine runs**, not the project's published
performance. The method lives in [../BENCHMARKS.md](../BENCHMARKS.md).
Newest first.

| Date | Tree | Mixed-case `UNIQUE` prune | Notes |
|------|------|---------------------------|--------|
| [2026-08-16](2026-08-16-ascii-fold.md) | ASCII-fold PR (`*2` magics) | **1 candidate** (was 8,000) | Current. Fold at index time. |
| [2026-08-15](2026-08-15-v0.1.1.md) | `0.1.1` (`30c2776`) | 8,000 candidates | Mmap load tax gone; fold still broken. |
| [2026-08-15](2026-08-15-v0.1.0.md) | `0.1.0` (`532d51c`) | 8,000 candidates | Baseline. Open walked every posting list. |

Current numbers for this tree: [2026-08-16-ascii-fold.md](2026-08-16-ascii-fold.md).
