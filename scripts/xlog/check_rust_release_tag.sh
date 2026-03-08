#!/usr/bin/env bash
set -euo pipefail

usage() {
  cat <<'USAGE'
Validate a Rust release tag against workspace manifest versions.

Usage:
  scripts/xlog/check_rust_release_tag.sh --tag <vX.Y.Z[-preview.N]>
USAGE
}

script_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
repo_root="$(cd "${script_dir}/../.." && pwd)"

tag=""

while (($# > 0)); do
  case "$1" in
    --tag)
      tag="${2:-}"
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

if [[ -z "$tag" ]]; then
  echo "error: --tag is required" >&2
  usage >&2
  exit 2
fi

if [[ ! "$tag" =~ ^v([0-9]+\.[0-9]+\.[0-9]+(-[0-9A-Za-z.-]+)?)$ ]]; then
  echo "error: tag must match v<semver>, got: ${tag}" >&2
  exit 1
fi

release_version="${BASH_REMATCH[1]}"
release_channel="ga"
is_preview="false"
release_title="Rust GA ${release_version}"
if [[ "$release_version" == *-* ]]; then
  release_channel="preview"
  is_preview="true"
  release_title="Rust Preview ${release_version}"
fi

manifest_version() {
  local file="$1"
  awk -F'"' '/^version = "/ { print $2; exit }' "$file"
}

dependency_version() {
  local file="$1"
  local dep="$2"
  awk -v dep="$dep" '
    $0 ~ "^" dep "[[:space:]]*=" {
      if (match($0, /version = "[^"]+"/)) {
        value = substr($0, RSTART, RLENGTH)
        gsub(/^version = "/, "", value)
        gsub(/"$/, "", value)
        print value
        exit
      }
    }
  ' "$file"
}

assert_eq() {
  local label="$1"
  local expected="$2"
  local actual="$3"
  if [[ "$expected" != "$actual" ]]; then
    echo "error: ${label} expected ${expected}, got ${actual}" >&2
    exit 1
  fi
}

xlog_core_manifest="${repo_root}/crates/xlog-core/Cargo.toml"
xlog_manifest="${repo_root}/crates/xlog/Cargo.toml"
xlog_uniffi_manifest="${repo_root}/crates/xlog-uniffi/Cargo.toml"
xlog_android_manifest="${repo_root}/crates/xlog-android-jni/Cargo.toml"
xlog_harmony_manifest="${repo_root}/crates/mars-xlog-harmony-napi/Cargo.toml"
xlog_sys_manifest="${repo_root}/crates/xlog-sys/Cargo.toml"

assert_eq "crates/xlog-core version" "$release_version" "$(manifest_version "$xlog_core_manifest")"
assert_eq "crates/xlog version" "$release_version" "$(manifest_version "$xlog_manifest")"
assert_eq "crates/xlog-uniffi version" "$release_version" "$(manifest_version "$xlog_uniffi_manifest")"
assert_eq "crates/xlog-android-jni version" "$release_version" "$(manifest_version "$xlog_android_manifest")"
assert_eq "crates/mars-xlog-harmony-napi version" "$release_version" "$(manifest_version "$xlog_harmony_manifest")"
assert_eq "crates/xlog-sys version" "$release_version" "$(manifest_version "$xlog_sys_manifest")"

assert_eq "mars-xlog -> mars-xlog-core dependency version" "$release_version" "$(dependency_version "$xlog_manifest" "mars-xlog-core")"
assert_eq "mars-xlog-uniffi -> mars-xlog dependency version" "$release_version" "$(dependency_version "$xlog_uniffi_manifest" "mars-xlog")"
assert_eq "mars-xlog-android-jni -> mars-xlog dependency version" "$release_version" "$(dependency_version "$xlog_android_manifest" "mars-xlog")"
assert_eq "mars-xlog-harmony-napi -> mars-xlog dependency version" "$release_version" "$(dependency_version "$xlog_harmony_manifest" "mars-xlog")"

write_output() {
  local key="$1"
  local value="$2"
  if [[ -n "${GITHUB_OUTPUT:-}" ]]; then
    printf '%s=%s\n' "$key" "$value" >> "$GITHUB_OUTPUT"
  fi
}

write_output release_tag "$tag"
write_output release_version "$release_version"
write_output release_channel "$release_channel"
write_output is_preview "$is_preview"
write_output release_title "$release_title"

cat <<EOF
release_tag=${tag}
release_version=${release_version}
release_channel=${release_channel}
is_preview=${is_preview}
release_title=${release_title}
EOF
