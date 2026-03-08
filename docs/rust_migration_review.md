# Rust Migration Review

## 1. 当前结论

基于当前仓库代码和 2026-03-08 最新 benchmark 基线，Rust 迁移已经不能再描述为“功能基本完成，但性能仍系统性落后于 C++”。

更准确的判断是：

1. 协议、解码、压缩/加密、formatter、bindings 和 Rust backend 主路径已经完成迁移。
2. benchmark 层面，Rust 已经在全量双端矩阵里取得明显领先，而不是“多数场景接近 C++”。
3. benchmark 结果不会放宽项目红线；当前仍有语义级阻断项未清零，因此不能把项目描述成“只剩性能问题”。

配套文档分工：

1. 代码级正确性与性能审查见 `docs/rust_core_performance_review.md`
2. 语义红线与阻断项清单见 `docs/rust_semantic_redlines.md`
3. benchmark 体系、基线与扩展计划见 `docs/benchmark_strategy.md`

## 2. 已确认稳定的对齐面

以下内容当前可以视为已完成：

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

## 3. 当前 benchmark 状态

最新全量双端矩阵基线：`artifacts/bench-compare/20260308-p0-full-matrix`

核心结论：

1. 吞吐更优场景：Rust `31 / 31`
2. 平均延迟更优场景：Rust `31 / 31`
3. P99 更优场景：Rust `30 / 31`
4. P999 更优场景：Rust `26 / 31`
5. sync throughput ratio gmean：`4.278`
6. async throughput ratio gmean：`1.412`

这意味着：

1. sync 已不再是主性能矛盾
2. async 也不再是“整体不够用”，而是只剩局部 tail 和输出体积效率问题
3. `async_4t_zstd3` 仍是唯一需要单独盯的 tail 场景
4. async 小消息 zlib 场景的 `bytes/msg` 仍显著偏大

最新 Rust Criterion 基线：`artifacts/criterion/20260308-p0-full-review`

结构性结论：

1. `core_formatter` 很稳定，约 `114ns ~ 126ns`
2. `core_crypto` 很稳定，约 `164ns ~ 1233ns`
3. 真正贵的是 async public write path，而不是 formatter/crypto
4. `core_compress_decode/zstd_*` 仍高噪声，更适合作为诊断信号而不是 hard gate

## 4. 当前高优先级阻断项

项目红线始终是 `语义级阻断项为 0`。当前实现尚未满足这条要求。

当前 active blocker 已收敛到 1 项：

1. `FileManager` 的本地长度缓存与 rollback 更强依赖单写者独占

另外还有 2 项必须诚实描述但不应再误写成“Rust 偏离 C++”的问题：

1. 当前 sync / fatal 不能按“每条日志立即落文件”理解
2. 清空 mmap 或删除 cache 前没有 durability barrier

需要明确：

1. recovery / oneshot split-write framing 风险不应再继续列为当前 blocker
2. 当前代码已经把 recovered block 与 `MAGIC_END` 连续写出，这项问题属于已修复、需防回归

## 5. 当前实现与 C++ 的主要差异

### 5.1 Sync

当前 sync 路径的最重要结论已经改变：

1. sync 性能已经明显领先 C++，不再需要把 `sync_4t` 当成主性能热点
2. sync 侧后续优先级应从“继续追吞吐”转向“把语义边界写准确，并收口文件所有权假设”
3. 与 C++ 的主要差异不再是基础写能力，而是当前 Rust 对本地缓存长度与活跃文件状态的依赖更重

### 5.2 Async

当前 async 路径已经证明：

1. `checkout_async_state()` 不再是主成本来源
2. 主成本集中在 `append` 阶段与 frontend queue backpressure
3. 剩余性能差距集中在 `async_4t_zstd3` tail latency 与 async 小消息 `bytes/msg`

因此，当前 async 后续工作不应继续围绕“fixed cost 是否系统性拖后腿”展开，而应围绕：

1. pending block 聚合行为
2. 压缩 flush 粒度
3. queue / backpressure 策略
4. flush requeue 与 tail latency

## 6. benchmark 的角色

benchmark 相关 runner、矩阵、Criterion、CI baseline 与回归脚手架，统一见 `docs/benchmark_strategy.md`。

当前只保留项目级判断：

1. Rust 已经具备性能竞争力，不再系统性落后于 C++
2. 当前主性能问题不是 sync 吞吐，而是 async 尾延迟、输出体积与可观测性
3. 任何 benchmark 收益都不能抵消 `docs/rust_semantic_redlines.md` 中列出的语义阻断项

## 7. 当前最值得做的事情

### 7.1 P0: async 结构化归因计数已落地

当前已经可以在 `bench_backend --stage-profile` 里直接看到：

1. pending block finalization 次数
2. 每块行数分布
3. finalize 原因分布
4. raw input / payload bytes per block
5. frontend `block_send` ratio
6. engine flush requeue 次数

这意味着下一步性能工作可以直接基于数据做定向优化，而不是继续先补观测面。

### 7.2 P0: 收口 active blocker

下一步优先级最高的语义工作是：

1. 明确 `.xlog` 是否允许多 writer 竞争写入
2. 如果允许，就修复 `FileManager` 的本地长度缓存与 rollback 逻辑
3. 如果不允许，就把单写者独占假设写进文档、测试和接入约束

### 7.3 当前不建议进入主线的方向

这些方向当前不建议优先进入主线：

1. 继续把 sync 吞吐当成第一性能目标
2. per-thread async pending pipeline
3. per-thread sync file handle / append-only 重构
4. `MS_ASYNC` / `madvise` 一类 OS 级 mmap 调优
5. lock-free / SIMD 大改
6. 面向单机型 benchmark 结果的 runtime 特调

原因很简单：这些方向要么已经不是当前主瓶颈，要么会明显扩大语义与验证成本。

补充结论：

1. 本轮已回看运行时代码，当前未发现按 `Apple / M2 / arm64 / x86_64` 做性能分支的逻辑
2. 现有 async queue / batch / retry 常量属于通用策略常量，不应被解读为机型特调
3. 后续进入主线的性能调整，必须要求跨设备方向一致

## 8. 对后续改动的最低测试要求

后续优化不能只看 benchmark，至少必须绑定以下回归面：

1. async 语义与恢复
   - `cargo test -p mars-xlog-core --test async_engine`
   - `cargo test -p mars-xlog-core --test mmap_recovery`
   - `cargo test -p mars-xlog-core --test oneshot_flush`
2. sync 文件路径与 rotation
   - `cargo test -p mars-xlog-core file_manager:: -- --nocapture`
3. Rust backend 端到端
   - `cargo test -p mars-xlog --lib`
4. benchmark
   - async 改动至少重跑 `async_4t` / `async_4t_zstd3` / `async_4t_large_entropy`
   - 写路径或压缩改动至少重跑 `cargo bench -p mars-xlog --bench criterion_write_path`
   - 里程碑改动重跑全量双端矩阵

另需持续保留的测试重点：

1. recovery / oneshot contiguous append 防回归
2. `FileManager` 外部 writer 干扰场景
3. cache/log durability barrier

## 9. 当前 review 总结

当前可以明确下结论：

1. Rust 迁移的主体工作已经完成，项目整体不再处于“功能迁移阶段”。
2. benchmark 结果已经支持“Rust 具备外部接入和受控生产验证价值”的判断。
3. 但 active semantic blocker 还没有清零，因此还不能把当前状态描述成“正式生产收口版”，更不能进入移除 C++ 依赖阶段。
4. 当前主线工作应是“补 async observability + 收口 FileManager 语义风险 + 定向治理 async tail/bytes tradeoff”。
