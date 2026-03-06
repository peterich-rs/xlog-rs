#!/usr/bin/env bash
set -euo pipefail

usage() {
  cat <<'USAGE'
Run Phase 5 regression/performance checks for Rust default backend rollout.

Usage:
  scripts/xlog/run_phase5_regression.sh [options]

Options:
  --out-dir <dir>         Artifact root (default: artifacts/phase5/<timestamp>)
  --messages <n>          Benchmark message count (default: 20000)
  --mode <async|sync>     Benchmark appender mode (default: async)
  --compress <zlib|zstd>  Benchmark compression mode (default: zlib)
  --threads <n>           Benchmark worker threads (default: 1)
  --flush-every <n>       Async flush cadence per thread (default: 0)
  --count <n>             Record count for phase2c2 fixture generation (default: 32)
  --skip-setup            Skip Python2 decoder setup for phase2c2
  --skip-phase2c2         Skip official decoder compatibility regression
  --skip-bindings         Skip JNI/UniFFI/NAPI cargo checks
  --skip-bench            Skip backend benchmarks
  -h, --help              Show help
USAGE
}

script_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
repo_root="$(cd "${script_dir}/../.." && pwd)"

out_dir=""
messages=20000
mode="async"
compress="zlib"
threads=1
flush_every=0
count=32
skip_setup=0
skip_phase2c2=0
skip_bindings=0
skip_bench=0

while (($# > 0)); do
  case "$1" in
    --out-dir)
      out_dir="${2:-}"
      shift 2
      ;;
    --messages)
      messages="${2:-}"
      shift 2
      ;;
    --mode)
      mode="${2:-}"
      shift 2
      ;;
    --compress)
      compress="${2:-}"
      shift 2
      ;;
    --threads)
      threads="${2:-}"
      shift 2
      ;;
    --flush-every)
      flush_every="${2:-}"
      shift 2
      ;;
    --count)
      count="${2:-}"
      shift 2
      ;;
    --skip-setup)
      skip_setup=1
      shift
      ;;
    --skip-phase2c2)
      skip_phase2c2=1
      shift
      ;;
    --skip-bindings)
      skip_bindings=1
      shift
      ;;
    --skip-bench)
      skip_bench=1
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

if [[ "$mode" != "async" && "$mode" != "sync" ]]; then
  echo "--mode must be async or sync" >&2
  exit 2
fi
if [[ "$compress" != "zlib" && "$compress" != "zstd" ]]; then
  echo "--compress must be zlib or zstd" >&2
  exit 2
fi
if ! [[ "$messages" =~ ^[0-9]+$ ]] || [[ "$messages" -eq 0 ]]; then
  echo "--messages must be a positive integer" >&2
  exit 2
fi
if ! [[ "$threads" =~ ^[0-9]+$ ]] || [[ "$threads" -eq 0 ]]; then
  echo "--threads must be a positive integer" >&2
  exit 2
fi
if ! [[ "$flush_every" =~ ^[0-9]+$ ]]; then
  echo "--flush-every must be a non-negative integer" >&2
  exit 2
fi
if ! [[ "$count" =~ ^[0-9]+$ ]] || [[ "$count" -eq 0 ]]; then
  echo "--count must be a positive integer" >&2
  exit 2
fi

if [[ -z "$out_dir" ]]; then
  ts="$(date +%Y%m%d-%H%M%S)"
  out_dir="${repo_root}/artifacts/phase5/${ts}"
fi

logs_dir="${out_dir}/logs"
bench_dir="${out_dir}/bench"
summary_file="${out_dir}/summary.txt"
metrics_file="${out_dir}/bench_metrics.jsonl"
status_file="${out_dir}/status.txt"

mkdir -p "$logs_dir" "$bench_dir"
: > "$summary_file"
: > "$metrics_file"

failures=0
failed_steps=()
last_exit_code=0

