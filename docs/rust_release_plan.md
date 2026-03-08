# Rust 发布计划

## 1. 目标

下一阶段的发布目标是：

1. 以 Rust 实现为主，准备 `crates.io` 发布路径
2. 对外发布不包含 C++ 版本的安装/默认依赖链
3. 保留仓库内 C++ backend 作为 parity、benchmark 和兼容性参考，而不是发布主线

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

### 3.1 crates.io 发布拓扑尚未收口

当前 dry-run 结果：

1. `cargo publish --dry-run -p mars-xlog-core --allow-dirty`
   - 通过
   - 但 manifest 缺少 `description / documentation / homepage / repository`
2. `cargo publish --dry-run -p mars-xlog --allow-dirty`
   - 失败
   - 原因：`mars-xlog-core` 尚未存在于 crates.io

这说明当前至少有两个事实：

1. 发布顺序必须先解决 `mars-xlog-core`，再发布 `mars-xlog`
2. `mars-xlog` 的 crates.io 拓扑还要重新审视，不能直接把当前 workspace 依赖关系视为最终发布形态

### 3.2 crate 元数据还不符合正式发布要求

当前 crate manifest 普遍缺少：

1. `description`
2. `documentation`
3. `homepage`
4. `repository`
5. `readme`
6. `keywords`
7. `categories`
8. `rust-version`

这不是装饰项，而是正式发布质量的一部分。

### 3.3 发布范围还没定稿

当前需要明确哪些 crate 是发布对象，哪些必须保持 workspace-only：

建议默认发布对象：

1. `mars-xlog-core`
2. `mars-xlog`

建议默认不发布：

1. `mars-xlog-sys`
2. `mars-xlog-uniffi`
3. `mars-xlog-android-jni`
4. `oh-xlog`

原因：

1. 当前阶段目标是 Rust-only 发布，不是把 legacy/C++ 或平台 binding 一起推到 crates.io
2. bindings 和 `-sys` crate 仍更适合留在仓库内随源码、示例工程和平台构建链维护

### 3.4 发布包内容需要收口

当前打包结果显示：

1. `mars-xlog-core` package 会带上 benchmark fixture、bench 文件和 example
2. `mars-xlog` package 会带上 benchmark example 和 benches

这不一定错误，但需要明确决策：

1. 哪些文件是发布包真正需要的
2. 哪些 benchmark 资产只应留在仓库，不应进入 crates.io tarball

如果不主动收口，后续发布包会持续把本地 benchmark 资产一起带出去。

### 3.5 仍存在 1 个语义级阻断项

当前 active blocker 见 [rust_semantic_redlines.md](/Users/zhangfan/develop/github.com/xlog-rs/docs/rust_semantic_redlines.md)：

1. `FileManager` 的本地长度缓存与 rollback 更强依赖单写者独占

这项阻断不必阻止 `Preview` 发布，但会阻止“正式 GA 替代版”的表述。

## 4. 发布阶段划分

### 4.1 Phase A: Rust Preview

目标：

1. 在 crates.io 上提供 Rust-only 可安装版本
2. 对外 API、README、docs.rs 和基础示例可用
3. 保持 C++ backend 在仓库内用于 benchmark / parity，而不是发布默认链路

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
2. `mars-xlog` 的发布版是否完全去掉 `mars-xlog-sys` 的 crates.io 依赖链
3. 哪些 crate 显式加上 `publish = false`

建议方向：

1. `mars-xlog-core`、`mars-xlog` 作为发布对象
2. `mars-xlog-sys`、bindings crate 先全部 `publish = false`
3. 把 C++ backend 保留为 workspace / repo 内能力，而不是 crates.io 发布能力

如果 `mars-xlog` 的发布版仍然需要引用 `mars-xlog-sys`，那就和“Rust-only 发布”目标矛盾，必须先改拓扑。

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

补充说明：

1. 当前 workspace `repository` 仍指向 `https://github.com/Tencent/mars`
2. 发布前必须明确仓库身份，不能继续沿用上游仓库地址作为当前 crate 的发布仓库标识

### 5.3 P0: 收口发布包内容

需要为发布对象建立明确的 `include / exclude` 策略，至少回答：

1. 是否保留 benches
2. 是否保留 benchmark fixtures
3. 是否保留 benchmark-only examples
4. 是否保留内部诊断脚本相关资产

建议方向：

1. 默认只发布运行时代码、必要测试、README、LICENSE/NOTICE 和最小示例
2. benchmark 数据、fixture、大型 example 优先留在仓库，不进发布包

### 5.4 P1: API 与语义说明收口

发布前必须把外部使用者最容易误解的点写清楚：

1. sync / fatal 当前不是“每条日志立即 durable”
2. 当前 durability 边界
3. 当前 active semantic blocker 对使用约束的影响
4. `bench-internals` 不是公共稳定 API
5. `mars-xlog-core` 是否承诺稳定公共 API，还是仅作为 `mars-xlog` 的依赖实现

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
2. 再调整 `mars-xlog` 的发布依赖拓扑，确保它能作为 Rust-only crate 做 dry-run
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
