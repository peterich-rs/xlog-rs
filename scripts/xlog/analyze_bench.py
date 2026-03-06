#!/usr/bin/env python3
"""Analyze xlog benchmark artifacts and generate a compact report.

Example:
  python3 scripts/xlog/analyze_bench.py
  python3 scripts/xlog/analyze_bench.py --root artifacts/bench-compare/20260306-full-matrix-latest
"""

from __future__ import annotations

import argparse
import json
import math
from pathlib import Path
from typing import Any, Dict, Iterable, List, Sequence


def load_json(path: Path) -> Any:
    return json.loads(path.read_text(encoding="utf-8"))


def load_jsonl(path: Path) -> List[Dict[str, Any]]:
    rows: List[Dict[str, Any]] = []
    if not path.exists():
        return rows
    with path.open("r", encoding="utf-8") as f:
        for line in f:
            line = line.strip()
            if not line:
                continue
            rows.append(json.loads(line))
    return rows


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


def latest_bench_root(base: Path) -> Path:
    if not base.exists():
        raise FileNotFoundError(f"benchmark base directory not found: {base}")
    candidates = [p for p in base.iterdir() if p.is_dir()]
    if not candidates:
        raise FileNotFoundError(f"no benchmark directories under: {base}")
    candidates.sort(key=lambda p: (p.stat().st_mtime, p.name), reverse=True)
    return candidates[0]


def median(values: Sequence[float]) -> float:
    if not values:
        return float("nan")
    data = sorted(values)
    n = len(data)
    mid = n // 2
    if n % 2 == 1:
        return data[mid]
    return (data[mid - 1] + data[mid]) / 2.0


def gmean(values: Sequence[float]) -> float:
    if not values:
        return float("nan")
    if any(v <= 0 for v in values):
        return float("nan")
    return math.exp(sum(math.log(v) for v in values) / len(values))


def classify_scenario(name: str) -> str:
    return "async" if name.startswith("async_") else "sync"


def to_percent_from_ratio(ratio: float) -> float:
    return (ratio - 1.0) * 100.0


def roundf(v: float, digits: int = 3) -> float:
    if isinstance(v, float) and (math.isnan(v) or math.isinf(v)):
        return v
    return round(float(v), digits)


def to_markdown_table(headers: Sequence[str], rows: Sequence[Sequence[str]]) -> List[str]:
    lines = [
        "| " + " | ".join(headers) + " |",
        "| " + " | ".join([":---"] + ["---:" for _ in headers[1:]]) + " |",
    ]
    for row in rows:
        lines.append("| " + " | ".join(row) + " |")
    return lines


def build_pairs(summary_rows: List[Dict[str, Any]]) -> List[Dict[str, Any]]:
    by_scenario: Dict[str, Dict[str, Dict[str, Any]]] = {}
    for row in summary_rows:
        by_scenario.setdefault(row["scenario"], {})[row["backend"]] = row

    pairs: List[Dict[str, Any]] = []
    for scenario in sorted(by_scenario):
        item = by_scenario[scenario]
        rust = item.get("rust")
        cpp = item.get("cpp")
        if not rust or not cpp:
            continue
        pairs.append(
            {
                "scenario": scenario,
                "class": classify_scenario(scenario),
                "rust": {
                    "throughput_mps": float(rust["throughput_mps"]),
                    "lat_avg_ns": float(rust["lat_avg_ns"]),
                    "lat_p99_ns": float(rust["lat_p99_ns"]),
                    "lat_p999_ns": float(rust["lat_p999_ns"]),
                    "bytes_per_msg": float(rust["bytes_per_msg"]),
                },
                "cpp": {
                    "throughput_mps": float(cpp["throughput_mps"]),
                    "lat_avg_ns": float(cpp["lat_avg_ns"]),
                    "lat_p99_ns": float(cpp["lat_p99_ns"]),
                    "lat_p999_ns": float(cpp["lat_p999_ns"]),
                    "bytes_per_msg": float(cpp["bytes_per_msg"]),
                },
                "thr_ratio": float(rust["throughput_mps"]) / float(cpp["throughput_mps"]),
                "avg_ratio": float(rust["lat_avg_ns"]) / float(cpp["lat_avg_ns"]),
                "p99_ratio": float(rust["lat_p99_ns"]) / float(cpp["lat_p99_ns"]),
                "p999_ratio": float(rust["lat_p999_ns"]) / float(cpp["lat_p999_ns"]),
                "bytes_ratio": float(rust["bytes_per_msg"]) / float(cpp["bytes_per_msg"]),
            }
        )
    return pairs


