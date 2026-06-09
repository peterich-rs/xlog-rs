# Rust 与 CLI 发布流程

本文定义 Rust crate、CLI 二进制、Homebrew cask 与 npm 包的发布流程。

发布链路拆成四个可独立执行的工作流：

1. `rust-crates-release`：发布 `mars-xlog-core`、`mars-xlog`、`mars-xlog-cli` 到 crates.io
2. `cli-binary-release`：构建 macOS/Linux/Windows 的 `mars-xlog` 二进制并上传 GitHub Release assets
3. `brew-cask-release`：校验并上传 Homebrew cask，cask 使用 `cli-binary-release` 产出的 macOS assets
4. `npm-release`：发布 npm 包 `mars-xlog-cli`，安装时下载 GitHub Release 里的预编译 CLI

`full-release` 是全量编排入口，会按顺序组合这些发布段。

## 1. 版本号策略

当前 Rust crate、CLI 二进制、Homebrew cask 与 npm 包仍使用同一个 release version。

需要保持一致的位置：

1. `crates/xlog-core/Cargo.toml`
2. `crates/xlog/Cargo.toml`
3. `crates/xlog-cli/Cargo.toml`
4. `packages/mars-xlog-cli-npm/package.json`
5. `Casks/mars-xlog.rb`
6. workspace 内对 `mars-xlog-core` / `mars-xlog` 的 path 依赖版本

使用统一版本的原因是：`mars-xlog-cli` 依赖 `mars-xlog-core` 的 decoder API。若 CLI 发布到 crates.io，必须确保它依赖的 core 版本已经包含对应 API。

版本更新命令：

```bash
scripts/xlog/set_rust_release_version.sh <version>
```

tag 校验命令：

```bash
scripts/xlog/check_rust_release_tag.sh --tag v<version>
```

## 2. Tag 规则

正式 release tag 使用：

```text
v<version>
```

示例：

```text
v0.1.0-preview.3
v0.1.0
```

要求：

1. tag 必须指向 `main` 上已经合入的 release commit
2. tag 中的版本号必须与所有 manifest、npm package 和 cask 版本一致
3. 不要把同一个版本号重新打到不同 commit 上

创建方式：

```bash
git tag -a v0.1.0 -m "Rust GA 0.1.0"
git push origin v0.1.0
```

## 3. 工作流入口

### 3.1 full-release

文件：

1. `.github/workflows/rust_release.yml`

触发方式：

1. 手动 `workflow_dispatch`，传入 `tag_name`

执行内容：

1. 等待 tag commit 对应的 `main` push `rust-ci` 成功
2. 调用 `rust-crates-release`
3. 调用 `cli-binary-release`
4. 调用 `brew-cask-release`
5. 调用 `npm-release`

这是正常全量发版入口。push tag 不会自动触发该工作流，避免只想发布 brew/npm 时误触发 crates.io 全量发布。

### 3.2 rust-crates-release

文件：

1. `.github/workflows/rust_crates_release.yml`

触发方式：

1. 被 `full-release` 调用
2. 手动 `workflow_dispatch`，传入 `tag_name`

发布顺序：

1. 运行 `scripts/xlog/check_mars_xlog_core_release.sh`
2. 若 `mars-xlog-core` 当前版本尚未发布，则发布 `mars-xlog-core`
3. 等待 `mars-xlog-core` 在 crates.io 可见
4. 运行 `scripts/xlog/check_mars_xlog_release.sh`
5. 若 `mars-xlog` 当前版本尚未发布，则发布 `mars-xlog`
6. 等待 `mars-xlog` 在 crates.io 可见
7. 运行 `scripts/xlog/check_mars_xlog_cli_release.sh`
8. 若 `mars-xlog-cli` 当前版本尚未发布，则发布 `mars-xlog-cli`

需要 secret：

1. `CARGO_REGISTRY_TOKEN`

### 3.3 cli-binary-release

文件：

1. `.github/workflows/cli_binary_release.yml`

触发方式：

1. 被 `full-release` 调用
2. 手动 `workflow_dispatch`，传入 `tag_name`

