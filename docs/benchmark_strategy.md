# Benchmark 基线与扩展策略

## 1. 文档定位

本文是 benchmark 体系的独立入口，专门记录：

1. 当前 benchmark 基础设施已经具备的能力
2. 现阶段仍然存在的可信度与可归因缺口
3. 后续的扩展顺序与治理要求

迁移评审文档只保留项目级结论，不再重复承载完整 benchmark 设计细节。

## 1.1 最近实现进展（2026-03-06）

本次实现已补齐一批文档里定义的扩展项，重点如下：

1. 端到端 benchmark 入口扩展
   - `crates/xlog/examples/bench_backend.rs` 新增 `compress-level / pub-key / warmup / time-buckets / json-pretty`
   - 新增 `payload_profile`（`compressible / semi_structured / human_text / high_entropy`）与 `payload_seed`
   - 输出指标补齐为 `lat_min / avg / stdev / p50 / p95 / p99 / p999 / max`、`output_bytes`、`bytes_per_msg`、timeline bucket
2. 诊断层微基准入口落地
   - 新增 `crates/xlog-core/examples/bench_components.rs`
   - 覆盖 `zlib stream level 6/9`、`zstd stream/chunk`、`TEA encrypt`、`ECDH derive`、`formatter` 相关基准
3. manifest-driven runner 增强
   - `scripts/xlog/run_bench_matrix.sh` 新增 backend 顺序策略：`fixed / alternating / randomized`
   - `randomized` 支持 seed，默认记录到 metadata，便于复现
   - `metadata.json` 扩展了 CPU 型号、内存总量、OS 版本、governor/频率策略、`rustc/cargo` 版本、manifest hash
4. 矩阵分层落地
   - 新增 `scripts/xlog/bench_matrix_baseline.tsv`
   - 新增 `scripts/xlog/bench_matrix_stress.tsv`
   - 新增 `scripts/xlog/bench_matrix_feature.tsv`
   - `scripts/xlog/bench_matrix.tsv` 保留为综合矩阵，并显式标注为 legacy all-in-one

## 1.2 最新双端全量结果摘要（2026-03-06）

数据来源：

1. 全量矩阵目录：`artifacts/bench-compare/20260306-full-matrix-latest`
2. 场景规模：`31 scenarios × 2 backends × 1 run = 62` 条 raw 结果
3. 完整性：`failures = 0`

对比口径（Rust/CPP）：

1. 吞吐更优场景：`20 / 31`（`64.5%`）
2. 平均延迟更优场景：`20 / 31`（`64.5%`）
3. P99 更优场景：`12 / 31`（`38.7%`）
4. P999 更优场景：`12 / 31`（`38.7%`）

按分层矩阵（几何均值，ratio < 1 表示 Rust 延迟/体积更低）：

1. baseline（10 场景）
   - throughput ratio gmean: `1.456`
   - p99 ratio gmean: `1.384`
   - p999 ratio gmean: `1.129`
2. stress（10 场景）
   - throughput ratio gmean: `1.484`
   - p99 ratio gmean: `0.789`
   - p999 ratio gmean: `0.934`
3. feature（11 场景）
   - throughput ratio gmean: `1.089`
   - p99 ratio gmean: `2.688`
   - p999 ratio gmean: `1.980`

关键结论：

1. Rust 在吞吐与均值延迟上整体领先，但 async 特性路径存在明显 tail 风险，尤其 flush / zstd / 4t+ 场景。
2. sync 压力与 boundary/cache 场景 Rust 优势明显，且 tail 也有显著改善。
3. feature 矩阵目前是主要风险区，后续优化优先级应放在 async tail 收敛而不是继续扩大吞吐峰值。

不提交全量产物的落地策略：

1. 仓库内仅保留分析脚本与结论摘要文档。
2. 大体量 benchmark 原始产物继续放在本地 artifacts 或外部制品系统，不进 git。
3. 每次重跑后更新本节关键汇总值即可满足回归审查。
4. 统一使用 `python3 scripts/xlog/analyze_bench.py --root <artifact_dir>` 生成汇总报告与对比表。

## 1.3 Async P2 定向结果（2026-03-07）

