#!/usr/bin/env python3
"""Compare benchmark roots and detect regressions with layer-aware thresholds.

Example:
  python3 scripts/xlog/check_bench_regression.py \
    --baseline-root artifacts/bench-compare/20260301-baseline \
    --current-root artifacts/bench-compare/20260306-full-matrix-latest \
    --backend rust
"""

from __future__ import annotations

import argparse
import json
from pathlib import Path
from typing import Any, Dict, Iterable, List, Tuple


def load_json(path: Path) -> Any:
    return json.loads(path.read_text(encoding="utf-8"))


def parse_manifest_scenarios(path: Path) -> set[str]:
    scenarios: set[str] = set()
    if not path.exists():
        return scenarios
    with path.open("r", encoding="utf-8") as f:
        for raw in f:
            line = raw.strip()
            if not line or line.startswith("#"):
                continue
            scenarios.add(line.split("\t", 1)[0])
    return scenarios


def to_markdown_table(headers: List[str], rows: List[List[str]]) -> List[str]:
    lines = [
        "| " + " | ".join(headers) + " |",
        "| " + " | ".join([":---"] + ["---:" for _ in headers[1:]]) + " |",
    ]
    for row in rows:
        lines.append("| " + " | ".join(row) + " |")
    return lines


def pct_change(current: float, baseline: float) -> float:
    if baseline == 0:
        return 0.0
    return (current / baseline - 1.0) * 100.0


def get_layer_map(repo_root: Path) -> Dict[str, str]:
    manifests = {
        "baseline": repo_root / "scripts/xlog/bench_matrix_baseline.tsv",
        "stress": repo_root / "scripts/xlog/bench_matrix_stress.tsv",
        "feature": repo_root / "scripts/xlog/bench_matrix_feature.tsv",
    }
    mapping: Dict[str, str] = {}
    for layer, path in manifests.items():
        for scenario in parse_manifest_scenarios(path):
            mapping[scenario] = layer
    return mapping


def build_index(summary_rows: List[Dict[str, Any]], backends: set[str]) -> Dict[Tuple[str, str], Dict[str, float]]:
    out: Dict[Tuple[str, str], Dict[str, float]] = {}
    for row in summary_rows:
        backend = str(row.get("backend", ""))
        if backend not in backends:
            continue
        scenario = str(row.get("scenario", ""))
        if not scenario:
            continue
        out[(scenario, backend)] = {
            "throughput_mps": float(row["throughput_mps"]),
            "lat_avg_ns": float(row["lat_avg_ns"]),
            "lat_p99_ns": float(row["lat_p99_ns"]),
            "lat_p999_ns": float(row["lat_p999_ns"]),
            "bytes_per_msg": float(row["bytes_per_msg"]),
        }
    return out


def resolve_thresholds(config: Dict[str, Any], layer: str) -> Dict[str, float]:
    default = dict(config.get("default") or {})
    layers = config.get("layers") or {}
    merged = dict(default)
    merged.update(layers.get(layer) or {})
    return {
        "throughput_drop_pct": float(merged.get("throughput_drop_pct", 8.0)),
        "avg_increase_pct": float(merged.get("avg_increase_pct", 20.0)),
        "p99_increase_pct": float(merged.get("p99_increase_pct", 35.0)),
        "p999_increase_pct": float(merged.get("p999_increase_pct", 50.0)),
        "bytes_increase_pct": float(merged.get("bytes_increase_pct", 20.0)),
    }


def format_pct(v: float) -> str:
    return f"{v:+.1f}%"


