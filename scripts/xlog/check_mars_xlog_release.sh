#!/usr/bin/env bash
set -euo pipefail

usage() {
  cat <<'USAGE'
Run release preflight checks for mars-xlog.

Usage:
  scripts/xlog/check_mars_xlog_release.sh [options]

Options:
  --out-dir <dir>           Artifact root (default: artifacts/release/<timestamp>-mars-xlog)
  --allow-dirty             Allow cargo package/publish dry-run on a dirty worktree
  --skip-tests              Skip cargo test
  --skip-crates-io-check    Skip crates.io visibility check and publish dry-run
  -h, --help                Show help
USAGE
}

script_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
repo_root="$(cd "${script_dir}/../.." && pwd)"

timestamp="$(date +"%Y%m%d-%H%M%S")"
out_dir="${repo_root}/artifacts/release/${timestamp}-mars-xlog"
allow_dirty=0
skip_tests=0
skip_crates_io_check=0

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
    --skip-crates-io-check)
      skip_crates_io_check=1
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

package_list_cmd=(cargo package -p mars-xlog --locked --list)
publish_dry_run_cmd=(cargo publish --dry-run -p mars-xlog --locked)
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
  echo "[release-xlog] RUN ${name}: $*" | tee -a "$summary_file"
  if "$@" >"$logfile" 2>&1; then
    echo "[release-xlog] PASS ${name}" | tee -a "$summary_file"
    last_exit_code=0
  else
    last_exit_code=$?
    failures=$((failures + 1))
    failed_steps+=("${name}:${last_exit_code}")
    echo "[release-xlog] FAIL ${name} (exit=${last_exit_code}, log=${logfile})" | tee -a "$summary_file"
  fi
}

record_blocked_step() {
  local name="$1"
  local reason="$2"
  local logfile="$out_dir/logs/${name}.log"
  : > "$logfile"
  printf '%s\n' "$reason" >> "$logfile"
  failures=$((failures + 1))
  failed_steps+=("${name}:blocked")
  echo "[release-xlog] BLOCK ${name} (${reason})" | tee -a "$summary_file"
}

core_version="$(cargo pkgid -p mars-xlog-core | sed -E 's/.*#.*@([^ ]+)$/\1/')"

: > "$summary_file"
echo "# mars-xlog Release Preflight" >> "$summary_file"
echo >> "$summary_file"
echo "- artifact_root: \`$out_dir\`" >> "$summary_file"
echo "- worktree: \`$(git -C "$repo_root" status --short | wc -l | tr -d ' ')\` changed paths" >> "$summary_file"
echo "- allow_dirty: \`$allow_dirty\`" >> "$summary_file"
echo "- core_dependency: \`mars-xlog-core = ${core_version}\`" >> "$summary_file"
echo >> "$summary_file"

run_step package_list "${package_list_cmd[@]}"
if [[ "$last_exit_code" -eq 0 ]]; then
  cp "$out_dir/logs/package_list.log" "$out_dir/package_list.txt"
fi

run_step rustdoc_missing_docs cargo rustdoc -p mars-xlog --lib --features macros,tracing -- -D missing-docs

if [[ "$skip_tests" -eq 0 ]]; then
  run_step tests cargo test -p mars-xlog --all-features
fi

if [[ "$skip_crates_io_check" -eq 0 ]]; then
  run_step core_dependency_in_registry \
    cargo info "mars-xlog-core@${core_version}" --registry crates-io
fi

if [[ "$skip_crates_io_check" -eq 1 ]]; then
  echo "[release-xlog] SKIP publish_dry_run (requires mars-xlog-core ${core_version} to be visible on crates.io)" | tee -a "$summary_file"
elif [[ "$last_exit_code" -eq 0 ]]; then
  run_step publish_dry_run "${publish_dry_run_cmd[@]}"
else
  record_blocked_step \
    publish_dry_run \
    "mars-xlog-core ${core_version} is not visible to Cargo in the crates.io registry yet; publish mars-xlog-core first and retry after registry propagation"
fi

echo >> "$summary_file"
if [[ "$failures" -eq 0 ]]; then
  echo "release preflight: success" | tee "$status_file" >> "$summary_file"
  exit 0
fi

echo "release preflight: failed (${failures})" | tee "$status_file" >> "$summary_file"
echo "- failed_steps: \`${failed_steps[*]}\`" >> "$summary_file"
exit 1
