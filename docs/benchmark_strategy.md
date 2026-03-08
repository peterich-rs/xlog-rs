# Benchmark 基线与扩展策略

## 1. 文档定位

本文只记录当前仍然有效的 benchmark 基础设施、最新基线结论、仍未收口的可信度缺口，以及后续治理顺序。

迁移评审文档不再重复承载完整 benchmark 设计细节；项目级结论统一在：

1. `docs/rust_migration_review.md`
2. `docs/rust_core_performance_review.md`
3. `docs/xlog_rust_migration_plan.md`

## 2. 当前 benchmark 体系

当前体系已经明确分成两层：

1. 标准 Rust 微基准
   - `cargo bench -p mars-xlog-core --bench criterion_components`
   - `cargo bench -p mars-xlog --bench criterion_write_path`
2. 端到端矩阵与跨后端对比
   - `scripts/xlog/run_bench_matrix.sh --manifest scripts/xlog/bench_matrix.tsv --out-root <dir> --backends rust,cpp --runs 1 --components`
3. 需要更细归因时，再打开 feature-gated profile
   - `cargo run --release -p mars-xlog --example bench_backend --no-default-features --features rust-backend,bench-internals -- --out-dir <dir> --stage-profile ...`

当前能力面：

1. `bench_backend.rs`
   - 支持 `mode / threads / compress / compress-level / msg-size / flush-every / cache-days / max-file-size / pub-key`
   - 支持 `payload_profile`（`compressible / semi_structured / human_text / high_entropy`）与 `payload_seed`
   - 输出 `lat_min / avg / stdev / p50 / p95 / p99 / p999 / max / output_bytes / bytes_per_msg`
   - `bench-internals` 下可输出 Rust sync/async stage profile
2. `run_bench_matrix.sh`
   - manifest-driven 场景管理
   - backend 顺序支持 `fixed / alternating / randomized`
   - 输出 `manifest.tsv / results_raw.jsonl / summary.md / summary.json / metadata.json / run.log`
3. `criterion`
   - `criterion_components.rs`：formatter / compress encode / compress decode / crypto
   - `criterion_write_path.rs`：公共 Rust API 的 `flush-per-msg` 与 `batch256+flush` 两类写路径语义
4. CI 与回归治理
   - committed baseline：`benchmarks/criterion/macos14-arm64`
   - baseline 刷新脚本：`scripts/xlog/update_ci_criterion_baseline.sh --from-root <artifact_root>`
   - matrix / criterion 共用 `check_bench_regression.py`

仓库入库边界：

1. `artifacts/` 全树都是本地运行产物，必须忽略，不入 git
2. 需要长期维护的 benchmark 内容，只允许以 `benchmarks/` 下的摘要数据和文档形式入库
3. committed benchmark data 不应携带本机绝对路径、raw jsonl、run.log、`.xlog`、`.mmap3` 等运行时痕迹

## 3. 最新基线

### 3.1 双端全量矩阵（2026-03-08）

数据来源：

1. 目录：`artifacts/bench-compare/20260308-p0-full-matrix`
2. 场景规模：`31 scenarios × 2 backends × 1 run`
3. 环境：`Apple M2 Max / macOS 15.7.3 / arm64`
4. worktree：`git_dirty = false`

这轮矩阵的核心统计：

1. 吞吐更优场景：Rust `31 / 31`
2. 平均延迟更优场景：Rust `31 / 31`
3. P99 更优场景：Rust `30 / 31`
4. P999 更优场景：Rust `26 / 31`
5. `strict_winners = 23`
6. 当前 bucket 统计里 `tail_risk = 0`，但 `async_4t_zstd3` 仍是唯一在吞吐近乎持平时 tail 明显落后于 C++ 的场景

按分组几何均值：

1. overall
   - throughput ratio gmean: `2.414`
   - p99 ratio gmean: `0.263`
   - p999 ratio gmean: `0.364`
2. async
   - throughput ratio gmean: `1.412`
   - p99 ratio gmean: `0.473`
   - p999 ratio gmean: `0.470`
3. sync
   - throughput ratio gmean: `4.278`
   - p99 ratio gmean: `0.141`
   - p999 ratio gmean: `0.278`

这轮矩阵给出的当前判断：

1. Rust 已经不是“多数场景接近 C++”，而是“全量矩阵里吞吐全部超过 C++”。
2. sync 已不再是主要性能矛盾；继续把 `sync_4t` 作为主性能热点已经和最新代码、最新数据不匹配。
3. async 大部分场景也已经明显领先，剩余真正需要盯的是局部 tail 和输出体积。
4. `async_4t_zstd3` 仍是唯一需要单独盯的 tail 场景：吞吐已基本持平并略优于 C++，但 `p99/p999` 仍落后。
5. async 小消息 zlib 场景的 `bytes/msg` 仍明显偏大，说明剩余问题更接近“压缩/聚合策略”和“控制面抖动”，不是基础写能力不足。

