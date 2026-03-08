# Rust 迁移语义红线与当前阻断项

## 1. 文档目的

本文定义 Rust 迁移阶段的语义红线，以及当前代码中仍未关闭的语义级阻断项。

这里的要求高于 benchmark 结果、高于局部性能收益，也高于“当前实现整体可用”的判断。

## 2. 绝对红线

项目当前的硬门槛始终是：

**语义级阻断项必须为 0。**

这条要求适用于以下所有判断：

1. 是否可以宣称 Rust 迁移已经稳定收口
2. 是否可以把某轮 perf 优化视为可接受主线状态
3. 是否可以进入移除 C++ backend 依赖阶段
4. 是否可以仅凭 benchmark 优势认定当前实现已经达标

补充说明：

1. benchmark 领先不能抵消语义阻断项
2. 任何未被文档声明、测试覆盖、兼容性验证接受的行为变化，都应先按阻断项处理
3. 如果某项行为要被正式改成新的语义，必须先更新规范、测试和兼容性结论，再讨论性能收益

## 3. 处理原则

后续处理按下面三类区分：

1. Rust 相对 C++ 的语义偏离
   - 必须优先按 C++ 当前核心实现语义对齐
2. Rust 相对 C++ 的功能性 bug / 健壮性问题
   - 可以朝更稳健、更高性能方向修，但不能改坏原有功能
3. C++ / Rust 共享的语义边界或历史限制
   - 不能误写成更强语义
   - 如果要升级成更强保证，必须作为双端共同变更处理

## 4. 不可退让的语义约束

后续优化必须同时满足以下约束：

1. 不改变日志协议与解码兼容性
   - header / tailer 结构
   - magic 取值
   - sync / async seq 语义
   - `ECDH(secp256k1) + TEA` 语义
2. 不弱化恢复语义
   - mmap 文件命名与容量
   - startup recover / oneshot flush 行为
   - torn tail / pending block 修复策略
3. 不在未明示的前提下改写 sync / fatal 语义
   - sync 是否允许用户态缓冲
   - fatal 是否必须等价于即时写入 / 即时可见
4. 不在未建立 durability 保证前销毁恢复源
   - 清空 mmap
   - 删除 cache file
5. 不默默引入更强的单写者独占假设
   - 如果实现要求目标 `.xlog` 只能由单进程独占写入，必须在文档和测试里显式写明

## 5. 当前未关闭的 Rust 侧阻断项

截至当前代码，Rust 侧仍有 `1` 项未关闭的高优先级阻断项。

### S0-1: FileManager 本地长度缓存与 rollback 更强依赖单写者独占

相关代码：

1. `crates/xlog-core/src/file_manager.rs`
   - `ActiveAppendFile.logical_len / disk_len`
   - `AppendTargetCache.local_len / merged_len`
   - `rollback_file_to_len()`

现状：

1. active file、`AppendTargetCache.local_len`、`merged_len` 都依赖本地缓存长度
2. 出现写失败时会执行 `rollback_file_to_len`
3. 这些路径默认假设本进程掌握的文件长度就是正确长度

为什么这是阻断项：

1. 如果同一 `.xlog` 文件也被其他 writer 追加，本地缓存长度可能落后于真实长度
2. 一旦本进程写失败并 rollback，可能把外部 writer 新写入的数据一起截断
3. 当前 Rust 比 C++ 更重地依赖本地长度缓存与活跃文件路由缓存，因此风险更集中

收口要求：

1. 明确 `.xlog` 是否允许多 writer 进程竞争写入
2. 如果允许，就必须修复本地长度缓存与 rollback 逻辑
3. 如果不允许，就必须把独占假设写进文档、测试和外部接入约束，并确保所有入口遵守

## 6. 已关闭但必须防回归的红线

下面这项不应再继续列为“当前 active blocker”，但必须保留回归测试与文档说明。

### C0-closed: recovery / oneshot 的 recovered block 必须保持连续写入

当前代码：

1. `crates/xlog-core/src/appender_engine.rs`
2. `crates/xlog-core/src/oneshot.rs`

现状：

1. `recovered_pending_block` 会先在内存里补齐 `MAGIC_END`
2. 再作为单个连续 block 交给 `append_log_bytes()`
3. 不再把 recovered payload 和尾标记拆成多次追加

这意味着：

1. 旧文档里“split-write framing 风险仍未关闭”的表述已经过期
2. 这项问题当前应视为已修复，而不是继续作为项目阻断项
3. 但回归测试必须持续覆盖 recovery / oneshot 的 contiguous append 语义

## 7. 当前 C++ / Rust 共享的语义边界

下面这些问题是真问题，但不应表述成“Rust 偏离了 C++ 核心语义”。

### C1: sync / fatal 不能按“每条日志同步落文件”理解

现状：

1. Rust sync 当前是 keep-open 用户态缓冲写
2. C++ sync 当前也是 keep-open 的 `FILE* + fwrite` / stdio buffering
3. 现有语义不等价于“每条日志立即 durable”

因此：

1. 这不是 Rust 相对 C++ 的独有偏离
2. 但文档不能再把当前 sync / fatal 描述成“每条日志立即落文件”或“fatal 在 sync 下强制落盘”

### C2: 删除恢复源前没有 durability barrier

现状：

1. Rust 和 C++ 都是在普通 `write_all` / `fwrite` 或 copy 成功后，清 mmap / 删 cache 源
2. 双方都没有 `sync_data` / `sync_all` / `fsync` 级稳定存储屏障

因此：

1. 这是当前整套方案共享的 crash window
2. 不能把它描述成 Rust 最近优化才引入的问题
3. 如果后续要把“删源前目标已 durable”升格为红线，必须按双端共同语义升级处理

## 8. 当前结论

当前可以明确下结论：

1. “语义级阻断项为 0”仍然是项目硬门槛，没有被 benchmark 结果放宽
2. 当前 Rust 侧 active blocker 已收敛到 `FileManager` 的文件所有权与 rollback 假设
3. recovery / oneshot split-write framing 风险已不再是 active blocker，但必须防回归
4. 另外还存在 2 项 C++ / Rust 共享的语义边界，文档和后续设计必须诚实描述

## 9. 退出条件

只有在以下条件同时满足后，才可以把该文档判定为收口：

1. 上述 Rust 侧阻断项全部关闭，阻断项计数回到 `0`
2. 文档定义、实现行为、测试结论三者一致
3. benchmark 结果是在不跨越语义红线的前提下取得
4. 对 C++ / Rust 共享语义边界的表述不再自相矛盾
