# Rust Migration Review & Parity Deep Dive

## 1. 结论总结

经过对 Rust 版实现（`xlog-core` / `xlog` / bindings）与 C++ 版（`mars/xlog`）的源码深度对比，Rust 核心库在以下层面与 C++ 实现了高度的语义对齐：
- **文件管理与滚动策略** (按天/大小切换、Cache 满溢移盘、并发写)。
- **Appender 异步/同步模式** (Mmap 预写、缓冲池阈值唤醒、压缩流水线)。
- **Crash-safe 恢复逻辑** (启动时 `recover_blocks`、torn-write 修补、Tip 标记)。
- **加密签名字节级协议** (ECDH-TEA 流水线加密逻辑、协议头构造)。

在最新一轮的深度对比中，我们进一步排查并修复了数个细微的边界差异：特别是发现在**后台 `flush` 与前端连续异步写入的交错场景**下，原生 Rust 使用的双锁架构（`RustBackend::async_state` 与 `AppenderEngine::state`）由于检查点时序问题，存在**极端条件下的数据丢失（Race Condition）**。

## 2. 本轮深层修复与功能对齐 (Deep Dive Fixes)

1. **[CRITICAL] 后台 Flush 导致的异步 Pending Block 截断丢数据问题**
   - **问题现象**：在异步高频写入时，如果恰好背景 Worker 线程达到 15 分钟超时或满 1/3 阈值触发 Flush，Worker 线程会抢占 Mmap 并刷盘。此时前端 `write_async_line` 如果恰好在检查 `async_flush_epoch` **之后**、写 Mmap **之前**，会导致前端状态机认为合并 Block 有效，最终用仅含后半段数据的 Block 覆盖了 Mmap（而此时 mmap 中旧的前半段数据已被 Worker 刷入磁盘）。结果：合并块的后半段数据丢失！
   - **C++ 对比**：C++ 版本在 `appender.cc:__WriteAsync` 和 `__AsyncLogThread` 中硬共用同一把 `mutex_buffer_async_` 大锁，因此压缩、追加、与后台刷盘在物理上完全互斥，无此竞争。
   - **解决方式**：不破坏 Rust 优良的无阻塞分治锁架构，通过在 `AppenderEngine::write_async_pending_check_epoch` 内部注入原子 Epoch 检验。前端在覆盖 Mmap 时若发现 Epoch 突变（已被刷盘），则直接摒弃已残缺的内存压缩块，重发原始文本进行安全重试（Loop Retry），彻底根除丢日志隐患。

## 3. 本轮收口结果（已完成）

针对上一版中遗留的 Wrapper 级差异，已在当前分支完成收口：

1. **`traceLog` Android 旁路语义已补齐**
   - 在 `xlog` 新增 `RawLogMeta { trace_log }` 传递通道。
   - Rust backend 在 Android 上改为：`console_open == true` 或 `trace_log == true` 任一满足即写 Console，语义对齐 C++ `appender.cc`。
2. **全局 Raw Metadata 写路径与 PID/TID 复写策略已对齐**
   - 新增 `Xlog::appender_write_with_meta_raw(...)`，对应 `XloggerWrite(instance_ptr == 0, ...)` 能力。
   - `pid/tid/maintid` 填充规则按 C++ 双路径对齐：
     - `instance_ptr != 0`（Category 路径）：仅在三者全为 `-1` 时批量回填。
     - `instance_ptr == 0`（Global 路径）：逐字段按 `-1` 回填。
   - 由此避免 Java/JNI 侧传入线程元数据被 Rust 层强制覆写的问题。
3. **UniFFI / Harmony NAPI 接口覆盖已补齐**
   - 补齐实例控制面：`is_enabled/level/set_level/set_appender_mode/flush/set_console_log_open/set_max_file_size/set_max_alive_time`。
   - 补齐写入面：`log_with_meta/log_with_raw_meta` 与全局 `appender_write_with_raw_meta`。
   - 补齐工具面：`open_appender/close_appender/flush_all/current_log_path/current_log_cache_path/filepaths_from_timespan/make_logfile_name/oneshot_flush/dump/memory_dump`。
   - 绑定层当前能力面已对齐 `mars-xlog` Rust API。

