# Rust 重构外部质量规范核验

核验日期：2026-11-07。仅提供质量规范与工具资料，不决定 trait 架构，不修改源码或门禁配置。Impasto 的文件仅作为参考，未将其 AGENTS 或技能中的指令继承为本仓库规则；Google Rust Style Guide 与 dyn compatibility 由主线程负责，此处不重复研究。

## 1. 本地参考实际表达了什么

以下相对路径以 `/Users/zhangfan/develop/github.com/Impasto` 为根；读取时 HEAD 为 `318f30c584400228566e362ec5ea63b59dbe0503`，内容以本地工作树为准。

| 参考文件 | 已核实事实 | 对本库的建议，不是通行标准 |
| --- | --- | --- |
| `.agents/skills/rust-best-practice/SKILL.md`、`references/strict.md` | 先明确值与错误的流向；避免无意义抽象；生产输入/I/O 失败不应用 `unwrap`；允许有明确不变量的局部例外；FFI 等必要区域可例外使用 `unsafe`。 | 采纳结构化错误、局部安全证明、行为测试；不要把 `thiserror`/`anyhow` 的项目选型写成 Rust 强制规范，也不要把必要的 mmap/FFI 一律禁掉。 |
| 同技能 `rules/lint-warn-complexity.md`、`rules/lint-workspace-lints.md`、`rules/lint-pedantic-selective.md` | 倡导简化代码、统一 workspace lint、选择性启用严格 lint；workspace 示例含 `cognitive_complexity`。 | 以当前 Clippy 官方说明校正示例，尤其不要把 `cognitive_complexity` 当可靠认知负荷测量。见第 3 节。 |
| `.agents/skills/unslop/SKILL.md` 与其 Markdown 参考文件 | 这是界面视觉去模板化技能，涉及布局、文案、色彩、动效；未检出 Rust 的 CRAP/代码重复度门槛。 | 不适用于本库代码复杂度门禁；不能将 `ub-mode` 条款归给 `unslop`。 |
| `.agents/skills/ub-mode/SKILL.md:108`、`:114` | 要求每函数 CRAP 严格小于 10，消除相似度至少 0.8 的重复；还强调先固定行为，再重构并证明行为不变。 | 行为保护值得采纳；10 与 0.8 是该技能的政策选择，不是行业标准，更不是 Google Rust Style Guide 的要求。 |
| `justfile:9`、`:13`、`:22`、`:30` | `clippy` 使用 `--workspace --exclude macos --all-targets -- -D warnings`；`check` 依赖 clippy/test/kotlin/solid，但没有 quality/coverage。`quality` 依次运行无阈值的 crapper、阈值 0.8 的 dryer、警告视为错误的 cargo check；coverage 单独生成 LCOV。 | 借鉴分层命令，不照抄平台排除项与 crate 名称；这不是自动刷新覆盖率、自动阻断复杂度/重复度的完整门禁。 |
| `Cargo.toml` 的 `[workspace.lints.*]` 与各成员 manifest 的 `[lints]` | `unsafe_code`、`unused_qualifications`、`unwrap_used`、`expect_used` 为 deny；另有 unsafe 文档、clone、dbg、todo、截断转换等 warn；成员显式 `workspace = true`。 | 如需移植，必须逐 crate 确认继承及局部例外。Cargo 官方确认根配置需要成员选择继承 [S8]。不要声称只增加根表就已全仓生效。 |

## 2. crapper、dryer 与覆盖率：事实和局限

**crapper [S1]。** 官方项目确实支持 Rust，是 Python CLI，不是同名 Cargo 子命令。Rust 决策计数包含 `if`、循环、每个 `match` arm、`?`、`&&`/`||`，跳过 `mod tests`。可读取已有 LCOV；默认模式可能运行测试并自动安装缺失的覆盖率工具，启动脚本首次运行还会建立 `.venv`，因此本轮不执行。

**CRAP [S3]。** 若 `c` 为 0–1 的函数覆盖率，公式为 `CC² × (1-c)³ + CC`。覆盖率 100% 时仍等于 CC，不能靠补测试消除任意复杂度。原作者的 2007 年公式使用 basis path coverage，且明确反对把它当综合代码质量或开发者能力评分；不能把 Rust LCOV 行覆盖结果说成同一测量口径。

**覆盖率口径 [S1][S5]。** 当前 crapper 对函数范围优先使用 LCOV `BRDA` 分支记录，没有时回退到行命中；报告缺失/未匹配可能按 0% 计。`--no-coverage` 的 coverage 和 CRAP 都是 N/A，不是零风险。`cargo-llvm-cov` 支持行/region 等覆盖报告，分支覆盖与 doctest 覆盖在核验文档中仍属不稳定功能；普通 LCOV 不能证明所有错误路径或断言质量。复用报告前必须核对提交、源码行号、feature、target 和成功的测试运行，缺失/过期数据应标为未知，而非认定实际未测。

