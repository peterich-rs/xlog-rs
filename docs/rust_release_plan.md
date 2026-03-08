# Rust 发布计划

## 1. 目标

下一阶段的发布目标是：

1. 以 Rust 实现为主，准备 `crates.io` 发布路径
2. 对外发布不包含 C++ 版本的安装/默认依赖链
3. 保留仓库内 legacy C++ 代码作为兼容性参考，而不是发布主线或 `mars-xlog` 依赖链

非目标：

1. 这一阶段不移除仓库内的 C++ 代码与 benchmark 参考
2. 不因为 benchmark 已领先就跳过语义红线
3. 不把当前状态直接表述成 GA 替代版

## 2. 当前状态

当前项目已经具备两个重要前提：

1. benchmark 层面，Rust 已在最新全量矩阵里整体超过 C++
2. benchmark / profiling / CI 基础设施已经收口到可长期维护的形态

但当前还不满足“正式 GA 发布”的条件，原因不是性能，而是发布面和语义边界还没有完全收口。

当前应区分两种发布状态：

1. `Preview` 发布
   - 可以发布 Rust-only crate
   - 必须诚实写明当前语义边界和已知限制
   - 不承诺完全替代 C++ 生产版
2. `GA` 发布
   - 语义级阻断项必须为 `0`
   - 发布面、文档、测试、CI、回归和支持矩阵全部稳定

按当前代码状态，更合理的目标是先准备 `Preview`，而不是直接宣称 `GA`。

## 3. 当前已确认的发布阻断项

在 Cargo 拓扑上，需要先区分两个概念：

1. 对外主推的 release-facing crate
2. 为这个 crate 提供实现的内部依赖 crate

当前更现实、也更符合 Cargo 发布机制的模型是：

1. 对外主推 `mars-xlog`
2. `mars-xlog-core` 作为实现层依赖存在
3. `mars-xlog-sys` 和各 binding crate 保持 workspace-only

如果未来要把整个发布对象进一步收敛成“单 package、无独立 core crate”的形态，那需要把 core 代码并入顶层 crate。这是单独的包结构重构，不应和本轮发布资料补齐混在一起。

### 3.1 crates.io 发布拓扑已收口到纯 Rust，但仍有发布顺序要求

当前 dry-run 结果：

1. `cargo publish --dry-run -p mars-xlog-core --allow-dirty`
   - 通过
   - release metadata 和 README 已补齐
2. `cargo publish --dry-run -p mars-xlog --allow-dirty`
   - 失败
   - 原因：`mars-xlog-core` 尚未存在于 crates.io

因此当前至少有两个事实：

1. 发布顺序必须先解决 `mars-xlog-core`，再发布 `mars-xlog`
2. `mars-xlog` 的 crates.io 拓扑已经收口为纯 Rust，但仍要按 crates.io 依赖顺序发布

### 3.2 crate 元数据基础面已补齐

这轮已经补齐发布对象的基础元数据：

1. `description`
2. `documentation`
3. `homepage`
4. `repository`
5. `readme`
6. `keywords`
7. `categories`
8. `rust-version`

剩余工作不再是“有没有这些字段”，而是随着最终发布包名字和 docs.rs 地址定稿后，是否还需要做一次统一调整。

### 3.3 发布范围已定稿

当前发布范围已经部分收口。

当前推荐的发布模型是：

1. 外部用户只需要在 `Cargo.toml` 里依赖 `mars-xlog`
2. `mars-xlog-core` 可以作为实现层 crate 发布到 crates.io，但不作为主推入口
3. workspace 中的 legacy/C++/binding crate 不进入公开发布面

建议默认发布对象：

1. `mars-xlog-core`
2. `mars-xlog`

建议默认不发布：

1. `mars-xlog-sys`
2. `mars-xlog-uniffi`
3. `mars-xlog-android-jni`
4. `oh-xlog`

当前状态：

1. 上述非发布对象已经标记 `publish = false`
2. `mars-xlog` 已不再携带 `mars-xlog-sys` 或 `cpp-backend` 到公开发布面

