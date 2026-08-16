#!/usr/bin/env bash
# ─────────────────────────────────────────────────────────────────────────────
# Benchmark: rgx vs ripgrep vs grep
#
# Builds a synthetic corpus and times a variety of regex patterns with all
# three tools. Reports a formatted comparison table with speedup ratios.
#
# Environment variables (all optional):
#   BIN          Path to rgx binary          (default: target/release/rgx)
#   RG           Path to rg binary           (default: auto-detect)
#   GREP         Path to grep binary         (default: auto-detect)
#   CORPUS       Corpus directory            (default: /tmp/rgx-bench-corpus)
#   N_FILES      Number of files to generate (default: 2000)
#   SEED         Random seed for corpus      (default: 42)
#   ITERS        Best-of-N iterations        (default: 5)
#   WARMUP       Warmup iterations           (default: 2)
#   CSV          Output CSV file path        (default: empty = no CSV)
#   JSON_OUT     Output JSON file path       (default: empty = no JSON)
# ─────────────────────────────────────────────────────────────────────────────
set -euo pipefail
BENCH_START_S=$(date +%s)

# ── Colours ──────────────────────────────────────────────────────────────────
if [[ -t 1 ]]; then
  BOLD='\033[1m'
  DIM='\033[2m'
  GREEN='\033[32m'
  YELLOW='\033[33m'
  CYAN='\033[36m'
  RED='\033[31m'
  RESET='\033[0m'
else
  BOLD='' DIM='' GREEN='' YELLOW='' CYAN='' RED='' RESET=''
fi

# ── Configuration ────────────────────────────────────────────────────────────
BIN="${BIN:-$(pwd)/target/release/rgx}"
RG="${RG:-$(command -v rg 2>/dev/null || true)}"
GREP="${GREP:-$(command -v grep 2>/dev/null || true)}"
CORPUS="${CORPUS:-/tmp/rgx-bench-corpus}"
N_FILES="${N_FILES:-10000}"
SEED="${SEED:-42}"
ITERS="${ITERS:-5}"
WARMUP="${WARMUP:-2}"
CSV="${CSV:-}"
JSON_OUT="${JSON_OUT:-}"

BAR_MAX=20  # max chars for the longest bar in a row

# ── Platform detection ───────────────────────────────────────────────────────
OS="$(uname -s)"
TIME_CMD=""
TIME_MODE=""  # "macos", "gnu", or "basic"

if [[ "$OS" == "Darwin" ]]; then
  TIME_CMD="/usr/bin/time"
  TIME_MODE="macos"
elif command -v /usr/bin/time &>/dev/null && /usr/bin/time --version 2>&1 | grep -q "GNU"; then
  TIME_CMD="/usr/bin/time"
  TIME_MODE="gnu"
elif command -v gtime &>/dev/null; then
  TIME_CMD="gtime"
  TIME_MODE="gnu"
else
  TIME_MODE="basic"
fi

# ── Header ───────────────────────────────────────────────────────────────────
echo
echo -e "  ${BOLD}rgx × ripgrep × grep${RESET}  benchmark"
echo -e "  ${DIM}──────────────────────────────────${RESET}"
echo

# Build rgx if needed
if [[ ! -x "$BIN" ]]; then
  echo -e "  ${CYAN}▸${RESET} Building rgx (release)…"
  cargo build --release >/dev/null 2>&1
fi

RGX_VER=$("$BIN" --version 2>/dev/null || echo "dev")
echo -e "  ${DIM}rgx${RESET}      ${RGX_VER}"
if [[ -n "$RG" ]]; then
  RG_VER=$("$RG" --version 2>/dev/null | head -1)
  echo -e "  ${DIM}ripgrep${RESET}  ${RG_VER}"
else
  echo -e "  ${DIM}ripgrep${RESET}  ${RED}not found${RESET}"
fi
if [[ -n "$GREP" ]]; then
  GREP_VER=$("$GREP" --version 2>/dev/null | head -1)
  echo -e "  ${DIM}grep${RESET}     ${GREP_VER}"
