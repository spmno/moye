#!/usr/bin/env bash
# scripts/coverage.sh — per-file 100% coverage gate + baseline tracking.
#
# GAP-5 fix: the 100% line-coverage gate applies ONLY to new/modified files
# since the baseline commit; existing files are NOT retroactively gated. The
# baseline lcov is recorded on first run. Coverage of existing files must
# never decrease (stale_state: "baseline 记录后不变,后续只升不降").
#
# Required cargo subcommands (installed once via `cargo install`, NOT
# Cargo.toml dev-deps — they are binaries, not libraries):
#   cargo install cargo-nextest cargo-llvm-cov
#
# Usage:
#   bash scripts/coverage.sh            # record baseline (1st run) / gate (later)
#   bash scripts/coverage.sh --check    # gate only, never update baseline lcov
#
# Exit codes: 0 = gate passed; 1 = gate failed (coverage regression or < 100%).
set -euo pipefail

EVIDENCE_DIR=".omo/evidence"
LCOV_PATH="${EVIDENCE_DIR}/task-1-dsh-borrow-refactor.lcov"
BASELINE_REF="${EVIDENCE_DIR}/coverage-baseline-ref"
BASELINE_LCOV="${EVIDENCE_DIR}/coverage-baseline.lcov"
NEW_FILE_THRESHOLD=100  # percent line coverage required for new/modified files

die() { echo "[coverage] error: $*" >&2; exit 1; }