def subset_stats(rows: Sequence[Dict[str, Any]]) -> Dict[str, Any]:
    if not rows:
        return {
            "count": 0,
            "thr_better": 0,
            "avg_better": 0,
            "p99_better": 0,
            "p999_better": 0,
            "bytes_lower": 0,
        }

    thr = [r["thr_ratio"] for r in rows]
    avg = [r["avg_ratio"] for r in rows]
    p99 = [r["p99_ratio"] for r in rows]
    p999 = [r["p999_ratio"] for r in rows]
    bpp = [r["bytes_ratio"] for r in rows]

    return {
        "count": len(rows),
        "thr_better": sum(1 for x in thr if x > 1.0),
        "avg_better": sum(1 for x in avg if x < 1.0),
        "p99_better": sum(1 for x in p99 if x < 1.0),
        "p999_better": sum(1 for x in p999 if x < 1.0),
        "bytes_lower": sum(1 for x in bpp if x < 1.0),
        "thr_gmean": gmean(thr),
        "avg_gmean": gmean(avg),
        "p99_gmean": gmean(p99),
        "p999_gmean": gmean(p999),
        "bytes_gmean": gmean(bpp),
        "thr_median": median(thr),
        "avg_median": median(avg),
        "p99_median": median(p99),
        "p999_median": median(p999),
        "bytes_median": median(bpp),
    }


def compute_layer_map(repo_root: Path) -> Dict[str, str]:
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


def aggregate_raw_by_backend(raw_rows: Sequence[Dict[str, Any]]) -> Dict[str, Dict[str, float]]:
    out: Dict[str, Dict[str, float]] = {}
    for row in raw_rows:
        backend = row.get("backend")
        result = row.get("result") or {}
        if backend is None:
            continue
        entry = out.setdefault(
            backend,
            {
                "scenario_count": 0.0,
                "total_messages": 0.0,
                "total_elapsed_ms": 0.0,
                "total_output_bytes": 0.0,
            },
        )
        entry["scenario_count"] += 1
        entry["total_messages"] += float(result.get("messages", 0.0))
        entry["total_elapsed_ms"] += float(result.get("elapsed_ms", 0.0))
        entry["total_output_bytes"] += float(result.get("output_bytes", 0.0))

    for backend, entry in out.items():
        elapsed = entry["total_elapsed_ms"]
        msgs = entry["total_messages"]
        entry["aggregate_mps"] = (msgs / elapsed * 1000.0) if elapsed > 0 else float("nan")
        entry["avg_bytes_per_msg"] = (entry["total_output_bytes"] / msgs) if msgs > 0 else float("nan")
        entry["scenario_count"] = int(entry["scenario_count"])
    return out