else
  echo -e "  ${DIM}grep${RESET}     ${RED}not found${RESET}"
fi
echo -e "  ${DIM}os${RESET}       ${OS}  ${DIM}(resource tracking: ${TIME_MODE})${RESET}"
echo

# ── Generate corpus ──────────────────────────────────────────────────────────
echo -e "  ${CYAN}▸${RESET} Generating corpus  ${DIM}(${N_FILES} files, seed=${SEED})${RESET}"
mkdir -p "$CORPUS/src"

python3 - "$CORPUS" "$N_FILES" "$SEED" <<'PY'
import os, random, sys
root, n, seed = sys.argv[1], int(sys.argv[2]), int(sys.argv[3])
random.seed(seed)

# Common words — appear in nearly every file
common = ["fn", "let", "mut", "return", "self", "use", "mod", "pub",
          "impl", "struct", "trait", "enum", "type", "match", "if",
          "else", "for", "while", "loop", "break", "continue"]

# Medium words — sprinkled in ~20% of files
medium = ["HashMap", "BTreeMap", "async", "await", "tokio", "spawn",
          "Arc", "Mutex", "channel", "recv", "serialize", "deserialize",
          "serde", "derive", "Display", "Iterator", "lifetime", "borrow"]

# Rare words — appear in ~2% of files
rare = ["SENTINEL_XYZZY", "phant0m_thread", "__intrinsic_alloc",
        "zwj_codepoint", "QUUX_MARKER_42", "nebula_vortex",
        "hyperdrive_init", "xray_fluorescence"]

exts = [".rs", ".py", ".js", ".go", ".txt"]
for i in range(n):
    lines = []
    for _ in range(random.randint(40, 150)):
        parts = [random.choice(common) for _ in range(random.randint(3, 12))]
        # ~20% chance to include a medium word
        if random.random() < 0.20:
            parts[random.randint(0, len(parts)-1)] = random.choice(medium)
        # ~2% chance to include a rare word
        if random.random() < 0.02:
            parts.insert(random.randint(0, len(parts)), random.choice(rare))
        parts.append(random.choice(["", "", "", ";", "{", "}", "()", ",", "=",
                                     "// comment", "/* block */", "->", "=>",
                                     "123", "0xff", '"literal"']))
        lines.append(" ".join(parts))
    ext = random.choice(exts)
    path = os.path.join(root, "src", f"file_{i:05d}{ext}")
    with open(path, "w") as f:
        f.write("\n".join(lines) + "\n")
PY

CORPUS_SIZE=$(du -sh "$CORPUS" 2>/dev/null | cut -f1 | xargs)
echo -e "  ${CYAN}▸${RESET} Building rgx index"
"$BIN" --build "unused" "$CORPUS" >/dev/null 2>&1 || true
INDEX_SIZE=$(du -sh "$CORPUS/.rgx" 2>/dev/null | cut -f1 | xargs || echo "n/a")
echo -e "    corpus ${BOLD}${CORPUS_SIZE}${RESET}  index ${BOLD}${INDEX_SIZE}${RESET}"
echo

# ── Warm up ──────────────────────────────────────────────────────────────────
echo -e "  ${CYAN}▸${RESET} Warming up  ${DIM}(${WARMUP} rounds)${RESET}"
for (( w=0; w<WARMUP; w++ )); do
  "$BIN" "hello" "$CORPUS" >/dev/null 2>&1 || true
  [[ -n "$RG" ]] && "$RG" "hello" "$CORPUS" >/dev/null 2>&1 || true
  [[ -n "$GREP" ]] && "$GREP" -rn "hello" "$CORPUS" >/dev/null 2>&1 || true
done