执行内容：

1. 校验 tag
2. 构建并打包下面四个平台：
   - `aarch64-apple-darwin`
   - `x86_64-apple-darwin`
   - `x86_64-unknown-linux-gnu`
   - `x86_64-pc-windows-msvc`
3. 创建或更新 GitHub Release
4. 上传 `mars-xlog-v<version>-<target>.tar.gz` 与 `.sha256`

Homebrew cask 和 npm 包都依赖该工作流产出的 assets。

如果只想先产出可下载 CLI 二进制，可以只运行这个工作流，不需要发布 crates.io、brew cask 或 npm。

### 3.4 brew-cask-release

文件：

1. `.github/workflows/brew_cask_release.yml`

触发方式：

1. 被 `full-release` 调用
2. 手动 `workflow_dispatch`，传入 `tag_name`

执行内容：

1. 校验 `Casks/mars-xlog.rb` 语法和版本
2. 确认 GitHub Release 中已经存在 macOS CLI assets
3. 上传 `Casks/mars-xlog.rb` 到 GitHub Release

如果只想提供 brew 安装能力，需要先运行 `cli-binary-release`，再运行这个工作流。不需要发布 crates.io 或 npm。

### 3.5 npm-release

文件：

1. `.github/workflows/npm_release.yml`

触发方式：

1. 被 `full-release` 调用
2. 手动 `workflow_dispatch`，传入 `tag_name`

执行内容：

1. 校验 GitHub Release 中已经存在 CLI assets
2. `npm pack`
3. 若 npm 当前版本尚未发布，则发布 `mars-xlog-cli`

需要 secret：

1. `NPM_TOKEN`

npm 包安装时会下载 GitHub Release 里的 CLI 二进制，因此必须先完成 `cli-binary-release`。

## 4. 推荐发版流程

### 4.1 全量发布

```bash
git checkout main
git pull origin main

scripts/xlog/set_rust_release_version.sh 0.1.0

scripts/xlog/check_rust_release_tag.sh --tag v0.1.0
scripts/xlog/check_mars_xlog_core_release.sh
scripts/xlog/check_mars_xlog_release.sh --skip-crates-io-check
scripts/xlog/check_mars_xlog_cli_release.sh --skip-crates-io-check
npm pack --dry-run --json ./packages/mars-xlog-cli-npm

git add .
git commit -m "Prepare Rust 0.1.0 release"
git push origin main
```

等 `main` 上 `rust-ci` 通过后：

```bash
git tag -a v0.1.0 -m "Rust GA 0.1.0"
git push origin v0.1.0
```

然后手动运行：

```text
full-release
tag_name=v0.1.0
```

### 4.2 只发布 brew 可用的 CLI 二进制和 cask

前提仍然需要 main 上存在正确版本号和 tag。

先手动运行：

```text
cli-binary-release
tag_name=v<version>
```

再手动运行：

```text
brew-cask-release
tag_name=v<version>
```

完成后 GitHub Release 会包含 macOS assets，`Casks/mars-xlog.rb` 可以安装：

```bash
brew install --cask ./Casks/mars-xlog.rb
```

### 4.3 只发布 npm

前提：

1. `cli-binary-release` 已经完成
2. GitHub Release 中已经有所有平台的 CLI assets
3. `NPM_TOKEN` 已配置

手动运行：

```text
npm-release
tag_name=v<version>
```

## 5. 失败处理

各工作流按版本幂等：

1. crates.io 上已存在的 crate 版本会跳过发布
2. npm 上已存在的包版本会跳过发布
3. GitHub Release assets 可以通过重跑 `cli-binary-release` 重新上传
4. cask 可以通过重跑 `brew-cask-release` 重新上传

如果 release commit 需要变化，不要复用旧 tag 和旧版本号；修新 commit、递增版本、重新打 tag。

## 6. 必要 secrets

全量发布需要：

1. `CARGO_REGISTRY_TOKEN`
2. `NPM_TOKEN`

只运行 `cli-binary-release` / `brew-cask-release` 不需要 crates.io 或 npm secret。