def component_summary(rows: Sequence[Dict[str, Any]]) -> Dict[str, Any]:
    if not rows:
        return {"has_components": False, "variants": []}

    by_variant: Dict[str, List[Dict[str, Any]]] = {}
    for row in rows:
        by_variant.setdefault(str(row.get("variant", "unknown")), []).append(row)

    def numeric_values(items: Sequence[Dict[str, Any]], key: str) -> List[float]:
        out: List[float] = []
        for item in items:
            value = item.get(key)
            if isinstance(value, (int, float)):
                out.append(float(value))
        return out

    variants = []
    for variant, items in sorted(by_variant.items()):
        ops = [float(i.get("ops_per_sec", 0.0)) for i in items]
        ratios = [float(i.get("ratio", 0.0)) for i in items]
        cpu_user = numeric_values(items, "cpu_user_ms")
        cpu_system = numeric_values(items, "cpu_system_ms")
        rss = numeric_values(items, "max_rss_kb")
        io_read_syscalls = numeric_values(items, "io_read_syscalls")
        io_write_syscalls = numeric_values(items, "io_write_syscalls")
        io_write_bytes = numeric_values(items, "io_write_bytes")
        syscalls_per_op = numeric_values(items, "syscalls_per_op")
        scanned_entries = numeric_values(items, "scanned_entries")
        moved_files = numeric_values(items, "moved_files")
        deleted_files = numeric_values(items, "deleted_files")
        variants.append(
            {
                "variant": variant,
                "count": len(items),
                "ops_per_sec_median": median(ops),
                "ops_per_sec_mean": sum(ops) / len(ops),
                "ratio_mean": sum(ratios) / len(ratios),
                "ratio_min": min(ratios),
                "ratio_max": max(ratios),
                "cpu_user_ms_mean": (sum(cpu_user) / len(cpu_user)) if cpu_user else None,
                "cpu_user_ms_median": median(cpu_user) if cpu_user else None,
                "cpu_system_ms_mean": (sum(cpu_system) / len(cpu_system)) if cpu_system else None,
                "cpu_system_ms_median": median(cpu_system) if cpu_system else None,
                "max_rss_kb_max": max(rss) if rss else None,
                "io_read_syscalls_mean": (sum(io_read_syscalls) / len(io_read_syscalls))
                if io_read_syscalls
                else None,
                "io_write_syscalls_mean": (sum(io_write_syscalls) / len(io_write_syscalls))
                if io_write_syscalls
                else None,
                "io_write_mb_mean": (sum(io_write_bytes) / len(io_write_bytes) / (1024.0 * 1024.0))
                if io_write_bytes
                else None,
                "syscalls_per_op_mean": (sum(syscalls_per_op) / len(syscalls_per_op))
                if syscalls_per_op
                else None,
                "scanned_entries_mean": (sum(scanned_entries) / len(scanned_entries))
                if scanned_entries
                else None,
                "moved_files_sum": sum(moved_files) if moved_files else None,
                "deleted_files_sum": sum(deleted_files) if deleted_files else None,
            }
        )

    return {"has_components": True, "variants": variants}


def pair_to_tsv_row(p: Dict[str, Any]) -> str:
    cols = [
        p["scenario"],
        p["class"],
        f"{p['thr_ratio']:.9f}",
        f"{p['avg_ratio']:.9f}",
        f"{p['p99_ratio']:.9f}",
        f"{p['p999_ratio']:.9f}",
        f"{p['bytes_ratio']:.9f}",
        f"{p['rust']['throughput_mps']:.3f}",
        f"{p['cpp']['throughput_mps']:.3f}",
        f"{p['rust']['lat_p99_ns']:.3f}",
        f"{p['cpp']['lat_p99_ns']:.3f}",
        f"{p['rust']['lat_p999_ns']:.3f}",
        f"{p['cpp']['lat_p999_ns']:.3f}",
    ]
    return "\t".join(cols)


