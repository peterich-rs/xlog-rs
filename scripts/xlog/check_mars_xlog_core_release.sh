#!/usr/bin/env bash
set -euo pipefail

usage() {
  cat <<'USAGE'
Run release preflight checks for mars-xlog-core.

Usage:
  scripts/xlog/check_mars_xlog_core_release.sh [options]

Options:
  --out-dir <dir>     Artifact root (default: artifacts/release/<timestamp>-mars-xlog-core)
  --allow-dirty       Allow cargo package/publish dry-run on a dirty worktree
  --skip-tests        Skip cargo test
  -h, --help          Show help
USAGE
}

script_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
repo_root="$(cd "${script_dir}/../.." && pwd)"

timestamp="$(date +"%Y%m%d-%H%M%S")"
out_dir="${repo_root}/artifacts/release/${timestamp}-mars-xlog-core"
allow_dirty=0
skip_tests=0

while (($# > 0)); do
  case "$1" in
    --out-dir)
      out_dir="$2"
      shift 2
      ;;
    --allow-dirty)
      allow_dirty=1
      shift
      ;;
    --skip-tests)
      skip_tests=1
      shift
      ;;
    -h|--help)
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

mkdir -p "$out_dir/logs"
summary_file="$out_dir/summary.md"
status_file="$out_dir/status.txt"

cargo_dirty_args=()
if [[ "$allow_dirty" -eq 1 ]]; then
  cargo_dirty_args+=(--allow-dirty)
fi

package_list_cmd=(cargo package -p mars-xlog-core --locked --list)
publish_dry_run_cmd=(cargo publish --dry-run -p mars-xlog-core --locked)
if [[ "$allow_dirty" -eq 1 ]]; then
  package_list_cmd+=(--allow-dirty)
  publish_dry_run_cmd+=(--allow-dirty)
fi

failures=0
last_exit_code=0
failed_steps=()

run_step() {
  local name="$1"
  shift
  local logfile="$out_dir/logs/${name}.log"
  echo "[release-core] RUN ${name}: $*" | tee -a "$summary_file"
  if "$@" >"$logfile" 2>&1; then
    echo "[release-core] PASS ${name}" | tee -a "$summary_file"
    last_exit_code=0
  else
    last_exit_code=$?
    failures=$((failures + 1))
    failed_steps+=("${name}:${last_exit_code}")
    echo "[release-core] FAIL ${name} (exit=${last_exit_code}, log=${logfile})" | tee -a "$summary_file"
  fi
}

: > "$summary_file"
echo "# mars-xlog-core Release Preflight" >> "$summary_file"
echo >> "$summary_file"
echo "- artifact_root: \`$out_dir\`" >> "$summary_file"
echo "- worktree: \`$(git -C "$repo_root" status --short | wc -l | tr -d ' ')\` changed paths" >> "$summary_file"
echo "- allow_dirty: \`$allow_dirty\`" >> "$summary_file"
echo >> "$summary_file"

run_step package_list "${package_list_cmd[@]}"
if [[ "$last_exit_code" -eq 0 ]]; then
  cp "$out_dir/logs/package_list.log" "$out_dir/package_list.txt"
fi

run_step publish_dry_run "${publish_dry_run_cmd[@]}"

run_step rustdoc_missing_docs cargo rustdoc -p mars-xlog-core --lib -- -D missing-docs
if [[ "$last_exit_code" -ne 0 ]]; then
  if command -v rg >/dev/null 2>&1; then
    docs_missing_count="$(rg -c "error: missing documentation" "$out_dir/logs/rustdoc_missing_docs.log" || true)"
  else
    docs_missing_count="$(grep -c "error: missing documentation" "$out_dir/logs/rustdoc_missing_docs.log" || true)"
  fi
  echo "- rustdoc missing-docs errors: \`${docs_missing_count:-unknown}\`" >> "$summary_file"
fi

if [[ "$skip_tests" -eq 0 ]]; then
  run_step tests cargo test -p mars-xlog-core
fi

echo >> "$summary_file"
if [[ "$failures" -eq 0 ]]; then
  echo "release preflight: success" | tee "$status_file" >> "$summary_file"
  exit 0
fi

echo "release preflight: failed (${failures})" | tee "$status_file" >> "$summary_file"
echo "- failed_steps: \`${failed_steps[*]}\`" >> "$summary_file"
exit 1