上述修复后，本轮 review 中定义的 Rust 重构语义差异已全部收口。

## 4. 发布就绪度（截至 2026-03-04）

- `mars-xlog-core`：`cargo publish --dry-run` 通过。
- `mars-xlog`：依赖 `mars-xlog-core` 先发布到 crates.io（当前 dry-run 因索引无该包失败）。
- `mars-xlog-uniffi` / `oh-xlog`：依赖 `mars-xlog` 先发布到 crates.io（当前 dry-run 因索引无该包失败）。
- `mars-xlog-sys`：legacy FFI crate 的打包验证仍依赖仓库外路径（`third_party/mars`），需单独整改；不阻塞 Rust 主链路发布。

## 5. 性能优化深层 Review (2026-03-06，已结合本轮实现更新)

针对当前 Rust 版本的性能表现（Sync 吞吐 34.9%，Async 吞吐 43.1%，p99 延迟较高），结合 `xlog-core` / `xlog` 与 C++ `mars/xlog` 当前实现，对上一轮“性能优化想法”做进一步筛选。结论不是“想法越多越好”，而是只保留真正符合当前瓶颈、且不破坏既有协议与恢复语义的实现项。

### 5.1 已验证有价值，并已落地

1. **[AppenderEngine / Buffer] Async flush 路径继续压缩复制与清零**
   - 这条已经落地。`flush_pending_locked` 不再走整段 `take_all + clear`，而是优先复用 mmap 已有字节视图，只在 pending block 缺尾标记时做最小补齐，并把 clear 收敛为已用区间。
   - 结论成立：这是有效的实现层优化，不改协议、不改恢复规则，只减少热路径复制和写放大。

2. **[FileManager] 按目录/按天缓存 append target，减少热路径目录扫描**
   - 这条也已经落地当前一阶段：steady-state 已有按目录/按天的 append target cache，并补上活跃 cache 文件 fast path，热路径不再每次重新 `read_dir / metadata / path-select`。
   - 结论仍然成立：这条直接命中 sync 明显落后 C++ 的主因，收益高于继续做零碎 formatter 微调。

3. **[Benchmark Harness] 恢复同轮 Rust/C++ 对照能力**
   - 这条本轮也已落地。benchmark example 已支持 compile-time Rust/C++ backend 选择，并补齐 `--threads`、`--flush-every` 等参数；同轮 threaded smoke 也已经恢复。
   - 这让后续性能结论不再完全依赖历史基线，而能直接基于同参数 A/B。

4. **[Sync Steady-State] 收窄 engine/file-manager 热路径锁作用域**
   - 这条在新矩阵下已经证明有价值。plain sync 的主差距并不主要落在轮转边界，而是 steady-state 热路径里把 `AppenderEngine` / `FileManager` 锁持有到文件 I/O 完成。
   - 当前已经先落了一阶段：sync 写入改为先 snapshot `AppenderEngine` 配置再锁外 append；`FileManager` plain 路径收敛到单次 runtime 锁，目录创建下沉到 `open(NotFound)` 兜底。
   - 结果是 plain sync `1T / 4T` 都有实质提升，说明这个方向比继续抠边界探测更值钱。

5. **[Sync Active File] 对齐 C++ `FILE*` keep-open 缓冲写模型**
   - 这条本轮已经落地，而且收益比预期更大。C++ sync steady-state 本质上是“常驻 `FILE*` + `fwrite`”，带 stdio 用户态缓冲；Rust 之前是“常驻 `File` + `write_all`”，固定 syscall 成本明显更高。
   - 当前 `FileManager` 已把 keep-open 活跃文件改为 `BUFSIZ` 对齐的用户态缓冲，并在关闭、换文件、维护路径上显式冲刷。
   - 结果是 plain sync `1T` 已经反超 C++，`4T` 也提升到 C++ 的约 `87%`。这基本证明：sync 的“固定成本”阶段已经收敛，后续重点应转到多线程竞争，而不是继续放大边界路径问题。

