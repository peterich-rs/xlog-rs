# Rust Migration Review

## 1. 当前结论

基于当前仓库代码和 benchmark 基线，Rust 迁移不能再简单描述为“语义完全收口，只剩性能对齐”。

更准确的判断是：

1. 协议、解码、压缩/加密、formatter、bindings 和大部分恢复路径已经完成迁移。
2. benchmark 层面，Rust 已经不是系统性落后，多个场景已经达到或超过 C++。
3. 但 benchmark 结果不会放宽项目红线，`语义级阻断项为 0` 仍是强制要求；当前 Rust 侧仍有高优先级语义 / 功能阻断项未收口。

配套文档分工：

1. 逐条代码级分析见 `docs/rust_core_performance_review.md`
2. 语义红线与阻断项清单见 `docs/rust_semantic_redlines.md`
3. benchmark 体系与扩展计划见 `docs/benchmark_strategy.md`

本文只保留当前已经收敛后的项目级结论。

## 2. 已确认稳定的对齐面

以下内容当前仍可以视为已完成：

1. 协议与可解码性
   - sync / async header/tailer
   - zlib / zstd 路径
   - async seq 语义
   - crypt / no-crypt 协议字段
2. 加密与压缩基本语义
   - `ECDH(secp256k1) + TEA`
   - async 仅加密 8-byte 对齐部分
   - zstd async streaming + `windowLog=16`
3. formatter 与 metadata
   - line formatting
   - raw metadata 回填策略
   - Android `traceLog` 旁路语义
   - global / category 路径差异
4. 对外能力面
   - `mars-xlog` 默认 Rust backend
   - default appender / named instance
   - JNI / UniFFI / NAPI 覆盖当前 Rust API

## 3. 当前高优先级阻断项

项目要求始终是 `语义级阻断项为 0`。当前实现尚未满足这条红线。

需要优先收口的 Rust 侧阻断项主要有 2 类：

1. recovery / oneshot 零拷贝把 recovered block 和 `MAGIC_END` 分段写入，存在跨进程 framing 风险
2. `FileManager` 的本地长度缓存与 rollback 更强地假设文件独占，外部 writer 干扰下可能错误截断

另外还有 2 项必须诚实描述但不应再误写成“Rust 偏离 C++”的问题：

1. 当前 sync / fatal 不能按“每条日志立即落文件”理解
2. 清空 mmap 或删除 cache 前没有 durability barrier

这几项不意味着整体迁移失败，但意味着当前不能把最近 perf 提交整体视为“低风险实现层优化”。

## 4. 当前实现与 C++ 的主要差异

### 4.1 Sync

当前和 C++ 的最关键差异已经不只是固定 syscall 成本，而是 sync 写入语义边界：

1. Rust sync steady-state 性能已经明显改善
2. 主要串行点已经收敛到 `FileManager.runtime`
3. 但当前 sync 路径本身就不应再按“每条日志立即写入文件”理解；这点 Rust 与 C++ 当前语义一致

因此，sync 后续工作首先要把文档语义写准确，再决定是否要在 Rust/C++ 两侧共同升级成更强保证。

### 4.2 Async

当前 async 路径已经证明：

1. `async_4t` 吞吐不是系统性问题
2. `async_4t_flush256` 已接近 C++
3. 剩余主差距集中在 `async_1t` fixed cost 与 tail latency

当前 async 最值得关注的实现差异：

1. 单 pending-state 模型仍然带来串行化固定成本
2. `compress.rs` 仍有流式压缩双缓冲复制
3. flush worker 的 `try_lock + sleep(1ms) + requeue` 更偏吞吐优先，而非 tail-latency 最优

## 5. benchmark 角色

benchmark 相关设计、矩阵、已落地能力和扩展顺序，统一放到 `docs/benchmark_strategy.md`。

当前只保留项目级判断：

1. `sync_4t` 的多线程竞争仍是 sync 主热点
2. `async_1t` 的 fixed cost 与 p99 仍是 async 主热点
3. 任何 benchmark 收益都不能抵消 `docs/rust_semantic_redlines.md` 中列出的语义阻断项

## 6. 最近一轮代码变化带来的新判断

当前分支最近一轮 perf 提交不能统一归为“低风险优化”。

更准确的划分是：

1. 低风险且方向正确
   - formatter / crypto scratch buffer 复用
   - sync 热路径移除非必要 engine 锁
   - tid thread-local 缓存
2. 有价值但带来明确语义取舍
   - async mmap persist cadence 节流
   - defer async flush when engine state is busy
3. 高风险，需要重新审视
   - recovery / oneshot 零拷贝分段写
   - active file / cached length / rollback 路径
   - sync keep-open 相关优化如果继续放大独占假设，也应按高风险处理

## 7. 当前最值得做的事情

### 7.1 P0: 先修正文档和语义假设

必须先明确以下问题：

1. sync 模式是否允许用户态缓冲
2. fatal 是否必须等价于立即落文件
3. recovery / oneshot 是否允许分段写 recovered block
4. cache/log 搬运删除源文件前，是否要求目标端已 durable

项目级要求不变：上面这些问题没有收口前，语义级阻断项就不能记为 `0`。

### 7.2 P0: 在不扩大语义风险的前提下继续做性能收敛

下一步重点：

1. `FileManager.runtime` 继续缩短 plain sync 热路径职责
2. `compress.rs` 双缓冲收敛
3. formatter / 时间格式化 / 冗余时钟调用 profiling

### 7.3 当前不建议进入主线的方向

这些方向暂时不建议进入主线：

1. per-thread async pending block
2. per-thread sync file handle / append-only 重构
3. `MS_ASYNC` / `madvise` 一类 OS 级 mmap 调优
4. lock-free / SIMD 大改

原因不是这些方向永远没价值，而是它们当前都不是最短路径，且语义验证成本明显更高。

## 8. 对后续改动的最低测试要求

后续性能优化不能只看 benchmark，至少必须绑定以下回归面：

1. async 语义与恢复
   - `cargo test -p mars-xlog-core --test async_engine`
   - `cargo test -p mars-xlog-core --test mmap_recovery --test oneshot_flush`
2. sync 文件路径与 rotation
   - `cargo test -p mars-xlog-core file_manager:: -- --nocapture`
3. Rust backend 端到端
   - `cargo test -p mars-xlog --lib`
4. benchmark
   - sync 变更至少重跑 `sync_1t` / `sync_4t` / `sync_4t_rotate_only`
   - async 变更至少重跑 `async_1t` / `async_4t` / `async_4t_flush256`

另需补齐但当前尚缺的测试：

1. sync fatal 即时可见性
2. recovery / oneshot 多进程 interleaving
3. cache/log durability barrier

## 9. 当前 review 总结

当前可以明确下结论：

1. Rust 迁移的主体工作已经完成，项目整体不再处于“功能迁移阶段”。
2. 但 `core` crate 当前仍存在高优先级语义风险，不能宣称“完全稳定收口”，更不能把语义级阻断项视为已清零。
3. 后续 review 只需要继续回答三个问题：
   - `sync_4t` 的串行区还能怎么缩
   - `async_1t` 的 fixed cost 和 p99 还能怎么降
   - 最近 perf 优化是否破坏了文档要求的语义边界
