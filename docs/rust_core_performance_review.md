# mars-xlog-core 深度性能与正确性审查

> 审查日期: 2026-03-06
> 审查范围: `crates/xlog-core/` + `crates/xlog/src/backend/rust.rs`
> benchmark 基线: `artifacts/bench-compare/20260306-harness-matrix-rerun`

## 1. 当前结论

当前 Rust 实现的协议兼容、解码兼容、压缩/加密主路径和主要 benchmark 结果都已经收敛，但不能再把 `core` crate 描述为“只剩纯性能问题”。

这轮深度 review 的结论是：

1. 当前分支确实拿到了明显的性能改善，尤其是 `sync_1t`、`async_4t` 和最近的 `sync_4t` 定向 rerun。
2. 但最近几轮性能优化里，已经出现了会影响语义边界的高风险点，主要集中在恢复路径零拷贝、文件所有权假设，以及若继续扩大会放大问题的 sync keep-open 路径。
3. 项目级门槛仍然是 `语义级阻断项为 0`；当前 benchmark 改善不代表这条红线已经满足。
4. 因此，当前主任务不是继续盲目追 benchmark，而是先把“哪些优化仍然语义安全”重新收口，再继续做实现层性能对齐。

一句话总结：

- 当前代码总体可用，benchmark 也不差。
- 但 `core` 现在不是“无阻断问题的高性能稳定态”，而是“性能已接近目标、但存在几处必须明确处理的语义风险”。
- 项目级红线与阻断项清单以 `docs/rust_semantic_redlines.md` 为准。

## 2. Benchmark 事实基线

当前主基线仍应只看 `20260306-harness-matrix-rerun`：

| scenario | rust / cpp | 结论 |
| :--- | ---: | :--- |
| `async_1t` | 81.0% | 仍有明显 fixed cost 和单线程 p99 问题 |
| `async_4t` | 110.7% | Rust 已超过 C++，不是当前主瓶颈 |
| `async_4t_flush256` | 97.3% | 已非常接近，需要谨慎归因 |
| `sync_1t` | 111.7% | Rust 已领先 |
| `sync_4t` | 81.8% | 主差距仍在多线程 steady-state |
| `sync_4t_rotate_only` | 90.0% | 已接近 |
| `sync_4t_cache_only` | 145.6% | Rust 已领先 |
| `sync_4t_boundary` | 1120.7% | Rust 显著领先 |

另外，当前分支已有一次定向 rerun 观察值：

- `sync_4t`: 约 `403,461.676 msg/s`
- 对比当前 C++ 基线 `420,277.596 msg/s`

这说明：

1. `FileManager` 热路径继续收缩锁职责，仍然是有效方向。
2. 但性能数据本身不能替代语义审查，尤其是 sync/flush/durability 语义。

## 3. 当前热路径判断

### 3.1 Sync

当前 sync 热路径已经不再先争用 `engine.state` 来读取 `file_manager` 和 `max_file_size`。主要串行点已经收敛到：

1. `format_record_parts_into`
2. `AppenderEngine::write_block`
3. `FileManager.runtime` 互斥区

这个判断成立，也是最近 `sync_4t` 变好的直接原因。

### 3.2 Async

当前 async 路径的真实约束不是“同一把 mutex 长时间持有”，而是：

1. `checkout_async_state()` 通过 `busy + Condvar` 维持单 pending-state 独占
2. 压缩、加密和 engine append 期间，其他线程不能 checkout 新状态
3. flush worker 仍然需要和前台线程共享 `engine.state`

因此，async 的问题本质是单 pending-block 模型下的 fixed cost 与控制面抖动，而不是一个简单的“大 mutex”。

## 4. 严重问题

以下问题按严重度排序，优先级高于继续追 benchmark。

### S0-1: 恢复路径和 oneshot 零拷贝引入了 split-write framing 风险

相关位置：

- `crates/xlog-core/src/appender_engine.rs`
- `crates/xlog-core/src/oneshot.rs`
- `crates/xlog-core/src/file_manager.rs`

当前针对 `recovered_pending_block` 的实现会把恢复数据和 `MAGIC_END` 拆成多段：