# Emit one line "LF:LH" for a given source file path by scanning an lcov file.
# lcov record groups: SF:<path> ... LF:<n> ... LH:<n> ... endf:
lcov_file_stats() {
  local lcov="$1" target="$2"
  # cargo-llvm-cov emits paths like "src/main.rs" (project-relative). Match on
  # the trailing path to be robust to absolute/relative variation.
  awk -v t="$target" '
    function norm(p) { sub(/^\.\//, "", p); return p }
    $0 ~ "^SF:" {
      sf = $0; sub(/^SF:/, "", sf); sf = norm(sf)
      in_rec = (sf == t) || (index(sf, t) == length(sf) - length(t) + 1 && length(t) > 0)
      lf = ""; lh = ""
    }
    $0 ~ "^LF:" && in_rec { lf = $0; sub(/^LF:/, "", lf) }
    $0 ~ "^LH:" && in_rec { lh = $0; sub(/^LH:/, "", lh) }
    $0 == "end_of_record" && in_rec { print lf ":" lh; in_rec = 0 }
  ' "$lcov"
}

main() {
  local check_only=0
  if [[ "${1:-}" == "--check" ]]; then check_only=1; fi

  [[ -f Cargo.toml ]] || die "must run from project root (Cargo.toml not found)"
  command -v cargo >/dev/null || die "cargo not found"
  cargo llvm-cov --help >/dev/null 2>&1 || \
    die "cargo-llvm-cov subcommand missing — run: cargo install cargo-llvm-cov"

  mkdir -p "$EVIDENCE_DIR"

  # Step 1: generate coverage via cargo-llvm-cov (runs the test suite under
  # nextest if available, else the default harness). This is the slow step.
  echo "[coverage] generating lcov via cargo-llvm-cov (may take a minute)..."
  if ! cargo llvm-cov --lcov --output-path "$LCOV_PATH" --workspace; then
    die "cargo llvm-cov failed — see output above"
  fi
  [[ -s "$LCOV_PATH" ]] || die "lcov output is empty: $LCOV_PATH"

  # Step 2: first run establishes the baseline (no gating yet — there are no
  # files "new/modified since baseline" when the baseline is being created).
  if [[ ! -f "$BASELINE_REF" ]]; then
    git rev-parse HEAD > "$BASELINE_REF"
    cp "$LCOV_PATH" "$BASELINE_LCOV"
    local base; base=$(cat "$BASELINE_REF")
    echo "[coverage] baseline commit: $base"
    echo "[coverage] baseline lcov:  $BASELINE_LCOV"
    echo "[coverage] current lcov:    $LCOV_PATH"
    echo "[coverage] no new/modified files to gate (baseline just established)"
    echo "coverage gate passed"
    exit 0
  fi

  local base; base=$(cat "$BASELINE_REF")
  echo "[coverage] baseline commit: $base"

  # Step 3: enumerate new/modified .rs files under src/ and tests/ since baseline.
  # --diff-filter=AM = Added + Modified (not Deleted).
  local -a new_files=()
  while IFS= read -r line; do
    [[ -n "$line" ]] && new_files+=("$line")
  done < <(git diff --name-only --diff-filter=AM "$base" -- 'src/' 'tests/' 2>/dev/null | grep '\.rs$' || true)

  local failed=0

  if [[ ${#new_files[@]} -eq 0 ]]; then
    echo "[coverage] no new/modified Rust files since baseline"
  else
    echo "[coverage] gating ${#new_files[@]} new/modified file(s) at ${NEW_FILE_THRESHOLD}% line coverage:"
    local f lf lh pct
    for f in "${new_files[@]}"; do
      echo "  - $f"
      IFS=':' read -r lf lh <<< "$(lcov_file_stats "$LCOV_PATH" "$f")"
      if [[ -z "${lf:-}" ]]; then
        echo "    [WARN] no coverage record (uninstrumented or test-only); skipping"
        continue
      fi
      if [[ "$lf" -eq 0 ]]; then
        echo "    [WARN] 0 instrumented lines; skipping"
        continue
      fi
      pct=$(( lh * 100 / lf ))
      if [[ "$pct" -lt "$NEW_FILE_THRESHOLD" ]]; then
        echo "    FAIL: ${lh}/${lf} lines hit (${pct}%) < ${NEW_FILE_THRESHOLD}%"
        failed=1
      else
        echo "    OK:   ${lh}/${lf} lines hit (${pct}%)"
      fi
    done
  fi

  # Step 4: stale_state guard — coverage of pre-existing files must not drop
  # below the baseline ratio (only-raise-never-fall). Skipped on first run.
  if [[ -f "$BASELINE_LCOV" ]]; then
    local regressions=0
    echo "[coverage] checking pre-existing files for coverage regressions vs baseline..."
    # Iterate every SF in the baseline lcov; compare baseline LH/LF vs current.
    while IFS= read -r sf; do
      sf="${sf#SF:}"
      [[ "$sf" == /* ]] && sf="${sf#$(pwd)/}"
      local b_lf b_lh c_lf c_lh
      IFS=':' read -r b_lf b_lh <<< "$(lcov_file_stats "$BASELINE_LCOV" "$sf")"
      IFS=':' read -r c_lf c_lh <<< "$(lcov_file_stats "$LCOV_PATH" "$sf")"
      [[ -z "${b_lf:-}" || -z "${c_lf:-}" ]] && continue
      [[ "$b_lf" -eq 0 || "$c_lf" -eq 0 ]] && continue
      local b_pct c_pct
      b_pct=$(( b_lh * 1000 / b_lf ))   # permille for resolution
      c_pct=$(( c_lh * 1000 / c_lf ))
      if [[ "$c_pct" -lt "$b_pct" ]]; then
        echo "    REGRESSION: $sf ${b_lh}/${b_lf} -> ${c_lh}/${c_lf}"
        regressions=1
      fi
    done < <(grep '^SF:' "$BASELINE_LCOV" || true)
    if [[ "$regressions" -ne 0 ]]; then
      echo "[coverage] coverage regression(s) detected on pre-existing files"
      failed=1
    else
      echo "[coverage] no coverage regressions on pre-existing files"
    fi
  fi

  # Refresh the baseline lcov snapshot when not in --check mode and the gate
  # passed (so the next run compares against the latest good state).
  if [[ "$check_only" -eq 0 && "$failed" -eq 0 ]]; then
    cp "$LCOV_PATH" "$BASELINE_LCOV"
    git rev-parse HEAD > "$BASELINE_REF"
  fi

  if [[ "$failed" -eq 0 ]]; then
    echo "coverage gate passed"
    exit 0
  fi
  echo "coverage gate FAILED" >&2
  exit 1
}

main "$@"
