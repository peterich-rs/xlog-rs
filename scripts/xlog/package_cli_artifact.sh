#!/usr/bin/env bash
set -euo pipefail

usage() {
  cat <<'USAGE'
Package the mars-xlog CLI binary as a release artifact.

Usage:
  scripts/xlog/package_cli_artifact.sh --version <version> --target <rust-target> [--out-dir <dir>]

The binary must already be built at target/<rust-target>/release/.
USAGE
}

script_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
repo_root="$(cd "${script_dir}/../.." && pwd)"

version=""
target=""
out_dir="${repo_root}/artifacts/cli"

while (($# > 0)); do
  case "$1" in
    --version)
      version="${2:-}"
      shift 2
      ;;
    --target)
      target="${2:-}"
      shift 2
      ;;
    --out-dir)
      out_dir="${2:-}"
      shift 2
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

if [[ -z "$version" || -z "$target" ]]; then
  usage >&2
  exit 2
fi

bin_name="mars-xlog"
if [[ "$target" == *windows* || "$target" == *msvc* ]]; then
  bin_name="mars-xlog.exe"
fi

src="${repo_root}/target/${target}/release/${bin_name}"
if [[ ! -f "$src" ]]; then
  echo "error: missing built binary: ${src}" >&2
  exit 1
fi

mkdir -p "$out_dir"
tmp_dir="$(mktemp -d)"
trap 'rm -rf "$tmp_dir"' EXIT

cp "$src" "${tmp_dir}/${bin_name}"
chmod 0755 "${tmp_dir}/${bin_name}"

archive="${out_dir}/mars-xlog-v${version}-${target}.tar.gz"
tar -czf "$archive" -C "$tmp_dir" "$bin_name"

if command -v shasum >/dev/null 2>&1; then
  shasum -a 256 "$archive" > "${archive}.sha256"
elif command -v sha256sum >/dev/null 2>&1; then
  sha256sum "$archive" > "${archive}.sha256"
fi

echo "$archive"
