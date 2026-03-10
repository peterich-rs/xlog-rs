# mars-xlog-core 深度性能与正确性审查

> 审查日期: 2026-03-08
> 审查范围: `crates/xlog-core/` + `crates/xlog/src/backend/rust.rs`
> benchmark 基线: `artifacts/bench-compare/20260308-p0-full-matrix`

## 0. 2026-03-10 本机双端复测结论（新增）

本次新增一轮 **同机双端全量 benchmark**，并为 Rust 侧补充了 Prometheus metrics 快照：

- 双端基线: `artifacts/bench-compare/20260310-core-matrix-dual`
- Rust metrics 快照: `artifacts/bench-compare/20260310-core-metrics-dual`

### 0.1 全量双端对比（本机）

全量 31 个场景统计：

1. Rust 吞吐更高：`28 / 31`
2. Rust P99 更低：`24 / 31`

极值：

1. 最差吞吐比：`async_4t_large_entropy` = `0.699x`（Rust 低于 C++）
2. 最差 P99 比：`async_1t_zlib6` = `1.682x`（Rust 高于 C++）
3. 最好吞吐比：`sync_4t_boundary` = `17.81x`
4. 最好 P99 比：`sync_8t_boundary` = `0.015x`

Rust 吞吐落后或持平的场景：

1. `async_4t_large_entropy` = `0.699x`
2. `async_4t_zstd3` = `0.801x`
3. `async_1t_entropy` = `0.999x`

Rust P99 反超失败的场景（P99 比值 > 1）：

1. `async_1t_zlib6` = `1.682x`
2. `async_4t_zstd3` = `1.615x`
3. `async_1t` = `1.537x`
4. `async_1t_crypto` = `1.500x`
5. `async_1t_zstd3` = `1.500x`
6. `async_1t_human` = `1.497x`
7. `async_1t_entropy` = `1.219x`

### 0.2 Rust metrics 关键卡点（本机）

Rust metrics 只在 Rust 侧采集（C++ 未接入 metrics）：

1. **Async 高线程背压明显**  
   `async_8t_dense` / `async_8t_flush64`  
   - `queue_full_total` ≈ `437,814` / `426,590`  
   - `queue_block_avg_ns` ≈ `49,869 ns` / `50,932 ns`  
   - 说明：瓶颈在后台压缩/刷盘吞吐，前端格式化不是主成本。

2. **大包高熵 async 以后台为主瓶颈**  
   `async_4t_large_entropy`  
   - `stage_total_avg_ns` ≈ `69,164 ns`  
   - `queue_block_avg_ns` ≈ `272,851 ns`  
   - `flush_requeue_total` ≈ `4,086`  
   - 说明：压缩+刷盘主导，flush requeue 频繁。

3. **Sync 边界/缓存/轮转瓶颈不在 append**  
   `sync_8t_cache` / `sync_8t_boundary` / `sync_8t_rotate`  
   - `engine_write_block_avg_ns` ≈ `37k/36k/24k ns`  
   - `file_append_avg_ns` ≈ `100~140 ns`  
   - 说明：瓶颈在 write_block/管理逻辑而非文件 append。

### 0.3 复现方式（双端）

双端矩阵跑法：

```
scripts/xlog/run_bench_matrix.sh \
  --manifest scripts/xlog/bench_matrix.tsv \
  --out-root artifacts/bench-compare/20260310-core-matrix-dual \
  --backends rust,cpp
```

Rust metrics 快照跑法（每个场景单次快照）：

```
cargo run --release -p mars-xlog --example bench_backend \
  --no-default-features --features rust-backend,metrics-prometheus -- \
  --out-dir <dir> --metrics-out <path>.prom ... (按 manifest 逐条执行)
```

## 1. 当前结论

当前 Rust 实现已经不适合再被描述为“性能接近目标，但仍主要落后于 C++”。

这轮 review 的结论是：

1. 最新双端全量矩阵里，Rust 在 `31 / 31` 场景吞吐更好，在 `31 / 31` 场景平均延迟更好。
2. sync 性能已经明显领先 C++，继续把 `sync_4t` 作为主性能热点不再成立。
3. async 也已整体领先，剩余有价值的差距主要收敛到 `async_4t_zstd3` tail latency、async 小消息 `bytes/msg` 偏大，以及刚落地的 block/flush 级 observability 还需要被进一步消费。
4. 正确性层面，旧文档里的 recovery / oneshot split-write framing 风险已经关闭；当前仍未关闭的 active blocker 是 `FileManager` 的文件所有权与 rollback 假设。