相对 2026-03-06 旧矩阵，当前更可靠的判断应只保留到分组几何均值：

1. overall throughput ratio gmean：`1.322 -> 2.414`
2. overall p99 ratio gmean：`1.461 -> 0.263`
3. overall p999 ratio gmean：`1.296 -> 0.364`
4. async throughput ratio gmean：`1.138 -> 1.412`
5. async p99 ratio gmean：`3.051 -> 0.473`
6. sync throughput ratio gmean：`1.551 -> 4.278`

这说明最新主矛盾已经不是“Rust 相对 C++ 吞吐不足”，而是 async 聚合/压缩行为变化后的体积效率与局部 tail。

### 3.2 Rust Criterion 基线（2026-03-08）

数据来源：

1. 本轮 review 运行：`artifacts/criterion/20260308-p0-full-review`
2. CI committed baseline：`benchmarks/criterion/macos14-arm64`

最新本地 review run 的结构性结论：

1. `core_formatter` 很稳定，group mean 约 `120ns`
2. `core_crypto` 很稳定，group mean 约 `566ns`
3. `public_write_*` 里 async 与 sync 的差距依旧明显，说明热点仍在 async 提交链路和 flush/压缩控制面
4. `public_write_flush_per_msg` group mean 约 `40.0us`
5. `public_write_batch256_flush` group mean 约 `454.3us`
6. `core_compress_decode/zstd_*` 仍然高噪声，不适合作为强硬单指标门禁

相对前一轮本地 review run 的 Criterion 对比报告显示 `11` 个 threshold violation，但主要由两类组成：

1. `stddev` 漂移
2. 少量 `zstd` encode/decode throughput 波动

因此当前 Criterion 的正确用法是：

1. formatter / crypto / sync write-path：适合稳定回归
2. zstd decode 及高噪声场景：更适合作为诊断信号，而不是一刀切的 hard gate

### 3.3 最新 stage profile 观察

本轮额外做了两次 Rust 定向 stage profile：

1. `async_4t_zstd3`
2. `async_4t` zlib

结论：

1. `checkout_async_state()` 已经不是当前主成本来源。
2. `async_4t` zlib：`queue_full_count = 191196`，`block_send_ratio = 0.387`，`append avg/p99 ≈ 3.53us / 25.6us`
3. `async_4t_zstd3`：`queue_full_count = 33836`，`block_send_ratio = 0.280`，`append avg/p99 ≈ 1.38us / 23.6us`
4. 两个场景的 `flush_requeue_count = 0`，pending block 最终都只在 `explicit_flush` 时 finalize，说明当前主矛盾不在 engine flush requeue，而在 frontend queue pressure 和 direct block send 对聚合行为的破坏。
5. 这组信号支持把后续优化收敛到 async queue/backpressure 与 pending block 聚合策略，而不是 formatter/crypto 微优化。

## 4. 当前仍未收口的 benchmark 缺口

### 4.1 全量矩阵仍是单次运行

当前全量双端矩阵默认仍是 `runs=1`。

这足够做方向判断，但不够单独承担正式发布级签字。仍待补齐：

1. baseline / stress / feature 的多次运行口径固化
2. 更明确的 runner 环境隔离规约
3. matrix 基线的固定命名与归档策略

### 4.2 数据分布仍以合成为主

当前 payload profile 已经有四类，但仍然是规则化生成，不是业务回放。

仍待补齐：

1. 真实业务分布回放数据集
2. Unicode / multiline / 长 tag / 长 path 等文本形态
3. steady-state / burst / wave / skew 等更真实的时序形态

### 4.3 async 归因已补到 block 级

当前 stage profile 已同时覆盖两层信息：

1. 延迟分布：`format / checkout / append / force_flush`
2. 结构化 block 统计：
   - pending block finalization 次数
   - lines per block
   - finalize 原因分布
   - raw input / payload bytes per block
   - frontend `block_send` ratio
   - engine flush requeue count

当前 `bench_backend --stage-profile` 的 JSON 已经可以直接解释：

1. block 是怎么被 finalize 的
2. 每块积累了多少行与多少字节
3. `bytes/msg` 偏大更像是 block 聚合问题、压缩问题，还是 queue/backpressure 问题

### 4.4 当前双端主差距已改写

以下旧判断已不再适合作为当前优化优先级：

