# Rust 维护门槛

本文定义 `mars-xlog-core` 与 `mars-xlog` 的长期维护门槛。

适用范围：

1. `crates/xlog-core`
2. `crates/xlog`

目标：

1. 把常规维护动作标准化到 workflow
2. 在 PR 和 `main` 上持续验证格式、静态检查、构建、测试和发布前检查
3. 让覆盖率保持在较高水平，并对新增行为持续补测试

## 1. 硬门槛

下面这些检查应视为长期硬门槛：

1. `cargo fmt --all --check`
2. `cargo clippy -p mars-xlog-core -p mars-xlog --all-targets --all-features --locked -- -D warnings`
3. `cargo test -p mars-xlog-core -p mars-xlog --all-features --locked`
4. `cargo check` 的关键 feature / target 组合
5. release preflight 脚本可执行且结果符合预期

对应 workflow：

1. [rust_ci.yml](/Users/zhangfan/develop/github.com/xlog-rs/.github/workflows/rust_ci.yml)
2. [rust_coverage.yml](/Users/zhangfan/develop/github.com/xlog-rs/.github/workflows/rust_coverage.yml)

## 2. 平台要求

常规维护至少覆盖：

1. Linux
2. macOS
3. Windows

当前策略：

1. Linux 负责 `fmt`、`clippy`、feature 组合 `cargo check`、release preflight
2. Linux / macOS / Windows 都负责 `cargo test`
3. 覆盖率采集先固定在 Linux

## 3. Feature / 构建组合

需要长期维持的最低构建矩阵：

1. `mars-xlog-core`：`cargo check --all-targets`
2. `mars-xlog`：默认特性
3. `mars-xlog`：`--all-features`
4. `mars-xlog`：`--no-default-features --features rust-backend`

原因：

1. 默认 Rust 发布面必须持续可编译
2. `tracing` / `macros` 不能只靠偶然构建通过
3. release-facing crate 不能只在单一 feature 组合下有保障

## 4. 测试与覆盖率要求

覆盖率策略分两层：

1. 测试存在性要求
2. 覆盖率报告要求

### 4.1 测试存在性要求

每类变更至少满足下面要求：

1. 公共 API 变更：补充单元测试或文档测试
2. 协议 / 压缩 / 加密 / 恢复语义变更：补充回归测试
3. bugfix：必须有能复现旧问题并验证新行为的测试
4. 发布流程 / preflight 变更：至少补脚本级验证或 workflow 级验证

### 4.2 覆盖率要求

覆盖率 workflow 当前负责：

1. 生成 `mars-xlog-core` / `mars-xlog` 的 HTML coverage 报告
2. 上传覆盖率 artifacts，供 review 和后续阈值讨论使用

当前策略是：

1. 先把覆盖率采集标准化
2. 先要求新增逻辑必须伴随测试
3. 等 coverage baseline 稳定后，再决定是否引入固定百分比阈值

在 baseline 固定前，review 仍应以“新增行为是否被测试覆盖”作为直接门槛，而不是等待单一总覆盖率数字。

## 5. 发布相关要求

日常 CI 不直接发布 crate，但必须维持下面两点：

1. `scripts/xlog/check_mars_xlog_core_release.sh --skip-tests`
2. `scripts/xlog/check_mars_xlog_release.sh --skip-tests --skip-crates-io-check`

这保证：

1. 发布脚本不会在日常维护中腐坏
2. 包内容、文档构建、locked 依赖和本地 release 流程能持续被验证

## 6. 维护原则

后续如果要调整维护门槛，遵循下面原则：

1. 不降低已有硬门槛来迁就临时变更
2. 如需例外，优先缩小例外范围，而不是把整个检查降级
3. 先修代码或补测试，再讨论是否需要豁免
4. release-facing crate 的维护门槛应高于 workspace 其他辅助 crate