本轮只做 Rust 定向场景验证（`runs=3`），用于评估 async 链路 `P2`（producer 单线程走 clone，多 producer 自动切分片 buffer 池）的收益是否稳定。

执行命令：

1. `scripts/xlog/run_bench_matrix.sh --manifest scripts/xlog/bench_matrix_baseline.tsv --out-root /tmp/xlog-async-stage-profile-20260307-p2hybrid --runs 3 --backends rust --filter '^(async_1t|async_4t_flush256)$' --skip-build`

对比基线：

1. 参考基线为上一轮无池化版本（worker batch + flush merge），口径同样是 `runs=3`

结果摘要（Rust）：

1. `async_1t`
   - throughput: `286008.780 -> 286113.772`（`+0.04%`）
   - p99: `7125.333ns -> 7430.333ns`（`+4.28%`）
   - p999: `41874.667ns -> 42208.000ns`（`+0.80%`）
2. `async_4t_flush256`
   - throughput: `242716.457 -> 247018.270`（`+1.77%`）
   - p99: `57791.667ns -> 57652.333ns`（`-0.24%`）
   - p999: `148180.667ns -> 147569.667ns`（`-0.41%`）

当前结论：

1. `P2` 在多线程 flush 场景（`async_4t_flush256`）有可复现增益。
2. `async_1t` 吞吐基本持平，但 tail（p99/p999）略有回退，仍需下一轮继续压缩固定成本和尾延迟。
3. 按“只提交脚本与结论、不提交原始产物”规则，本节仅保留统计结论，完整原始数据继续留在本地 artifacts。

## 2. 当前判断

当前 benchmark 体系已经比 `artifacts/bench-compare/20260306-harness-matrix-rerun` 阶段完整得多，但仍然不能直接视为“性能归因体系已经完成”。

更准确的判断是：

1. 回归层已经从单一脚本向 manifest-driven matrix 迈出关键一步
2. 诊断层已经有了微基准入口，但覆盖还不够完整
3. 数据维度和结果治理已经明显改善，但还没有完全达到“高可信、可复现、可归因”的终态

benchmark 的角色应拆成两层：

1. 回归层
   - 保留 Rust / C++ 端到端对比
   - 作为版本间和后端间的稳定基线
2. 诊断层
   - 用更细粒度矩阵和微基准回答“为什么慢”
   - 为后续优化提供热点归因，而不是只给吞吐结论

## 3. 当前已落地能力

### 3.1 端到端入口

`crates/xlog/examples/bench_backend.rs` 当前已经具备这些能力：

1. 基础维度
   - `mode`
   - `threads`
   - `compress`
   - `compress-level`
   - `msg-size`
   - `flush-every`
   - `cache-days`
   - `max-file-size`
   - `pub-key`
2. 基线可信度增强
   - `--warmup`
   - `--time-buckets`
   - `--stage-profile`（Rust sync 子阶段采样）
   - `--json-pretty`
3. 输出指标增强
   - throughput
   - `lat_min / avg / stdev / p50 / p95 / p99 / p999 / max`
   - `output_bytes`
   - `bytes_per_msg`
   - `sync_stage_profile`（`total / format / block / engine_write`）
   - timeline bucket

### 3.2 微基准入口

`crates/xlog-core/examples/bench_components.rs` 已提供独立组件微基准入口，当前覆盖：

1. 压缩
   - zlib level 6 / 9
   - zstd stream / chunk
   - zlib streaming
2. 加密
   - TEA encrypt
   - ECDH key derive
3. formatter
   - `format_record`
4. 文件 I/O 路径
   - append keep-open / close-after-write
   - rotate（小文件尺寸触发）
   - cache write route
   - `flush_append_only` / `flush_sweep_only`
   - `move_old_cache_files`
   - `move_old_cache_files_only`
   - `flush_via_delete_expired`
   - `delete_expired_scan_only`
   - `delete_expired_files`
5. 资源指标
   - 组件基准输出 `cpu_user_ms / cpu_system_ms / max_rss_kb`
   - Linux 下补充 `/proc/self/io` 指标：`io_read_syscalls / io_write_syscalls / io_read_bytes / io_write_bytes`
   - I/O 子阶段事件指标：`scanned_entries / moved_files / deleted_files`

