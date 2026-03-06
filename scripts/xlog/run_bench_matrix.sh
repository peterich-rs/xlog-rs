#!/usr/bin/env bash
set -euo pipefail

usage() {
  cat <<'USAGE'
Automated benchmark matrix runner.

Usage:
  scripts/xlog/run_bench_matrix.sh --manifest <tsv> --out-root <dir> [options]

Options:
  --manifest <file>       Path to TSV manifest (required)
  --out-root <dir>        Output root directory (required)
  --runs <n>              Runs per scenario per backend (default: 3)
  --backends <list>       Comma-separated backends: rust,cpp (default: rust,cpp)
  --filter <pattern>      Only run scenarios matching this grep pattern
  --backend-order <mode>  fixed|alternating|randomized (default: randomized)
  --order-seed <text>     Seed for randomized backend order (default: unix epoch seconds)
  --skip-build            Skip cargo build step
  --components            Also run component micro-benchmarks
  --component-iterations <n>
                          Iterations per component benchmark run (default: 100000)
  --component-sizes <csv> Comma-separated component payload sizes (default: 16,96,256,1024,4096)
  -h, --help              Show help
USAGE
}

manifest=""
out_root=""
runs=3
backends="rust,cpp"
filter=""
skip_build=0
run_components=0
backend_order_policy="randomized"
order_seed=""
component_iterations=100000
component_sizes="16,96,256,1024,4096"

while [[ $# -gt 0 ]]; do
  case "$1" in
    --manifest)     manifest="$2";              shift 2 ;;
    --out-root)     out_root="$2";              shift 2 ;;
    --runs)         runs="$2";                  shift 2 ;;
    --backends)     backends="$2";              shift 2 ;;
    --filter)       filter="$2";                shift 2 ;;
    --backend-order) backend_order_policy="$2"; shift 2 ;;
    --order-seed)   order_seed="$2";            shift 2 ;;
    --skip-build)   skip_build=1;                shift   ;;
    --components)   run_components=1;            shift   ;;
    --component-iterations) component_iterations="$2"; shift 2 ;;
    --component-sizes) component_sizes="$2";    shift 2 ;;
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

if [[ -z "$manifest" ]]; then
  echo "error: --manifest is required" >&2
  exit 2
fi
if [[ -z "$out_root" ]]; then
  echo "error: --out-root is required" >&2
  exit 2
fi
if [[ ! -f "$manifest" ]]; then
  echo "error: manifest file not found: $manifest" >&2
  exit 2
fi
if ! [[ "$runs" =~ ^[0-9]+$ ]] || [[ "$runs" -eq 0 ]]; then
  echo "error: --runs must be a positive integer" >&2
  exit 2
fi
if ! [[ "$component_iterations" =~ ^[0-9]+$ ]] || [[ "$component_iterations" -eq 0 ]]; then
  echo "error: --component-iterations must be a positive integer" >&2
  exit 2
fi
case "$backend_order_policy" in
  fixed|alternating|randomized) ;;
  *)
    echo "error: --backend-order must be fixed|alternating|randomized" >&2
    exit 2
    ;;
esac

script_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
repo_root="$(cd "${script_dir}/../.." && pwd)"

mkdir -p "$out_root"
cp "$manifest" "$out_root/manifest.tsv"

results_raw="$out_root/results_raw.jsonl"
: > "$results_raw"
log_file="$out_root/run.log"
: > "$log_file"
started_at_utc="$(date -u +"%Y-%m-%dT%H:%M:%SZ")"
started_epoch="$(date +%s)"
if [[ -z "$order_seed" ]]; then
  order_seed="$started_epoch"
fi

log() {
  echo "[$(date +%H:%M:%S)] $*" | tee -a "$log_file"
}

json_escape() {
  printf '%s' "$1" | sed 's/\\/\\\\/g; s/"/\\"/g'
}

