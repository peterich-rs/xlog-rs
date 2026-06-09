# Rust 发布计划

> 更新日期: 2026-03-16

## 1. 发布目标

当前发布目标分两阶段：

1. `Preview`
   - 发布 Rust-only crate
   - 明确已知语义边界和限制
2. `GA`
   - 发布面、支持矩阵、文档、测试和验证全部稳定
   - 不再依赖 C++ backend 作为发布语义背书

## 2. 当前结论

当前不再有 Rust 侧 active semantic blocker 卡住发布。

当前阻碍 `GA` 的不是未声明语义偏差，而是：

1. 发布顺序
2. 支持矩阵
3. 跨设备验证
4. 发布资料和节奏收口

## 3. 当前发布拓扑

建议公开发布：

1. `mars-xlog-core`
2. `mars-xlog`

建议保持 workspace-only：

1. `mars-xlog-sys`
2. bindings crates

当前 crates.io 发布顺序必须是：

1. 先发 `mars-xlog-core`
2. 等 index 可见
3. 再发 `mars-xlog`

## 4. 进入 Preview 前必须满足

1. `cargo package --list` 与 `cargo publish --dry-run` 可通过
2. README、crate docs、feature flags、known limitations 写清楚
3. 基础测试与回归门禁可通过

## 5. 进入 GA 前必须满足

1. 语义护栏持续稳定
2. 支持矩阵定稿
3. 跨设备验证形成固定节奏
4. 版本策略、变更日志、升级指南稳定

发布流程细节见 [rust_release_process.md](rust_release_process.md)。
