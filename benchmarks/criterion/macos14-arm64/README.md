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