1. `sync_4t` 是当前主性能热点
2. `async_1t` fixed cost 是当前唯一 async 主矛盾
3. Rust 仍有大量场景系统性落后于 C++

当前更准确的说法是：

1. sync 性能已经不再是主问题
2. async 剩余问题集中在局部 tail 和体积效率
3. 需要优先补 observability，而不是继续拍脑袋追吞吐

## 5. 后续推进顺序

### 5.1 P0：async 结构化归因计数已完成

当前已落地：

1. `bench_backend` JSON 输出 `pending_blocks`
2. `flush_every / explicit_flush / threshold / timeout / stop` finalize reason 计数
3. `block_send_ratio`
4. `flush_requeue_count`

因此当前下一步不再是“先补计数”，而是“基于这组计数做定向实验”。

### 5.2 P1：围绕 async 压缩/聚合策略做定向实验

优先处理：

1. `compress.rs` 流式压缩 flush 粒度
2. frontend queue drain batch 与 backpressure 策略
3. `async_4t_zstd3` 的 tail latency 收敛
4. `async_4t_large_entropy` 的 throughput / bytes tradeoff 解释清楚

### 5.3 代码 review 结论：当前未发现机型特调代码

本轮已回看运行时代码，重点检查：

1. CPU/机型识别分支
2. benchmark 场景名直接进入主逻辑
3. 面向单机型结果写死的 runtime tuning

当前结论：

1. 未发现按 `Apple / M2 / arm64 / x86_64 / target_arch` 做运行时性能分支的代码。
2. 当前 async queue capacity、batch、retry 等常量属于通用策略常量，不是按某个 benchmark 场景或某台机器动态切换。
3. 已清理掉一条把 `sync_4t` workload 直接写进运行时代码注释的历史表述，避免 benchmark 语义继续泄漏到产品代码。

因此后续优化必须遵守：

1. 不接受面向单机型、单 runner 的特调进入主线。
2. 进入默认实现的优化，至少要在 `macOS arm64`、`Linux x86_64` 和目标移动端设备上确认方向一致。
3. 单设备成立、跨设备不稳定的收益，只能保留在本地实验或 benchmark 说明里，不能写死进 runtime 策略。

### 5.4 P2：继续扩展真实分布与 CI 治理

优先处理：

1. baseline / stress / feature 的固定运行频率与规模边界
2. CI / 本地 / 里程碑矩阵的职责拆分
3. 真实业务数据集接入
4. 高噪声 Criterion case 的阈值与展示策略继续收敛

## 6. 建议工作流

1. 标准 Rust 微基准
   - `scripts/xlog/run_criterion_bench.sh --out-root artifacts/criterion/<run_name>`
2. 需要阶段归因时
   - `cargo run --release -p mars-xlog --example bench_backend --no-default-features --features rust-backend,bench-internals -- --out-dir <dir> --stage-profile ...`
3. 跑双端矩阵
   - `scripts/xlog/run_bench_matrix.sh --manifest scripts/xlog/bench_matrix.tsv --out-root <current_root> --backends rust,cpp --runs 1 --components`
4. 单次分析
   - `python3 scripts/xlog/analyze_bench.py --root <current_root>`
5. 回归判定
   - `python3 scripts/xlog/check_bench_regression.py --baseline-root <baseline_root> --current-root <current_root> --backend rust`
   - `python3 scripts/xlog/check_bench_regression.py --kind criterion --baseline-root <criterion_baseline_root> --current-root <criterion_current_root>`

## 7. 当前完成度快照（2026-03-08）

已完成：

1. manifest-driven 双端矩阵 runner
2. metadata / raw / summary / regression 产物治理
3. payload profile 四分类
4. baseline / stress / feature 矩阵拆分
5. 标准 Criterion 基准、CI baseline、阈值回归脚手架
6. feature-gated async/sync stage profiler
7. async pending block 级结构化归因计数

仍未完成：

1. 真实业务分布回放
2. matrix 多次运行基线固化
3. 高噪声 codec 场景的最终门禁策略

## 8. 退出条件

benchmark 体系达到“可信基线 + 基本可归因”至少需要同时满足：

1. 端到端 runner 输出稳定，且 baseline/stress/feature 三类矩阵边界清晰
2. 全量矩阵不再依赖单次运行才能做关键判断
3. 关键 async 热点已有 block/flush/compression 级归因，而不是只看吞吐与 p99
4. Criterion 与 matrix 的 CI 门禁边界清晰，不把高噪声场景和稳定场景混在一套规则里
5. benchmark 结论与当前代码、当前迁移文档不再互相矛盾