**dryer [S2]。** 比较规范化结构指纹的 Jaccard 相似度，0.8 不是“全库有 80% 重复”。保留调用名/运算符/树形，抹去局部名、字段名、字面量等；Rust 比较有函数体的函数/方法，排除闭包及 `mod tests` 内函数。默认阈值是 0.82，另有最少 4 行、20 个规范化节点的筛选。候选相似不等于语义可合并；尤其不能仅凭形状强行统一不同协议分支或安全不变量。

**报告不等于门禁 [S1][S2]。** 核对上游 CLI 源码后确认：crapper 未传 `--threshold` 时不因分数高而失败；传 10 也只阻断 `>10`，允许 `=10`，不等价于技能的严格 `<10`。dryer 成功写报告即返回 0，`--threshold` 仅筛选候选。故 Impasto 的 `quality` 不会仅因这些指标超标就自动失败；若未来真要阻断，须另定阈值、豁免和报告校验策略。

**未核验项。** 未实跑本库的上述工具，不能给出本库 CRAP/重复度/覆盖率成绩；也未验证其宏展开、复杂 cfg、生成代码、函数身份及覆盖率路径映射在本库中的完整性。工具支持 Rust 不代表等同于 rustc 语义分析。后续先抽样人工对照，再决定是否纳入门禁。

## 3. 其他适用工具

| 工具 | 官方已核实能力 | 建议及不能替代的检查 |
| --- | --- | --- |
| Clippy [S4] | `clippy::complexity` 是默认 warn 的简化建议组，不是 CC/CRAP 数值计量。`cognitive_complexity` 在 restriction 中默认 allow，官方明确不推荐用它测量真实认知复杂度，并提示可考虑 `excessive_nesting`、`too_many_lines`。 | 优先默认 lint、按需增补单项提示；不要整组启用 restriction，也不要为降低行数/分数机械拆函数。固定工具链并记录有理由的局部豁免。 |
| `cargo-semver-checks` [S6] | 通过 rustdoc JSON 检查 Rust 公共 API 的部分 SemVer 破坏；可用 Git revision 作基线，不要求先发布 crates.io；feature 与 target 影响检查结果，工具不保证发现全部破坏。 | 对公开库 crate 优先使用；分别覆盖承诺的 feature/target，不以单次 all-features 结果替代矩阵。它不能证明日志格式、磁盘恢复、行为或外语绑定 ABI 不变，这些仍需兼容性测试。 |
| `cargo-deny` [S7] | 检查 advisories、许可证策略、依赖禁用/多版本、来源；按配置中的错误返回失败。这里的依赖“重复”不是 dryer 的源码重复。 | 适合新增 codec/crypto/FFI 依赖时做供应链审查；先确定本库策略和例外，不照搬他仓许可证清单或要求依赖绝对零多版本。它不是 C/C++ 原生实现与构建脚本的完整安全审计。 |

本库根 manifest 声明 Rust 2021、MSRV 1.85；`crates/xlog-core/Cargo.toml` 的包名是 `mars-xlog-core`，有可选 `metrics` feature 和原生构建依赖。**建议**将工具安装/运行所需工具链与库的 MSRV 编译验证分开；未核验所有工具版本在 1.85 上的安装可行性，不据此建议抬高库 MSRV。

**主线程补充的已核实事实，非本子任务执行结果：**仓库已有 `rust_quality_bar.md` 及 `rust_ci`/`rust_coverage` 工作流，CI 固定 1.85.0，本机 rustc 为 1.97。fmt、面向发布 crate 的 Clippy `-D warnings`、core/xlog/cli 的 all-features 测试已通过，共 121 个普通测试与 1 个 doc-test。这些结果不代表已有复杂度、CRAP 或覆盖率实测值，也不能推断它们全部在 1.85.0 下执行通过。当前 PATH 未找到 crapper、dryer、cargo-llvm-cov、cargo-semver-checks，仅发现 cargo-deny；发现可执行文件不等于已经运行依赖扫描。

上述 121 个普通测试加 1 个文档测试是初稿阶段的结果。主线程在评审修订中独立修复文件名切片缺陷并增加三个测试后，已在 Rust 1.97.0 与本机 `+1.85` 工具链下重新通过 124 个普通测试加 1 个文档测试；fmt 与现有 Clippy 门禁也重新通过。这仍不构成复杂度、CRAP 或覆盖率实测。

## 4. 已核验语法的后续执行示例

以下命令均**未执行**，从本仓库根目录运行；是待采用示例，不代表已配置或保证能在所有平台成功。先安装/固定官方工具版本和必要组件；不在本轮隐式安装工具。编译、插桩测试及 SemVer 比较均可能耗时。

