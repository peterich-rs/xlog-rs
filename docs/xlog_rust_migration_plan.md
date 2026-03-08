# Xlog Rust 当前状态与下一步计划

## 1. 文档目的

本文只记录当前仍然有效的 Rust 迁移结论、语义约束和后续计划。

历史阶段、旧版性能假设和已经被最新代码与 benchmark 推翻的中间结论，不再作为当前规划基线。

配套文档分工：

1. 代码级性能与正确性审查见 `docs/rust_core_performance_review.md`
2. 语义红线与阻断项清单见 `docs/rust_semantic_redlines.md`
3. benchmark 体系、基线与扩展计划见 `docs/benchmark_strategy.md`

## 2. 当前项目状态

截至当前代码：

1. Rust 运行时迁移已经完成，`mars-xlog` 默认走 Rust backend。
2. `xlog-core` 已覆盖协议、压缩、加密、mmap、文件管理、appender engine、dump 与 registry。
3. JNI / UniFFI / Harmony NAPI 能力面已对齐当前 Rust API。
4. `mars-xlog-sys` 与 C++ backend 仍保留，用作 benchmark / parity 基线。
5. 当前主任务是“收口剩余语义红线 + 解释并优化 async 剩余差距”，不是继续补迁移功能。

当前源码分层：

```text
bindings (JNI / UniFFI / NAPI)
    ↓
crates/xlog
    ↓
crates/xlog-core
    ↓
crates/xlog-sys + third_party/mars (仅用于 C++ backend 对照)
```

## 3. 后续优化必须满足的硬约束

后续所有优化都必须满足以下约束：

1. `语义级阻断项必须始终为 0`
2. 不修改日志协议：
   - header / tailer 结构
   - magic 取值
   - sync / async seq 语义
   - `ECDH(secp256k1) + TEA` 加密语义
3. 不弱化恢复语义：
   - mmap 文件名与容量
   - startup recover / oneshot flush 行为
   - torn tail / pending block 修复策略
4. 不在文档未声明的前提下修改 sync 语义：
   - sync 是否允许用户态缓冲
   - fatal 是否必须等价于即时写入 / 可见
5. 不在未建立 durability 保证前提前销毁恢复源：
   - 清空 mmap
   - 删除 cache file
6. 不修改对外 API 和绑定语义：
   - `Xlog` / default appender / named instance 行为
   - raw metadata / traceLog / global write path
7. 不引入面向单机型、单 runner 的性能特调：
   - 不按 `Apple / M2 / arm64 / x86_64` 做 runtime 分支
   - 不把某一次 benchmark 的局部最优常量直接写死进默认实现
   - 进入主线的性能调整必须在多设备上方向一致

## 4. 当前 benchmark 基线

迁移计划不再承载完整 benchmark 细节，只保留主线规划需要的结论。

最新全量双端矩阵：`artifacts/bench-compare/20260308-p0-full-matrix`

当前只保留以下结论：

1. Rust 在全量矩阵里 `31 / 31` 场景吞吐优于 C++
2. sync 已不再是主性能热点
3. async 仍有局部 tail 与 `bytes/msg` 效率问题，但不是系统性落后
4. `async_4t_zstd3` 仍是唯一需要单独盯的 tail 场景
5. benchmark 结果只能指导性能优先级，不能改变“语义级阻断项必须为 0”的硬约束
6. 当前已回看运行时代码，未发现按机型或 benchmark 场景做的性能分支

具体基线、runner、矩阵与扩展顺序见 `docs/benchmark_strategy.md`。

## 5. 当前明确存在的风险

当前计划必须把 `docs/rust_semantic_redlines.md` 中列出的 active blocker 视为第一优先级。

当前已确认的风险类型包括：

1. Rust 侧 active blocker
   - `FileManager` 的本地长度缓存与 rollback 更强依赖文件独占假设
2. C++ / Rust 共享但必须诚实描述的语义边界
   - 当前 sync / fatal 不能按“每条日志立即落文件”理解
   - 清空 mmap / 删除 cache 前未建立 durability barrier
3. 性能层面的未解释差距
   - `async_4t_zstd3` tail latency
   - async 小消息 zlib 场景 `bytes/msg` 偏大
   - async pending block / flush 行为虽然已有结构化计数，但还需要基于这些计数完成定向解释与优化

需要明确：

1. recovery / oneshot split-write framing 风险已不再是 active blocker
2. 这项问题当前应转为防回归测试要求，而不是继续写进主线阻断判断