csv_to_array() {
  local input="$1"
  local __resultvar="$2"
  IFS=',' read -ra _tmp <<< "$input"
  for i in "${!_tmp[@]}"; do
    _tmp[$i]="${_tmp[$i]// /}"
  done
  eval "$__resultvar=(\"\${_tmp[@]}\")"
}

backends_json_array() {
  local arr=("$@")
  local first=1
  printf '['
  for be in "${arr[@]}"; do
    [[ -z "$be" ]] && continue
    if [[ "$first" -eq 0 ]]; then
      printf ','
    fi
    printf '"%s"' "$(json_escape "$be")"
    first=0
  done
  printf ']'
}

hash_u32() {
  printf '%s' "$1" | cksum | awk '{print $1}'
}

reverse_backends() {
  local arr=("$@")
  local idx
  for ((idx=${#arr[@]} - 1; idx >= 0; idx--)); do
    printf '%s\n' "${arr[idx]}"
  done
}

shuffle_backends_deterministic() {
  local seed="$1"
  shift
  local arr=("$@")
  local n="${#arr[@]}"
  if (( n <= 1 )); then
    printf '%s\n' "${arr[@]}"
    return 0
  fi

  local i j h tmp joined
  for ((i=n-1; i>0; i--)); do
    joined="${arr[*]}"
    h="$(hash_u32 "${seed}|${i}|${joined}")"
    j=$((h % (i + 1)))
    tmp="${arr[i]}"
    arr[i]="${arr[j]}"
    arr[j]="$tmp"
  done

  printf '%s\n' "${arr[@]}"
}

extract_first_number() {
  local key="$1"
  local json_line="$2"
  printf '%s\n' "$json_line" | sed -nE "s/.*\"${key}\":([0-9]+(\.[0-9]+)?).*/\1/p" | head -n 1
}

is_nonneg_int() {
  [[ "$1" =~ ^[0-9]+$ ]]
}

detect_cpu_count() {
  if command -v getconf >/dev/null 2>&1; then
    getconf _NPROCESSORS_ONLN 2>/dev/null && return 0
  fi
  if command -v sysctl >/dev/null 2>&1; then
    sysctl -n hw.ncpu 2>/dev/null && return 0
  fi
  echo "unknown"
}

detect_cpu_model() {
  if command -v sysctl >/dev/null 2>&1; then
    local v
    v="$(sysctl -n machdep.cpu.brand_string 2>/dev/null || true)"
    if [[ -n "$v" ]]; then
      echo "$v"
      return 0
    fi
  fi
  if [[ -r /proc/cpuinfo ]]; then
    local v
    v="$(awk -F: '/model name/ {print $2; exit}' /proc/cpuinfo | sed 's/^ *//')"
    if [[ -n "$v" ]]; then
      echo "$v"
      return 0
    fi
  fi
  echo "unknown"
}

detect_mem_total_bytes() {
  if command -v sysctl >/dev/null 2>&1; then
    local v
    v="$(sysctl -n hw.memsize 2>/dev/null || true)"
    if [[ -n "$v" ]]; then
      echo "$v"
      return 0
    fi
  fi
  if [[ -r /proc/meminfo ]]; then
    awk '/MemTotal:/ {print $2 * 1024; exit}' /proc/meminfo
    return 0
  fi
  echo "unknown"
}

detect_cpu_governor() {
  if [[ -r /sys/devices/system/cpu/cpu0/cpufreq/scaling_governor ]]; then
    cat /sys/devices/system/cpu/cpu0/cpufreq/scaling_governor
    return 0
  fi
  echo "unknown"
}

detect_cpu_freq_policy_khz() {
  local min_file="/sys/devices/system/cpu/cpu0/cpufreq/scaling_min_freq"
  local max_file="/sys/devices/system/cpu/cpu0/cpufreq/scaling_max_freq"
  if [[ -r "$min_file" && -r "$max_file" ]]; then
    printf '%s-%s' "$(cat "$min_file")" "$(cat "$max_file")"
    return 0
  fi
  echo "unknown"
}

detect_os_version() {
  local os
  os="$(uname -s)"
  if [[ "$os" == "Darwin" ]] && command -v sw_vers >/dev/null 2>&1; then
    printf '%s %s (%s)' \
      "$(sw_vers -productName 2>/dev/null || echo Darwin)" \
      "$(sw_vers -productVersion 2>/dev/null || echo unknown)" \
      "$(sw_vers -buildVersion 2>/dev/null || echo unknown)"
    return 0
  fi
  if [[ -r /etc/os-release ]]; then
    local pretty
    pretty="$(awk -F= '/^PRETTY_NAME=/{gsub(/"/,"",$2);print $2; exit}' /etc/os-release)"
    if [[ -n "$pretty" ]]; then
      echo "$pretty"
      return 0
    fi
  fi
  uname -srv
}

detect_command_version() {
  local cmd="$1"
  if command -v "$cmd" >/dev/null 2>&1; then
    "$cmd" --version 2>/dev/null | head -n 1
    return 0
  fi
  echo "unknown"
}

sha256_file() {
  local path="$1"
  if command -v shasum >/dev/null 2>&1; then
    shasum -a 256 "$path" | awk '{print $1}'
    return 0
  fi
  if command -v sha256sum >/dev/null 2>&1; then
    sha256sum "$path" | awk '{print $1}'
    return 0
  fi
  echo "unknown"
}

git_dirty_state() {
  if git -C "$repo_root" rev-parse --verify HEAD >/dev/null 2>&1; then
    if git -C "$repo_root" diff --quiet --ignore-submodules HEAD --; then
      echo false
    else
      echo true
    fi
  else
    echo false
  fi
}

csv_to_array "$backends" requested_backends
if [[ "${#requested_backends[@]}" -eq 0 ]]; then
  echo "error: --backends must not be empty" >&2
  exit 2
fi
for be in "${requested_backends[@]}"; do
  case "$be" in
    rust|cpp) ;;
    *)
      echo "error: unsupported backend '$be' (allowed: rust, cpp)" >&2
      exit 2
      ;;
  esac