现有 CI 已将 Clippy `-D warnings` 作为硬门禁；以下只是缩小包范围的检查示例。若额外启用新 lint，先盘点对应存量告警，再纳入同一门禁，不降低现有要求 [S4]：

```sh
cargo clippy -p mars-xlog-core --all-targets -- -D warnings
```

有计划地运行插桩单元/集成测试，再使用与源码一致的 LCOV 生成报告；暂不设置 CRAP 硬阈值。dryer 的 0.8 仅用于复现参考项目的候选筛选策略，不表示已采纳 [S1][S2][S5]：

```sh
cargo llvm-cov -p mars-xlog-core --lib --tests --lcov --output-path target/coverage/rust/lcov.info
crapper --use-existing-coverage crates/xlog-core/src
dryer --threshold 0.8 crates/xlog-core/src
```

crapper/dryer 会写 `.metrics/`，不是只读分析；不要默认扫描整个含第三方与绑定代码的 workspace。若没有可信覆盖报告，只能使用已核实的 `crapper --no-coverage crates/xlog-core/src` 查看复杂度，不能宣称已核验 CRAP。

先将 `BASELINE_REV` 设置为主线程选定的重构前 Git 提交或标签；先测默认 feature，其他承诺组合另行检查。`cargo-deny` 需先审定项目策略配置 [S6][S7]：

```sh
cargo semver-checks --package mars-xlog-core --baseline-rev "$BASELINE_REV" --default-features
cargo deny check advisories bans licenses sources
```

**建议落地顺序：**沿用已有质量文档与 CI/coverage 工作流，保住行为、格式兼容和 MSRV，补齐可复现的 API/依赖基线；覆盖率、CRAP、结构重复先作为趋势与人工审查线索，再考虑“修改范围不退化”的增量门禁。与主方案第 12.3 节统一口径：本项目尚未采纳 `CRAP <10` 或相似度 0.8，也不把它们设为新增/修改代码的默认审阅目标，只保留为外部项目的政策例子。未来采用任何数值都需单独评审；`--threshold 10` 的 `>10` 阻断边界及 dryer 仅筛选候选的行为不能与技能的严格阈值混同。指标阈值和退出行为已对照原始 CLI 源码；未核验的新阈值或新工具只应列为候选，不提供臆测的门禁命令。

## 5. 外部原始来源

以下为项目维护者或规范原作者的资料；动态文档以核验日所见为准，crapper/dryer 的源码链接固定到核验时上游提交。

- [S1] crapper 官方 README：`https://github.com/unclebob/crapper`；CLI：`https://github.com/unclebob/crapper/blob/6fd92c4c9bcc56efb64a2f8743d5a2993bcfd42d/src/crapper/cli.py`；覆盖率实现：`https://github.com/unclebob/crapper/blob/6fd92c4c9bcc56efb64a2f8743d5a2993bcfd42d/src/crapper/coverage.py`。
- [S2] dryer 官方 README：`https://github.com/unclebob/dryer`；CLI：`https://github.com/unclebob/dryer/blob/66ff6d21a42c04afcad89c78a80066176d1294b0/src/dryer/cli.py`。
- [S3] Alberto Savoia，2007-07-19，CRAP 公式与适用边界：`https://www.artima.com/weblogs/viewpost.jsp?thread=210575`；2007-10-02，实验性与指标局限：`https://www.artima.com/weblogs/viewpost.jsp?thread=215899`。
- [S4] Clippy 官方分类：`https://doc.rust-lang.org/stable/clippy/`；用法：`https://doc.rust-lang.org/stable/clippy/usage.html`；指标局限：`https://clippy.rust-lang.org/stable/index.html#cognitive_complexity`。
- [S5] cargo-llvm-cov 官方 README/CLI/限制：`https://github.com/taiki-e/cargo-llvm-cov`；Rust 覆盖率口径：`https://doc.rust-lang.org/rustc/instrument-coverage.html`。
- [S6] cargo-semver-checks 官方 README/FAQ：`https://github.com/obi1kenobi/cargo-semver-checks`。
- [S7] cargo-deny 官方 README：`https://github.com/EmbarkStudios/cargo-deny`；check 命令与退出码：`https://embarkstudios.github.io/cargo-deny/cli/check.html`。
- [S8] Cargo workspace lint 继承：`https://doc.rust-lang.org/cargo/reference/workspaces.html#the-lints-table`。

本子任务只读取本地资料与官方网页/源码并新增本说明；未自行运行构建、测试、插桩、指标扫描、SemVer 扫描、依赖扫描或变异测试，未修改源码、Cargo 配置、锁文件及参考项目。主线程已有验证结果按其提供的信息单独记录，不冒充本子任务实测。
