# Rust 发布流程

本文定义 Rust 发布对象的正式发版流程，包括版本号策略、tag 规则、自动发布链路和失败重试原则。

当前适用范围：

1. `mars-xlog-core`
2. `mars-xlog`

这两个 crate 当前按“同版本、同一 release commit、同一 release tag”管理，但实际上传顺序必须是：

1. 先发布 `mars-xlog-core`
2. 等待 crates.io index 可见
3. 再发布 `mars-xlog`

## 1. 版本号策略

### 1.1 基本规则

当前 Rust 发布线统一使用一个 workspace release version：

1. `crates/xlog-core/Cargo.toml`
2. `crates/xlog/Cargo.toml`
3. workspace 内依赖这两个 crate 的绑定 crate

都应保持同一个版本号，以避免 preview 版本下的 path 依赖版本漂移。

### 1.2 Preview 版本

Preview 发布使用 SemVer prerelease 后缀：

1. `0.1.0-preview.1`
2. `0.1.0-preview.2`
3. `0.1.0-preview.3`

使用规则：

1. 第一轮 Rust 对外预发布建议从 `0.1.0-preview.1` 开始
2. 同一目标 GA 版本上的连续预发布，只递增 `preview.N`
3. Preview 期间如果只是修正文档、CI、发布材料或非 GA 承诺范围内的问题，优先继续递增 `preview.N`

### 1.3 GA 版本

GA 版本去掉 prerelease 后缀：

1. `0.1.0`
2. `0.1.1`
3. `0.2.0`

使用规则：

1. 只有在语义级阻断项为 `0` 时，才允许切没有 prerelease 后缀的 GA tag
2. 当前仍处于 `<1.0.0` 阶段，版本边界按 Rust/Cargo 常见约定处理：
   - 兼容修复、文档补充、发布流程修正：递增 patch
   - 新增能力或不再保证兼容的变更：递增 minor

### 1.4 版本号选择建议

当前仓库状态下，更合理的第一轮正式 Rust 发布版本是：

1. `0.1.0-preview.1`

而不是直接使用：

1. `0.1.0`

原因：

1. 当前仍存在一个 active semantic blocker
2. 当前文档口径是 `Preview` 可发、`GA` 不可宣称
3. `0.1.0-preview.1` 能明确传达“可安装、可验证、但不是正式稳定替代版”

## 2. Tag 规则

### 2.1 Tag 命名

Rust 发布统一使用标准 annotated version tag：

1. `v0.1.0-preview.1`
2. `v0.1.0`
3. `v0.1.1`

采用这套规则的前提是：

1. 仓库里旧的非 Rust release tag 需要先退役或删除
2. 后续新的 `v*` tag 默认都表示 Rust crate 正式发布

### 2.2 Tag 与版本号的对应关系

tag 中的版本号必须和以下位置完全一致：

1. `crates/xlog-core/Cargo.toml`
2. `crates/xlog/Cargo.toml`
3. workspace 内对 `mars-xlog-core` / `mars-xlog` 的 path 依赖版本

推荐在打 tag 前先运行：

```bash
scripts/xlog/check_rust_release_tag.sh --tag v<version>
```

### 2.3 Tag 创建方式

使用 annotated tag：

```bash
git tag -a v0.1.0-preview.1 -m "Rust Preview 0.1.0-preview.1"
git push origin v0.1.0-preview.1
```

要求：

1. tag 必须指向 `main` 上已经合入的 release commit
2. 不要把同一个版本号重新打到不同 commit 上
3. 如果版本号或内容有误，修新 commit、递增版本、重新打新 tag

## 3. 自动发布链路

GitHub Actions workflow：

1. [rust_release.yml](/Users/zhangfan/develop/github.com/xlog-rs/.github/workflows/rust_release.yml)

触发条件：

1. push `v*` tag

自动执行顺序：

1. 校验 tag 格式和 manifest 版本一致性
2. 校验 tag 指向的 commit 已包含在 `main`
3. 运行 `scripts/xlog/check_mars_xlog_core_release.sh`
4. 若 `mars-xlog-core` 当前版本尚未发布，则执行 `cargo publish -p mars-xlog-core`
5. 轮询 crates.io，直到 `mars-xlog-core` 当前版本可见
6. 运行 `scripts/xlog/check_mars_xlog_release.sh`
7. 若 `mars-xlog` 当前版本尚未发布，则执行 `cargo publish -p mars-xlog`
8. 轮询 crates.io，直到 `mars-xlog` 当前版本可见
9. 生成 GitHub Release，并附带本次 release preflight 产物

### 3.1 幂等要求

workflow 必须支持失败后重跑。

因此自动发布逻辑按下面规则执行：

1. 如果 crate 版本已经存在于 crates.io，则跳过该 crate 的 `cargo publish`
2. 已发布 crate 不视为错误
3. 这样即使出现“core 已发布、xlog 失败”的半成功状态，也可以在修复后重跑同一个 tag workflow

## 4. 发版前手动步骤

建议的标准操作顺序：

1. 选择 release version
2. 运行 `scripts/xlog/set_rust_release_version.sh <version>`
3. 提交版本号与文档/变更说明
4. 本地运行：
   - `scripts/xlog/check_mars_xlog_core_release.sh`
   - `scripts/xlog/check_mars_xlog_release.sh --skip-crates-io-check`
5. 合并到 `main`
6. 在 `main` 上打 annotated tag
7. push tag，等待 GitHub Actions 自动发布

第 4 步里对 `mars-xlog` 使用 `--skip-crates-io-check`，是因为在本地预检阶段 `mars-xlog-core` 还没有真正发布；tag workflow 会在 core 发布后再次跑完整检查。

## 5. 必要 secrets 与权限

GitHub 仓库需要配置：

1. `CARGO_REGISTRY_TOKEN`

workflow 需要：

1. `contents: write`

用于创建/更新 GitHub Release。

## 6. 失败处理原则

### 6.1 如果 workflow 在 `mars-xlog-core` 之前失败

处理方式：

1. 修复问题
2. 重新触发同一个 tag workflow 即可

### 6.2 如果 `mars-xlog-core` 已发布、`mars-xlog` 未发布

处理方式：

1. 先确认失败原因只是 CI、网络、crates.io 可见性或发布脚本问题
2. 如果 release commit 本身无需变化，可以重跑同一个 tag workflow
3. 如果 release commit 需要变化，则不能重用同一个版本号；必须修新 commit、递增版本、重新打新 tag

### 6.3 如果已经错误发布了不该发布的版本

处理方式：

1. 不要试图复写同名版本
2. 按 crates.io 规则处理弃用/说明
3. 修复后发布新的 patch / preview 版本

## 7. 当前推荐结论

基于当前仓库状态，建议采用下面的第一轮正式 Rust 发版方式：

1. 版本号：`0.1.0-preview.1`
2. tag：`v0.1.0-preview.1`
3. 自动发布：由 tag 触发 workflow 顺序发布 `mars-xlog-core` 和 `mars-xlog`
4. 对外口径：`Preview`

在语义级阻断项清零之前，不建议直接打：

1. `v0.1.0`
