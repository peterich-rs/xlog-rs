#!/usr/bin/env python3
"""Compare benchmark roots and detect regressions for matrix/criterion artifacts.

Examples:
  python3 scripts/xlog/check_bench_regression.py \
    --baseline-root artifacts/bench-compare/20260301-baseline \
    --current-root artifacts/bench-compare/20260306-full-matrix-latest \
    --backend rust

  python3 scripts/xlog/check_bench_regression.py \
    --kind criterion \
    --baseline-root artifacts/criterion/20260308-initial-baseline \
    --current-root artifacts/criterion/20260309-current
"""

from __future__ import annotations

import argparse
import json
from pathlib import Path
from typing import Any, Dict, Iterable, List, Sequence, Tuple


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


def to_markdown_table(headers: Sequence[str], rows: Sequence[Sequence[str]]) -> List[str]:
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


def format_pct(v: float) -> str:
    return f"{v:+.1f}%"


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


def detect_kind(root: Path) -> str:
    if (root / "summary.json").exists():
        return "matrix"
    if (root / "criterion_summary.json").exists():
        return "criterion"
    raise FileNotFoundError(
        f"unable to detect benchmark kind under {root}: expected summary.json or criterion_summary.json"
    )


def build_matrix_index(summary_rows: List[Dict[str, Any]], backends: set[str]) -> Dict[Tuple[str, str], Dict[str, Any]]:
    out: Dict[Tuple[str, str], Dict[str, Any]] = {}
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


def build_criterion_index(summary_rows: List[Dict[str, Any]]) -> Dict[str, Dict[str, Any]]:
    out: Dict[str, Dict[str, Any]] = {}
    for row in summary_rows:
        bench_id = str(row.get("bench_id", ""))
        if not bench_id:
            continue
        out[bench_id] = {
            "bench_id": bench_id,
            "group_id": str(row.get("group_id", "")),
            "function_id": str(row.get("function_id", "")),
            "value_str": str(row.get("value_str", "")),
            "time_ns": float(row["time_ns"]),
            "mean_ns": float(row["mean_ns"]),
            "median_ns": float(row["median_ns"]),
            "std_dev_ns": float(row["std_dev_ns"]),
            "throughput_bytes_per_sec": float(row["throughput_bytes_per_sec"])
            if row.get("throughput_bytes_per_sec") is not None
            else None,
            "throughput_mib_per_sec": float(row["throughput_mib_per_sec"])
            if row.get("throughput_mib_per_sec") is not None
            else None,
            "bytes_per_iter": float(row["bytes_per_iter"]) if row.get("bytes_per_iter") is not None else None,
        }
    return out


def resolve_matrix_thresholds(config: Dict[str, Any], layer: str) -> Dict[str, float]:
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


def resolve_criterion_thresholds(config: Dict[str, Any], group_id: str, bench_id: str) -> Dict[str, float]:
    default = dict(config.get("criterion_default") or {})
    if "throughput_drop_pct" not in default:
        default["throughput_drop_pct"] = (config.get("default") or {}).get("throughput_drop_pct", 8.0)

    overrides = config.get("criterion_overrides") or {}
    default.update((overrides.get("groups") or {}).get(group_id) or {})
    default.update((overrides.get("benches") or {}).get(bench_id) or {})
    return {
        "throughput_drop_pct": float(default.get("throughput_drop_pct", 8.0)),
        "time_increase_pct": float(default.get("time_increase_pct", 10.0)),
        "mean_increase_pct": float(default.get("mean_increase_pct", 10.0)),
        "median_increase_pct": float(default.get("median_increase_pct", 10.0)),
        "std_dev_increase_pct": float(default.get("std_dev_increase_pct", 25.0)),
    }