# ── Timing helper ────────────────────────────────────────────────────────────
# Runs a command $ITERS times, returns best wall-time, match count,
# peak RSS (MB), and CPU time (user+sys ms).
# Output format:  wall_ms:count:rss_mb:cpu_ms
bench_run() {
  local cmd="$1"; shift
  local best_wall=999999999
  local best_count=0 best_rss="—" best_cpu="—"
  local tmptime tmpout

  for (( i=0; i<ITERS; i++ )); do
    tmptime=$(mktemp)
    tmpout=$(mktemp)

    local count=0 wall_ms=0 rss_mb="—" cpu_ms="—"

    if [[ "$TIME_MODE" == "macos" ]]; then
      # macOS: /usr/bin/time -l  (RSS in bytes)
      TMPOUT="$tmpout" "$TIME_CMD" -l sh -c '"$0" "$@" > "$TMPOUT" 2>/dev/null' \
        "$cmd" "$@" 2>"$tmptime" || true
      count=$(wc -l < "$tmpout" | tr -d ' ')

      local real_s user_s sys_s rss_bytes
      real_s=$(awk '/real/{print $1; exit}' "$tmptime")
      user_s=$(awk '/user/{print $1; exit}' "$tmptime")
      sys_s=$(awk '/sys/{print $1; exit}' "$tmptime")
      rss_bytes=$(awk '/maximum resident set size|peak memory footprint/{print $1; exit}' "$tmptime")

      wall_ms=$(python3 -c "print(int(float('${real_s:-0}') * 1000))")
      cpu_ms=$(python3 -c "print(int((float('${user_s:-0}') + float('${sys_s:-0}')) * 1000))")
      rss_mb=$(python3 -c "print(round(${rss_bytes:-0} / 1048576, 1))")

    elif [[ "$TIME_MODE" == "gnu" ]]; then
      # GNU time: -v flag  (RSS in kilobytes)
      TMPOUT="$tmpout" "$TIME_CMD" -v sh -c '"$0" "$@" > "$TMPOUT" 2>/dev/null' \
        "$cmd" "$@" 2>"$tmptime" || true
      count=$(wc -l < "$tmpout" | tr -d ' ')

      local elapsed user_s sys_s rss_kb
      # Elapsed wall clock: "h:mm:ss" or "m:ss.ss"
      elapsed=$(awk -F': ' '/Elapsed \(wall clock\)/{print $2}' "$tmptime")
      user_s=$(awk -F': ' '/User time/{print $2}' "$tmptime")
      sys_s=$(awk -F': ' '/System time/{print $2}' "$tmptime")
      rss_kb=$(awk -F': ' '/Maximum resident set size/{print $2}' "$tmptime")

      # Convert elapsed "h:mm:ss.ss" or "m:ss.ss" to ms
      wall_ms=$(python3 -c "
parts = '${elapsed:-0}'.split(':')
if len(parts) == 3:
    s = int(parts[0])*3600 + int(parts[1])*60 + float(parts[2])
elif len(parts) == 2:
    s = int(parts[0])*60 + float(parts[1])
else:
    s = float(parts[0])
print(int(s * 1000))
")
      cpu_ms=$(python3 -c "print(int((float('${user_s:-0}') + float('${sys_s:-0}')) * 1000))")
      rss_mb=$(python3 -c "print(round(${rss_kb:-0} / 1024, 1))")

    else
      # Fallback: wall-clock only via perl or date
      local start_us end_us
      if command -v perl &>/dev/null; then
        start_us=$(perl -MTime::HiRes=time -e 'printf "%.0f\n", time()*1000000')
        count=$("$cmd" "$@" 2>/dev/null | tee "$tmpout" | wc -l | tr -d ' ')
        end_us=$(perl -MTime::HiRes=time -e 'printf "%.0f\n", time()*1000000')
        wall_ms=$(( (end_us - start_us) / 1000 ))
      else
        local start_s end_s
        start_s=$(date +%s)
        count=$("$cmd" "$@" 2>/dev/null | tee "$tmpout" | wc -l | tr -d ' ')
        end_s=$(date +%s)
        wall_ms=$(( (end_s - start_s) * 1000 ))
      fi
    fi

    if (( wall_ms < best_wall )); then
      best_wall=$wall_ms
      best_count=$count
      best_rss=$rss_mb
      best_cpu=$cpu_ms
    fi

    rm -f "$tmpout" "$tmptime"
  done

  echo "${best_wall}:${best_count}:${best_rss}:${best_cpu}"
}

# ── Bar renderer ─────────────────────────────────────────────────────────────
# render_bar <value> <max_in_row>  →  prints coloured bar chars
make_bar() {
  local val=$1 max_val=$2
  if (( max_val == 0 )); then echo ""; return; fi
  local len=$(python3 -c "print(max(1, round($val / $max_val * $BAR_MAX)))")
  local bar=""
  for (( b=0; b<len; b++ )); do bar+="█"; done
  echo "$bar"
}

# ── Patterns ─────────────────────────────────────────────────────────────────
# Grouped by selectivity: how many files the pattern matches.
# "common" = nearly all files  →  index can't prune much
# "selective" = ~20% of files  →  index prunes most candidates
# "rare" = ~2% of files        →  index eliminates almost everything
PATTERNS=(
  "common|fn return"
  "common|impl.*struct"
  "common|match.*enum"
  "selective|HashMap.*BTreeMap"
  "selective|async.*await"
  "selective|serialize.*derive"
  "selective|tokio.*spawn"
  "rare|SENTINEL_XYZZY"
  "rare|phant0m_thread"
  "rare|zwj_codepoint.*QUUX"
  "rare|nebula_vortex"
  "rare|hyperdrive_init"
)

# ── Collect results ──────────────────────────────────────────────────────────
declare -a R_CAT=() R_PAT=()
declare -a R_RGX_MS=() R_RG_MS=() R_GREP_MS=()
declare -a R_RGX_N=() R_RG_N=() R_GREP_N=()
declare -a R_RGX_RSS=() R_RG_RSS=() R_GREP_RSS=()
declare -a R_RGX_CPU=() R_RG_CPU=() R_GREP_CPU=()

echo -e "  ${CYAN}▸${RESET} Running benchmarks  ${DIM}(best-of-${ITERS})${RESET}"
echo

# Helper to parse bench_run output: wall:count:rss:cpu
parse_result() {
  local result="$1" field="$2"
  case "$field" in
    wall)  echo "$result" | cut -d: -f1 ;;
    count) echo "$result" | cut -d: -f2 ;;
    rss)   echo "$result" | cut -d: -f3 ;;
    cpu)   echo "$result" | cut -d: -f4 ;;
  esac
}

