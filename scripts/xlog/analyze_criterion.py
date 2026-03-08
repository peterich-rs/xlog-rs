#!/usr/bin/env python3
"""Normalize Criterion benchmark artifacts into a compact summary/report.

Examples:
  python3 scripts/xlog/analyze_criterion.py
  python3 scripts/xlog/analyze_criterion.py --root target/criterion --profile initial-baseline
"""

from __future__ import annotations

import argparse
import json
import math
from pathlib import Path
from typing import Any, Dict, Iterable, List, Sequence


DEFAULT_ROOT = Path("target/criterion")


def load_json(path: Path) -> Any:
    return json.loads(path.read_text(encoding="utf-8"))


def latest_profile_dirs(root: Path) -> List[Path]:
    matches = sorted(root.rglob("new/estimates.json"))
    return matches


def iter_profile_dirs(root: Path, profile: str) -> Iterable[Path]:
    for estimates in sorted(root.rglob(f"{profile}/estimates.json")):
        if estimates.parent.name != profile:
            continue
        benchmark_json = estimates.parent / "benchmark.json"
        if not benchmark_json.exists():
            continue
        yield estimates.parent


def ns_to_throughput_bytes_per_sec(bytes_per_iter: float, time_ns: float) -> float | None:
    if bytes_per_iter <= 0 or time_ns <= 0:
        return None
    return bytes_per_iter * 1_000_000_000.0 / time_ns


def roundf(value: float | None, digits: int = 3) -> float | None:
    if value is None:
        return None
    if math.isnan(value) or math.isinf(value):
        return value
    return round(float(value), digits)


def to_markdown_table(headers: Sequence[str], rows: Sequence[Sequence[str]]) -> List[str]:
    lines = [
        "| " + " | ".join(headers) + " |",
        "| " + " | ".join([":---"] + ["---:" for _ in headers[1:]]) + " |",
    ]
    for row in rows:
        lines.append("| " + " | ".join(row) + " |")
    return lines


def fmt_opt(value: float | None, digits: int = 3) -> str:
    if value is None:
        return "-"
    return f"{value:.{digits}f}"


def parse_rows(root: Path, profile: str) -> List[Dict[str, Any]]:
    rows: List[Dict[str, Any]] = []
    for profile_dir in iter_profile_dirs(root, profile):
        benchmark = load_json(profile_dir / "benchmark.json")
        estimates = load_json(profile_dir / "estimates.json")
        throughput = benchmark.get("throughput") or {}
        bytes_per_iter = None
        if isinstance(throughput, dict) and "Bytes" in throughput:
            bytes_per_iter = float(throughput["Bytes"])

        mean_ns = float(estimates["mean"]["point_estimate"])
        median_ns = float(estimates["median"]["point_estimate"])
        std_dev_ns = float(estimates["std_dev"]["point_estimate"])
        slope = estimates.get("slope") or {}
        slope_ns = float(slope.get("point_estimate", mean_ns))
        throughput_bps = ns_to_throughput_bytes_per_sec(bytes_per_iter or 0.0, slope_ns)
        throughput_mib = None
        if throughput_bps is not None:
            throughput_mib = throughput_bps / (1024.0 * 1024.0)

        rows.append(
            {
                "bench_id": str(benchmark.get("full_id", "")),
                "directory_name": str(benchmark.get("directory_name", "")),
                "group_id": str(benchmark.get("group_id", "")),
                "function_id": str(benchmark.get("function_id", "")),
                "value_str": str(benchmark.get("value_str", "")),
                "profile": profile,
                "time_ns": roundf(slope_ns),
                "mean_ns": roundf(mean_ns),
                "median_ns": roundf(median_ns),
                "std_dev_ns": roundf(std_dev_ns),
                "bytes_per_iter": roundf(bytes_per_iter),
                "throughput_bytes_per_sec": roundf(throughput_bps),
                "throughput_mib_per_sec": roundf(throughput_mib),
            }
        )

    rows.sort(key=lambda item: item["bench_id"])
    return rows


