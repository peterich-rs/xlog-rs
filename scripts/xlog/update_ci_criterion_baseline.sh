#!/usr/bin/env bash
set -euo pipefail

usage() {
  cat <<'USAGE'
Copy a locally generated Criterion artifact root into the committed CI baseline directory.

Usage:
  scripts/xlog/update_ci_criterion_baseline.sh --from-root <dir> [--to-root <dir>]

Options:
  --from-root <dir>  Source artifact root containing criterion_summary.json
  --to-root <dir>    Destination baseline directory
                     (default: benchmarks/criterion/macos14-arm64)
  --help             Show this help
USAGE
}

from_root=""
to_root="benchmarks/criterion/macos14-arm64"

while [[ $# -gt 0 ]]; do
  case "$1" in
    --from-root)
      from_root="$2"
      shift 2
      ;;
    --to-root)
      to_root="$2"
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

if [[ -z "$from_root" ]]; then
  echo "--from-root is required" >&2
  usage >&2
  exit 2
fi

repo_root="$(cd "$(dirname "$0")/../.." && pwd)"
from_root="$(python3 - <<'PY' "$from_root"
from pathlib import Path
import sys
print(Path(sys.argv[1]).resolve())
PY
)"
to_root="$(python3 - <<'PY' "$repo_root" "$to_root"
from pathlib import Path
import sys
print((Path(sys.argv[1]) / sys.argv[2]).resolve())
PY
)"

summary_src="$from_root/criterion_summary.json"
metadata_src="$from_root/metadata.json"
if [[ ! -f "$summary_src" ]]; then
  echo "missing summary file: $summary_src" >&2
  exit 1
fi

mkdir -p "$to_root"

python3 - <<'PY' "$to_root/criterion_summary.json" "$summary_src"
import json
import sys
from pathlib import Path

out = Path(sys.argv[1])
summary_path = Path(sys.argv[2])
data = json.loads(summary_path.read_text(encoding='utf-8'))

# Committed baseline data should stay environment-agnostic. Keep the compact
# benchmark summary, but strip machine-local paths from local artifact exports.
data.pop('criterion_root', None)

out.write_text(json.dumps(data, ensure_ascii=False, indent=2) + '\n', encoding='utf-8')
PY

python3 - <<'PY' "$to_root/baseline_metadata.json" "$metadata_src"
import json
import sys
from pathlib import Path

out = Path(sys.argv[1])
metadata_path = Path(sys.argv[2])
payload = {}
if metadata_path.exists():
    data = json.loads(metadata_path.read_text(encoding='utf-8'))
    payload = {
        'baseline_name': data.get('baseline_name'),
        'git_commit': data.get('git_commit'),
        'git_branch': data.get('git_branch'),
        'rustc_version_verbose': data.get('rustc_version_verbose'),
        'cargo_version': data.get('cargo_version'),
        'generated_started_at_utc': data.get('started_at_utc'),
        'generated_finished_at_utc': data.get('finished_at_utc'),
        'git_dirty': data.get('git_dirty'),
    }
out.write_text(json.dumps(payload, ensure_ascii=False, indent=2) + '\n', encoding='utf-8')
PY

cat > "$to_root/README.md" <<'EOF'
# Criterion CI Baseline

This directory stores the committed Criterion summary used by CI regression checks on
the `macos-14` runner.

Repository policy:

1. Only curated benchmark data and documentation belong under `benchmarks/`.
2. Raw run outputs stay under the ignored `artifacts/` tree and must not be committed.
3. Committed summaries should not include machine-local absolute paths.

Refresh flow:

1. Run:
   scripts/xlog/run_criterion_bench.sh --out-root artifacts/criterion/<stamp> --baseline-name <name>
2. Copy it into this directory with:
   scripts/xlog/update_ci_criterion_baseline.sh --from-root artifacts/criterion/<stamp>
3. Review the diff in `criterion_summary.json` before committing.
EOF

printf '{"to_root": "%s", "summary": "%s"}\n' "$to_root" "$to_root/criterion_summary.json"