def main() -> None:
    parser = argparse.ArgumentParser(description="Analyze benchmark matrix artifacts")
    parser.add_argument("--root", type=Path, help="artifact root directory")
    parser.add_argument("--base", type=Path, default=Path("artifacts/bench-compare"), help="artifact base directory")
    parser.add_argument("--top-n", type=int, default=8, help="number of top/bottom scenarios in report")
    parser.add_argument("--strict-thr", type=float, default=1.2, help="strict winner throughput threshold")
    parser.add_argument("--tail-thr", type=float, default=1.5, help="tail regression threshold")
    parser.add_argument("--out-md", type=Path, help="output markdown report path")
    parser.add_argument("--out-json", type=Path, help="output json report path")
    parser.add_argument("--out-pairs", type=Path, help="output pair tsv path")
    args = parser.parse_args()

    repo_root = Path(__file__).resolve().parents[2]
    root = args.root.resolve() if args.root else latest_bench_root((repo_root / args.base).resolve())

    summary_path = root / "summary.json"
    raw_path = root / "results_raw.jsonl"
    metadata_path = root / "metadata.json"
    components_path = root / "components.jsonl"

    if not summary_path.exists():
        raise FileNotFoundError(f"summary file missing: {summary_path}")

    summary_rows = load_json(summary_path)
    raw_rows = load_jsonl(raw_path)
    metadata = load_json(metadata_path) if metadata_path.exists() else {}
    component_rows = load_jsonl(components_path)

    pairs = build_pairs(summary_rows)
    layer_map = compute_layer_map(repo_root)
    for p in pairs:
        p["layer"] = layer_map.get(p["scenario"], "unknown")

    overall = subset_stats(pairs)
    async_rows = [p for p in pairs if p["class"] == "async"]
    sync_rows = [p for p in pairs if p["class"] == "sync"]

    by_layer: Dict[str, Dict[str, Any]] = {}
    for layer in ("baseline", "stress", "feature", "unknown"):
        layer_rows = [p for p in pairs if p["layer"] == layer]
        if layer_rows:
            by_layer[layer] = subset_stats(layer_rows)

    pairs_sorted_thr_desc = sorted(pairs, key=lambda x: x["thr_ratio"], reverse=True)
    pairs_sorted_thr_asc = sorted(pairs, key=lambda x: x["thr_ratio"])
    pairs_sorted_p999_desc = sorted(pairs, key=lambda x: x["p999_ratio"], reverse=True)
    pairs_sorted_p999_asc = sorted(pairs, key=lambda x: x["p999_ratio"])

    strict_winners = [
        p
        for p in pairs
        if p["thr_ratio"] > args.strict_thr and p["p99_ratio"] < 1.0 and p["p999_ratio"] < 1.0
    ]
    tail_risk = [
        p
        for p in pairs
        if p["thr_ratio"] > 1.1 and (p["p99_ratio"] > args.tail_thr or p["p999_ratio"] > args.tail_thr)
    ]

    failures = [
        row
        for row in raw_rows
        if int(row.get("exit_code", 0) or 0) != 0 or str(row.get("error", "") or "") != ""
    ]

    raw_agg = aggregate_raw_by_backend(raw_rows)
    comp_summary = component_summary(component_rows)

    report = {
        "artifact_root": str(root),
        "metadata": metadata,
        "raw": {
            "total_rows": len(raw_rows),
            "failure_rows": len(failures),
            "failures": failures,
            "aggregate_by_backend": raw_agg,
        },
        "pair_stats": {
            "overall": overall,
            "async": subset_stats(async_rows),
            "sync": subset_stats(sync_rows),
            "layers": by_layer,
            "strict_winners": strict_winners,
            "tail_risk": tail_risk,
            "top_throughput": pairs_sorted_thr_desc[: args.top_n],
            "bottom_throughput": pairs_sorted_thr_asc[: args.top_n],
            "worst_p999": pairs_sorted_p999_desc[: args.top_n],
            "best_p999": pairs_sorted_p999_asc[: args.top_n],
        },
        "components": comp_summary,
    }

    out_md = args.out_md.resolve() if args.out_md else root / "analysis_report.md"
    out_json = args.out_json.resolve() if args.out_json else root / "analysis_report.json"
    out_pairs = args.out_pairs.resolve() if args.out_pairs else root / "analysis_pairs.tsv"

    out_pairs.parent.mkdir(parents=True, exist_ok=True)
    pair_header = "\t".join(
        [
            "scenario",
            "class",
            "thr_ratio",
            "avg_ratio",
            "p99_ratio",
            "p999_ratio",
            "bytes_ratio",
            "rust_throughput_mps",
            "cpp_throughput_mps",
            "rust_p99_ns",
            "cpp_p99_ns",
            "rust_p999_ns",
            "cpp_p999_ns",
        ]
    )
    out_pairs.write_text(
        pair_header + "\n" + "\n".join(pair_to_tsv_row(p) for p in pairs) + "\n",
        encoding="utf-8",
    )

    def fmt_ratio(r: float) -> str:
        return f"{r:.3f} ({to_percent_from_ratio(r):+.1f}%)"

    lines: List[str] = []
    lines.append("# Benchmark Analysis Report")
    lines.append("")
    lines.append(f"- artifact_root: `{root}`")
    if metadata:
        lines.append(f"- started_at_utc: `{metadata.get('started_at_utc', '')}`")
        lines.append(f"- finished_at_utc: `{metadata.get('finished_at_utc', '')}`")
        lines.append(f"- duration_seconds: `{metadata.get('duration_seconds', '')}`")
        lines.append(f"- backends: `{','.join(metadata.get('backends', []))}`")
        lines.append(f"- scenario_count: `{metadata.get('scenario_count', '')}`")
        lines.append(f"- runs: `{metadata.get('runs', '')}`")
    lines.append("")

    lines.append("## Data Integrity")
    lines.append("")
    lines.extend(
        to_markdown_table(
            ["Item", "Value"],
            [
                ["summary rows", str(len(summary_rows))],
                ["paired scenarios", str(len(pairs))],
                ["raw rows", str(len(raw_rows))],
                ["failure rows", str(len(failures))],
            ],
        )
    )
    lines.append("")

    lines.append("## Pair Stats")
    lines.append("")
    stats_rows = [
        ["overall", overall],
        ["async", subset_stats(async_rows)],
        ["sync", subset_stats(sync_rows)],
    ]
    for layer, st in by_layer.items():
        stats_rows.append([f"layer:{layer}", st])

    lines.extend(
        to_markdown_table(
            [
                "Group",
                "Count",
                "Thr Better",
                "Avg Better",
                "P99 Better",
                "P999 Better",
                "Bytes Lower",
                "Thr GMean",
                "P99 GMean",
                "P999 GMean",
            ],
            [
                [
                    name,
                    str(st["count"]),
                    f"{st['thr_better']}/{st['count']}",
                    f"{st['avg_better']}/{st['count']}",
                    f"{st['p99_better']}/{st['count']}",
                    f"{st['p999_better']}/{st['count']}",
                    f"{st['bytes_lower']}/{st['count']}",
                    f"{st['thr_gmean']:.3f}",
                    f"{st['p99_gmean']:.3f}",
                    f"{st['p999_gmean']:.3f}",
                ]
                for name, st in stats_rows
            ],
        )
    )
    lines.append("")

    lines.append("## Throughput Extremes")
    lines.append("")
    lines.extend(
        to_markdown_table(
            ["Scenario", "Thr Ratio", "P99 Ratio", "P999 Ratio", "Bytes Ratio"],
            [
                [
                    p["scenario"],
                    fmt_ratio(p["thr_ratio"]),
                    fmt_ratio(p["p99_ratio"]),
                    fmt_ratio(p["p999_ratio"]),
                    fmt_ratio(p["bytes_ratio"]),
                ]
                for p in pairs_sorted_thr_desc[: args.top_n]
            ],
        )
    )
    lines.append("")
    lines.extend(
        to_markdown_table(
            ["Scenario", "Thr Ratio", "P99 Ratio", "P999 Ratio", "Bytes Ratio"],
            [
                [
                    p["scenario"],
                    fmt_ratio(p["thr_ratio"]),
                    fmt_ratio(p["p99_ratio"]),
                    fmt_ratio(p["p999_ratio"]),
                    fmt_ratio(p["bytes_ratio"]),
                ]
                for p in pairs_sorted_thr_asc[: args.top_n]
            ],
        )
    )
    lines.append("")

    lines.append("## Tail Latency Extremes (P999)")
    lines.append("")
    lines.extend(
        to_markdown_table(
            ["Scenario", "P999 Ratio", "Thr Ratio", "P99 Ratio"],
            [
                [
                    p["scenario"],
                    fmt_ratio(p["p999_ratio"]),
                    fmt_ratio(p["thr_ratio"]),
                    fmt_ratio(p["p99_ratio"]),
                ]
                for p in pairs_sorted_p999_desc[: args.top_n]
            ],
        )
    )
    lines.append("")
    lines.extend(
        to_markdown_table(
            ["Scenario", "P999 Ratio", "Thr Ratio", "P99 Ratio"],
            [
                [
                    p["scenario"],
                    fmt_ratio(p["p999_ratio"]),
                    fmt_ratio(p["thr_ratio"]),
                    fmt_ratio(p["p99_ratio"]),
                ]
                for p in pairs_sorted_p999_asc[: args.top_n]
            ],
        )
    )
    lines.append("")

    lines.append("## Tradeoff Buckets")
    lines.append("")
    lines.extend(
        to_markdown_table(
            ["Bucket", "Count", "Scenarios"],
            [
                [
                    "strict_winners",
                    str(len(strict_winners)),
                    ", ".join(p["scenario"] for p in strict_winners) or "-",
                ],
                [
                    "tail_risk",
                    str(len(tail_risk)),
                    ", ".join(p["scenario"] for p in tail_risk) or "-",
                ],
            ],
        )
    )
    lines.append("")

    lines.append("## Raw Aggregate")
    lines.append("")
    lines.extend(
        to_markdown_table(
            ["Backend", "Scenarios", "Messages", "Elapsed ms", "Aggregate mps", "Avg bytes/msg"],
            [
                [
                    backend,
                    str(v["scenario_count"]),
                    str(int(v["total_messages"])),
                    f"{v['total_elapsed_ms']:.3f}",
                    f"{v['aggregate_mps']:.3f}",
                    f"{v['avg_bytes_per_msg']:.3f}",
                ]
                for backend, v in sorted(raw_agg.items())
            ],
        )
    )
    lines.append("")

    if comp_summary.get("has_components"):
        lines.append("## Component Microbench")
        lines.append("")

        def fmt_opt(value: Any, digits: int = 3) -> str:
            if value is None:
                return "-"
            return f"{float(value):.{digits}f}"

        lines.extend(
            to_markdown_table(
                [
                    "Variant",
                    "Count",
                    "Ops/s Mean",
                    "Ops/s Median",
                    "Ratio Mean",
                    "Syscalls/Op Mean",
                    "IO Write MB Mean",
                    "CPU User ms Mean",
                    "CPU Sys ms Mean",
                    "Max RSS KB",
                    "Scanned Mean",
                    "Moved Sum",
                    "Deleted Sum",
                    "Ratio Range",
                ],
                [
                    [
                        v["variant"],
                        str(v["count"]),
                        f"{v['ops_per_sec_mean']:.3f}",
                        f"{v['ops_per_sec_median']:.3f}",
                        f"{v['ratio_mean']:.6f}",
                        fmt_opt(v.get("syscalls_per_op_mean"), 6),
                        fmt_opt(v.get("io_write_mb_mean")),
                        fmt_opt(v.get("cpu_user_ms_mean")),
                        fmt_opt(v.get("cpu_system_ms_mean")),
                        fmt_opt(v.get("max_rss_kb_max"), 0),
                        fmt_opt(v.get("scanned_entries_mean")),
                        fmt_opt(v.get("moved_files_sum"), 0),
                        fmt_opt(v.get("deleted_files_sum"), 0),
                        f"{v['ratio_min']:.6f}..{v['ratio_max']:.6f}",
                    ]
                    for v in comp_summary["variants"]
                ],
            )
        )
        lines.append("")

    out_md.parent.mkdir(parents=True, exist_ok=True)
    out_md.write_text("\n".join(lines) + "\n", encoding="utf-8")
    out_json.parent.mkdir(parents=True, exist_ok=True)
    out_json.write_text(json.dumps(report, ensure_ascii=False, indent=2) + "\n", encoding="utf-8")

    summary_line = {
        "artifact_root": str(root),
        "out_md": str(out_md),
        "out_json": str(out_json),
        "out_pairs": str(out_pairs),
        "paired_scenarios": len(pairs),
        "failure_rows": len(failures),
        "strict_winners": len(strict_winners),
        "tail_risk": len(tail_risk),
    }
    print(json.dumps(summary_line, ensure_ascii=False))


if __name__ == "__main__":
    main()