idx=0
for entry in "${PATTERNS[@]}"; do
  category="${entry%%|*}"
  pattern="${entry#*|}"
  R_CAT+=("$category")
  R_PAT+=("$pattern")

  echo -ne "\r    ${DIM}[$((idx+1))/${#PATTERNS[@]}]${RESET} ${pattern}                    \r"

  result=$(bench_run "$BIN" "$pattern" "$CORPUS")
  R_RGX_MS+=("$(parse_result "$result" wall)")
  R_RGX_N+=("$(parse_result "$result" count)")
  R_RGX_RSS+=("$(parse_result "$result" rss)")
  R_RGX_CPU+=("$(parse_result "$result" cpu)")

  if [[ -n "$RG" ]]; then
    result=$(bench_run "$RG" "$pattern" "$CORPUS")
    R_RG_MS+=("$(parse_result "$result" wall)")
    R_RG_N+=("$(parse_result "$result" count)")
    R_RG_RSS+=("$(parse_result "$result" rss)")
    R_RG_CPU+=("$(parse_result "$result" cpu)")
  else
    R_RG_MS+=("—"); R_RG_N+=("—"); R_RG_RSS+=("—"); R_RG_CPU+=("—")
  fi

  if [[ -n "$GREP" ]]; then
    result=$(bench_run "$GREP" -rn "$pattern" "$CORPUS")
    R_GREP_MS+=("$(parse_result "$result" wall)")
    R_GREP_N+=("$(parse_result "$result" count)")
    R_GREP_RSS+=("$(parse_result "$result" rss)")
    R_GREP_CPU+=("$(parse_result "$result" cpu)")
  else
    R_GREP_MS+=("—"); R_GREP_N+=("—"); R_GREP_RSS+=("—"); R_GREP_CPU+=("—")
  fi

  idx=$((idx + 1))