done

log "Manifest: $manifest"
log "Output: $out_root"
log "Backend order policy: $backend_order_policy (seed=$order_seed)"

# ─── Build step ─────────────────────────────────────────────────────────
if [[ "$skip_build" -eq 0 ]]; then
  for be in "${requested_backends[@]}"; do
    log "Building ${be}-backend (release)..."
    cargo build --release -p mars-xlog --example bench_backend \
      --no-default-features --features "${be}-backend" 2>&1 | tail -1 | tee -a "$log_file"
  done
fi

# ─── Component micro-benchmarks ────────────────────────────────────────
if [[ "$run_components" -eq 1 ]]; then
  log "Running component micro-benchmarks..."
  comp_out="$out_root/components.jsonl"
  : > "$comp_out"

  csv_to_array "$component_sizes" component_size_arr
  for size in "${component_size_arr[@]}"; do
    if ! [[ "$size" =~ ^[0-9]+$ ]] || [[ "$size" -eq 0 ]]; then
      echo "error: invalid component size: $size" >&2
      exit 2
    fi
    cargo run --release -p mars-xlog-core --example bench_components -- \
      all --iterations "$component_iterations" --payload-size "$size" \
      2>>"$log_file" | tee -a "$comp_out"
  done
  log "Component benchmarks complete -> $comp_out"
fi