1. `append_log_slices(&[recovered, &end], ...)`
2. `FileManager` 再把这些 slice 逐段 `write_all`

如果同一目标文件在这些 syscall 之间被其他进程追加：

1. `recovered` 与 `MAGIC_END` 可能被第三方写入打断
2. block framing 会损坏
3. 这与项目已经显式支持的 “other process / oneshot flush” 场景是冲突的

因此，`af3dd2c` 这类“恢复路径零拷贝”改动不是纯低风险优化。

### S0-2: 本地缓存长度 + rollback 假设独占文件所有权

相关位置：

- `crates/xlog-core/src/file_manager.rs`

最近 sync/path 优化引入了：

1. `active_file.logical_len / disk_len`
2. `AppendTargetCache.local_len / merged_len`
3. 写失败时的 `rollback_file_to_len`

如果文件在本进程之外也被修改：

1. 本地缓存长度可能落后于真实长度
2. 之后一旦本进程发生写失败
3. `rollback_file_to_len()` 可能把外部进程追加的数据一起截掉

如果项目明确假设“同一 `.xlog` 文件永远只由一个 writer 进程独占”，需要写进文档；如果不是，这就是语义风险。

## 5. 当前共享语义边界

下面这些点是真问题，但不应再表述成“Rust 偏离了当前 C++ 参考实现”。

### C1: sync / fatal 不等价于每条日志立即落文件

Rust 现在是显式 keep-open 用户态缓冲写；但 C++ 当前也是 keep-open 的 `FILE* + fwrite` / stdio buffering，sync 下 `FlushSync()` 仍是 no-op。  
因此需要修正的是文档和语义叙述，而不是把这一点误记成 Rust 独有偏差。

### C2: 清空 mmap 或删除 cache 之前没有建立稳定存储屏障

Rust 和 C++ 目前都没有在删除恢复源前建立 `sync_data` / `fsync` 级稳定存储保证。  
这说明当前整套方案都存在 crash window；如果要把这项升级成强语义红线，就必须按双端共同改造处理。

## 6. 中等级风险与性能取舍

### S1-1: async mmap persist cadence 扩大了 crash-loss window

当前阈值：

- 每 `32` 次更新
- 或每 `32 KiB`
- 或每 `250 ms`

这是明显的吞吐优化，但它不再等价于“每次 async 更新都尽快持久化到 mmap”。如果 async 语义定义为 best-effort，这个取舍可以接受；如果文档暗示强恢复可见性，就必须改写文档。

### S1-2: `try_lock + sleep(1ms) + requeue` 是吞吐优先策略，不是 tail-latency 最佳实践

这不是 correctness bug，但它会让：

1. flush 请求延迟更依赖调度时机
2. `async_1t` / `flush256` 的 p99 归因更复杂

因此，当前不能把 `async_4t_flush256` 简单归因为单一 `msync(MS_SYNC)` 成本。

### S1-3: `append_log_slices` / `append_file_to_file` 仍然不是 I/O 最佳实践

当前还有两类明显但次级的问题：

1. 多 slice 写入仍是多次 `write_all`，不是单次拼接或 `write_vectored`
2. `append_file_to_file()` 仍用 `4 KiB` buffer，属于冷路径但偏保守

这些问题更多影响性能和原子性质量，不是当前最高级的功能阻断。

## 7. 哪些已经和旧文档不同

以下结论已经不能再出现在“当前状态”文档里：

1. `compress_level` 尚未接入 zlib async 路径
2. `current_tid()` 仍然每条日志都做系统调用
3. mmap 预分配仍然使用一次性大 `Vec`
4. 当前只剩“继续做低风险性能收敛”，没有新的语义风险
5. `async_4t` 是当前最主要吞吐瓶颈

当前真实状态是：

1. `compress_level` 已接入
2. tid 已做 thread-local 缓存
3. mmap 预分配已改为栈缓冲循环写零
4. 当前最需要修正文档的是 sync / recovery / durability 语义边界
5. async 主差距是 `async_1t` fixed cost 和 tail latency

## 8. 最近 perf 提交逐条结论

### 风险最低

- `45f2385 perf: reuse hot path formatter and crypto buffers`
  - 方向正确
  - 属于经典 hot-path allocation 收敛
  - 当前看不到明显语义破坏