def main() -> None:
    parser = argparse.ArgumentParser(description="Compare two benchmark roots and gate regressions")
    parser.add_argument("--baseline-root", type=Path, required=True, help="baseline artifact root")
    parser.add_argument("--current-root", type=Path, required=True, help="current artifact root")
    parser.add_argument(
        "--backend",
        type=str,
        default="rust",
        help="comma-separated backends to compare (default: rust)",
    )
    parser.add_argument(
        "--thresholds",
        type=Path,
        default=Path("scripts/xlog/bench_regression_thresholds.json"),
        help="threshold config json path",
    )
    parser.add_argument("--top-n", type=int, default=12, help="top regression rows in markdown report")
    parser.add_argument("--out-md", type=Path, help="markdown output path")
    parser.add_argument("--out-json", type=Path, help="json output path")
    parser.add_argument(
        "--allow-regressions",
        action="store_true",
        help="always exit 0 even if regressions exist",
    )
    parser.add_argument(
        "--strict-missing",
        action="store_true",
        help="treat missing scenario/backend pairs as failures",
    )
    args = parser.parse_args()

    repo_root = Path(__file__).resolve().parents[2]
    baseline_root = args.baseline_root.resolve()
    current_root = args.current_root.resolve()
    backend_set = {x.strip() for x in args.backend.split(",") if x.strip()}
    if not backend_set:
        raise ValueError("backend set cannot be empty")

    baseline_summary = load_json(baseline_root / "summary.json")
    current_summary = load_json(current_root / "summary.json")
    threshold_cfg = load_json((repo_root / args.thresholds).resolve())
    layer_map = get_layer_map(repo_root)

    baseline_idx = build_index(baseline_summary, backend_set)
    current_idx = build_index(current_summary, backend_set)

    baseline_keys = set(baseline_idx.keys())
    current_keys = set(current_idx.keys())
    common_keys = sorted(baseline_keys & current_keys)
    missing_in_current = sorted(baseline_keys - current_keys)
    new_in_current = sorted(current_keys - baseline_keys)

    regressions: List[Dict[str, Any]] = []
    comparisons: List[Dict[str, Any]] = []

    for scenario, backend in common_keys:
        baseline = baseline_idx[(scenario, backend)]
        current = current_idx[(scenario, backend)]
        layer = layer_map.get(scenario, "unknown")
        thr = resolve_thresholds(threshold_cfg, layer)

        throughput_change_pct = pct_change(current["throughput_mps"], baseline["throughput_mps"])
        avg_change_pct = pct_change(current["lat_avg_ns"], baseline["lat_avg_ns"])
        p99_change_pct = pct_change(current["lat_p99_ns"], baseline["lat_p99_ns"])
        p999_change_pct = pct_change(current["lat_p999_ns"], baseline["lat_p999_ns"])
        bytes_change_pct = pct_change(current["bytes_per_msg"], baseline["bytes_per_msg"])

        flags = []
        if throughput_change_pct < -thr["throughput_drop_pct"]:
            flags.append("throughput")
        if avg_change_pct > thr["avg_increase_pct"]:
            flags.append("avg")
        if p99_change_pct > thr["p99_increase_pct"]:
            flags.append("p99")
        if p999_change_pct > thr["p999_increase_pct"]:
            flags.append("p999")
        if bytes_change_pct > thr["bytes_increase_pct"]:
            flags.append("bytes")

        item = {
            "scenario": scenario,
            "backend": backend,
            "layer": layer,
            "throughput_change_pct": throughput_change_pct,
            "avg_change_pct": avg_change_pct,
            "p99_change_pct": p99_change_pct,
            "p999_change_pct": p999_change_pct,
            "bytes_change_pct": bytes_change_pct,
            "thresholds": thr,
            "flags": flags,
        }
        comparisons.append(item)
        if flags:
            # Normalized severity score across all violated dimensions.
            score = 0.0
            if "throughput" in flags:
                score = max(score, abs(throughput_change_pct) / thr["throughput_drop_pct"])
            if "avg" in flags:
                score = max(score, avg_change_pct / thr["avg_increase_pct"])
            if "p99" in flags:
                score = max(score, p99_change_pct / thr["p99_increase_pct"])
            if "p999" in flags:
                score = max(score, p999_change_pct / thr["p999_increase_pct"])
            if "bytes" in flags:
                score = max(score, bytes_change_pct / thr["bytes_increase_pct"])
            item["severity_score"] = score
            regressions.append(item)

    regressions.sort(key=lambda x: x.get("severity_score", 0.0), reverse=True)

    by_layer: Dict[str, Dict[str, int]] = {}
    for item in comparisons:
        layer = item["layer"]
        st = by_layer.setdefault(layer, {"total": 0, "regressions": 0})
        st["total"] += 1
        if item["flags"]:
            st["regressions"] += 1

    missing_issue_count = len(missing_in_current) if args.strict_missing else 0
    failure_count = len(regressions) + missing_issue_count
    status = "pass" if failure_count == 0 else "fail"

    report = {
        "status": status,
        "baseline_root": str(baseline_root),
        "current_root": str(current_root),
        "backends": sorted(backend_set),
        "threshold_file": str((repo_root / args.thresholds).resolve()),
        "totals": {
            "compared_pairs": len(common_keys),
            "regressions": len(regressions),
            "missing_in_current": len(missing_in_current),
            "new_in_current": len(new_in_current),
            "missing_counted_as_failures": missing_issue_count,
            "failure_count": failure_count,
        },
        "by_layer": by_layer,
        "regressions": regressions,
        "missing_in_current": [{"scenario": s, "backend": b} for (s, b) in missing_in_current],
        "new_in_current": [{"scenario": s, "backend": b} for (s, b) in new_in_current],
    }

    out_md = args.out_md.resolve() if args.out_md else current_root / "regression_report.md"
    out_json = args.out_json.resolve() if args.out_json else current_root / "regression_report.json"

    lines: List[str] = []
    lines.append("# Benchmark Regression Report")
    lines.append("")
    lines.append(f"- baseline_root: `{baseline_root}`")
    lines.append(f"- current_root: `{current_root}`")
    lines.append(f"- backends: `{','.join(sorted(backend_set))}`")
    lines.append(f"- threshold_file: `{(repo_root / args.thresholds).resolve()}`")
    lines.append(f"- strict_missing: `{args.strict_missing}`")
    lines.append(f"- status: `{status}`")
    lines.append("")
    lines.append("## Summary")
    lines.append("")
    lines.extend(
        to_markdown_table(
            ["Item", "Value"],
            [
                ["compared_pairs", str(report["totals"]["compared_pairs"])],
                ["regressions", str(report["totals"]["regressions"])],
                ["missing_in_current", str(report["totals"]["missing_in_current"])],
                ["new_in_current", str(report["totals"]["new_in_current"])],
                ["failure_count", str(report["totals"]["failure_count"])],
            ],
        )
    )
    lines.append("")

    lines.append("## Layer Breakdown")
    lines.append("")
    lines.extend(
        to_markdown_table(
            ["Layer", "Compared", "Regressions", "Regression Rate"],
            [
                [
                    layer,
                    str(v["total"]),
                    str(v["regressions"]),
                    f"{(v['regressions'] / v['total'] * 100.0):.1f}%" if v["total"] else "0.0%",
                ]
                for layer, v in sorted(by_layer.items())
            ],
        )
    )
    lines.append("")

    lines.append("## Top Regressions")
    lines.append("")
    if regressions:
        lines.extend(
            to_markdown_table(
                [
                    "Scenario",
                    "Backend",
                    "Layer",
                    "Throughput",
                    "Avg",
                    "P99",
                    "P999",
                    "Bytes",
                    "Flags",
                    "Severity",
                ],
                [
                    [
                        r["scenario"],
                        r["backend"],
                        r["layer"],
                        format_pct(r["throughput_change_pct"]),
                        format_pct(r["avg_change_pct"]),
                        format_pct(r["p99_change_pct"]),
                        format_pct(r["p999_change_pct"]),
                        format_pct(r["bytes_change_pct"]),
                        ",".join(r["flags"]),
                        f"{r['severity_score']:.2f}",
                    ]
                    for r in regressions[: args.top_n]
                ],
            )
        )
    else:
        lines.append("No threshold violations detected.")
    lines.append("")

    lines.append("## Missing/New Pairs")
    lines.append("")
    lines.extend(
        to_markdown_table(
            ["Type", "Count", "Pairs"],
            [
                [
                    "missing_in_current",
                    str(len(missing_in_current)),
                    ", ".join(f"{s}/{b}" for (s, b) in missing_in_current[: args.top_n]) or "-",
                ],
                [
                    "new_in_current",
                    str(len(new_in_current)),
                    ", ".join(f"{s}/{b}" for (s, b) in new_in_current[: args.top_n]) or "-",
                ],
            ],
        )
    )
    lines.append("")

    out_md.parent.mkdir(parents=True, exist_ok=True)
    out_md.write_text("\n".join(lines) + "\n", encoding="utf-8")
    out_json.parent.mkdir(parents=True, exist_ok=True)
    out_json.write_text(json.dumps(report, ensure_ascii=False, indent=2) + "\n", encoding="utf-8")

    summary = {
        "status": status,
        "compared_pairs": len(common_keys),
        "regressions": len(regressions),
        "missing_in_current": len(missing_in_current),
        "new_in_current": len(new_in_current),
        "failure_count": failure_count,
        "out_md": str(out_md),
        "out_json": str(out_json),
    }
    print(json.dumps(summary, ensure_ascii=False))

    if failure_count > 0 and not args.allow_regressions:
        raise SystemExit(2)


if __name__ == "__main__":
    main()