# ─── Read manifest and run scenarios ────────────────────────────────────
scenario_count=0
while IFS= read -r line || [[ -n "$line" ]]; do
  [[ "$line" =~ ^#.*$ ]] && continue
  [[ -z "$line" ]] && continue

  IFS='|' read -r \
    scenario messages mode threads compress compress_level msg_size flush_every \
    cache_days max_file_size pub_key warmup time_buckets payload_profile payload_seed \
    <<< "$(printf '%s' "$line" | tr '\t' '|')"

  [[ "$scenario" =~ ^#.*$ ]] && continue
  [[ "$scenario" == "scenario" ]] && continue
  [[ -z "$scenario" ]] && continue

  if [[ -n "$filter" ]] && ! echo "$scenario" | grep -qE "$filter"; then
    continue
  fi

  compress_level="${compress_level:-6}"
  flush_every="${flush_every:-0}"
  cache_days="${cache_days:-0}"
  max_file_size="${max_file_size:-0}"
  warmup="${warmup:-500}"
  time_buckets="${time_buckets:-0}"
  payload_profile="${payload_profile:-compressible}"
  payload_seed="${payload_seed:-20260306}"

  scenario_count=$((scenario_count + 1))
  log "━━━ Scenario: $scenario (messages=$messages mode=$mode threads=$threads compress=$compress level=$compress_level msg_size=$msg_size payload=$payload_profile) ━━━"

  mkdir -p "$out_root/${scenario}"
  for be in "${requested_backends[@]}"; do
    : > "$out_root/${scenario}/results_${be}.jsonl"
  done

  for run_idx in $(seq 1 "$runs"); do
    backend_order=("${requested_backends[@]}")
    case "$backend_order_policy" in
      fixed)
        ;;
      alternating)
        if (( ${#backend_order[@]} > 1 )) && (( (scenario_count + run_idx) % 2 == 0 )); then
          backend_order=()
          while IFS= read -r reversed_backend; do
            backend_order+=("$reversed_backend")
          done < <(reverse_backends "${requested_backends[@]}")
        fi
        ;;
      randomized)
        backend_order=()
        while IFS= read -r randomized_backend; do
          backend_order+=("$randomized_backend")
        done < <(shuffle_backends_deterministic "${order_seed}|${scenario}|${run_idx}" "${requested_backends[@]}")
        ;;
    esac

    log "  run ${run_idx}/${runs} backend order: ${backend_order[*]}"

    for be in "${backend_order[@]}"; do
      feature="${be}-backend"
      results_file="$out_root/${scenario}/results_${be}.jsonl"
      run_dir="$out_root/${scenario}/${be}-run${run_idx}"
      cache_dir_arg=""
      rm -rf "$run_dir"

      cmd=(
        cargo run --release -p mars-xlog --example bench_backend
        --no-default-features --features "$feature" --
        --out-dir "$run_dir"
        --prefix "${scenario}-${be}"
        --messages "$messages"
        --mode "$mode"
        --compress "$compress"
        --compress-level "$compress_level"
        --msg-size "$msg_size"
        --payload-profile "$payload_profile"
        --payload-seed "$payload_seed"
        --threads "$threads"
        --flush-every "$flush_every"
        --warmup "$warmup"
        --max-file-size "$max_file_size"
      )

      if is_nonneg_int "${time_buckets:-}" && [[ "$time_buckets" -gt 0 ]]; then
        cmd+=(--time-buckets "$time_buckets")
      fi

      if [[ -n "${pub_key:-}" ]]; then
        cmd+=(--pub-key "$pub_key")
      fi

      if is_nonneg_int "${cache_days:-}" && [[ "$cache_days" -gt 0 ]]; then
        cache_dir_arg="$out_root/${scenario}/${be}-cache${run_idx}"
        rm -rf "$cache_dir_arg"
        cmd+=(--cache-dir "$cache_dir_arg" --cache-days "$cache_days")
      fi

      log "  [${be}] run ${run_idx}/${runs}..."
      if output=$("${cmd[@]}" 2>>"$log_file"); then
        echo "$output" >> "$results_file"
        printf '{"scenario":"%s","backend":"%s","run_index":%d,"run_dir":"%s","result":%s}\n' \
          "$(json_escape "$scenario")" \
          "$(json_escape "$be")" \
          "$run_idx" \
          "$(json_escape "$run_dir")" \
          "$output" >> "$results_raw"

        tps="$(extract_first_number "throughput_mps" "$output")"
        lat_p99="$(extract_first_number "lat_p99_ns" "$output")"
        log "  [${be}] run ${run_idx}: ${tps:-?} mps, p99=${lat_p99:-?} ns"
      else
        code=$?
        log "  [${be}] run ${run_idx}: FAILED (exit=${code})"
      fi
    done
  done
done < "$manifest"

log "━━━ Matrix complete: ${scenario_count} scenarios × ${runs} runs ━━━"
log "Raw results: $results_raw"
log "Log: $log_file"

# ─── Generate summary ──────────────────────────────────────────────────
summary_file="$out_root/summary.md"
summary_json="$out_root/summary.json"
summary_rows_tmp="$out_root/.summary_rows.tsv"
: > "$summary_rows_tmp"

for scenario_dir in "$out_root"/*/; do
  scenario_name="$(basename "$scenario_dir")"
  for results_file in "$scenario_dir"/results_*.jsonl; do
    [[ -f "$results_file" ]] || continue
    be="$(basename "$results_file" | sed 's/results_//;s/\.jsonl//')"

    if [[ -s "$results_file" ]]; then
      row=$(
        awk -F'[,:]' '
        BEGIN { n=0; tps=0; avg=0; p99=0; p999=0; bpm=0 }
        {
          for(i=1;i<=NF;i++) {
            gsub(/["{} ]/, "", $i)
            if($i=="throughput_mps") { tps+=$(i+1) }
            if($i=="lat_avg_ns") { avg+=$(i+1) }
            if($i=="lat_p99_ns") { p99+=$(i+1) }
            if($i=="lat_p999_ns") { p999+=$(i+1) }
            if($i=="bytes_per_msg") { bpm+=$(i+1) }
          }
          n++
        }
        END {
          if(n>0) {
            printf "%s\t%s\t%.3f\t%.3f\t%.3f\t%.3f\t%.3f\n", scenario, be, tps/n, avg/n, p99/n, p999/n, bpm/n
          }
        }
        ' scenario="$scenario_name" be="$be" "$results_file"
      )
      if [[ -n "$row" ]]; then
        printf '%s\n' "$row" >> "$summary_rows_tmp"
      fi
    fi
  done
done

{
  echo "# Benchmark Matrix Summary"
  echo
  echo "| Scenario | Backend | Throughput (mps) | Avg Lat (ns) | P99 Lat (ns) | P999 Lat (ns) | Output (bytes/msg) |"
  echo "| :--- | :--- | ---: | ---: | ---: | ---: | ---: |"
  while IFS=$'\t' read -r scenario_name be tps avg p99 p999 bpm; do
    printf '| %s | %s | %.0f | %.0f | %.0f | %.0f | %.1f |\n' \
      "$scenario_name" "$be" "$tps" "$avg" "$p99" "$p999" "$bpm"
  done < "$summary_rows_tmp"
} > "$summary_file"

{
  echo "["
  first=1
  while IFS=$'\t' read -r scenario_name be tps avg p99 p999 bpm; do
    if [[ "$first" -eq 0 ]]; then
      echo ","
    fi
    printf '  {"scenario":"%s","backend":"%s","throughput_mps":%s,"lat_avg_ns":%s,"lat_p99_ns":%s,"lat_p999_ns":%s,"bytes_per_msg":%s}' \
      "$(json_escape "$scenario_name")" \
      "$(json_escape "$be")" \
      "$tps" \
      "$avg" \
      "$p99" \
      "$p999" \
      "$bpm"
    first=0
  done < "$summary_rows_tmp"
  echo
  echo "]"
} > "$summary_json"

finished_at_utc="$(date -u +"%Y-%m-%dT%H:%M:%SZ")"
finished_epoch="$(date +%s)"
metadata_file="$out_root/metadata.json"

manifest_hash="$(sha256_file "$manifest")"
git_branch="$(git -C "$repo_root" branch --show-current 2>/dev/null || echo unknown)"
git_commit="$(git -C "$repo_root" rev-parse HEAD 2>/dev/null || echo unknown)"
git_dirty="$(git_dirty_state)"
host_name="$(hostname)"
os_name="$(uname -s)"
os_arch="$(uname -m)"
os_version="$(detect_os_version)"
cpu_count="$(detect_cpu_count)"
cpu_model="$(detect_cpu_model)"
mem_total_bytes="$(detect_mem_total_bytes)"
cpu_governor="$(detect_cpu_governor)"
cpu_freq_policy_khz="$(detect_cpu_freq_policy_khz)"
rustc_version="$(detect_command_version rustc)"
cargo_version="$(detect_command_version cargo)"

{
  echo "{"
  printf '  "started_at_utc":"%s",\n' "$started_at_utc"
  printf '  "finished_at_utc":"%s",\n' "$finished_at_utc"
  printf '  "duration_seconds":%d,\n' "$((finished_epoch - started_epoch))"
  printf '  "hostname":"%s",\n' "$(json_escape "$host_name")"
  printf '  "os":"%s",\n' "$(json_escape "$os_name")"
  printf '  "os_version":"%s",\n' "$(json_escape "$os_version")"
  printf '  "arch":"%s",\n' "$(json_escape "$os_arch")"
  printf '  "cpu_count":"%s",\n' "$(json_escape "$cpu_count")"
  printf '  "cpu_model":"%s",\n' "$(json_escape "$cpu_model")"
  printf '  "memory_total_bytes":"%s",\n' "$(json_escape "$mem_total_bytes")"
  printf '  "cpu_governor":"%s",\n' "$(json_escape "$cpu_governor")"
  printf '  "cpu_freq_policy_khz":"%s",\n' "$(json_escape "$cpu_freq_policy_khz")"
  printf '  "rustc_version":"%s",\n' "$(json_escape "$rustc_version")"
  printf '  "cargo_version":"%s",\n' "$(json_escape "$cargo_version")"
  printf '  "git_branch":"%s",\n' "$(json_escape "$git_branch")"
  printf '  "git_commit":"%s",\n' "$(json_escape "$git_commit")"
  printf '  "git_dirty":%s,\n' "$git_dirty"
  printf '  "cargo_profile":"release",\n'
  printf '  "manifest":"%s",\n' "$(json_escape "$manifest")"
  printf '  "manifest_copy":"%s",\n' "$(json_escape "$out_root/manifest.tsv")"
  printf '  "manifest_sha256":"%s",\n' "$(json_escape "$manifest_hash")"
  printf '  "backends":%s,\n' "$(backends_json_array "${requested_backends[@]}")"
  printf '  "backend_order_policy":"%s",\n' "$(json_escape "$backend_order_policy")"
  printf '  "backend_order_seed":"%s",\n' "$(json_escape "$order_seed")"
  printf '  "runs":%d,\n' "$runs"
  printf '  "filter":"%s",\n' "$(json_escape "$filter")"
  printf '  "components":%s,\n' "$([[ "$run_components" -eq 1 ]] && echo true || echo false)"
  printf '  "component_iterations":%s,\n' "$(json_escape "$component_iterations")"
  printf '  "component_sizes":"%s",\n' "$(json_escape "$component_sizes")"
  printf '  "scenario_count":%d,\n' "$scenario_count"
  printf '  "results_schema_version":"v2",\n'
  printf '  "results_raw":"%s",\n' "$(json_escape "$results_raw")"
  printf '  "summary_markdown":"%s",\n' "$(json_escape "$summary_file")"
  printf '  "summary_json":"%s",\n' "$(json_escape "$summary_json")"
  printf '  "run_log":"%s"\n' "$(json_escape "$log_file")"
  echo "}"
} > "$metadata_file"

rm -f "$summary_rows_tmp"

log "Summary: $summary_file"
log "Summary JSON: $summary_json"
log "Metadata: $metadata_file"