run_step() {
  local name="$1"
  shift
  local logfile="${logs_dir}/${name}.log"

  echo "[phase5] >>> ${name}" | tee -a "$summary_file"
  set +e
  (
    set -o pipefail
    "$@"
  ) 2>&1 | tee "$logfile"
  local code=${PIPESTATUS[0]}
  set -e

  last_exit_code=$code
  if [[ "$code" -eq 0 ]]; then
    echo "[phase5] PASS ${name}" | tee -a "$summary_file"
  else
    echo "[phase5] FAIL ${name} (exit=${code}, log=${logfile})" | tee -a "$summary_file"
    failures=$((failures + 1))
    failed_steps+=("${name}:${code}")
  fi
}

echo "[phase5] artifact_root=${out_dir}" | tee -a "$summary_file"
echo "[phase5] bench messages=${messages}, mode=${mode}, compress=${compress}, threads=${threads}, flush_every=${flush_every}" | tee -a "$summary_file"

if [[ "$skip_phase2c2" -eq 0 ]]; then
  phase2c2_cmd=(
    "${repo_root}/scripts/xlog/run_phase2c2_official.sh"
    --out-dir "${out_dir}/phase2c2_fixtures"
    --backend rust
    --count "$count"
  )
  if [[ "$skip_setup" -eq 1 ]]; then
    phase2c2_cmd+=(--skip-setup)
  fi
  run_step "phase2c2_official" "${phase2c2_cmd[@]}"
fi

if [[ "$skip_bindings" -eq 0 ]]; then
  run_step "check_uniffi_rust" cargo check -p mars-xlog-uniffi --no-default-features --features rust-backend
  run_step "check_android_jni_rust" cargo check -p mars-xlog-android-jni --no-default-features --features rust-backend
  run_step "check_ohos_napi_rust" cargo check -p oh-xlog --no-default-features --features rust-backend
fi

if [[ "$skip_bench" -eq 0 ]]; then
  run_step \
    "bench_rust" \
    cargo run --release -p mars-xlog --example bench_backend --no-default-features --features rust-backend -- \
      --out-dir "${bench_dir}/rust" \
      --prefix "phase5-rust" \
      --messages "$messages" \
      --mode "$mode" \
      --compress "$compress" \
      --threads "$threads" \
      --flush-every "$flush_every"

  if [[ "$last_exit_code" -eq 0 ]]; then
    metric_line="$(grep -E '^\{.*"throughput_mps".*\}$' "${logs_dir}/bench_rust.log" | tail -n 1 || true)"
    if [[ -n "$metric_line" ]]; then
      echo "$metric_line" >> "$metrics_file"
      echo "[phase5] metric rust: ${metric_line}" | tee -a "$summary_file"
    fi
  fi

  run_step \
    "bench_cpp" \
    cargo run --release -p mars-xlog --example bench_backend --no-default-features --features cpp-backend -- \
      --out-dir "${bench_dir}/cpp" \
      --prefix "phase5-cpp" \
      --messages "$messages" \
      --mode "$mode" \
      --compress "$compress" \
      --threads "$threads" \
      --flush-every "$flush_every"

  if [[ "$last_exit_code" -eq 0 ]]; then
    metric_line="$(grep -E '^\{.*"throughput_mps".*\}$' "${logs_dir}/bench_cpp.log" | tail -n 1 || true)"
    if [[ -n "$metric_line" ]]; then
      echo "$metric_line" >> "$metrics_file"
      echo "[phase5] metric cpp: ${metric_line}" | tee -a "$summary_file"
    fi
  fi
fi

if [[ "$failures" -eq 0 ]]; then
  echo "phase5 regression: success" > "$status_file"
  echo "[phase5] success" | tee -a "$summary_file"
  echo "[phase5] summary=${summary_file}"
  echo "[phase5] metrics=${metrics_file}"
  exit 0
fi

echo "phase5 regression: failed (${failures})" > "$status_file"
echo "[phase5] failed_steps=${failed_steps[*]}" | tee -a "$summary_file"
echo "[phase5] summary=${summary_file}"
echo "[phase5] metrics=${metrics_file}"
exit 1
