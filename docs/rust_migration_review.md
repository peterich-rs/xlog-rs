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
