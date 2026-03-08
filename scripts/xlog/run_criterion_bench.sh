#!/usr/bin/env bash
set -euo pipefail

usage() {
  cat <<'USAGE'
Run Criterion benchmarks and export a normalized baseline/current summary.

Usage:
  scripts/xlog/run_criterion_bench.sh --out-root <dir> [options]

Options:
  --out-root <dir>        Output directory for exported summary/report (required)
  --baseline-name <name>  Criterion baseline/profile name to save/analyze
                          (default: basename of out-root)
  --criterion-root <dir>  Criterion root directory (default: target/criterion)
  --cargo-target-dir <d>  Override CARGO_TARGET_DIR
  --skip-clean            Keep existing criterion root instead of deleting it first
  --help                  Show this help
USAGE
}

out_root=""
baseline_name=""
criterion_root="target/criterion"
cargo_target_dir=""
skip_clean=0

while [[ $# -gt 0 ]]; do
  case "$1" in
    --out-root)
      out_root="$2"
      shift 2
      ;;
    --baseline-name)
      baseline_name="$2"
      shift 2
      ;;
    --criterion-root)
      criterion_root="$2"
      shift 2
      ;;
    --cargo-target-dir)
      cargo_target_dir="$2"
      shift 2
      ;;
    --skip-clean)
      skip_clean=1
      shift
      ;;
    --help|-h)
      usage
      exit 0
      ;;
    *)
      echo "unknown argument: $1" >&2
      usage >&2
      exit 2
      ;;
  esac
done

if [[ -z "$out_root" ]]; then
  echo "--out-root is required" >&2
  usage >&2
  exit 2
fi

repo_root="$(cd "$(dirname "$0")/../.." && pwd)"
out_root="$(python3 - <<'PY' "$out_root"
from pathlib import Path
import sys
print(Path(sys.argv[1]).resolve())
PY
)"

if [[ -z "$baseline_name" ]]; then
  baseline_name="$(basename "$out_root")"
fi

if [[ -n "$cargo_target_dir" ]]; then
  export CARGO_TARGET_DIR="$cargo_target_dir"
  criterion_root="$cargo_target_dir/criterion"
fi

criterion_root="$(python3 - <<'PY' "$criterion_root"
from pathlib import Path
import sys
print(Path(sys.argv[1]).resolve())
PY
)"

mkdir -p "$out_root/logs"

if [[ "$skip_clean" -eq 0 ]]; then
  rm -rf "$criterion_root"
fi

start_utc="$(date -u +%Y-%m-%dT%H:%M:%SZ)"
start_epoch="$(date +%s)"
commit="$(git -C "$repo_root" rev-parse HEAD)"
branch="$(git -C "$repo_root" rev-parse --abbrev-ref HEAD)"
rustc_version="$(rustc -Vv | tr '\n' ';' | sed 's/;$/\n/')"
cargo_version="$(cargo -V)"
hostname_value="$(hostname)"
git_status_path="$out_root/git_status_porcelain.txt"
git -C "$repo_root" status --short > "$git_status_path"

run_bench() {
  local label="$1"
  shift
  echo "[criterion] running $label"
  (
    cd "$repo_root"
    "$@"
  ) 2>&1 | tee "$out_root/logs/${label}.log"
}

run_bench \
  "mars_xlog_core" \
  cargo bench -p mars-xlog-core --bench criterion_components -- --noplot --save-baseline "$baseline_name"

run_bench \
  "mars_xlog" \
  cargo bench -p mars-xlog --bench criterion_write_path -- --noplot --save-baseline "$baseline_name"

python3 "$repo_root/scripts/xlog/analyze_criterion.py" \
  --root "$criterion_root" \
  --profile "$baseline_name" \
  --out-json "$out_root/criterion_summary.json" \
  --out-md "$out_root/criterion_report.md" \
  | tee "$out_root/logs/analyze_criterion.log"

end_utc="$(date -u +%Y-%m-%dT%H:%M:%SZ)"
end_epoch="$(date +%s)"
duration_seconds="$((end_epoch - start_epoch))"

python3 - <<'PY' \
  "$out_root/metadata.json" \
  "$start_utc" \
  "$end_utc" \
  "$duration_seconds" \
  "$baseline_name" \
  "$criterion_root" \
  "$commit" \
  "$branch" \
  "$hostname_value" \
  "$rustc_version" \
  "$cargo_version" \
  "$git_status_path"
import json
import sys
from pathlib import Path

out = Path(sys.argv[1])
git_status_lines = Path(sys.argv[12]).read_text(encoding="utf-8").splitlines()
metadata = {
    "started_at_utc": sys.argv[2],
    "finished_at_utc": sys.argv[3],
    "duration_seconds": int(sys.argv[4]),
    "baseline_name": sys.argv[5],
    "criterion_root": sys.argv[6],
    "git_commit": sys.argv[7],
    "git_branch": sys.argv[8],
    "hostname": sys.argv[9],
    "rustc_version_verbose": sys.argv[10],
    "cargo_version": sys.argv[11],
    "git_dirty": bool(git_status_lines),
    "git_status_porcelain": git_status_lines,
    "commands": [
        "cargo bench -p mars-xlog-core --bench criterion_components -- --noplot --save-baseline " + sys.argv[5],
        "cargo bench -p mars-xlog --bench criterion_write_path -- --noplot --save-baseline " + sys.argv[5],
    ],
}
out.write_text(json.dumps(metadata, ensure_ascii=False, indent=2) + "\n", encoding="utf-8")
PY

summary_line="$(python3 - <<'PY' "$out_root/criterion_summary.json"
import json
import sys
from pathlib import Path
report = json.loads(Path(sys.argv[1]).read_text(encoding='utf-8'))
print(json.dumps({
    'out_root': str(Path(sys.argv[1]).resolve().parent),
    'baseline_name': report['profile'],
    'bench_count': report['bench_count'],
    'criterion_root': report['criterion_root'],
}, ensure_ascii=False))
PY
)"

echo "$summary_line"
