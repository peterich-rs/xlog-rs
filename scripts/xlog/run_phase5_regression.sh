#!/usr/bin/env bash
set -euo pipefail

usage() {
  cat <<'EOF'
Run Phase 5 regression/performance checks for Rust default backend rollout.

Usage:
  scripts/xlog/run_phase5_regression.sh [options]

Options:
  --out-dir <dir>               Artifact root (default: artifacts/phase5/<timestamp>)
  --messages <n>                Benchmark message count per backend (default: 20000)
  --mode <async|sync>           Benchmark appender mode (default: async)
  --compress <zlib|zstd>        Benchmark compression mode (default: zlib)
  --count <n>                   Record count for phase2c2 fixture generation (default: 32)
  --skip-setup                  Skip Python2 decoder setup for phase2c2
  --skip-phase2c2               Skip official decoder compatibility regression
  --skip-bindings               Skip JNI/UniFFI/NAPI dual-backend cargo checks
  --skip-bench                  Skip Rust/FFI benchmark comparison
  --min-throughput-ratio <v>    Require rust/ffi throughput ratio >= v (default: 0, disabled)
  --max-p99-ratio <v>           Require rust/ffi p99 latency ratio <= v (default: 0, disabled)
  -h, --help                    Show help

Notes:
  - Benchmark uses `cargo run --release` for both backends.
  - Ratio gates are disabled when threshold <= 0.
EOF
}

script_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
repo_root="$(cd "${script_dir}/../.." && pwd)"

out_dir=""
messages=20000
mode="async"
compress="zlib"
count=32
skip_setup=0
skip_phase2c2=0
skip_bindings=0
skip_bench=0
min_throughput_ratio="0"
max_p99_ratio="0"

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
    --min-throughput-ratio)
      min_throughput_ratio="${2:-}"
      shift 2
      ;;
    --max-p99-ratio)
      max_p99_ratio="${2:-}"
      shift 2
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
rust_metric=""
ffi_metric=""

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

extract_json_number() {
  local line="$1"
  local key="$2"
  echo "$line" | sed -n "s/.*\"${key}\":\\([0-9.]*\\).*/\\1/p"
}

append_metric_from_log() {
  local backend="$1"
  local logfile="${logs_dir}/bench_${backend}.log"
  local line
  line="$(grep -E '^\{.*"throughput_mps".*\}$' "$logfile" | tail -n 1 || true)"
  if [[ -z "$line" ]]; then
    echo "[phase5] FAIL bench_${backend} missing json metrics" | tee -a "$summary_file"
    failures=$((failures + 1))
    failed_steps+=("bench_${backend}_metrics:1")
    return
  fi
  echo "$line" >> "$metrics_file"
  if [[ "$backend" == "rust" ]]; then
    rust_metric="$line"
  else
    ffi_metric="$line"
  fi
  echo "[phase5] metric ${backend}: ${line}" | tee -a "$summary_file"
}

evaluate_bench_thresholds() {
  if [[ -z "$rust_metric" || -z "$ffi_metric" ]]; then
    return
  fi

  local rust_tp ffi_tp rust_p99 ffi_p99
  rust_tp="$(extract_json_number "$rust_metric" "throughput_mps")"
  ffi_tp="$(extract_json_number "$ffi_metric" "throughput_mps")"
  rust_p99="$(extract_json_number "$rust_metric" "lat_p99_ns")"
  ffi_p99="$(extract_json_number "$ffi_metric" "lat_p99_ns")"

  if [[ -z "$rust_tp" || -z "$ffi_tp" || -z "$rust_p99" || -z "$ffi_p99" ]]; then
    echo "[phase5] WARN cannot parse bench ratios from metrics" | tee -a "$summary_file"
    return
  fi

  local tp_ratio p99_ratio
  tp_ratio="$(awk -v r="$rust_tp" -v f="$ffi_tp" 'BEGIN { if (f == 0) print "0"; else printf "%.4f", r / f }')"
  p99_ratio="$(awk -v r="$rust_p99" -v f="$ffi_p99" 'BEGIN { if (f == 0) print "0"; else printf "%.4f", r / f }')"

  echo "[phase5] ratio rust_over_ffi throughput=${tp_ratio}, p99=${p99_ratio}" | tee -a "$summary_file"

  if awk -v t="$min_throughput_ratio" 'BEGIN { exit !(t > 0) }'; then
    local tp_ok
    tp_ok="$(awk -v v="$tp_ratio" -v min="$min_throughput_ratio" 'BEGIN { if (v >= min) print "1"; else print "0" }')"
    if [[ "$tp_ok" != "1" ]]; then
      echo "[phase5] FAIL throughput ratio ${tp_ratio} < ${min_throughput_ratio}" | tee -a "$summary_file"
      failures=$((failures + 1))
      failed_steps+=("bench_throughput_gate:1")
    fi
  fi
  if awk -v t="$max_p99_ratio" 'BEGIN { exit !(t > 0) }'; then
    local p99_ok
    p99_ok="$(awk -v v="$p99_ratio" -v max="$max_p99_ratio" 'BEGIN { if (v <= max) print "1"; else print "0" }')"
    if [[ "$p99_ok" != "1" ]]; then
      echo "[phase5] FAIL p99 ratio ${p99_ratio} > ${max_p99_ratio}" | tee -a "$summary_file"
      failures=$((failures + 1))
      failed_steps+=("bench_p99_gate:1")
    fi
  fi
}

echo "[phase5] artifact_root=${out_dir}" | tee -a "$summary_file"
echo "[phase5] bench messages=${messages}, mode=${mode}, compress=${compress}" | tee -a "$summary_file"

if [[ "$skip_phase2c2" -eq 0 ]]; then
  phase2c2_cmd=(
    "${repo_root}/scripts/xlog/run_phase2c2_official.sh"
    --out-dir "${out_dir}/phase2c2_fixtures"
    --backend both
    --count "$count"
  )
  if [[ "$skip_setup" -eq 1 ]]; then
    phase2c2_cmd+=(--skip-setup)
  fi
  run_step "phase2c2_official" "${phase2c2_cmd[@]}"
fi

if [[ "$skip_bindings" -eq 0 ]]; then
  for backend in rust ffi; do
    feature="${backend}-backend"
    run_step "check_uniffi_${backend}" cargo check -p mars-xlog-uniffi --no-default-features --features "$feature"
    run_step "check_android_jni_${backend}" cargo check -p mars-xlog-android-jni --no-default-features --features "$feature"
    run_step "check_ohos_napi_${backend}" cargo check -p oh-xlog --no-default-features --features "$feature"
  done
fi

if [[ "$skip_bench" -eq 0 ]]; then
  for backend in rust ffi; do
    feature="${backend}-backend"
    run_step \
      "bench_${backend}" \
      cargo run --release -p mars-xlog --example bench_backend --no-default-features --features "$feature" -- \
        --out-dir "${bench_dir}/${backend}" \
        --prefix "phase5-${backend}" \
        --messages "$messages" \
        --mode "$mode" \
        --compress "$compress"
    if [[ "$last_exit_code" -eq 0 ]]; then
      append_metric_from_log "$backend"
    fi
  done
  evaluate_bench_thresholds
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
