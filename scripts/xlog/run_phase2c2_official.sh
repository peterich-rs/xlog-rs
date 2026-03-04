#!/usr/bin/env bash
set -euo pipefail

usage() {
  cat <<'EOF'
Run Phase 2C-2 official crypt decoder regression end-to-end.

Usage:
  scripts/xlog/run_phase2c2_official.sh [--out-dir <dir>] [--backend rust] [--count <n>] [--skip-setup]

Options:
  --out-dir <dir>      Fixture output directory (default: artifacts/xlog-fixtures/<timestamp>-phase2c2)
  --backend <value>    rust (default: rust)
  --count <n>          Records per case (default: 16)
  --skip-setup         Skip decoder env setup step
  -h, --help           Show this help text
EOF
}

script_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
repo_root="$(cd "${script_dir}/../.." && pwd)"
setup_script="${repo_root}/scripts/xlog/setup_py2_decoder_env.sh"
gen_script="${repo_root}/scripts/xlog/gen_fixtures.sh"
decode_script="${repo_root}/scripts/xlog/decode_compare.sh"

out_dir=""
backend="rust"
count=16
skip_setup=0

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
    --skip-setup)
      skip_setup=1
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

if [[ "$backend" != "rust" ]]; then
  echo "--backend must be: rust" >&2
  exit 2
fi

if [[ -z "$out_dir" ]]; then
  ts="$(date +%Y%m%d-%H%M%S)"
  out_dir="${repo_root}/artifacts/xlog-fixtures/${ts}-phase2c2"
fi

if [[ "$skip_setup" != "1" ]]; then
  "$setup_script"
fi

if [[ -z "${XLOG_PY2_BIN:-}" ]]; then
  default_py2="${HOME}/.pyenv/versions/xlog-py2-decoder/bin/python2"
  if [[ -x "$default_py2" ]]; then
    export XLOG_PY2_BIN="$default_py2"
  fi
fi

if [[ -z "${XLOG_PY2_BIN:-}" || ! -x "${XLOG_PY2_BIN}" ]]; then
  echo "python2 decoder binary unavailable. run: ${setup_script}" >&2
  exit 2
fi

echo "[phase2c2] generating fixtures: $out_dir"
"$gen_script" --out-dir "$out_dir" --backend "$backend" --count "$count" --include-crypt

echo "[phase2c2] running official decoder comparison"
"$decode_script" "$out_dir" --decoder official --py2-bin "$XLOG_PY2_BIN"

echo "[phase2c2] success"
echo "[phase2c2] fixture root: $out_dir"