这意味着 benchmark 体系已经不再只有端到端吞吐对比，也开始具备初步归因能力。

### 3.3 manifest-driven matrix runner

`scripts/xlog/run_bench_matrix.sh` 与 `scripts/xlog/bench_matrix.tsv` 已经把端到端场景从固定脚本参数推进到 manifest-driven 运行。

当前综合 manifest 已覆盖 `31` 个场景，主要维度包括：

1. backend
   - Rust / C++
2. mode
   - async / sync
3. thread sweep
   - 1 / 4 / 8
4. message size
   - 96B / 128B / 4096B（并覆盖特定功能场景）
5. compress
   - zlib level 6
   - zlib level 9
   - zstd level 3
6. flush cadence
   - 0 / 64 / 256 / 1024
7. crypto
   - off / on
8. cache / rotate / boundary

当前 runner 已会保存：

1. `manifest.tsv`
2. `results_raw.jsonl`
3. `summary.md`
4. `summary.json`
5. `metadata.json`
6. `run.log`

当前 runner 已补上的可信度治理包括：

1. Rust / C++ backend 顺序支持 `fixed / alternating / randomized`，并可记录 seed 便于复现
2. `results_raw.jsonl` 记录 `scenario / backend / run_index / run_dir`
3. `metadata.json` 记录时间、主机、CPU、git commit、branch、build profile、manifest、backend policy

## 4. 当前仍然存在的缺口

虽然基础设施已经明显增强，但下面这些点还没有完全收口。

### 4.1 可信基线仍有缺口

1. backend 顺序策略虽然已支持 `randomized`，但默认策略与冷热隔离规约尚未标准化（例如 PR/CI 场景是否强制随机顺序）
2. `metadata.json` 已包含 CPU/内存/OS/rustc/cargo 等信息，但硬件负载与运行态信息仍不足
   - 后台负载快照
   - 温控/频率波动窗口
   - 磁盘可用空间与 I/O 压力
3. 结果目录已有 `manifest/raw/summary/metadata/log`，但跨运行对比模板和稳定命名约定还需要继续固化

这意味着结果已经比旧脚本可信得多，但环境偏差治理还没有完全收口。

### 4.2 数据分布仍偏合成

当前矩阵已经补了 size sweep、compress、crypto 和 flush 维度，且 payload profile 已经落地 `compressible / semi_structured / human_text / high_entropy` 四类。

但“数据分布治理完成”仍然不能算达标，原因是这些 profile 目前仍以规则化生成为主，尚未引入真实业务分布回放。

仍待补齐的核心数据形态：

1. `compressible`
2. `semi_structured`
3. `human_text`
4. `high_entropy`

仍待系统化的分布维度：

1. level / tag / file / func 分布
2. ASCII / Unicode / multiline / long tag / long path
3. steady-state / burst / wave / mixed-thread skew

### 4.3 诊断层覆盖还不完整

当前微基准入口已经有了，但还不够回答全部热点来源。

仍待增加的重点：

1. formatter / record-build 更细粒度拆分
2. compress / crypto 组合矩阵系统化
3. 文件 I/O 第三阶段已落地，下一步是把子阶段指标接入回归阈值与趋势看板

### 4.4 矩阵治理还没有完全成型

矩阵已经完成第一步分层，当前已提供：

1. `baseline_matrix`
2. `stress_matrix`
3. `feature_matrix`

当前剩余问题是运行频率、规模上限和 CI 接入策略还需要继续固化，否则分层会停留在“文件拆分”而不是“治理闭环”。

## 5. 后续推进顺序

benchmark 后续按以下顺序推进。

### 阶段一：把端到端 benchmark 固化成可信基线

优先处理：

1. 在现有交替执行基础上评估是否需要随机化和额外 warmup 隔离
2. 固化 warmup / runs / messages 的统一口径
3. 继续扩展 `metadata` 字段
4. 明确结果目录命名与归档约定

目标不是继续加更多 case，而是先让现有结果更可信、更可复现。

### 阶段二：扩展 payload profile

优先处理：