原因：

1. 当前阶段目标是 Rust-only 发布，不是把 legacy/C++ 或平台 binding 一起推到 crates.io
2. bindings 和 `-sys` crate 仍更适合留在仓库内随源码、示例工程和平台构建链维护

### 3.4 发布包内容需要收口

这轮已经完成第一步收口：

1. `mars-xlog-core` 不再把 benchmark fixture、bench 文件和 example 带入发布包
2. `mars-xlog` 不再把 benchmark example 和 benches 带入发布包

当前剩余问题：

1. 需要继续用 `cargo package --list` 和 dry-run 校验发布包内容
2. 发布顺序仍要求先有 `mars-xlog-core`，再发布 `mars-xlog`

### 3.4.1 `mars-xlog-core` 当前 preflight 快照（2026-03-08）

当前已经建立了可重复执行的本地 preflight：

1. `scripts/xlog/check_mars_xlog_core_release.sh`

当前快照结果：

1. `cargo package -p mars-xlog-core --list`
   - 通过
   - 发布包包含 `28` 个文件
2. `cargo publish --dry-run -p mars-xlog-core --allow-dirty`
   - 通过
3. `cargo test -p mars-xlog-core --test async_engine --test compress_roundtrip --test dump --test mmap_recovery --test oneshot_flush --test protocol_compat`
   - 通过
4. `cargo rustdoc -p mars-xlog-core --lib -- -D missing-docs`
   - 失败
   - 当前共有 `239` 个 public API 文档缺失错误

因此 `mars-xlog-core` 当前状态可以描述为：

1. Cargo 打包与 dry-run 发布路径可用
2. 核心集成测试可用
3. 正式发布质量仍被 public API 文档覆盖阻断

### 3.5 仍存在 1 个语义级阻断项

当前 active blocker 见 [rust_semantic_redlines.md](/Users/zhangfan/develop/github.com/xlog-rs/docs/rust_semantic_redlines.md)：

1. `FileManager` 的本地长度缓存与 rollback 更强依赖单写者独占

这项阻断不必阻止 `Preview` 发布，但会阻止“正式 GA 替代版”的表述。

## 4. 发布阶段划分

### 4.1 Phase A: Rust Preview

目标：

1. 在 crates.io 上提供 Rust-only 可安装版本
2. 对外 API、README、docs.rs 和基础示例可用
3. 保持 legacy C++ 代码只在仓库内存在，不再进入 `mars-xlog` 发布链路

要求：

1. 发布对象和 publish policy 明确
2. `cargo publish --dry-run`、`cargo package --list`、docs.rs 构建可通过
3. README、crate docs、feature flags、known limitations 写清楚
4. benchmark、CI、回归门禁已经接入并稳定工作

### 4.2 Phase B: Rust GA

目标：

1. 可以对外明确描述为主推版本
2. 不再依赖 C++ backend 作为发布语义背书

额外要求：

1. 语义级阻断项回到 `0`
2. sync / fatal / durability 语义文档稳定
3. 发布验证覆盖目标平台矩阵
4. 支持边界、升级路径、已知限制都已经对外写明

## 5. 需要完成的工作

### 5.1 P0: 收口发布拓扑和 publish policy

这是第一优先级。

必须先定：

1. `mars-xlog-core` 是否作为公开依赖 crate 正式发布
2. `mars-xlog` 是否保持当前纯 Rust 依赖拓扑
3. 哪些 crate 显式加上 `publish = false`

建议方向：

1. `mars-xlog-core`、`mars-xlog` 作为发布对象
2. `mars-xlog-sys`、bindings crate 先全部 `publish = false`
3. 把 legacy C++ 代码保留为 workspace / repo 内参考，而不是 crates.io 发布能力

当前这三条都已经完成；剩余阻断主要是发布顺序与发布验证。

### 5.2 P0: 补全包元数据和 docs.rs 基础面

所有发布对象至少补齐：

