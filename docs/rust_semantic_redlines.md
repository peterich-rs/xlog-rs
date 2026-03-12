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
3. 是否可以宣称已经进入纯 Rust `GA` / 稳定替代阶段
4. 是否可以仅凭 benchmark 优势认定当前实现已经达标

补充说明：

1. benchmark 领先不能抵消语义阻断项
2. 任何未被文档声明、测试覆盖、兼容性验证接受的行为变化，都应先按阻断项处理
3. 如果某项行为要被正式改成新的语义，必须先更新规范、测试和兼容性结论，再讨论性能收益

`Preview` 发布口径单独说明如下：

1. `Preview` crates.io 发布不等价于“语义阻断项已经清零”
2. 在阻断项未清零时，只能以 `Preview` / 预览版口径发布，不能写成 GA、稳定替代版或完全对齐 C++ 生产版
3. `Preview` 发布前必须把 active blocker、使用约束和已知语义边界写进发布文档与 crate README

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

截至当前代码，Rust 侧 active blocker 已清零。

## 6. 已关闭但必须防回归的红线

下面这项不应再继续列为“当前 active blocker”，但必须保留回归测试与文档说明。

### S0-1-closed: FileManager 单写者假设已显式化并通过锁文件强制

当前代码：

1. `crates/xlog-core/src/file_manager.rs`

现状：

1. `FileManager::new` 在 `log_dir` 下创建 `<name_prefix>.lock` 并独占锁
2. 锁生命周期与实例一致，同一 `(log_dir, name_prefix)` 的多进程初始化会失败
3. 文档明确禁止多进程共享同一 `.xlog`

回归要求：

1. 锁文件行为必须保留，不能退回为仅文档约束
2. 同一 `(log_dir, name_prefix)` 的多进程并发创建应失败

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

### C2-closed-for-rust: Rust 删除恢复源前已建立 durability barrier

现状：

1. Rust 在 oneshot recovery 和 cache/log merge 路径上，会在删除 `.mmap3` 或 cache 源文件前先对目标文件执行 `sync_data`
2. 这关闭了 Rust 自身“目标写入成功但尚未 durable 就删源”的 crash window
3. vendored Mars C++ 快照当前仍未跟进这一语义升级，本仓库这次也没有修改 `third_party/mars`

因此：

1. 这项问题不再是 Rust 当前实现的语义边界
2. 但它仍然是与 vendored C++ 快照之间需要诚实说明的实现差异
3. 如果后续要求双端都满足“删源前目标已 durable”，需要单独推进上游 C++/vendor 同步

## 8. 当前结论

当前可以明确下结论：

1. “语义级阻断项为 0”仍然是 Rust `GA` / 稳定替代口径的硬门槛，没有被 benchmark 结果放宽
2. 当前 Rust 侧 active blocker 已清零，`FileManager` 单写者假设已显式化并通过锁文件强制
3. 发布口径仍需按 release plan 明确界定，不能仅凭 blocker 清零就宣称 “GA” 或 “完全收口”
4. recovery / oneshot split-write framing 风险已不再是 active blocker，但必须防回归
5. 当前仍有 1 项 C++ / Rust 共享语义边界，以及 1 项 vendored C++ 尚未跟进的 durability 差异，文档和后续设计必须诚实描述

## 9. 退出条件

只有在以下条件同时满足后，才可以把该文档判定为收口：

1. 上述 Rust 侧阻断项全部关闭，阻断项计数回到 `0`
2. 文档定义、实现行为、测试结论三者一致
3. benchmark 结果是在不跨越语义红线的前提下取得
4. 对 C++ / Rust 共享语义边界的表述不再自相矛盾