done
echo -e "\r                                                        \r"

# ══════════════════════════════════════════════════════════════════════════════
# OUTPUT
# ══════════════════════════════════════════════════════════════════════════════

SEP="  ${DIM}──────────────────────────────────────────────────────────────────${RESET}"

# ── Performance table ────────────────────────────────────────────────────────
echo -e "  ${BOLD}Performance${RESET}  ${DIM}(best-of-${ITERS}, ${WARMUP} warmup)${RESET}"
echo -e "$SEP"
# Header
printf "  ${BOLD}%-22s %5s %5s %6s${RESET}  " \
  "pattern" "rgx" "rg" "grep"
echo -e "${GREEN}■${RESET}${DIM}rgx${RESET} ${CYAN}■${RESET}${DIM}rg${RESET} ${YELLOW}■${RESET}${DIM}grep${RESET}"
echo -e "$SEP"

prev_cat=""
total_rgx=0; total_rg=0; total_grep=0; n_rg=0; n_grep=0

for (( i=0; i<${#R_PAT[@]}; i++ )); do
  cat="${R_CAT[$i]}"
  pat="${R_PAT[$i]}"
  rgx_ms="${R_RGX_MS[$i]}"
  rg_ms="${R_RG_MS[$i]}"
  grep_ms="${R_GREP_MS[$i]}"

  # Category header
  if [[ "$cat" != "$prev_cat" ]]; then
    [[ -n "$prev_cat" ]] && echo
    echo -e "  ${DIM}▸ ${cat}${RESET}"
    prev_cat="$cat"
  fi

  # Totals
  total_rgx=$((total_rgx + rgx_ms))
  if [[ "$rg_ms" != "—" ]]; then
    total_rg=$((total_rg + rg_ms)); n_rg=$((n_rg + 1))
  fi
  if [[ "$grep_ms" != "—" ]]; then
    total_grep=$((total_grep + grep_ms)); n_grep=$((n_grep + 1))
  fi

  # Row max for bar scaling
  row_max=$rgx_ms
  [[ "$rg_ms"   != "—" ]] && (( rg_ms   > row_max )) && row_max=$rg_ms
  [[ "$grep_ms" != "—" ]] && (( grep_ms > row_max )) && row_max=$grep_ms

  # Build bars
  bar_rgx=$(make_bar "$rgx_ms" "$row_max")
  bar_rg=""
  bar_grep=""
  [[ "$rg_ms"   != "—" ]] && bar_rg=$(make_bar "$rg_ms" "$row_max")
  [[ "$grep_ms" != "—" ]] && bar_grep=$(make_bar "$grep_ms" "$row_max")

  # Truncate pattern for display
  pat_d="$pat"
  (( ${#pat_d} > 22 )) && pat_d="${pat_d:0:20}.."

  # Print data columns
  printf "  %-22s %5s %5s %6s  " "$pat_d" "$rgx_ms" "$rg_ms" "$grep_ms"

  # Print composite bar (3 coloured segments)
  echo -ne "${GREEN}${bar_rgx}${RESET}"
  if [[ -n "$bar_rg" ]]; then
    echo -ne " ${CYAN}${bar_rg}${RESET}"
  fi
  if [[ -n "$bar_grep" ]]; then
    echo -ne " ${YELLOW}${bar_grep}${RESET}"
  fi
  echo
done

echo -e "$SEP"
echo

# ── Resource usage table ─────────────────────────────────────────────────────
echo -e "  ${BOLD}Resource usage${RESET}  ${DIM}(from fastest run)${RESET}"
echo -e "$SEP"
printf "  ${BOLD}%-22s  %6s %6s %6s  %6s %6s %6s${RESET}\n" \
  "pattern" "rgx" "rg" "grep" "rgx" "rg" "grep"
printf "  ${BOLD}%-22s  ${DIM}%6s %6s %6s  %6s %6s %6s${RESET}\n" \
  "" "cpu ms" "cpu ms" "cpu ms" "MB" "MB" "MB"
echo -e "$SEP"

prev_cat_r=""
for (( i=0; i<${#R_PAT[@]}; i++ )); do
  cat="${R_CAT[$i]}"
  pat="${R_PAT[$i]}"

  if [[ "$cat" != "$prev_cat_r" ]]; then
    [[ -n "$prev_cat_r" ]] && echo
    echo -e "  ${DIM}▸ ${cat}${RESET}"
    prev_cat_r="$cat"
  fi

  pat_d="$pat"
  (( ${#pat_d} > 22 )) && pat_d="${pat_d:0:20}.."

  printf "  %-22s  %6s %6s %6s  %6s %6s %6s\n" \
    "$pat_d" \
    "${R_RGX_CPU[$i]}" "${R_RG_CPU[$i]}" "${R_GREP_CPU[$i]}" \
    "${R_RGX_RSS[$i]}" "${R_RG_RSS[$i]}" "${R_GREP_RSS[$i]}"
done

echo -e "$SEP"
echo

# ── Match verification table ────────────────────────────────────────────────
echo -e "  ${BOLD}Match verification${RESET}"
echo -e "$SEP"
printf "  ${BOLD}%-22s %7s %7s %7s  %s${RESET}\n" \
  "pattern" "rgx" "rg" "grep" ""
echo -e "$SEP"

any_mismatch=0
for (( i=0; i<${#R_PAT[@]}; i++ )); do
  pat="${R_PAT[$i]}"
  rgx_n="${R_RGX_N[$i]}"
  rg_n="${R_RG_N[$i]}"
  grep_n="${R_GREP_N[$i]}"

  # Check matches
  status="${GREEN}✓${RESET}"
  if [[ "$rg_n" != "—" && "$rgx_n" != "$rg_n" ]]; then
    status="${YELLOW}≠${RESET}"; any_mismatch=1
  fi
  if [[ "$grep_n" != "—" && "$rgx_n" != "$grep_n" ]]; then
    status="${YELLOW}≠${RESET}"; any_mismatch=1
  fi

  pat_d="$pat"
  (( ${#pat_d} > 22 )) && pat_d="${pat_d:0:20}.."

  printf "  %-22s %7s %7s %7s  " "$pat_d" "$rgx_n" "$rg_n" "$grep_n"
  echo -e "$status"
done

echo -e "$SEP"
if (( any_mismatch )); then
  echo -e "  ${YELLOW}≠${RESET} ${DIM}count differs — may be due to ignore rules or regex flavour${RESET}"
fi
echo

# ── Summary ──────────────────────────────────────────────────────────────────
echo -e "  ${BOLD}Summary${RESET}"
echo -e "$SEP"
printf "    ${GREEN}rgx${RESET}     %6s ms\n" "$total_rgx"
if [[ $n_rg -gt 0 ]]; then
  ratio_rg=$(python3 -c "
r = $total_rgx / $total_rg
print(f'{r:.2f}× ratio' if r >= 1 else f'{1/r:.1f}× faster')
")
  printf "    ${CYAN}rg ${RESET}     %6s ms   ${DIM}%s${RESET}\n" "$total_rg" "$ratio_rg"
fi
if [[ $n_grep -gt 0 ]]; then
  ratio_grep=$(python3 -c "
r = $total_rgx / $total_grep
print(f'{r:.2f}× ratio' if r >= 1 else f'{1/r:.1f}× faster')
")
  printf "    ${YELLOW}grep${RESET}    %6s ms   ${DIM}%s${RESET}\n" "$total_grep" "$ratio_grep"
fi

echo
echo -e "    corpus ${BOLD}${CORPUS_SIZE}${RESET}  ${DIM}(${N_FILES} files)${RESET}  index ${BOLD}${INDEX_SIZE}${RESET}  ${DIM}best-of-${ITERS}${RESET}"
echo -e "$SEP"

# ── CSV export ───────────────────────────────────────────────────────────────
if [[ -n "$CSV" ]]; then
  {
    echo "category,pattern,rgx_ms,rg_ms,grep_ms,rgx_matches,rg_matches,grep_matches"
    for (( i=0; i<${#R_PAT[@]}; i++ )); do
      echo "${R_CAT[$i]},\"${R_PAT[$i]}\",${R_RGX_MS[$i]},${R_RG_MS[$i]//—/},${R_GREP_MS[$i]//—/},${R_RGX_N[$i]},${R_RG_N[$i]//—/},${R_GREP_N[$i]//—/}"
    done
  } > "$CSV"
  echo -e "    ${DIM}CSV  → ${CSV}${RESET}"
fi

# ── JSON export ──────────────────────────────────────────────────────────────
if [[ -n "$JSON_OUT" ]]; then
  python3 - "$JSON_OUT" "$CORPUS" "$CORPUS_SIZE" "$INDEX_SIZE" \
    "$N_FILES" "$SEED" "$ITERS" "$WARMUP" \
    "${R_CAT[@]}" "---" \
    "${R_PAT[@]}" "---" \
    "${R_RGX_MS[@]}" "---" "${R_RG_MS[@]}" "---" "${R_GREP_MS[@]}" "---" \
    "${R_RGX_N[@]}" "---" "${R_RG_N[@]}" "---" "${R_GREP_N[@]}" \
    <<'PYEOF' 2>/dev/null && echo -e "    ${DIM}JSON → ${JSON_OUT}${RESET}" || echo -e "    ${RED}JSON export failed${RESET}"
import json, sys
args = sys.argv[1:]
out_path = args[0]
corpus, corpus_size, index_size = args[1], args[2], args[3]
n_files, seed, iters, warmup = int(args[4]), int(args[5]), int(args[6]), int(args[7])
rest = args[8:]
groups = []
g = []
for v in rest:
    if v == "---":
        groups.append(g); g = []
    else:
        g.append(v)
groups.append(g)
cats, pats = groups[0], groups[1]
rgx_ms, rg_ms, grep_ms = groups[2], groups[3], groups[4]
rgx_n, rg_n, grep_n = groups[5], groups[6], groups[7]
n = lambda v: None if v == "—" else int(v)
results = [{"category": cats[i], "pattern": pats[i],
            "rgx_ms": int(rgx_ms[i]), "rg_ms": n(rg_ms[i]), "grep_ms": n(grep_ms[i]),
            "rgx_matches": int(rgx_n[i]), "rg_matches": n(rg_n[i]), "grep_matches": n(grep_n[i])}
           for i in range(len(cats))]
with open(out_path, "w") as f:
    json.dump({"meta": {"corpus_path": corpus, "corpus_size": corpus_size,
               "index_size": index_size, "n_files": n_files, "seed": seed,
               "iterations": iters, "warmup": warmup}, "results": results}, f, indent=2)
PYEOF
fi

BENCH_END_S=$(date +%s)
BENCH_ELAPSED=$((BENCH_END_S - BENCH_START_S))
BENCH_MIN=$((BENCH_ELAPSED / 60))
BENCH_SEC=$((BENCH_ELAPSED % 60))
echo
if (( BENCH_MIN > 0 )); then
  echo -e "  ${DIM}Done in ${BENCH_MIN}m ${BENCH_SEC}s.${RESET}"
else
  echo -e "  ${DIM}Done in ${BENCH_SEC}s.${RESET}"
fi
echo