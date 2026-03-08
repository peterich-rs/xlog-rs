#!/usr/bin/env bash
set -euo pipefail

usage() {
  cat <<'USAGE'
Set the coordinated Rust release version across the workspace.

Usage:
  scripts/xlog/set_rust_release_version.sh <version>

Examples:
  scripts/xlog/set_rust_release_version.sh 0.1.0-preview.1
  scripts/xlog/set_rust_release_version.sh 0.1.0
USAGE
}

if (($# != 1)); then
  usage >&2
  exit 2
fi

version="$1"
if [[ ! "$version" =~ ^[0-9]+\.[0-9]+\.[0-9]+(-[0-9A-Za-z.-]+)?$ ]]; then
  echo "error: version must look like X.Y.Z or X.Y.Z-preview.N, got: ${version}" >&2
  exit 1
fi

script_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
repo_root="$(cd "${script_dir}/../.." && pwd)"

replace_first_package_version() {
  local file="$1"
  local tmp
  tmp="$(mktemp)"
  awk -v version="$version" '
    BEGIN { replaced = 0 }
    {
      if (!replaced && $0 ~ /^version = "/) {
        sub(/version = "[^"]+"/, "version = \"" version "\"")
        replaced = 1
      }
      print
    }
  ' "$file" > "$tmp"
  mv "$tmp" "$file"
}

replace_dependency_version() {
  local file="$1"
  local dep="$2"
  local tmp
  tmp="$(mktemp)"
  awk -v dep="$dep" -v version="$version" '
    {
      if ($0 ~ "^" dep "[[:space:]]*=") {
        sub(/version = "[^"]+"/, "version = \"" version "\"")
      }
      print
    }
  ' "$file" > "$tmp"
  mv "$tmp" "$file"
}

manifests=(
  "${repo_root}/crates/xlog-core/Cargo.toml"
  "${repo_root}/crates/xlog/Cargo.toml"
  "${repo_root}/crates/xlog-uniffi/Cargo.toml"
  "${repo_root}/crates/xlog-android-jni/Cargo.toml"
  "${repo_root}/crates/mars-xlog-harmony-napi/Cargo.toml"
  "${repo_root}/crates/xlog-sys/Cargo.toml"
)

for file in "${manifests[@]}"; do
  replace_first_package_version "$file"
done

replace_dependency_version "${repo_root}/crates/xlog/Cargo.toml" "mars-xlog-core"
replace_dependency_version "${repo_root}/crates/xlog-uniffi/Cargo.toml" "mars-xlog"
replace_dependency_version "${repo_root}/crates/xlog-android-jni/Cargo.toml" "mars-xlog"
replace_dependency_version "${repo_root}/crates/mars-xlog-harmony-napi/Cargo.toml" "mars-xlog"

cargo metadata --no-deps --format-version 1 >/dev/null
scripts/xlog/check_rust_release_tag.sh --tag "v${version}" >/dev/null

cat <<EOF
set coordinated Rust release version to ${version}
next steps:
  1. review git diff
  2. run scripts/xlog/check_mars_xlog_core_release.sh
  3. run scripts/xlog/check_mars_xlog_release.sh --skip-crates-io-check
  4. commit, merge to main, and tag v${version}
EOF