def group_summary(rows: Sequence[Dict[str, Any]]) -> List[Dict[str, Any]]:
    grouped: Dict[str, List[Dict[str, Any]]] = {}
    for row in rows:
        grouped.setdefault(row["group_id"], []).append(row)

    summary: List[Dict[str, Any]] = []
    for group_id, items in sorted(grouped.items()):
        time_values = [float(item["time_ns"]) for item in items if item.get("time_ns") is not None]
        throughput_values = [
            float(item["throughput_mib_per_sec"])
            for item in items
            if item.get("throughput_mib_per_sec") is not None
        ]
        summary.append(
            {
                "group_id": group_id,
                "bench_count": len(items),
                "time_ns_mean": roundf(sum(time_values) / len(time_values)) if time_values else None,
                "time_ns_max": roundf(max(time_values)) if time_values else None,
                "throughput_mib_mean": roundf(sum(throughput_values) / len(throughput_values))
                if throughput_values
                else None,
                "throughput_mib_min": roundf(min(throughput_values)) if throughput_values else None,
            }
        )
    return summary


def main() -> None:
    parser = argparse.ArgumentParser(description="Analyze Criterion benchmark output")
    parser.add_argument("--root", type=Path, default=DEFAULT_ROOT, help="criterion root (default: target/criterion)")
    parser.add_argument("--profile", type=str, default="new", help="criterion profile/baseline directory name")
    parser.add_argument("--out-json", type=Path, help="output summary json path")
    parser.add_argument("--out-md", type=Path, help="output markdown report path")
    args = parser.parse_args()

    root = args.root.resolve()
    if not root.exists():
        raise FileNotFoundError(f"criterion root missing: {root}")

    rows = parse_rows(root, args.profile)
    if not rows:
        raise FileNotFoundError(f"no Criterion benchmarks found under {root} for profile {args.profile!r}")

    groups = group_summary(rows)
    report = {
        "criterion_root": str(root),
        "profile": args.profile,
        "bench_count": len(rows),
        "groups": groups,
        "benches": rows,
    }

    out_json = args.out_json.resolve() if args.out_json else root / f"{args.profile}_summary.json"
    out_md = args.out_md.resolve() if args.out_md else root / f"{args.profile}_report.md"

    lines: List[str] = []
    lines.append("# Criterion Benchmark Report")
    lines.append("")
    lines.append(f"- criterion_root: `{root}`")
    lines.append(f"- profile: `{args.profile}`")
    lines.append(f"- bench_count: `{len(rows)}`")
    lines.append("")
    lines.append("## Groups")
    lines.append("")
    lines.extend(
        to_markdown_table(
            ["Group", "Benches", "Time Mean (ns)", "Time Max (ns)", "Thr Mean (MiB/s)", "Thr Min (MiB/s)"],
            [
                [
                    item["group_id"],
                    str(item["bench_count"]),
                    fmt_opt(item.get("time_ns_mean")),
                    fmt_opt(item.get("time_ns_max")),
                    fmt_opt(item.get("throughput_mib_mean")),
                    fmt_opt(item.get("throughput_mib_min")),
                ]
                for item in groups
            ],
        )
    )
    lines.append("")
    lines.append("## Bench Summary")
    lines.append("")
    lines.extend(
        to_markdown_table(
            ["Bench", "Time (ns)", "Mean (ns)", "Median (ns)", "StdDev (ns)", "Bytes/Iter", "Thr (MiB/s)"],
            [
                [
                    row["bench_id"],
                    fmt_opt(row.get("time_ns")),
                    fmt_opt(row.get("mean_ns")),
                    fmt_opt(row.get("median_ns")),
                    fmt_opt(row.get("std_dev_ns")),
                    fmt_opt(row.get("bytes_per_iter"), 0),
                    fmt_opt(row.get("throughput_mib_per_sec")),
                ]
                for row in rows
            ],
        )
    )
    lines.append("")

    out_json.parent.mkdir(parents=True, exist_ok=True)
    out_json.write_text(json.dumps(report, ensure_ascii=False, indent=2) + "\n", encoding="utf-8")
    out_md.parent.mkdir(parents=True, exist_ok=True)
    out_md.write_text("\n".join(lines) + "\n", encoding="utf-8")

    print(
        json.dumps(
            {
                "criterion_root": str(root),
                "profile": args.profile,
                "bench_count": len(rows),
                "out_json": str(out_json),
                "out_md": str(out_md),
            },
            ensure_ascii=False,
        )
    )


if __name__ == "__main__":
    main()