一句话总结：

- 当前 `core` crate 已经不是“性能不足的迁移中版本”。
- 更准确的状态是“核心性能已具备竞争力，async 结构化归因第一版已经落地，但仍需要基于这些计数做定向优化，并收口剩余语义红线”。

## 2. Benchmark 事实基线

当前主基线应看 `20260308-p0-full-matrix`。

代表性场景：

| scenario | rust / cpp throughput | 结论 |
| :--- | ---: | :--- |
| `sync_1t` | `3.359x` | Rust 明显领先 |
| `sync_4t` | `4.780x` | Rust 明显领先，已不是主差距 |
| `sync_8t_dense` | `4.185x` | Rust 明显领先 |
| `async_4t` | `2.040x` | Rust 明显领先 |
| `async_4t_flush256` | `1.876x` | Rust 明显领先，但体积偏大 |
| `async_4t_zstd3` | `1.013x` | 吞吐基本持平略优，但 tail 仍落后 |
| `async_4t_large_entropy` | `1.068x` | 略快于 C++，且 tail 明显更好 |

全局统计：

1. overall throughput ratio gmean: `2.414`
2. overall p99 ratio gmean: `0.263`
3. overall p999 ratio gmean: `0.364`
4. async throughput ratio gmean: `1.412`
5. sync throughput ratio gmean: `4.278`

Criterion 侧当前基线 `artifacts/criterion/20260308-p0-full-review` 也给出一致信号：

1. `core_formatter` 很稳
2. `core_crypto` 很稳
3. async public write path 仍显著重于 sync
4. `core_compress_decode/zstd_*` 仍高噪声

## 3. 当前热路径判断

### 3.1 Sync

当前 sync 热路径已经不再是主性能问题。

最新数据说明：

1. sync steady-state 竞争已经大幅收缩
2. `FileManager.runtime` 仍然是 sync 侧最值得关注的串行区，但它不再构成对 C++ 的主要性能 gap
3. sync 后续工作的重点应从“继续追吞吐”切换到“确保语义假设与文档一致”

因此：

1. sync 性能优化优先级应下调
2. sync 正确性与文件所有权语义应上调

### 3.2 Async

最新定向 stage profile 的结论更重要：

1. `checkout_async_state()` 已不再是主成本来源
2. `append` 阶段和 frontend queue backpressure 才是当前 async 主约束
3. `async_4t_zstd3` 的问题集中在 tail，而不是平均吞吐
4. `async_4t` zlib 的 `queue_full_count = 191196`、`block_send_ratio = 0.387`
5. `async_4t_zstd3` 的 `queue_full_count = 33836`、`block_send_ratio = 0.280`
6. 两个场景的 `flush_requeue_count = 0`，说明当前更该看 pending block 聚合与 queue 策略，而不是 engine flush worker

这意味着当前 async 需要回答的不是“是否还有一个大 mutex”，而是：

1. pending block 是如何被切分和 finalize 的
2. 为什么有些场景 `bytes/msg` 明显偏大
3. flush requeue 与 queue backpressure 如何放大 tail

## 4. 当前 active correctness risk

### S0-1: FileManager 本地长度缓存与 rollback 假设独占文件所有权

相关位置：

1. `crates/xlog-core/src/file_manager.rs`

当前风险来自：

1. `ActiveAppendFile.logical_len / disk_len`
2. `AppendTargetCache.local_len / merged_len`
3. 写失败后的 `rollback_file_to_len()`

如果目标 `.xlog` 文件存在外部 writer：

1. 本地缓存长度可能落后于真实长度
2. 本进程一旦写失败并 rollback，可能截掉外部 writer 已写入的数据

这项风险当前仍然是真正的 active blocker。

## 5. 已关闭但必须防回归的问题

### C0-closed: recovery / oneshot split-write framing 风险

旧文档中这项问题被列为 active blocker，但当前代码已经修复：

1. `crates/xlog-core/src/appender_engine.rs` 会把 recovered block 和 `MAGIC_END` 拼成单个连续 block 再写出
2. `crates/xlog-core/src/oneshot.rs` 也采用同样策略

因此：

1. 这项问题不应再继续作为当前阻断项
2. 但必须保留 recovery / oneshot 回归测试，防止未来再退回多段写出

