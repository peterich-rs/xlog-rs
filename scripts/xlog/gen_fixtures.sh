#!/usr/bin/env bash
set -euo pipefail

usage() {
  cat <<'EOF'
Generate Mars xlog migration fixtures (Phase 2C).

Usage:
  scripts/xlog/gen_fixtures.sh [--out-dir <dir>] [--backend rust] [--count <n>] [--include-crypt]

Options:
  --out-dir <dir>     Output root directory (default: artifacts/xlog-fixtures/<timestamp>)
  --backend <value>   rust (default: rust)
  --count <n>         Number of emitted records per case (default: 16)
  --include-crypt     Also generate crypt cases (requires decoder env with py2 + pyelliptic)
  -h, --help          Show this help text
EOF
}

script_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
repo_root="$(cd "${script_dir}/../.." && pwd)"

out_dir=""
backend="rust"
count=16
include_crypt=0

while (($# > 0)); do
  case "$1" in
    --out-dir)
      out_dir="${2:-}"
      shift 2
      ;;
    --backend)
      backend="${2:-}"
      shift 2
      ;;
    --count)
      count="${2:-}"
      shift 2
      ;;
    --include-crypt)
      include_crypt=1
      shift
      ;;
    -h|--help)
      usage
      exit 0
      ;;
    *)
      echo "unknown argument: $1" >&2
      usage
      exit 2
      ;;
  esac
done

if [[ -z "$out_dir" ]]; then
  ts="$(date +%Y%m%d-%H%M%S)"
  out_dir="${repo_root}/artifacts/xlog-fixtures/${ts}"
fi

if ! [[ "$count" =~ ^[0-9]+$ ]]; then
  echo "--count must be a positive integer" >&2
  exit 2
fi

if [[ "$backend" != "rust" ]]; then
  echo "--backend must be: rust" >&2
  exit 2
fi

mkdir -p "$out_dir"
manifest="${out_dir}/manifest.tsv"
printf "backend\tcompress\tmode\tcrypt\tprefix\tcount\tdir\n" > "$manifest"

pub_key="572d1e2710ae5fbca54c76a382fdd44050b3a675cb2bf39feebe85ef63d947aff0fa4943f1112e8b6af34bebebbaefa1a0aae055d9259b89a1858f7cc9af9df1"

run_case() {
  local selected_backend="$1"
  local compress="$2"
  local mode="$3"
  local crypt="$4"

  local prefix="${selected_backend}_${compress}_${mode}_${crypt}"
  local case_dir="${out_dir}/${selected_backend}/${prefix}"
  mkdir -p "$case_dir"

  local cmd=(
    cargo run -p mars-xlog --example gen_fixture
    --no-default-features
    --features "rust-backend"
    --
    --out-dir "$case_dir"
    --prefix "$prefix"
    --mode "$mode"
    --compress "$compress"
    --count "$count"
  )

  if [[ "$crypt" == "crypt" ]]; then
    cmd+=(--pub-key "$pub_key")
  fi

  echo "[gen] backend=$selected_backend compress=$compress mode=$mode crypt=$crypt"
  (cd "$repo_root" && "${cmd[@]}")

  printf "%s\t%s\t%s\t%s\t%s\t%s\t%s\n" \
    "$selected_backend" "$compress" "$mode" "$crypt" "$prefix" "$count" "$case_dir" >> "$manifest"
}

run_backend_matrix() {
  local selected_backend="$1"
  run_case "$selected_backend" "zlib" "sync" "nocrypt"
  run_case "$selected_backend" "zlib" "async" "nocrypt"
  run_case "$selected_backend" "zstd" "sync" "nocrypt"
  run_case "$selected_backend" "zstd" "async" "nocrypt"

  if [[ "$include_crypt" == "1" ]]; then
    run_case "$selected_backend" "zlib" "sync" "crypt"
    run_case "$selected_backend" "zlib" "async" "crypt"
    run_case "$selected_backend" "zstd" "sync" "crypt"
    run_case "$selected_backend" "zstd" "async" "crypt"
  fi
}

run_backend_matrix "$backend"

echo
echo "fixture root: $out_dir"
echo "manifest: $manifest"