1. `description`
2. `documentation`
3. `homepage`
4. `repository`
5. `readme`
6. `keywords`
7. `categories`
8. `rust-version`

同时需要：

1. crate 级 README
2. docs.rs 首页文档和 quick start
3. feature flag 说明
4. known limitations / non-goals 说明

这轮已经完成这部分基础补齐。

补充说明：

1. workspace `repository` 已改到当前仓库 `https://github.com/fannnzhang/xlog-rs`
2. 如果最终发布包名称调整，`documentation` 地址还需要再同步一次
3. `mars-xlog-core` 还没有达到 `-D missing-docs` 级别的文档覆盖，当前应把它视为 release blocker，而不是单纯的文档优化项

### 5.3 P0: 收口发布包内容

需要为发布对象建立明确的 `include / exclude` 策略，至少回答：

1. 是否保留 benches
2. 是否保留 benchmark fixtures
3. 是否保留 benchmark-only examples
4. 是否保留内部诊断脚本相关资产

建议方向：

1. 默认只发布运行时代码、必要测试、README、LICENSE/NOTICE 和最小示例
2. benchmark 数据、fixture、大型 example 优先留在仓库，不进发布包

当前第一步已完成，`mars-xlog` 已不再携带 C++ backend 源码和依赖链进入公开发布面。

### 5.4 P1: API 与语义说明收口

发布前必须把外部使用者最容易误解的点写清楚：

1. sync / fatal 当前不是“每条日志立即 durable”
2. 当前 durability 边界
3. 当前 active semantic blocker 对使用约束的影响
4. `bench-internals` 不是公共稳定 API
5. `mars-xlog-core` 是否承诺稳定公共 API，还是仅作为 `mars-xlog` 的依赖实现

对 `mars-xlog-core` 而言，当前更具体的收口顺序应是：

1. 先补 `appender_engine / buffer / protocol / record / registry` 的公开类型与方法文档
2. 再补 `compress / crypto / file_manager / oneshot` 的公开错误类型、常量和行为边界说明
3. 最后再用 `cargo rustdoc -p mars-xlog-core --lib -- -D missing-docs` 作为 release 通过条件

### 5.5 P1: 发布 CI 与验证流程

发布前应加上最小发布流水线：

1. `cargo package --list`
2. `cargo publish --dry-run -p mars-xlog-core`
3. `cargo publish --dry-run -p mars-xlog`
4. docs.rs 构建检查
5. 关键示例编译检查
6. 目标平台 smoke test

同时要求：

1. 发布前固定 benchmark baseline
2. 发布变更不能绕过当前 benchmark / regression 脚手架

### 5.6 P2: 达到 GA 所需的剩余工作

这些不一定阻止 `Preview`，但会阻止 `GA`：

1. 关闭 `FileManager` 语义阻断项
2. 发布支持矩阵定稿
3. 版本策略、变更日志、升级指南稳定
4. 跨设备验证形成固定节奏，而不是单机 benchmark 结论

## 6. 推荐执行顺序

建议按下面顺序推进：

1. 先定发布对象和 `publish = false` 范围
2. 先发布 `mars-xlog-core`，再验证 `mars-xlog` 的 crates.io dry-run
3. 补全 `mars-xlog-core` / `mars-xlog` 元数据与 README/docs.rs
4. 收口 package contents
5. 建立发布 CI
6. 再决定是否发第一个 `Preview`

## 7. Preview 退出条件

满足以下条件后，可以发第一个 Rust `Preview`：

1. `mars-xlog-core` 和 `mars-xlog` 都能通过 `cargo publish --dry-run`
2. 发布版不再要求用户引入 C++ backend
3. 文档能明确说明当前语义边界和 known limitations
4. 发布包内容已经收口
5. benchmark / regression / 基础测试通过

## 8. GA 退出条件

满足以下条件后，才可以进入 Rust `GA`：

1. 语义级阻断项为 `0`
2. 发布对象、文档、CI、支持矩阵稳定
3. 跨设备验证完成
4. benchmark 结果与对外发布描述一致
