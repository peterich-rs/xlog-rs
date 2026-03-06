#!/usr/bin/env bash
set -euo pipefail

messages=20000
mode=async
compress=zlib
msg_size=96
threads=1
flush_every=0
cache_days=0
max_file_size=0
runs=3
out_root=""

usage() {
  cat <<'EOF'
Usage:
  scripts/xlog/run_backend_compare.sh --out-root <dir> [options]

Options:
  --out-root <dir>        Output root for all benchmark artifacts (required)
  --messages <n>          Total messages per run (default: 20000)
  --mode <async|sync>     Appender mode (default: async)
  --compress <zlib|zstd>  Compression mode (default: zlib)
  --msg-size <n>          Payload bytes per message (default: 96)
  --threads <n>           Writer threads (default: 1)
  --flush-every <n>       Async flush cadence per thread (default: 0)
  --cache-days <n>        Cache retention days (default: 0)
  --max-file-size <n>     Max logfile size in bytes (default: 0)
  --runs <n>              Runs per backend (default: 3)
EOF
}

while [[ $# -gt 0 ]]; do
  case "$1" in
    --out-root)
      out_root="$2"
      shift 2
      ;;
    --messages)
      messages="$2"
      shift 2
      ;;
    --mode)
      mode="$2"
      shift 2
      ;;
    --compress)
      compress="$2"
      shift 2
      ;;
    --msg-size)
      msg_size="$2"
      shift 2
      ;;
    --threads)
      threads="$2"
      shift 2
      ;;
    --flush-every)
      flush_every="$2"
      shift 2
      ;;
    --cache-days)
      cache_days="$2"
      shift 2
      ;;
    --max-file-size)
      max_file_size="$2"
      shift 2
      ;;
    --runs)
      runs="$2"
      shift 2
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

mkdir -p "$out_root"

run_backend() {
  local feature="$1"
  local label="$2"
  local results_file="$out_root/results_${label}_${mode}.jsonl"
  rm -f "$results_file"

  for run in $(seq 1 "$runs"); do
    local run_dir="$out_root/${label}-${mode}-run${run}"
    local cache_dir="$out_root/${label}-${mode}-cache${run}"
    rm -rf "$run_dir" "$cache_dir"

    local cmd=(
      cargo run --release -p mars-xlog --example bench_backend
      --no-default-features --features "$feature" --
      --out-dir "$run_dir"
      --prefix "${label}-${mode}"
      --messages "$messages"
      --mode "$mode"
      --compress "$compress"
      --msg-size "$msg_size"
      --threads "$threads"
      --flush-every "$flush_every"
      --cache-days "$cache_days"
      --max-file-size "$max_file_size"
    )
    if [[ "$cache_days" -gt 0 ]]; then
      cmd+=(--cache-dir "$cache_dir")
    fi

    "${cmd[@]}" | tee -a "$results_file"
  done
}

run_backend rust-backend rust
run_backend cpp-backend cpp
