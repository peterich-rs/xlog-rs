# mars-xlog-core 代码架构与可维护性审查

> 审查日期: 2026-03-16
> 复核日期: 2026-03-17
> 最终收口日期: 2026-03-17
> 审查范围: `crates/xlog-core/src/`

## 当前结论

原始 review 列出的 active issue 当前已经全部处理，不再保留未收口项。

已经关闭或收口的内容包括：

1. recovery helper 重复实现
2. `oneshot_flush` 缺少端到端测试
3. `AppenderEngine` 缺少 async/sync 集成测试
4. mutex poison 在关键 setter / 查询路径上的不一致
5. header 偏移 magic number
6. `oneshot.rs` 的 `u64 -> usize` 截断问题
7. 热路径上重复 `format!(".{LOG_EXT}")`
8. `mmap_store` 的整块零填充分配
9. `ZlibStreamCompressor` 缺少 roundtrip 测试
10. 压缩器保留整块已发射输出
11. `ConsoleLevel` / `LogLevel` 重复类型
12. `FileManager` 命名层、writer 层、plain/cache 主路径未拆分

## 本轮收口内容

此前唯一剩余的 `FileManager` 耦合问题，已在本轮拆分中收口：

1. 命名层已经拆到 `file_naming.rs`
2. buffered writer 已拆到 `active_append.rs`
3. `RuntimeState` / `AppendTargetCache` 已拆到 `file_runtime.rs`
4. append target 解析已拆到 `file_target.rs`
5. cache/log 路由策略已拆到 `file_policy.rs`
6. cache move / expiry lifecycle 已拆到 `file_maintenance.rs`
7. `file_manager.rs` 现在主要保留对外 API、文件锁初始化和 append orchestration

因此当前 `mars-xlog-core` 在这份 code review 范围内，已经没有需要继续追踪的主要架构问题。

## 后续方向

后续如果继续演进，应以局部优化和测试补强为主，而不是继续围绕 review blocker 进行强制拆分：

1. 继续为 `file_policy.rs` / `file_maintenance.rs` 补更细粒度的边界测试
2. 在不牺牲可读性的前提下继续压缩 `file_manager.rs` 内部 orchestration helper
3. 维持 `clippy`、`async_engine`、`oneshot_flush`、`mmap_recovery`、`file_manager` 回归测试闭环