def matrix_comparisons(
    repo_root: Path,
    baseline_root: Path,
    current_root: Path,
    backend_set: set[str],
    threshold_cfg: Dict[str, Any],
) -> Dict[str, Any]:
    baseline_summary = load_json(baseline_root / "summary.json")
    current_summary = load_json(current_root / "summary.json")
    layer_map = get_layer_map(repo_root)

    baseline_idx = build_matrix_index(baseline_summary, backend_set)
    current_idx = build_matrix_index(current_summary, backend_set)

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
        thr = resolve_matrix_thresholds(threshold_cfg, layer)

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
            "kind": "matrix",
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

    return {
        "kind": "matrix",
        "comparisons": comparisons,
        "regressions": regressions,
        "missing_in_current": [{"scenario": s, "backend": b} for (s, b) in missing_in_current],
        "new_in_current": [{"scenario": s, "backend": b} for (s, b) in new_in_current],
        "compared_pairs": len(common_keys),
        "by_layer": by_layer,
        "backends": sorted(backend_set),
    }


def criterion_comparisons(
    baseline_root: Path,
    current_root: Path,
    threshold_cfg: Dict[str, Any],
) -> Dict[str, Any]:
    baseline_summary = load_json(baseline_root / "criterion_summary.json")
    current_summary = load_json(current_root / "criterion_summary.json")

    baseline_idx = build_criterion_index(list(baseline_summary.get("benches") or []))
    current_idx = build_criterion_index(list(current_summary.get("benches") or []))

    baseline_keys = set(baseline_idx.keys())
    current_keys = set(current_idx.keys())
    common_keys = sorted(baseline_keys & current_keys)
    missing_in_current = sorted(baseline_keys - current_keys)
    new_in_current = sorted(current_keys - baseline_keys)

    regressions: List[Dict[str, Any]] = []
    comparisons: List[Dict[str, Any]] = []

    for bench_id in common_keys:
        baseline = baseline_idx[bench_id]
        current = current_idx[bench_id]
        thr = resolve_criterion_thresholds(threshold_cfg, baseline["group_id"], bench_id)

        throughput_change_pct = 0.0
        if baseline.get("throughput_bytes_per_sec") and current.get("throughput_bytes_per_sec"):
            throughput_change_pct = pct_change(
                float(current["throughput_bytes_per_sec"]),
                float(baseline["throughput_bytes_per_sec"]),
            )

        time_change_pct = pct_change(float(current["time_ns"]), float(baseline["time_ns"]))
        mean_change_pct = pct_change(float(current["mean_ns"]), float(baseline["mean_ns"]))
        median_change_pct = pct_change(float(current["median_ns"]), float(baseline["median_ns"]))
        std_dev_change_pct = pct_change(float(current["std_dev_ns"]), float(baseline["std_dev_ns"]))

        flags = []
        if baseline.get("throughput_bytes_per_sec") is not None and current.get("throughput_bytes_per_sec") is not None:
            if throughput_change_pct < -thr["throughput_drop_pct"]:
                flags.append("throughput")
        if time_change_pct > thr["time_increase_pct"]:
            flags.append("time")
        if mean_change_pct > thr["mean_increase_pct"]:
            flags.append("mean")
        if median_change_pct > thr["median_increase_pct"]:
            flags.append("median")
        if std_dev_change_pct > thr["std_dev_increase_pct"]:
            flags.append("stddev")

        item = {
            "kind": "criterion",
            "bench_id": bench_id,
            "group_id": baseline["group_id"],
            "function_id": baseline["function_id"],
            "value_str": baseline["value_str"],
            "time_change_pct": time_change_pct,
            "mean_change_pct": mean_change_pct,
            "median_change_pct": median_change_pct,
            "std_dev_change_pct": std_dev_change_pct,
            "throughput_change_pct": throughput_change_pct,
            "threshold_source": {
                "group_id": baseline["group_id"],
                "bench_id": bench_id,
            },
            "thresholds": thr,
            "flags": flags,
        }
        comparisons.append(item)
        if flags:
            score = 0.0
            if "throughput" in flags:
                score = max(score, abs(throughput_change_pct) / thr["throughput_drop_pct"])
            if "time" in flags:
                score = max(score, time_change_pct / thr["time_increase_pct"])
            if "mean" in flags:
                score = max(score, mean_change_pct / thr["mean_increase_pct"])
            if "median" in flags:
                score = max(score, median_change_pct / thr["median_increase_pct"])
            if "stddev" in flags:
                score = max(score, std_dev_change_pct / thr["std_dev_increase_pct"])
            item["severity_score"] = score
            regressions.append(item)

    regressions.sort(key=lambda x: x.get("severity_score", 0.0), reverse=True)

    by_group: Dict[str, Dict[str, int]] = {}
    for item in comparisons:
        group = item["group_id"] or "unknown"
        st = by_group.setdefault(group, {"total": 0, "regressions": 0})
        st["total"] += 1
        if item["flags"]:
            st["regressions"] += 1

    return {
        "kind": "criterion",
        "comparisons": comparisons,
        "regressions": regressions,
        "missing_in_current": [{"bench_id": bench_id} for bench_id in missing_in_current],
        "new_in_current": [{"bench_id": bench_id} for bench_id in new_in_current],
        "compared_pairs": len(common_keys),
        "by_layer": by_group,
        "backends": [],
    }