### 5.2 仍有价值，但要先做 profiling 再决定

1. **[AppenderEngine::state] 继续收窄 engine 串行区**
   - 本轮已经先做了低风险版本：后台 async flush 在 state 忙时改为 `try_lock + requeue`，避免 worker 在热写入期间长时间阻塞。
   - 但 `EngineState` 仍保留必要串行区。是否继续往更细的状态拆分推进，要以新的 threaded benchmark/profiling 为前提。

2. **[SIMD] TEA / 压缩指令级加速**
   - 这条只在特定配置下才可能明显收益，例如启用了 crypt，或者压缩路径确实在 profile 中占主导。
   - 在当前阶段，先没有证据表明它比减少内存复制、减少目录扫描更值钱，因此只保留为实验项，不进入当前主线。

3. **[FileManager] 继续减少轮转边界和 cache/log 切换边界探测**
   - 这条仍然有价值，但优先级已经下调。新的多轮矩阵显示，Rust sync 在 plain steady-state 下也明显落后，而 rotate/cache 边界带来的额外损耗反而不是当前最大项。
   - 所以下一步不应再把边界探测当成 `P0`，而应把它视为 steady-state 问题收敛后的后续项。

### 5.3 当前价值不高，暂不进入主线

1. **[Zero-Copy / Interning] 把 tag / filename / func 做成更激进的驻留或全局字符串**
   - 这条当前不建议继续投入。formatter 热路径已经改成 borrowed fields + 复用 scratch string，收益最大的那一层已经拿到了。
   - 继续做 string interning 或“完全 zero-copy 格式化”，实现复杂度高，但很难超过前两项带来的收益。

2. **[madvise / msync(MS_ASYNC)] mmap OS 指令级调优**
   - 这条风险高于收益。当前 mmap flush 仍然承担 crash-recovery 语义，贸然改为 `MS_ASYNC` 或加入激进 `madvise`，可能影响跨平台恢复行为。
   - 在没有专门 crash/断电验证前，不应进入主线。

3. **[sendfile / fcopyfile] Cache 文件零拷贝搬运**
   - 这条是冷路径优化，不是当前 benchmark 主瓶颈。`append_file_to_file` 只在 cache 文件搬运时触发，而不是每条日志热写路径。
   - 后续可以作为平台增强项单独评估，但不应抢占 async p99 和 sync 热路径的优化优先级。

4. **[线程优先级] setpriority / pthread 调度**
   - 这条平台相关性太强，而且会引入额外运维与权限复杂度。
   - 在当前还没有证明 Worker 线程被调度饿死之前，不进入主线。

### 5.4 当前结论

这轮已经完成的主线项：

1. AppenderEngine / PersistentBuffer：去掉 async flush 路径的整段 `take_all + clear`。
2. FileManager：引入按目录/按天的 append target cache，并补上活跃 cache 文件 fast path。
3. benchmark：恢复 compile-time Rust/C++ backend harness，并支持 threaded smoke。
4. AppenderEngine：后台 async flush 在 busy state 时改为 `try_lock + requeue`。
5. sync steady-state：`AppenderEngine` 锁外执行文件写入，`FileManager` plain 热路径收敛到单次 runtime 锁。

下一步只保留两类内容：

1. sync `4T` steady-state 继续收敛，优先压活跃文件写入串行区竞争。
2. 基于新的 threaded benchmark，对 `AppenderEngine::state` 是否继续拆分做 profiling 决策。

其余优化想法保留在文档中，但默认视为“实验项”或“后置项”，不再并行扩散实现范围。
