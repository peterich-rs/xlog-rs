# Rust 迁移语义红线与当前阻断项

## 1. 文档目的

本文定义 Rust 迁移阶段的语义红线，以及当前代码中已经识别出的语义级阻断项。

这里的要求高于 benchmark 结果、高于局部性能收益，也高于“当前实现大体可用”的判断。

## 2. 绝对红线

项目当前的硬门槛不是“允许带着语义风险继续推进”，而是：

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

## 5. 当前未收口的 Rust 侧阻断项

当前代码还没有达到“语义级阻断项为 0”的要求。这里列的是 Rust 相对 C++ 当前核心实现仍未收口的阻断项。

### S0-1: recovery / oneshot 的 split write 存在跨进程 framing 风险

相关代码：

- `crates/xlog-core/src/appender_engine.rs:583-603`
- `crates/xlog-core/src/oneshot.rs:77-106`
- `crates/xlog-core/src/file_manager.rs:1106-1137`

现状：

1. 恢复 pending block 时会把 `recovered` 和 `MAGIC_END` 作为多个 slice 追加
2. `FileManager` 会逐段 `write_all`
3. 在多次 syscall 之间，如果有其他进程向同一目标 `.xlog` 追加，block 边界可能被打断

为什么这是阻断项：

1. recovery / oneshot 本身就是跨进程场景
2. recovered block 被打断会直接破坏 framing 完整性
3. C++ 对应路径是先在内存里补齐 `MAGIC_END`，再把完整连续块交给文件写入；Rust 当前更弱

收口要求：

1. recovered block + `MAGIC_END` 必须按单个连续 block 处理
2. 对齐 C++ 的恢复 / oneshot 合帧语义
3. 补多进程或 interleaving 模拟测试

### S0-2: FileManager 本地长度缓存与 rollback 更强依赖单写者独占

相关代码：

- `crates/xlog-core/src/file_manager.rs:529-535`
- `crates/xlog-core/src/file_manager.rs:540-590`
- `crates/xlog-core/src/file_manager.rs:728-803`
- `crates/xlog-core/src/file_manager.rs:876-883`
- `crates/xlog-core/src/file_manager.rs:1128-1158`

现状：

1. active file、`AppendTargetCache.local_len`、`merged_len` 都依赖本地缓存长度
2. 出现写失败时会执行 `rollback_file_to_len`
3. 这些路径默认假设本进程掌握的文件长度就是正确长度

为什么这是阻断项：

1. 如果同一 `.xlog` 文件也被其他 writer 追加，本地缓存长度可能落后
2. 一旦本进程写失败并 rollback，可能把外部 writer 新写入的数据一起截断
3. C++ 也会在失败时做 truncate rollback，但没有 Rust 当前这么重的本地长度缓存和路由缓存

收口要求：

1. 明确 `.xlog` 是否允许多 writer 进程竞争写入
2. 如果允许，就必须修复本地长度缓存与 rollback 逻辑
3. 如果不允许，就必须把独占假设写进文档与测试，并确保所有外部入口遵守

## 6. 当前 C++ / Rust 共享的语义边界

下面这些问题是真问题，但不应表述成“Rust 偏离了 C++ 核心语义”。

### C1: sync / fatal 不能再按“每条日志同步落文件”理解

现状：

1. Rust sync 当前是显式 keep-open 用户态缓冲写
2. C++ sync 当前也是 keep-open 的 `FILE* + fwrite` / stdio buffering
3. C++ sync 下 `FlushSync()` 也是 no-op，fatal 也没有 sync 特判强刷

因此：

1. 这不是 Rust 相对 C++ 的语义偏离
2. 但文档不能再把当前 sync / fatal 描述成“每条日志立即落文件”或“fatal 在 sync 下强制落盘”

### C2: 删除恢复源前没有 durability barrier

现状：

1. Rust 和 C++ 都是在普通 `write_all` / `fwrite` 或 copy 成功后，清 mmap / 删 cache 源
2. 双方都没有 `sync_data` / `sync_all` / `fsync` 级稳定存储屏障

因此：

1. 这是当前整套方案共享的 crash window
2. 不能把它描述成 Rust 最近优化才引入的问题
3. 如果后续要把“删源前目标已 durable”升格为红线，必须按双端共同语义升级处理

## 7. 当前结论

当前可以明确下结论：

1. “语义级阻断项为 0”仍然是项目硬门槛，没有被 benchmark 结果放宽
2. 当前 Rust 相对 C++ 仍至少还有上面 2 项未收口阻断项
3. 另外还存在 2 项 C++ / Rust 共享的语义边界，文档和后续设计必须诚实描述
4. 在这些问题收口前，不能把当前状态描述成“只剩纯 benchmark 对齐”

## 8. 退出条件

只有在以下条件同时满足后，才可以把该文档判定为收口：

1. 上述 Rust 侧阻断项全部关闭，阻断项计数回到 `0`
2. 文档定义、实现行为、测试结论三者一致
3. benchmark 结果是在不跨越语义红线的前提下取得
4. 对 C++ / Rust 共享语义边界的表述不再自相矛盾