## 6. 下一步主线

### 6.1 P0: 收口 active semantic blocker

目标：

- 让 `.xlog` 的文件所有权语义、`FileManager` 行为和测试约束重新一致

必须先回答：

1. `.xlog` 是否允许多 writer 竞争写入
2. 如果允许，如何修复本地长度缓存与 rollback
3. 如果不允许，如何把独占假设写进文档、测试和接入约束

主要代码位置：

- `crates/xlog-core/src/file_manager.rs`

### 6.2 P0: async 结构化 observability 已落地

当前已经完成：

1. `pending_blocks` 级统计输出
2. `threshold / explicit_flush / flush_every / timeout / stop` finalize reason 计数
3. `block_send_ratio`
4. `flush_requeue_count`

当前代码位置：

1. `crates/xlog/src/backend/rust.rs`
2. `crates/xlog/src/backend/stage_profile.rs`
3. `crates/xlog-core/src/appender_engine.rs`
4. `crates/xlog/examples/bench_backend.rs`

这意味着下一步主线不再是“补 observability”，而是“消费 observability”。

### 6.3 P1: 围绕 async 剩余差距做定向实验

目标：

- 收敛 `async_4t_zstd3` tail
- 解释 async 小消息 zlib `bytes/msg` 偏大原因

优先项：

1. `compress.rs` 流式压缩 flush 粒度实验
2. frontend queue drain batch 与 backpressure 策略实验
3. flush requeue / timeout flush 归因
4. `async_4t_large_entropy` 的 throughput / bytes tradeoff 解释

主要代码位置：

- `crates/xlog-core/src/compress.rs`
- `crates/xlog/src/backend/rust.rs`
- `crates/xlog-core/src/appender_engine.rs`

### 6.4 P2: 扩展矩阵可信度与真实 workload

优先项：

1. full matrix 多次运行基线固化
2. baseline / stress / feature 的职责边界进一步稳定
3. 真实业务分布回放数据集
4. 高噪声 Criterion case 的阈值治理

## 7. 当前不进入主线的方向

以下方向暂不进入当前主线：

1. 继续把 sync 吞吐当成第一性能目标
2. per-thread async pending pipeline
3. per-thread sync file handle / append-only 重构
4. `madvise` / `msync(MS_ASYNC)` / OS 指令级 mmap 调优
5. 大规模 lock-free / atomic 重构
6. SIMD / TEA 指令级特化
7. 面向单机型 benchmark 的 runtime 特调

原因很明确：这些方向要么已经不是当前 benchmark 主矛盾，要么会直接触碰协议 / 恢复 / flush 语义，验证成本明显更高。

## 8. 验收与退出条件

### 8.1 当前回归面

每一轮主线优化之后至少执行：

1. `cargo test -p mars-xlog-core --test async_engine`
2. `cargo test -p mars-xlog-core --test mmap_recovery`
3. `cargo test -p mars-xlog-core --test oneshot_flush`
4. `cargo test -p mars-xlog-core file_manager:: -- --nocapture`
5. `cargo test -p mars-xlog --lib`
6. 必要时补跑 bindings `cargo check`

### 8.2 benchmark 回归面

每一轮局部优化后先跑定向场景，再决定是否进入 full matrix。完整矩阵、runner 与结果治理规则见 `docs/benchmark_strategy.md`。

当前最低 benchmark 回归面：

1. async 结构优化至少重跑：
   - `async_4t`
   - `async_4t_zstd3`
   - `async_4t_large_entropy`
2. 压缩 / 写路径优化至少重跑：
   - `cargo bench -p mars-xlog-core --bench criterion_components`
   - `cargo bench -p mars-xlog --bench criterion_write_path`
3. 里程碑优化必须重跑：
   - `20260308-p0-full-matrix` 对应的全量双端矩阵

### 8.3 进入下一阶段的前提

只有在以下条件同时满足后，才进入移除 C++ 依赖阶段：

1. active semantic blockers 回到 `0`
2. Rust 在目标平台稳定满足性能门槛
3. Rust / C++ 双后端 benchmark 连续通过
4. 协议兼容、恢复兼容、绑定兼容全部稳定
5. matrix 与 Criterion 都已有稳定基线和回归门禁

当前结论：

- 还不能进入移除 C++ 依赖阶段
- 当前唯一正确的主线是“补 observability、收口 active blocker、再定向治理 async 剩余差距”
