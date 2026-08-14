#!/usr/bin/env bash
# Benchmark rgx (indexed search) against ripgrep on a synthetic corpus.
#
# Builds a corpus under TMPDIR with mixed code-like files, warms the rgx
# index, then times a few patterns with both tools. Timings are best-of-3.
set -euo pipefail

BIN="${BIN:-$(pwd)/target/release/rgx}"
RG="$(command -v rg || true)"
CORPUS="${CORPUS:-/tmp/rgx-corpus}"
N_FILES="${N_FILES:-2000}"
SEED="${SEED:-42}"

if [[ ! -x "$BIN" ]]; then
  echo "building rgx (release)..."
  cargo build --release >/dev/null
fi

echo "== corpus =="
mkdir -p "$CORPUS/src"
python3 - "$CORPUS" "$N_FILES" "$SEED" <<'PY'
import os, random, sys
root, n, seed = sys.argv[1], int(sys.argv[2]), int(sys.argv[3])
random.seed(seed)
words = ["hello", "world", "fn", "main", "return", "token", "shared", "index",
         "regex", "search", "query", "build", "scan", "cache", "buffer",
         "compute", "parse", "walk", "match", "sorted", "iterate", "vector"]
for i in range(n):
    lines = []
    for _ in range(random.randint(20, 80)):
        parts = [random.choice(words) for _ in range(random.randint(3, 12))]
        parts.append(random.choice(["", "", "", ";", "{", "}", "()", ",", "="]))
        lines.append(" ".join(parts))
    path = os.path.join(root, "src", f"file_{i:05d}.rs")
    with open(path, "w") as f:
        f.write("\n".join(lines) + "\n")
print(f"wrote {n} files")
PY

best() {
  local cmd="$1"; shift
  local t=999999
  for _ in 1 2 3; do
    local s=$( { /usr/bin/time -p $cmd "$@" >/dev/null; } 2>&1 | awk '/real/{print $2}' )
    # time -p reports seconds as x.xx; convert to ms
    s=$(python3 -c "print(int(float('$s')*1000))")
    if (( s < t )); then t=$s; fi
  done
  echo "$t"
}

echo "== building rgx index =="
"$BIN" --build "unused" "$CORPUS" >/dev/null 2>&1 || true

echo
echo "== warm query (hello) =="
"$BIN" "hello" "$CORPUS" >/dev/null

declare -a PATTERNS=("hello" "fn main" "sorted.*vector" "token [0-9]+" "shared|cache|compute")

echo
printf "%-24s %12s %12s %10s\n" "pattern" "rgx(ms)" "rg(ms)" "speedup"
for pat in "${PATTERNS[@]}"; do
  r=$(best "$BIN" "$pat" "$CORPUS")
  g="n/a"
  if [[ -n "$RG" ]]; then
    g=$(best "$RG" "$pat" "$CORPUS")
  fi
  if [[ "$g" != "n/a" && "$g" -gt 0 ]]; then
    sp=$(python3 -c "print(f'{$r/$g:.2f}x')")
  else
    sp="-"
  fi
  printf "%-24s %12s %12s %10s\n" "$pat" "$r" "$g" "$sp"
done

echo
echo "sizes:"
du -sh "$CORPUS" 2>/dev/null | cut -f1 | xargs echo "  corpus:"
du -sh "$CORPUS/.rgx" 2>/dev/null | cut -f1 | xargs echo "  index:"