### 中等风险，可接受但必须写明语义取舍

- `b809e1b perf: tune async mmap persist cadence`
  - 提升吞吐的方向成立
  - 但扩大了 crash-loss window
- `a5a05ba perf: defer async flush when engine state is busy`
  - 提升吞吐 / 减争用的方向成立
  - 但 p99 和 flush 时序更依赖调度

### 高风险，需要重新评估是否保留现状

- `af3dd2c perf: trim recovery and oneshot buffer copies`
  - 主要风险是 recovered block + end marker 的 split-write framing
- `ab5f1d9 perf: reuse active cache files in sync path`
  - 提高命中率和少扫目录是对的
  - 但更强地依赖本地缓存长度与路径状态正确
- `62ed8b0 perf: streamline sync steady-state writes`
  - 对 sync steady-state benchmark 有帮助
  - 但继续把语义往“用户态缓冲 + 本地 bookkeeping”方向推
- `3eb24a0 perf: buffer sync keep-open file writes`
  - 性能收益明确
  - 但这类方向是否可接受，需要以当前 C++ 语义和文档定义一起判断
- `b62fda7 perf: fast-path active sync file appends`
  - 继续缩短 steady-state 热路径
  - 但建立在前述 keep-open / active-file 假设之上

## 9. 当前是否符合高性能最佳实践

结论不能简单写成“是”或“否”。

### 已符合或基本符合的部分

1. 热路径 scratch buffer 复用是正确方向
2. sync 路径移除非必要 engine 锁是正确方向
3. tid thread-local 缓存、mmap 栈缓冲预分配都是合理微优化
4. `compress_level` 接入让配置面与实现一致

### 仍不算最佳实践的部分

1. sync 模式为吞吐牺牲了过多显式语义
2. 恢复路径零拷贝没有同时维护跨进程 append 原子性
3. cache/log 搬运没有 durability barrier
4. `FileManager` 仍把路径决策、缓存、活跃文件状态和真实 I/O 绑在同一把锁里
5. async 固定成本问题还没有被 profile 充分量化

所以更准确的说法是：

- 当前实现已经有不少高性能工程化实践。
- 但还不能称为“语义边界完全稳固的高性能最佳实践版本”。

## 10. 之后的主线优先级

### P0: 先收敛语义，再继续追 benchmark

1. 恢复 / oneshot 路径必须把 recovered block + `MAGIC_END` 作为单次连续 append 语义处理
2. 明确 `.xlog` 是否允许多 writer 竞争写入
3. 如果不允许，就把独占假设写进文档与测试；如果允许，就回收当前缓存长度与 rollback 假设
4. 对 C++ / Rust 共享的 sync / fatal / durability 语义边界，文档必须先写准确

### P1: 继续做不会改坏语义的性能优化

1. `compress.rs` 双缓冲收敛
2. formatter / 时间格式化 / 冗余时钟调用 profiling
3. `FileManager.runtime` 热路径继续拆职责
4. 冷路径 `append_file_to_file` buffer 放大或平台专用搬运优化

### 研究项，不进入当前主线

1. per-thread async pending pipeline
2. per-thread sync file handle / append-only 重构
3. `msync(MS_ASYNC)` / `madvise` 一类 OS 级 mmap 调优
4. SIMD / lock-free 大改

## 11. 当前测试结论与缺口

已执行并通过：

1. `cargo test -p mars-xlog-core --test async_engine`
2. `cargo test -p mars-xlog-core --test mmap_recovery`
3. `cargo test -p mars-xlog-core --test oneshot_flush`

这些测试说明：

1. 当前实现的已有回归面仍然能跑通
2. 但它们主要证明“现有实现自洽”，不能证明上面的语义风险不存在

仍缺少的关键测试：

1. 多进程或模拟 interleaving 下 recovered pending block append 的 framing 测试
2. 外部 writer 干扰下 rollback 不截断外部追加数据的测试
3. 如果后续决定升级 shared durability 语义，再补 cache/log 搬运与 mmap 清空前后的 durability 测试
4. 如果后续决定升级 sync/fatal 语义，再补即时可见性测试