## 6. 当前真正值得做的性能工作

### 6.1 P0: async 结构化归因第一版已完成

当前 `metrics` 采集已覆盖：

1. pending block finalization 次数
2. lines per block
3. finalize reason 分布
4. raw input / payload bytes per block
5. frontend `block_send` ratio
6. engine flush requeue 次数

因此当前最缺的已经不再是“更多计数”，而是基于这组计数去解释 async `bytes/msg` 与 tail。

### 6.2 P1: 只围绕 async 真正剩余的差距做实验

优先级应明确收敛到：

1. `compress.rs` 流式压缩 flush 粒度
2. frontend queue drain batch 与 backpressure 策略
3. `async_4t_zstd3` tail latency
4. `async_4t_large_entropy` 的 throughput / bytes tradeoff

### 6.3 不应再作为主线的方向

这些方向当前不应继续占据主线：

1. 以 `sync_4t` 为第一性能目标
2. 把 `async_1t` fixed cost 当成当前唯一 async 主矛盾
3. 为了追局部吞吐继续扩大未经声明的文件独占假设

### 6.4 当前未发现面向单机型 benchmark 的特调代码

本轮额外回看了运行时代码里的性能路径，重点检查：

1. 是否存在按 `Apple / M2 / arm64 / x86_64 / target_arch` 做的 runtime 分支
2. 是否把 benchmark 场景名或实验 runner 假设固化到主逻辑
3. 是否存在只为当前机型结果服务的特调常量

当前没有发现这类运行时特调代码。

需要区分的点是：

1. 现有 `queue capacity / batch / retry` 常量属于通用策略常量，不是机型识别分支
2. 这些常量未来仍然只能通过跨设备数据来调整，不能因为单机结果更好就直接进入主线
3. 一条把 `sync_4t` 直接写进运行时代码注释的历史表述已经清理，避免 benchmark 语义继续污染产品代码

## 7. 当前是否符合高性能最佳实践

结论不能简单写成“完全是”或“完全不是”。

### 已符合或基本符合的部分

1. 热路径 scratch buffer 复用是正确方向
2. sync 路径已经把主要性能矛盾压下去
3. tid thread-local 缓存、标准 Criterion、CI baseline 与低扰动 stage profiler 都是正确工程化方向
4. benchmark 体系已经从“临时打点”升级到可复跑、可回归、可归因的基础框架

### 仍不算收口的部分

1. `FileManager` 文件所有权与 rollback 语义仍未收口
2. async `bytes/msg` 与 tail 的成因还没有足够结构化数据解释
3. `zstd` decode 类基准仍然高噪声，不能过度解读

因此更准确的说法是：

- 当前实现已经具备了高性能工程化骨架。
- 但还不能称为“语义边界完全稳固、可直接去掉 C++ 对照的最终版”。

## 8. 主线优先级

### P0: 补 observability + 收口 active blocker

1. 基于新的 async pending block / finalize / flush requeue 计数做定向实验
2. 明确 `.xlog` 是否允许多 writer
3. 让 `FileManager` 的文档假设、实现行为、测试结论一致

### P1: 基于新计数做 async 定向优化

1. 压缩 flush 粒度实验
2. frontend batch / retry / backpressure 调参
3. `async_4t_zstd3` tail 收敛

### P2: 扩展真实业务分布与矩阵可信度

1. 真实 workload 数据集
2. matrix 多次运行基线
3. 高噪声 Criterion case 的阈值治理

## 9. 当前测试要求

每轮主线改动后至少执行：

1. `cargo test -p mars-xlog-core --test async_engine`
2. `cargo test -p mars-xlog-core --test mmap_recovery`
3. `cargo test -p mars-xlog-core --test oneshot_flush`
4. `cargo test -p mars-xlog-core file_manager:: -- --nocapture`
5. `cargo test -p mars-xlog --lib`
6. 与改动相关的定向 benchmark / Criterion / full matrix

## 10. 当前结论总结

当前可以明确：

1. Rust 现在已经具备明确的性能优势，而不是“还在追平 C++”。
2. 剩余真正重要的问题已经从 sync 吞吐转移到 async 归因、局部 tail 和语义收口。
3. 当前 review 的重点不应再是“继续证明 Rust 能跑得更快”，而应是“先把剩余风险解释清楚并关掉 active blocker”。