def render_matrix_report(
    lines: List[str],
    regressions: List[Dict[str, Any]],
    by_layer: Dict[str, Dict[str, int]],
    missing_in_current: List[Dict[str, Any]],
    new_in_current: List[Dict[str, Any]],
    top_n: int,
) -> None:
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
                    for r in regressions[:top_n]
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
                    ", ".join(f"{item['scenario']}/{item['backend']}" for item in missing_in_current[:top_n]) or "-",
                ],
                [
                    "new_in_current",
                    str(len(new_in_current)),
                    ", ".join(f"{item['scenario']}/{item['backend']}" for item in new_in_current[:top_n]) or "-",
                ],
            ],
        )
    )
    lines.append("")


def render_criterion_report(
    lines: List[str],
    regressions: List[Dict[str, Any]],
    by_group: Dict[str, Dict[str, int]],
    missing_in_current: List[Dict[str, Any]],
    new_in_current: List[Dict[str, Any]],
    top_n: int,
) -> None:
    lines.append("## Group Breakdown")
    lines.append("")
    lines.extend(
        to_markdown_table(
            ["Group", "Compared", "Regressions", "Regression Rate"],
            [
                [
                    group,
                    str(v["total"]),
                    str(v["regressions"]),
                    f"{(v['regressions'] / v['total'] * 100.0):.1f}%" if v["total"] else "0.0%",
                ]
                for group, v in sorted(by_group.items())
            ],
        )
    )
    lines.append("")

    lines.append("## Top Regressions")
    lines.append("")
    if regressions:
        lines.extend(
            to_markdown_table(
                ["Bench", "Group", "Time", "Mean", "Median", "StdDev", "Throughput", "Flags", "Severity"],
                [
                    [
                        r["bench_id"],
                        r["group_id"],
                        format_pct(r["time_change_pct"]),
                        format_pct(r["mean_change_pct"]),
                        format_pct(r["median_change_pct"]),
                        format_pct(r["std_dev_change_pct"]),
                        format_pct(r["throughput_change_pct"]),
                        ",".join(r["flags"]),
                        f"{r['severity_score']:.2f}",
                    ]
                    for r in regressions[:top_n]
                ],
            )
        )
    else:
        lines.append("No threshold violations detected.")
    lines.append("")

    lines.append("## Missing/New Benches")
    lines.append("")
    lines.extend(
        to_markdown_table(
            ["Type", "Count", "Benches"],
            [
                [
                    "missing_in_current",
                    str(len(missing_in_current)),
                    ", ".join(item["bench_id"] for item in missing_in_current[:top_n]) or "-",
                ],
                [
                    "new_in_current",
                    str(len(new_in_current)),
                    ", ".join(item["bench_id"] for item in new_in_current[:top_n]) or "-",
                ],
            ],
        )
    )
    lines.append("")


