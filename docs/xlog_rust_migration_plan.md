# Xlog Rust 当前状态与下一步计划

## 1. 文档目的

本文只记录当前仍然有效的 Rust 迁移结论、语义约束和后续计划。

历史阶段、旧版性能假设和已经被代码推翻的中间结论，不再作为当前规划基线。

配套文档分工：

1. 更细的代码级审查与 perf 提交风险说明，统一见 `docs/rust_core_performance_review.md`
2. 语义红线与阻断项清单，统一见 `docs/rust_semantic_redlines.md`
3. benchmark 体系与矩阵扩展计划，统一见 `docs/benchmark_strategy.md`

## 2. 当前项目状态

截至当前代码：

1. Rust 运行时迁移已经完成，`mars-xlog` 默认走 Rust backend。
2. `xlog-core` 已覆盖协议、压缩、加密、mmap、文件管理、appender engine、dump 与 registry。
3. JNI / UniFFI / Harmony NAPI 能力面已对齐当前 Rust API。
4. `mars-xlog-sys` 与 C++ backend 仍保留，用作 benchmark / parity 基线。
5. 当前主任务仍然是“性能对齐 + 语义边界回收”，不是继续补迁移功能。

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

当前兼容性验收仍以“官方解码结果一致”和“行为一致”为主，不要求压缩流逐字节一致。

## 4. 当前 benchmark 基线

benchmark 已改为单独维护，不再在迁移计划里承载完整矩阵细节。

当前只保留迁移主线需要的结论：

1. Rust sync 与 async 都不再是系统性落后
2. 当前主要性能热点仍是 `sync_4t` 多线程竞争和 `async_1t` fixed cost / p99
3. benchmark 结果只能指导性能优先级，不能改变“语义级阻断项必须为 0”的硬约束
4. 具体基线、runner、矩阵与扩展顺序见 `docs/benchmark_strategy.md`

## 5. 当前明确存在的风险

当前计划必须把 `docs/rust_semantic_redlines.md` 中列出的阻断项视为第一优先级。

当前已确认的风险类型包括：

1. Rust 相对 C++ 的阻断项
   - recovery / oneshot 零拷贝分段写存在 recovered block framing 风险
   - active file / cached length / rollback 更强依赖文件独占假设
2. C++ / Rust 共享但必须诚实描述的语义边界
   - 当前 sync / fatal 不能按“每条日志立即落文件”理解
   - 清空 mmap / 删除 cache 前未建立 durability barrier

这些问题不处理，语义级阻断项就不能回到 `0`，当前分支也不能被描述成“只剩性能 gap”。

## 6. 下一步主线

### 6.1 P0: 先收敛语义边界

目标：

- 让文档要求、测试要求和当前实现重新一致
- 让语义级阻断项回到 `0`

必须先回答：

1. sync 模式是否允许用户态缓冲
2. fatal 是否必须等价于即时写入 / 可见
3. recovery / oneshot 是否允许把 recovered block 和 `MAGIC_END` 分段写入
4. cache/log 搬运与 mmap clear 前，是否要求目标端已 durable

验收方式：

1. 补齐针对上面 4 点的回归测试
2. 更新 `docs/rust_migration_review.md`
3. 更新 `docs/rust_core_performance_review.md`

### 6.2 P0: 收敛 `sync_4t` 多线程竞争

目标：

- 让 Rust 在 `sync_4t` plain steady-state 下稳定达到并超过当前 C++ 基线

当前判断：

1. 固定成本已经基本清掉
2. 剩余主差距仍主要在 `FileManager.runtime`
3. 但不能再以牺牲 sync 语义为代价换吞吐

下一步优先项：

1. 继续拆 `FileManager.runtime` 的职责边界
2. 让 active file 命中后的热路径只持有最小必要锁
3. 把 path / target bookkeeping 与真实写入进一步解耦
4. 但不继续放大未经声明的单写者独占假设

主要代码位置：

- `crates/xlog-core/src/file_manager.rs`
- `crates/xlog-core/src/appender_engine.rs`

### 6.3 P0: 收敛 `async_1t` fixed cost

目标：

- 提升 `async_1t` throughput
- 降低单线程 `p99`

当前判断：

1. `compress_level` 已经接入，不再是待办项
2. 当前更值得优先看的是真实 fixed cost 构成
3. 压缩输出链路的双缓冲仍是高价值候选点

下一步优先项：

1. `compress.rs` 流式压缩双缓冲收敛
2. `backend/rust.rs` async 提交链路 profiling
3. formatter / 时间格式化 / 冗余时钟调用 profiling
4. flush 控制面的 p99 归因

主要代码位置：

- `crates/xlog/src/backend/rust.rs`
- `crates/xlog-core/src/compress.rs`
- `crates/xlog-core/src/appender_engine.rs`

### 6.4 P1: 冷路径与次级优化

这些方向可以做，但不应盖过 6.1 / 6.2 / 6.3：

1. `append_file_to_file` buffer 放大
2. `FileManager` 目录扫描与路径处理的小分配清理
3. formatter 细节微优化

## 7. 当前不进入主线的方向

以下方向暂不进入当前主线：

1. per-thread async pending pipeline
2. per-thread sync file handle / append-only 重构
3. `madvise` / `msync(MS_ASYNC)` / OS 指令级 mmap 调优
4. 大规模 lock-free / atomic 重构
5. SIMD / TEA 指令级特化

原因很明确：这些方向要么不是当前 benchmark 主瓶颈，要么会直接触碰协议 / 恢复 / flush 语义，验证成本明显更高。

## 8. 验收与退出条件

### 8.1 当前回归面

每一轮性能优化之后至少执行：

1. `cargo test -p mars-xlog-core --test async_engine`
2. `cargo test -p mars-xlog-core file_manager:: -- --nocapture`
3. `cargo test -p mars-xlog --lib`
4. 必要时补跑：
   - `mmap_recovery`
   - `oneshot_flush`
   - bindings `cargo check`

### 8.2 benchmark 回归面

每一轮局部优化后先跑定向场景，再决定是否进入 full matrix。完整矩阵、runner 与结果治理规则见 `docs/benchmark_strategy.md`。

当前最低 benchmark 回归面：

1. sync 优化至少重跑：
   - `sync_1t`
   - `sync_4t`
   - `sync_4t_rotate_only`
2. async 优化至少重跑：
   - `async_1t`
   - `async_4t`
   - `async_4t_flush256`
3. 里程碑优化必须重跑：
   - `20260306-harness-matrix-rerun` 对应的全量矩阵

### 8.3 进入下一阶段的前提

只有在以下条件同时满足后，才进入移除 C++ 依赖阶段：

1. Rust 在目标平台稳定满足性能门槛
2. Rust / C++ 双后端 benchmark 连续通过
3. 协议兼容、恢复兼容、绑定兼容全部稳定
4. 语义级阻断项回到 `0`

当前结论：

- 还不能进入移除 C++ 依赖阶段
- 当前唯一主任务是“收回语义边界 + 继续做实现层性能对齐”
