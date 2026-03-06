# Benchmark 基线与扩展策略

## 1. 文档定位

本文是 benchmark 体系的独立入口，专门记录：

1. 当前 benchmark 基础设施已经具备的能力
2. 现阶段仍然存在的可信度与可归因缺口
3. 后续的扩展顺序与治理要求

迁移评审文档只保留项目级结论，不再重复承载完整 benchmark 设计细节。

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
   - `--json-pretty`
3. 输出指标增强
   - throughput
   - `lat_min / avg / stdev / p50 / p95 / p99 / p999 / max`
   - `output_bytes`
   - `bytes_per_msg`
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

这意味着 benchmark 体系已经不再只有端到端吞吐对比，也开始具备初步归因能力。

### 3.3 manifest-driven matrix runner

`scripts/xlog/run_bench_matrix.sh` 与 `scripts/xlog/bench_matrix.tsv` 已经把端到端场景从固定脚本参数推进到 manifest-driven 运行。

当前 manifest 已覆盖 `24` 个场景，主要维度包括：

1. backend
   - Rust / C++
2. mode
   - async / sync
3. thread sweep
   - 1 / 2 / 4 / 8
4. message size
   - 16B / 96B / 512B / 4096B
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
4. `run.log`

## 4. 当前仍然存在的缺口

虽然基础设施已经明显增强，但下面这些点还没有完全收口。

### 4.1 可信基线仍有缺口

1. backend 执行顺序仍按 manifest 和 `--backends` 顺序跑，未做随机化或交替
2. 当前还没有单独的环境元信息文件
   - 主机
   - CPU 核数
   - git commit
   - build profile
   - 运行时间
3. 结果目录虽然已有 `manifest/raw/summary/log`，但还缺少稳定的 `metadata` 约定

这意味着结果已经比旧脚本可复现得多，但环境偏差治理还不够强。

### 4.2 数据分布仍偏合成

当前矩阵已经补了 size sweep、compress、crypto 和 flush 维度，但 payload profile 还没有正式拆成独立数据模型。

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
3. file manager / appender engine / flush / rotate / cache route 微基准

### 4.4 矩阵治理还没有完全成型

当前 `bench_matrix.tsv` 还是一个综合矩阵，尚未正式拆成：

1. `baseline_matrix`
2. `stress_matrix`
3. `feature_matrix`

这会导致同一个 manifest 同时承担日常回归、阶段压测和功能专项验证三类职责，规模与频率都不够清晰。

## 5. 后续推进顺序

benchmark 后续按以下顺序推进。

### 阶段一：把端到端 benchmark 固化成可信基线

优先处理：

1. backend 顺序随机化或交替执行
2. 固化 warmup / runs / messages 的统一口径
3. 为每次运行补 `metadata` 文件
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

1. file manager / appender engine 微基准
2. flush / rotate / cache route 微基准
3. 组件级资源指标输出

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
   - 管当前统一 runner
4. `crates/xlog/examples/bench_backend.rs`
   - 管端到端 benchmark 入口
5. `crates/xlog-core/examples/bench_components.rs`
   - 管诊断层微基准入口

## 7. 退出条件

benchmark 体系达到“可信基线 + 基本可归因”至少需要满足：

1. 端到端 runner 具备 manifest、metadata、raw、summary 的稳定输出
2. backend 顺序偏差得到治理
3. payload profile 不再只有规则化合成文本
4. baseline / stress / feature 三类矩阵边界清晰
5. 关键热点已有对应微基准，而不是只靠端到端吞吐猜测