def main() -> None:
    parser = argparse.ArgumentParser(description="Compare benchmark roots and gate regressions")
    parser.add_argument("--baseline-root", type=Path, required=True, help="baseline artifact root")
    parser.add_argument("--current-root", type=Path, required=True, help="current artifact root")
    parser.add_argument(
        "--kind",
        type=str,
        default="auto",
        choices=["auto", "matrix", "criterion"],
        help="artifact kind (default: auto)",
    )
    parser.add_argument(
        "--backend",
        type=str,
        default="rust",
        help="comma-separated backends to compare for matrix artifacts (default: rust)",
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

    kind = args.kind
    if kind == "auto":
        baseline_kind = detect_kind(baseline_root)
        current_kind = detect_kind(current_root)
        if baseline_kind != current_kind:
            raise ValueError(
                f"baseline/current artifact kind mismatch: {baseline_kind} vs {current_kind}"
            )
        kind = baseline_kind

    threshold_cfg = load_json((repo_root / args.thresholds).resolve())

    if kind == "matrix":
        result = matrix_comparisons(repo_root, baseline_root, current_root, backend_set, threshold_cfg)
    else:
        result = criterion_comparisons(baseline_root, current_root, threshold_cfg)

    missing_issue_count = len(result["missing_in_current"]) if args.strict_missing else 0
    failure_count = len(result["regressions"]) + missing_issue_count
    status = "pass" if failure_count == 0 else "fail"

    report = {
        "status": status,
        "kind": kind,
        "baseline_root": str(baseline_root),
        "current_root": str(current_root),
        "backends": result.get("backends", []),
        "threshold_file": str((repo_root / args.thresholds).resolve()),
        "totals": {
            "compared_pairs": result["compared_pairs"],
            "regressions": len(result["regressions"]),
            "missing_in_current": len(result["missing_in_current"]),
            "new_in_current": len(result["new_in_current"]),
            "missing_counted_as_failures": missing_issue_count,
            "failure_count": failure_count,
        },
        "by_layer": result["by_layer"],
        "regressions": result["regressions"],
        "missing_in_current": result["missing_in_current"],
        "new_in_current": result["new_in_current"],
    }

    out_md = args.out_md.resolve() if args.out_md else current_root / "regression_report.md"
    out_json = args.out_json.resolve() if args.out_json else current_root / "regression_report.json"

    lines: List[str] = []
    lines.append("# Benchmark Regression Report")
    lines.append("")
    lines.append(f"- kind: `{kind}`")
    lines.append(f"- baseline_root: `{baseline_root}`")
    lines.append(f"- current_root: `{current_root}`")
    if kind == "matrix":
        lines.append(f"- backends: `{','.join(sorted(result.get('backends', [])))}`")
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

    if kind == "matrix":
        render_matrix_report(
            lines,
            result["regressions"],
            result["by_layer"],
            result["missing_in_current"],
            result["new_in_current"],
            args.top_n,
        )
    else:
        render_criterion_report(
            lines,
            result["regressions"],
            result["by_layer"],
            result["missing_in_current"],
            result["new_in_current"],
            args.top_n,
        )

    out_md.parent.mkdir(parents=True, exist_ok=True)
    out_md.write_text("\n".join(lines) + "\n", encoding="utf-8")
    out_json.parent.mkdir(parents=True, exist_ok=True)
    out_json.write_text(json.dumps(report, ensure_ascii=False, indent=2) + "\n", encoding="utf-8")

    summary = {
        "status": status,
        "kind": kind,
        "compared_pairs": report["totals"]["compared_pairs"],
        "regressions": report["totals"]["regressions"],
        "missing_in_current": report["totals"]["missing_in_current"],
        "new_in_current": report["totals"]["new_in_current"],
        "failure_count": report["totals"]["failure_count"],
        "out_md": str(out_md),
        "out_json": str(out_json),
    }
    print(json.dumps(summary, ensure_ascii=False))

    if failure_count > 0 and not args.allow_regressions:
        raise SystemExit(2)


if __name__ == "__main__":
    main()
