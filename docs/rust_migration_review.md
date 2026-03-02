# Xlog Rust 迁移代码 Review 报告（2026-03-02 更新）

> 审查时间：2026-03-02  
> 审查范围：`crates/xlog-core/src/*` + `crates/xlog/src/backend/rust.rs`  
> 对照基线：`third_party/mars/mars/xlog/` C++ 实现

## 1. 结论

本轮已完成一批高风险行为对齐修复，Rust 侧在“恢复可靠性、文件一致性、控制台行为、sync+crypt 语义”上明显收敛。

当前状态：

- 已修复：**24 项**（含多项高风险）
- 仍待收敛：**3 项**（以接口覆盖差异为主）

---

## 2. 本轮已修复项

### 2.1 mmap/文件一致性（高优先级）

1. 启动时 mmap 恢复数据立即落盘（不再等待后续写入）。
   - `crates/xlog-core/src/appender_engine.rs`
2. torn-write 场景恢复逻辑放宽，按头部长度尽量保留可恢复前缀。
   - `crates/xlog-core/src/buffer.rs`
3. 追加写失败回滚：目标文件写入异常时回截到写前长度。
   - `crates/xlog-core/src/file_manager.rs`
4. cache->log 文件拼接失败回滚。
   - `crates/xlog-core/src/file_manager.rs`

### 2.2 引擎/生命周期行为

5. async flush 信号去重，避免无界 flush 命令积压。
   - `crates/xlog-core/src/appender_engine.rs`
6. `Async -> Sync` 模式切换改为异步触发 flush，不再阻塞等待。
   - `crates/xlog-core/src/appender_engine.rs`
7. 时钟回拨时复用上次日志文件路径，避免回拨导致文件选择异常。
   - `crates/xlog-core/src/file_manager.rs`
8. cache 可用空间阈值边界改为 `>= 1GiB`（与 C++ 一致）。
   - `crates/xlog-core/src/file_manager.rs`

### 2.3 协议/加密语义

9. sync + pubkey 场景改为 crypt magic + client_pubkey（payload 仍明文，符合 C++ 当前实现）。
   - `crates/xlog/src/backend/rust.rs`
10. 非法 server pubkey 改为降级 no-crypt（不再初始化失败）。
    - `crates/xlog/src/backend/rust.rs`
11. async seq 改为进程级全局序列（不再按 backend 实例）。
    - `crates/xlog/src/backend/rust.rs`
12. 压缩错误不再静默吞掉，失败时丢弃本次 block 构建。
    - `crates/xlog/src/backend/rust.rs`

### 2.4 控制台/API 语义

13. console 输出补齐 metadata（level/tag/file/func/line），Android tag 改为逐条日志 tag。
    - `crates/xlog-core/src/platform_console.rs`
14. Apple `set_console_fun` 不再 no-op，Rust backend 已接入模式切换。
    - `crates/xlog/src/backend/rust.rs`
15. `dump` 在默认 appender 不可用时返回空串（不再回退到 `memory_dump`）。
    - `crates/xlog/src/backend/rust.rs`
16. 默认 appender `open` 改为幂等，并保留全局 max-size/max-alive/console 粘滞设置。
    - `crates/xlog/src/backend/rust.rs`
17. 路径 API 的空 `prefix` 不再隐式回退到 `name_prefix`。
    - `crates/xlog-core/src/file_manager.rs`
18. `oneshot_flush` 改为 exact-size mmap 读取语义（与 C++ 一致，截断返回 `ReadFailed`）。
    - `crates/xlog-core/src/oneshot.rs`
19. async 路径改为“单 pending block + 流式增量压缩/加密 + flush 封尾”模型（与 C++ 主行为对齐）。
    - `crates/xlog/src/backend/rust.rs`
    - `crates/xlog-core/src/appender_engine.rs`
20. zstd async 压缩改为流式并显式设置 `windowLog=16`，按 chunk flush、block 结束时 end。
    - `crates/xlog-core/src/compress.rs`
    - `crates/xlog/src/backend/rust.rs`
21. async 高水位（4/5）告警恢复为实际写入日志内容，并保持在同一 pending stream 内。
    - `crates/xlog/src/backend/rust.rs`
22. `write_async_pending` 在 `Async -> Sync` 并发切换下新增 `InvalidMode` 兜底直写，避免最后一块丢失。
    - `crates/xlog/src/backend/rust.rs`
23. async pending mmap 持久化改为批量刷盘（每 N 次或强制条件刷盘），降低每条写入 `msync` 开销。
    - `crates/xlog-core/src/appender_engine.rs`
    - `crates/xlog-core/src/buffer.rs`
24. 后台线程 flush timeout 行为可配置（默认保持 15 分钟），便于稳定回归测试 timeout flush 语义。
    - `crates/xlog-core/src/appender_engine.rs`

---

## 3. 新增/调整回归测试

- `crates/xlog-core/src/buffer.rs`
  - `recover_pending_block_even_with_dirty_tail_bytes`
- `crates/xlog-core/tests/async_engine.rs`
  - `startup_drains_recovered_mmap_bytes_to_logfile`
  - `async_timeout_flushes_pending_block_without_explicit_flush`
  - `startup_recovers_pending_block_without_tailer`
- `crates/xlog-core/tests/mmap_recovery.rs`
  - 调整为 tailer torn 场景可恢复
- `crates/xlog-core/tests/oneshot_flush.rs`
  - 截断 mmap 改为 `ReadFailed`
- `crates/xlog/src/backend/rust.rs`
  - sync + pubkey 单测改为校验 crypt magic/client_pubkey
  - 新增 async zlib/zstd 多条日志合流单 block 回归
  - 新增 async crypt(zlib/zstd) 可解码、Async->Sync 不丢日志、高水位告警注入回归
- `crates/xlog-core/tests/compress_roundtrip.rs`
  - zstd 回归改为流式 compressor

---

## 4. 仍待对齐项（本轮未完全收口）

1. **`traceLog` 旁路 console 语义未接入**：Rust 目前无完整 `XLoggerInfo.traceLog` 等价入口。
2. **`XloggerWrite(instance_ptr==0)` 原语义未完全暴露**：Rust API 仍以 handle 写入为主，缺少完整 raw metadata 写路径。
3. **绑定层覆盖面仍小于 C++ 接口面**：UniFFI/NAPI 仍缺少部分控制/检索/维护能力。

---

## 5. 建议下一步

1. 在 API 层补齐 raw info/default instance 写入与 traceLog 语义。
2. 同步扩展 UniFFI/NAPI 覆盖，保持跨绑定行为一致。