1. 引入 `compressible / semi_structured / human_text / high_entropy`
2. 把 size sweep 和 payload profile 解耦
3. 补文字形态与时序形态维度

目标是让 benchmark 不再只围绕规则化合成日志。

### 阶段三：把矩阵分层，而不是继续无约束膨胀

优先处理：

1. 拆分 `baseline / stress / feature`
2. 给每组矩阵定义固定用途、运行频率和规模边界
3. 让 CI / 手动评估 / 阶段回归使用不同矩阵

### 阶段四：补足可归因微基准

优先处理：

1. 继续细化 file manager / appender engine 子路径（含 `delete_expired_files`）
2. 将 flush / rotate / cache 子阶段指标接入回归阈值（不仅展示，还要可阻断）
3. 组件级资源指标持续扩展（CPU/IO/内存）并补跨平台口径说明

目标是把“为什么慢”从推测变成直接观测。

### 阶段五：补治理与文档

优先处理：

1. benchmark 目录结构约定
2. 结果对比模板
3. 回归阈值与验收口径
4. baseline / stress / feature 的运行说明

## 6. 当前建议

当前 benchmark 工作不应再混进迁移评审主文档里，而应按下面方式使用：

1. `docs/benchmark_strategy.md`
   - 管 benchmark 体系设计、当前状态和扩展计划
2. `scripts/xlog/bench_matrix.tsv`
   - 管当前实际端到端矩阵清单
3. `scripts/xlog/run_bench_matrix.sh`
   - 管当前统一 runner、backend 交替执行、metadata / summary 产物
4. `crates/xlog/examples/bench_backend.rs`
   - 管端到端 benchmark 入口
5. `crates/xlog-core/examples/bench_components.rs`
   - 管诊断层微基准入口
6. `scripts/xlog/analyze_bench.py`
   - 管单次 benchmark 结果聚合与统计摘要
7. `scripts/xlog/check_bench_regression.py`
   - 管双运行结果对比与阈值回归判定
8. `scripts/xlog/bench_regression_thresholds.json`
   - 管 baseline/stress/feature 分层阈值

## 6.1 标准运行流程（建议）

1. 跑矩阵（示例：全量双端）
   - `scripts/xlog/run_bench_matrix.sh --manifest scripts/xlog/bench_matrix.tsv --out-root <current_root> --backends rust,cpp --runs 1 --components`
2. 跑单次分析
   - `python3 scripts/xlog/analyze_bench.py --root <current_root>`
3. 跑回归判定（对比上一次基线）
   - `python3 scripts/xlog/check_bench_regression.py --baseline-root <baseline_root> --current-root <current_root> --backend rust`
4. 更新文档基线摘要（只保留核心统计与结论，不提交原始产物）

说明：

1. `check_bench_regression.py` 默认按层使用阈值：`baseline/stress/feature`，配置在 `bench_regression_thresholds.json`
2. 如需在 CI 中阻断回归，保持默认行为即可（有回归时退出码 `2`）
3. 如需先观察不阻断，可加 `--allow-regressions`

## 7. 退出条件

benchmark 体系达到“可信基线 + 基本可归因”至少需要满足：

1. 端到端 runner 具备 manifest、metadata、raw、summary 的稳定输出
2. backend 顺序偏差得到治理，至少不能固定为单向顺序
3. payload profile 不再只有规则化合成文本
4. baseline / stress / feature 三类矩阵边界清晰
5. 关键热点已有对应微基准，而不是只靠端到端吞吐猜测

## 8. 当前完成度快照（2026-03-06）

已完成：

1. 端到端 runner 的 manifest/raw/summary/metadata 稳定输出
2. backend 顺序策略（`fixed/alternating/randomized`）与 seed 记录
3. payload profile 四分类落地
4. baseline/stress/feature 矩阵拆分
5. 单次分析脚本与跨运行回归判定脚本落地

仍未完成（下一阶段重点）：

1. formatter / record-build 更细粒度拆分与回归门槛定义
2. compress / crypto 组合矩阵系统化（含常用组合的分层阈值）
3. 真实业务分布回放数据集接入（当前仍以合成 profile 为主）
4. CI 周期化策略固化（各矩阵频率、规模上限、阻断阈值）
