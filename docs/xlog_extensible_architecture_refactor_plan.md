# xlog-rs 能力分层与可扩展架构重构方案

状态：方案已评审。首个实现切片已在 `feature/extensible-log-architecture` 落地，范围见第 15 节。方案正文仍是设计依据，不因部分实现而改写未完成阶段的约束。基于 2026-11-07、提交 `cdedbdf` 的工作区代码。

## 1. 结论与范围

建议把产品定位从“用 Rust 复刻 Mars Xlog”演进为“以 Mars 兼容实现为默认预设的可扩展日志库”。保留现有接入与磁盘兼容性，把真正需要由开发者替换的能力开放出来，而不是给每个模块都增加 trait。

核心决策：

1. **复用已有行格式扩展**：当前已经有 `WriteLine`、`Layout::Custom`，不是从零设计 formatter。
2. **开放文件内容格式、加密、文件命名三处 seam**：引入 `FileFormat`、`Encryptor`、`FileNaming`；文件格式另有独占可变状态的 encoder 会话接口。
3. **区分扩展名与内容协议**：把文件改名为 `.log` 不会使其变成文本；JSON Lines 也不能直接送入现在的 Mars buffer。
4. **兼容模式与扩展模式并行演进**：兼容模式保留 Mars 的 magic、头、压缩、TEA 尾块、mmap 与同步语义；新算法、新协议显式选择新模式，不冒充 Mars 兼容。
5. **先建立可验证的行为与错误契约，再拆代码**：恢复、滚动、清理、线程退出、配置复用都是扩展性的一部分。
6. **不同时重写所有运行时**：先以第二种真实实现证明 interface，再逐步复用底层文件写入与控制逻辑；不引入 Tokio，不修改 vendored Mars。

首轮覆盖本地追加式文件，包括 Mars、普通文本、JSON Lines、独立分段的自定义二进制格式。SQLite、网络上传、Parquet 等需要不同存储或随机写入语义的目标，不通过“换扩展名”宣称支持。

## 2. 现状与证据

以下是实际代码现状，不把旧方案中的目标当成已经实现的能力。

| 观察 | 当前证据 | 对重构的影响 |
| --- | --- | --- |
| 已有可借用的日志事件 | `crates/xlog/src/event.rs:9` 的 `Event<'a>` | 调用线程完成格式化，跨线程只转移拥有所有权的数据；不再增加一套相同事件模型 |
| 已有开放行格式 trait | `crates/xlog/src/layout.rs:44` 的 `WriteLine`，以及 `Layout::{Mars,Pieces,Custom}` | 继续兼容，不另造同义 trait；当前返回 `()`、输出 `String` 的限制需要明确 |
| 自定义行格式不改变磁盘容器 | `crates/xlog/src/lib.rs:817` 的 `custom_layout_replaces_plaintext_only` | 自定义 JSON 文本仍可被装进 Mars 文件，不能据此认为已有 JSONL 文件支持 |
| 加密实现仍然封闭 | `crates/xlog-core/src/crypto.rs:112` 的 `Crypt::{Off,Tea}` | TEA 是 Mars adapter 的实现，不应继续成为所有协议的核心领域类型 |
| 帧与加密细节耦合 | `crates/xlog-core/src/frame.rs:300` 的尾块回退与重写 | 新加密不能只替换一个 `encrypt(&[u8])` 调用；需要独立完整 segment 与元数据契约 |
| 引擎和持久化 buffer 理解 Mars 头 | `appender_engine.rs:413`、`buffer.rs:247`、`buffer.rs:281`、`buffer.rs:407`，路径均在 `crates/xlog-core/src/` | `HEADER_LEN`、长度回填、end hour、尾标记扫描不能直接用于任意文件内容 |
| 文件名和扩展名写死 | `crates/xlog-core/src/file_naming.rs:5`、`:41` | 命名、文件识别、索引恢复与列举都要调整，不是只替换 `format!` |
| 维护过程同样依赖扩展名 | `crates/xlog-core/src/file_maintenance.rs:45`、`:109` | 自定义后缀必须同步覆盖缓存迁移、保留期清理与归属判断 |
| 日期也参与滚动与路径缓存 | `crates/xlog-core/src/file_manager.rs:584` | 文件命名与轮转时机分开；换名字不应暗中改变按日切分 |
| facade 仍然承担很多运行时职责 | `crates/xlog/src/backend/rust.rs:65`、`:78`、`:671` | 其中包括缓冲池、前端队列、编码、flush、模式切换；只搬函数不能消除职责耦合 |
| 异步工作已不完全符合早期方案描述 | `crates/xlog/src/backend/rust.rs:1270` 格式化入队，`:671` 消费并编码 | 不照搬旧文档“压缩都在调用线程”的假设；需要重新绘制真实调用路径 |
| 配置复用不比较 layout | `crates/xlog/src/backend/rust.rs:1479` 与 `crates/xlog/src/lib.rs:834` | 当前同名实例保留首次 layout；新 trait 对象不能直接塞进 `PartialEq` 配置继续静默复用 |
| 错误在公共入口被压缩或丢弃 | `crates/xlog/src/backend/rust.rs:963`、`:1223`、`:1666` | 需要可观察的初始化、写入、flush 与退出结果，旧 `void` 风格入口留作适配 |
| 初始化存在兼容性降级 | `crates/xlog/src/backend/rust.rs:976`：公钥错误回退为不加密 | 新配置默认 fail closed；旧行为只能留在明确的兼容契约中 |

物理行数仅用于定位阅读热点，不等价于复杂度评分：`backend/rust.rs` 2370 行、`file_manager.rs` 1232 行、`appender_engine.rs` 1023 行，均包含测试。不能据此断言每个大文件都需要拆分，也不能编造当前 CRAP 或覆盖率数值。

P0 单独处理已有健壮性缺陷：`file_naming.rs:41` 原先在未校验长度和 UTF-8 边界时切片，短文件名及截点落在多字节字符内都会 panic。这不依赖 trait 重构，已用先失败的回归测试复现，并独立改为 `strip_prefix` 加受检 `get(8..)`；无效截点返回 `None`，保留既有合法名称和索引解析语义。日期合法性与索引规则的进一步收紧不混入本次修复。

## 3. 与已有方案及兼容红线的关系

`docs/xlog_core_rust_refactor.md` 明确限制为 Mars 行为不变，并以“调用方不能插入第三种压缩器”为前提不开放 trait。本次需求改变了这个产品前提，属于下一阶段的扩展架构，而不是对上一阶段的简单风格整理。

`docs/xlog_call_surface_refactor.md` 中的事件和行格式工作继续保留。`docs/rust_semantic_redlines.md` 的约束继续适用于兼容模式；新模式的格式与语义差异必须先有规范和测试，不能用“Rust 化”解释未声明的行为变化。

| 能力 | Mars 兼容模式 | 新的扩展模式 |
| --- | --- | --- |
| 默认入口 | 现有 `Xlog::init`、`init_with_layout`、FFI 接入保持可用 | 新的 builder 显式选择 |
| 行格式 | 默认 Mars；原有 `WriteLine` 仍可替换 payload 文本 | 复用 `WriteLine`，内置纯文本与 JSONL formatter |
| 内容协议 | Mars 头、magic、序号、压缩与尾标记不变 | 独立格式标识和版本，不复用 Mars magic 冒充兼容 |
| 加密 | 保留原 ECDH + TEA 语义，包括 sync payload 的历史行为 | 格式校验通过后使用自定义 `Encryptor`，支持认证加密元数据 |
| 命名 | 默认日期、分片、`.xlog` 和旧查询行为不变 | 自定义文件名与后缀，配套文件目录清单 |
| 恢复 | 保留 `.mmap3` 与 pending-block 修复，旧 oneshot 不变 | 独立版本 journal，恢复已编码 segment |
| 缓存迁移 | 保留既有字节拼接行为及删源前 durability barrier | 默认按完整文件迁移；没有格式拼接契约时禁止盲目合并 |
| 解码 | 现有 `decode_xlog`、官方 decoder 兼容性继续验证 | 内置格式可识别，自定义 decoder 需显式注册或由调用方提供 |

特别注意：当前测试 `sync_mode_with_pubkey_uses_crypt_magic_and_plain_payload` 证明 Mars sync 的 payload 保持明文。不得把“配置了公钥”描述为所有同步日志都已加密。新模式应让调度方式与加密策略正交。

文件名变化、行文本变化、内容协议变化分别具有不同的兼容级别。即使内容字节还是 Mars，更换名称或行布局也可能影响依赖默认命名或默认文本的外部工具，文档必须分别说明。

## 4. 目标分层

```text
调用入口：Rust handle / macros / tracing / JNI / UniFFI / N-API
                            |
                  过滤与 Event 元数据
                            |
                 WriteLine 文本格式化
                            |
                 有所有权的 RecordBytes
                            |
          调度与控制：同步 / 异步 / 背压 / flush fence
                    /                       \
       Mars 兼容编码路径                可扩展编码路径
       FrameWriter                    FileFormat + FormatEncoder
       ECDH + TEA                     压缩策略 + Encryptor
       pending edit                   完整 SealedSegment
       Mars mmap recovery             版本化 JournalReplay
                    \                       /
                文件存储与生命周期 module
        命名 / 轮转 / 追加 / 缓存迁移 / 清理 / 单写者锁
```

箭头表达职责依赖，不要求增加一层只做转发的对象。压缩与加密的具体顺序由格式契约确定；通用 runtime 不应强制所有协议接受同一种字节流水线。

### 4.1 保留封闭实现的位置

- Mars `BlockMagic`、合法头状态、TEA 尾块和 legacy decoder 继续用具体类型、enum、普通函数。
- 轮转规则首轮用具体 `RotationPolicy`，例如按时间、大小和组合规则；没有真实外部实现需求前不再增加 `RotationTrait`。
- 本地文件 IO 优先用 `std::io::Write`、`Path`、`PathBuf`；不要为 `write`、`flush` 分别复制一套同义 trait。[R2]
- `FileStore`、journal、clock、故障注入可以有私有 seam，但不自动成为公共承诺。
- 暂不创建每种能力一个 crate 的 workspace。先改善 module 的 interface，避免依赖与发布成本先于收益出现。

### 4.2 模块落点

| 位置 | 拥有的职责 | 不应再拥有的职责 |
| --- | --- | --- |
| `mars-xlog` 的 event/layout | 借用事件、级别过滤、文本序列化、兼容调用入口 | Mars header、恢复扫描、文件索引算法 |
| `mars-xlog-core` 的 format | 格式描述、encoder 会话、完整分段输出、格式合法组合 | 路径选择、目录删除、工作线程 |
| core 的 crypto | 开放 `Encryptor` 契约和错误；具体算法 adapter | 全局单例、文件命名、业务事件 |
| core 的 runtime | 命令顺序、背压、flush fence、状态切换、关闭 | 手工拼 magic、判断 `.xlog` 后缀 |
| core 的 storage | FileStore、journal、catalog、轮转和缓存策略 | TEA、zlib、日志字段 |
| core 的 compat/mars | 现有 frame/buffer/protocol/recovery 的兼容实现 | 其他格式的默认策略 |
| CLI 与各语言 binding | 参数转换、错误映射、受支持预设 | 重新实现编码、恢复或命名规则 |

这是逻辑归属，不是要求立即搬目录。已有 `pub mod` 和 re-export 属于下游可能依赖的接口，移动前建立兼容 re-export 与弃用周期；不能因为 crate 名叫 core 就任意破坏公共路径。

## 5. 行格式：复用 WriteLine，而不是重新发明

当前 interface 已经能让用户替换文本布局：

```rust
pub trait WriteLine: Send + Sync {
    fn write_line(&self, out: &mut String, event: &Event<'_>);
}
```

保留 `Layout::Mars`、`Layout::Pieces` 和 `Layout::Custom`。首轮补齐以下契约与 adapter：

1. formatter 在调用线程执行，允许并发调用；不得保存 `Event` 的借用，不得假定所有调用都来自同一线程。
2. 返回后输出必须是该次记录的完整文本；换行、长度限制由具体 formatter 定义。默认 Mars 截断规则不强加给自定义布局。
3. 内置 `PlainTextLine` 与 `JsonLine` 作为第二、第三种实际实现。JSONL 必须正确转义消息、tag、文件名与换行，不能靠字符串拼接假装序列化。
4. 格式化完成后，复用并转移拥有所有权的 `String` 或 bytes buffer；不把借用事件、`fmt::Arguments` 直接跨线程发送。
5. 明确禁止回调向同一 logger 递归写日志；如提供防护，采用每线程重入标记和独立错误通道，不在错误回调中再次调用主日志路径。

现有 trait 无 `Result`，不能在兼容发行中直接修改签名。第一阶段继续支持文本；需要任意二进制记录或可恢复的序列化错误时，在下一次明确的接口版本中增加 fallible `RecordEncoder`，由 adapter 包装 `WriteLine`。不要同时公开 `Formatter`、`LayoutWriter`、`LineEncoder` 等多套职责相同的名字。

**已接受的首轮缺口：**P4 的 formatter 错误处理上限就是 `WriteLine` 的上限。它不能向 `try_write_event` 返回用户 formatter 的可恢复错误，也不能靠包装成 `Ok(())`、输出空字符串、错误计数或捕获 panic 补出这一契约。若产品要求首轮支持可失败的用户序列化或字段转换，必须将 fallible `RecordEncoder` 提前，而不是宣称已有完整错误传播。

这里也要区分真实输入约束：`Event` 的消息、tag 等是 `&str`，安全 Rust 中这些值已经满足合法 UTF-8 不变量，控制字符、引号、反斜杠和合法 Unicode 都应正常转义，不属于“非法 UTF-8 导致 JSON 转义失败”。未验证的 bytes 与 FFI 输入应在构造 `Event` 前按明确策略拒绝或转换，不能伪造无效 `&str`。[R11]

P4 内置 `JsonLine` 仅处理固定 schema 的文本和数值字段，不接收用户自定义 `Serialize`、可失败的富化回调或 IO；时间转换等也必须有对所有合法输入明确的表示策略。通用 JSON 序列化接口仍可能因自定义 `Serialize` 或不支持的 map key 失败，不能由此推导任意序列化都不可失败。[R12] 如果所选实现对首轮支持的合法输入仍可能发生可恢复失败，P4 不得用 `unwrap`/panic 或丢记录绕过，必须缩小并声明支持范围，或提前引入 fallible interface。内存耗尽也不作为当前 `WriteLine` 能返回的普通错误来承诺。

真实 JSONL 文件的配置应是 `JsonLine + JsonLinesFormat`。`JsonLine + Mars preset` 仍表示 Mars 容器里的 JSON 文本。

## 6. 文件格式：抽象文件内容，而不是只有扩展名

### 6.1 第一版扩展契约

下面是 interface 草案，不是已经存在、可直接复制执行的代码。领域类型及错误类型需要在实现阶段补齐并经契约测试确定。

```rust
pub trait FileFormat: Send {
    fn descriptor(&self) -> FormatDescriptor;

    fn start_file(
        &mut self,
        context: &FileContext,
    ) -> Result<Box<dyn FormatEncoder>, FormatError>;
}

pub trait FormatEncoder: Send {
    fn push_record(&mut self, record: &[u8]) -> Result<(), FormatError>;

    fn seal_segment(
        &mut self,
        encryptor: &mut dyn Encryptor,
        output: &mut Vec<u8>,
    ) -> Result<Option<SegmentInfo>, FormatError>;

    fn finish_file(
        &mut self,
        encryptor: &mut dyn Encryptor,
        output: &mut Vec<u8>,
    ) -> Result<Option<SegmentInfo>, FormatError>;
}
```

`FileFormat` 是创建会话的 seam；`FormatEncoder` 代表一个文件的可变编码状态。两者不负责打开、删除或同步文件。普通开发者通常只选择内置预设；实现新协议的开发者才需要理解 encoder。

**P4 的加密参数决策：保留并实际执行 `NoEncryption`，不预先发布未经验证的扩展契约。**

1. P4 的 JSONL 配置只允许明确的内置“不加密”策略，由 builder 创建 `NoEncryption`；所有自定义 encryptor 或其他算法在初始化阶段返回不支持组合，且必须先于打开目标文件、创建恢复记录。不能仅检查插件自报的算法字符串是否为 `none`。
2. 每个非空 JSONL segment 经 `NoEncryption::seal` 透传，测试验证调用路径、输出 bytes 与原始 JSONL 相同，以及不附加二进制头、nonce 或 tag。空 flush/finish 可以不产生 segment，但不能让 encoder 接受一个从不调用的 encryptor 参数。
3. P4 的 format/encoder/encryptor 仍是 crate 内部原型，不纳入已发布的外部 trait 契约。P5 用真实加密的分段二进制格式验证同一 seam 的第二种行为，之后 P6 再定版公开；不把未来给公开 trait 加参数当成兼容演进。
4. 如果 P4 无法让这条透传路径真实执行，就从 P4 私有原型移除 encryptor 参数，P5 再调整草案。验收不接受“参数留着但暂时不用”。

必须先固定的契约：

- `FormatDescriptor` 含稳定的格式标识、版本与支持的编码组合，不把用户扩展压回一个只能列举内置格式的 enum。
- `FileContext` 含独立的文件 ID、格式版本和固定配置；文件重启或轮转不会错误沿用旧文件的 encoder。
- `push_record` 在成功时接管记录内容的内部副本；失败时不接纳该记录。runtime 在调用前控制单条大小与段大小，encoder 同样遵守已验证的内存预算。
- `seal_segment` / `finish_file` 使用已清空的复用输出缓冲，成功后才允许 runtime 将 bytes 与 `SegmentInfo` 绑定为私有字段的 `SealedSegment`。失败输出不能提交，encoder 进入失败状态或由明确的恢复操作处理，不能盲目重试并重复消费记录。
- 第一次输出包含需要的文件头，最后输出包含需要的结束信息；每次成功输出必须是该格式承诺可以提交、重放的完整追加单元。
- 完成后的 segment 不回写长度、不修改早先已提交字节；需要回写或随机访问的格式不属于第一版通用追加契约。
- `finish_file` 只发生一次，之后不得继续 push。若流式有尾部索引的格式不能在崩溃后恢复有效文件，初始化应拒绝宣称支持它，而非静默损坏。
- 文件大小预算包含头、尾、压缩和加密开销。只在完整 segment 边界轮转；单条记录超限采用显式拒绝或单独大文件策略，不从中间拆开一个未经允许拆分的格式单元。
- 在封段加密前确定目标 file ID，之后不能因大小检查把绑定旧文件 AAD 的 ciphertext 直接挪入新文件。第一版建议大小为软上限，允许超出至多一个有界 segment；需要硬上限时，格式必须提供可信的编码长度上界，在 seal 前预留头尾和加密空间并决定是否轮转。

`SealedSegment`、文件 ID 和提交确认不允许外部随意构造为“已持久化”；格式插件可以返回字节，但 durability 状态只能由 runtime 建立。

### 6.2 先用第二种实现证明 interface

| 实现 | 目的 | 配置约束 |
| --- | --- | --- |
| 现有 Mars preset | 默认兼容路径和性能对照 | 原来的完整组合与 pending 语义保留 |
| `JsonLinesFormat` | 验证没有 Mars header 的真实文件 | 一个 JSON 对象一行；P4 仅内置 `NoEncryption` 透传，初始化拒绝其他加密和压缩组合 |
| `PlainTextFormat` | 验证行布局与文件格式分离 | 字节透传；是否补换行由明确契约决定 |
| 自定义分段二进制示例 | 验证外部 format 与 encryptor 可组合 | 自有 format ID/version，完整 segment，配套 reader 与恢复测试 |

不为满足表格一次性实现全部算法。`JsonLinesFormat` 与一个位于集成测试或独立示例 crate 的自定义分段格式，足以先验证开放性。需要 seek、压缩包整包重写或数据库事务的格式，后续通过更高层存储目标 interface 处理。

### 6.3 压缩的处理

`CompressionKind` 目前承担 Mars magic 选择，不应作为全局格式体系的封闭集合。首轮继续把 zlib/zstd 作为 Mars 实现与内置格式的具体配置，不立即承诺任意 compressor 插件。

需要第三种压缩器时，再开放独立的 compressor 会话契约，并说明流式状态、finish、字典、算法 ID 和解码支持。公开自定义 `FileFormat` 本身已允许开发者在格式实现内选择其他压缩库；不必为了抽象完整性预先扩张公共 surface。

## 7. 加密：开放完整数据契约，不把 TEA 换成任意函数

```rust
pub trait Encryptor: Send {
    fn descriptor(&self) -> EncryptionDescriptor;

    fn seal(
        &mut self,
        context: &SealContext<'_>,
        plaintext: &[u8],
        output: &mut Vec<u8>,
    ) -> Result<EncryptionMetadata, EncryptError>;
}
```

此 interface 面向**完整 segment**，不是对任意小块原地改写的承诺。`&mut self` 允许算法保存计数器、会话和 key rotation 状态。实例由当前 writer 独占，不要求密钥状态 `Copy`、`Clone` 或 `Sync`。

需要明确：

1. `EncryptionDescriptor` 标识算法与版本；逐段 `EncryptionMetadata` 保存可公开的 `key_id`、nonce、必要的会话公钥和参数。tag 可由算法按契约放入 ciphertext 或独立字段，格式负责序列化，不能固定为 Mars 的 64 字节公钥。
2. `SealContext` 提供文件 ID、段序号和已规范化的 AAD。AAD 的构造必须在加密前固定，避免把加密后才知道且没有预先约定的值循环依赖地塞入头。
3. AEAD 同时校验 ciphertext 与关联数据；nonce 的唯一性要求取决于算法，不能把可回绕的 Mars `u16` 序号当成通用 nonce。[R6][R7]
4. 崩溃恢复只能重放已经持久化的 ciphertext；不能针对同一 nonce 重新加密不同内容。新进程会话、计数器溢出和 key rotation 必须有独立测试。
5. 加密错误不自动回退明文。`NoEncryption` 是用户明确选择的 adapter，不是异常处理的默认结果。
6. 外部错误通过 `Box<dyn std::error::Error + Send + Sync>` 等受限容器保留 source，再转为稳定的 `EncryptError`。不把 `anyhow::Error` 作为库的唯一公开错误。
7. 日志、`Debug`、metrics、journal/catalog 不包含私钥与派生对称密钥。密钥不为了“方便状态复制”实现 `Copy`；按选用库核验内存清理与秘密数据包装能力，不宣称 Rust 自动实现内存清零。
8. 解密不强加给 logger 的写端 trait。reader/CLI 通过单独的 decoder 与 key provider interface 接入；只有提供匹配 reader 的格式，才能保证恢复后的离线读取体验。

### 7.1 兼容约束

现有 ECDH + TEA 留在 Mars adapter。自定义 AEAD 往往需要 nonce、tag、算法标识等元数据，不能把它塞进原 TEA 位置而继续声称官方 decoder 可读。

第一版禁止 `Mars preset + 任意自定义 Encryptor`；需要新算法时使用新的分段格式。未来若设计 Mars 扩展协议，也必须获得新的格式规范与版本、配套 decoder，而非复用未声明的 magic。

原有“错误公钥回退不加密”仅限兼容入口，并明确记录为历史行为；新 builder 的密钥错误在初始化时报错。是否把旧入口改为严格模式属于独立的版本决策。

支持开发者接入自己的加密库或企业 KMS 策略，不等于鼓励自行设计密码算法。内置新方案应基于经过审阅的现有密码库；首轮只验证 interface 与组合约束，不顺带发明新安全协议。

KMS 获取或轮换会话密钥优先放在初始化/轮转阶段，明确超时与失败策略，不默认每条日志发起网络请求。若插件选择阻塞 IO，队列背压与退出延迟必须进入其性能契约。

## 8. 文件命名：与轮转、归属识别和恢复分离

```rust
pub trait FileNaming: Send {
    fn file_name(
        &mut self,
        identity: &FileIdentity,
    ) -> Result<RelativeFileName, NamingError>;
}
```

`FileIdentity` 由 runtime 创建，至少包含 logger namespace、稳定 file ID、轮转序号与创建时间。命名可以选择使用时间，也可以使用业务标识或 file ID，不要求能从文件名反推出日期。

`RelativeFileName` 是经过校验的 newtype，第一版只允许**一个普通文件名**，不允许任意路径。runtime 使用自己的根目录拼接路径，并维护同目录锁与创建策略。

### 8.1 不要求所有自定义文件名可逆

仅给 `FileNaming` 增加一个 `parse()`，会把哈希名、UUID 名与业务命名重新限制为固定模式。建议：

- 新模式以版本化 `FileCatalog` 为归属与枚举依据，保存 namespace、file ID、相对文件名、format/version、序号、创建时间和生命周期状态；不保存秘密数据。
- 文件名生成后先登记创建意图，再完成独占创建与确认；崩溃后的悬挂意图必须可协调，不能只在干净退出时写清单。
- catalog 原子更新、校验、备份/损坏处理属于持久化设计的一部分。无法安全恢复清单时 fail closed、保留文件，禁止靠后缀猜测然后删除。
- Mars 兼容模式仍用旧目录识别规则，不因引入 catalog 改写旧 `.mmap3` 与文件命名。导入既有 Mars 文件由专门的 parser 完成，不把所有用户命名都要求可逆。
- 不单独引入“扩展名 trait”。整个 basename 由 `FileNaming` 生成；格式描述可提供默认后缀建议，但实际内容与格式标识不由后缀决定。

采用 catalog 会增加实现成本；这是允许任意命名、同时保证正确轮转、恢复与安全清理的成本。首轮若暂不实现 catalog，只能公开受限且可逆的命名模板，不能称为完全开放命名。

### 8.2 P3 必须确定的 catalog 失败契约

清单不是清理功能的可选加速索引，而是新模式的归属依据。**安全归属优先于保留期与磁盘空间回收；无法确认归属时，保留期不再是必须执行删除的保证。**以下契约和确定性失败测试属于 P3 的完成条件，早于 P4 的 crash/journal 测试：

| 状态或输入 | P3 的强制行为 |
| --- | --- |
| 空的新 namespace、清单尚未创建 | 独占确认根目录/namespace 可初始化后创建清单；不得把非空未知目录直接认作新 namespace |
| 清单完整，namespace、file ID 和目录一致 | 仅列举、迁移、删除当前 namespace 已核验的登记文件；保留期只作用于这部分受管文件 |
| 清单之外有文件 | 标记为未归属并报告，不自动接纳、拼接、迁移或删除；即使后缀相同、名字相似、mtime 过期，也不能据此认领 |
| 既有清单缺失、校验失败、版本未知或归属冲突 | 初始化返回 `RecoveryRequired` 或对应结构化错误，不启动写入与维护 worker；不以空清单或按扩展名扫描作为 fallback |
| 运行中发现清单损坏或身份不一致 | 实例进入需恢复状态，停止新的写入及破坏性维护，返回/暴露错误并保留文件、journal 与诊断材料；不把部分可解析清单当作完整可信清单 |
| 无法按保留期回收而磁盘告急 | 报告受管/未归属文件与维护失败，按既定写入错误/背压策略处理；不得紧急扩大删除范围 |
| 需要重建或导入 | 只读盘点并显式恢复/导入，核验格式与归属后原子发布新清单；mtime、扩展名和命名前缀不能单独证明文件归属 |

`delete_expired_files` 现有实现按扩展名扫描目录，只能保留在旧模式内部。P3 的新清理路径必须直接消费已核验 catalog 条目，不得调用旧 scanner 再过滤。清单失效时列举/维护返回错误，不能以空列表或“清理成功但什么也没删”掩盖失败；健康清单下存在未归属文件时，单独报告保留的文件，不对其提供保留期保证。

目录隔离也提前到 P3：新模式的日志/缓存根目录不得落入仍由旧 scanner 管理的相同或重叠目录树。新旧部署必须使用独立存储根，直到旧维护入口具备受控归属检查；目录 marker 不能防止不认识它的旧二进制误删文件。当前版本能识别的根目录冲突应在初始化拒绝，跨版本部署限制写入升级与回滚说明。

P3 至少验证：清单损坏/缺失/未知版本时不删除也不迁移；未登记的过期 `.xlog` 不被新维护路径处理；其他 namespace 文件不被误操作；磁盘不足不触发扩展名 fallback；同目录或重叠目录配置被拒绝。P4 再增加这些状态与 journal 提交窗口交叉的 crash 测试，不以 P4 代替 P3 契约。

### 8.3 必须覆盖的约束

| 问题 | 处理方式 |
| --- | --- |
| 绝对路径、`..`、分隔符、Windows 前缀、NUL 等 | `RelativeFileName` 校验；跨平台用例，不仅检查字符串中有没有 `/`。[R5] |
| 名字重复 | `create_new` 或等价独占创建；有界重试，耗尽返回 `NameCollision`；不得因此追加到他人文件 |
| 同一目录多个 logger | namespace 参与锁与 catalog；删除只允许操作当前 namespace 的已登记文件 |
| 自定义命名没有日期 | 轮转仍由 `RotationPolicy` 决定，时间查询使用 catalog 而非正则猜日期 |
| 时区、夏令时、时钟回拨 | 兼容模式保留旧行为；新模式显式配置时间策略，并用 file ID/序号保证不覆盖 |
| 自定义子目录 | 第一版明确不支持；后续单独设计防路径逃逸与符号链接攻击的能力，不能自动放开 |
| 不同内容协议命中同一文件 | 校验 catalog/格式标识，拒绝混写；切换格式先封口并轮转 |
| 历史 `make_logfile_name(timespan, prefix)` | 旧入口保留旧语义；新 handle 提供 `list_files(FileQuery)`，不适用的旧式日期推导返回显式不支持 |

文件名校验不等于对不可信共享目录实现完整安全隔离；目录所有权、符号链接和平台文件打开语义仍需专项审阅。

## 9. 恢复与持久化：这次重构最关键的设计约束

### 9.1 不直接把 PersistentBuffer 泛型化

当前 buffer 不只是字节存储，还包含 Mars 扫描、pending header、长度回填与尾标记补全。将其机械改成 `PersistentBuffer<F>` 并不能得到通用恢复机制。

保留两个明确策略：

- `MarsRecovery`：保留现有 `.mmap3`、pending block 与 TEA 尾块兼容逻辑，现有 tests 不减少。
- `JournalReplay`：新格式只持久化已经编码、加密完成的完整 segment，并记录目标 file ID、期望 offset、长度和校验值；不在恢复时重新执行 formatter、compressor 或 encryptor。

新模式采用独立 journal 文件名和版本，不能复用旧 `.mmap3` 后指望旧 oneshot 自动理解。未知版本或不匹配的 format ID 应保留原文件并报错。

### 9.2 新模式的提交顺序

一次需要 crash-durable 保证的提交，至少包含：

1. 在有界内存中生成完整 ciphertext/encoded bytes，建立 `PreparedSegment`。
2. 写入可校验的 journal 记录及目标定位信息，并同步相应恢复记录；在此之前不可承诺“已持久化接收”。
3. 在 namespace 单写者锁下验证目标 file ID 与预期 offset，处理短写、部分写和相同 segment 的重放。
4. flush 目标用户态缓冲，再按所承诺等级执行 `sync_data` / 必要的文件和目录元数据同步。
5. 写入并持久化提交 checkpoint，再回收 journal。删除恢复源始终晚于目标 durability barrier。

仅调用 `Write::flush` 不代表稳定存储；`sync_data` 与 `sync_all` 的范围也不同。新文件创建、rename 和 catalog 切换还涉及目录元数据，必须按目标平台分别定义保证，不能把一个 `fsync` 调用宣传为跨平台绝对保证。[R2][R3]

### 9.3 重放去重必须显式设计

“目标已经写完、checkpoint 还没落盘”是必须覆盖的窗口。建议利用 file ID、预期 offset、segment 长度与 bytes 校验，在独占文件内识别重复输出；遇到目标被外部修改时保留 journal 并返回冲突，不猜测截断。

应区分三种情况：未写目标、仅写部分、已完整写入。恢复依据持久化意图和已验证前缀补全或跳过，而不是无条件 append。协议在 torn write、catalog/journal 损坏、目标文件遗失时能承诺什么，须通过子进程 kill 与故障注入再确认。

在完整故障模型与证据建立前，不承诺任意故障下的 exactly-once。先声明可靠接收点、允许丢失窗口、是否可能重复，以及 journal 清理条件。

### 9.4 缓存文件不再一律可拼接

现有 Mars cache merge 把源文件追加到目标，删源前执行 `sync_data`。这对任意格式不成立：文件头、nonce 域、索引、footer 可能都只能出现一次。

新模式默认保留文件身份：同文件系统优先按完整文件移动，跨文件系统复制到暂存文件、验证并同步、原子发布及更新 catalog，最后移除源。只有某格式明确定义“完整文件可拼接”且有测试时，才使用格式专属 merge adapter。

单写者锁 namespace 不以扩展名区分。两个配置即使后缀不同，只要可能写到同一目标集合，都不能各自认为自己拥有独占权。

## 10. 所有权、并发与分发策略

### 10.1 所有权

| 值或状态 | 所有者与可变性 |
| --- | --- |
| `Event<'_>` | 调用者提供，formatter 临时借用，返回后结束 |
| 文本/bytes buffer | 格式化 scratch 或 pool，入队时转移所有权，消费后归还 |
| 异步 encoder、compressor、encryptor | 写线程独占 `&mut`；不为共享而复制密钥或套多层 `Arc<Mutex<_>>` |
| 同步编码状态 | 同步路径独占；多线程调用时以一个明确的串行化所有者保护 |
| 文件句柄、catalog、轮转状态 | FileStore 独占；控制命令和数据写入有统一顺序 |
| 只读配置 | 初始化后固定；只有真实多持有者需求才使用 `Arc` |

Google guide 和本地 skill 都强调有实际用途才引入 trait；对当前四类外部替换需求，trait 有明确理由。enum 继续表达内部有限状态。[R1]

### 10.2 泛型与 dyn 的具体选择

- 构造入口可接受 `impl FileNaming + 'static`、`impl FileFormat + 'static`、`impl Encryptor + 'static`，调用者不必手工装箱。
- 注册后的普通 logger handle 不携带一路传播的五六个类型参数；运行时扩展与异质注册表使用必要的类型擦除。
- 将格式/加密动态调用放在 record 或 segment 粒度，不在每个字节循环里动态分发。
- 默认 Mars 路径保留具体类型的优化空间；只有 benchmark 证明确有收益时，再对低层 pipeline 提供静态分发入口，避免同时维护两套公共产品接口。
- 预期可用于 `dyn` 的公开 trait 不放泛型方法、无约束返回 `Self` 或原生 `async fn`；构造器移到具体类型或独立 builder。[R4]
- `Send` 只在对象跨线程转移时要求；`Sync` 只在实际共享调用时要求。现有 `WriteLine` 的 `Sync` 与调用线程并发语义一致，不把这一要求机械复制给所有状态会话。

Google 对库参数偏好泛型，同时保留代码体积等例外；这不等价于“整个库不能用 dyn”。这里选择类型擦除是为了非泛型 handle、运行时组合和 mobile/FFI 接入，不宣称动态分发没有成本。[R1][R4]

### 10.3 调度和退出

保留 `std::thread` 与现有 channel 技术栈。第一阶段不把目前两个工作环路强行合并；先给写入、flush、超时和模式切换建立顺序测试，再决定合并是否减少状态和锁。

新模式的背压使用明确的 `OverflowPolicy`，例如阻塞、有界超时或明确丢弃并计数。兼容模式保留现有行为，新模式默认建议有界队列加阻塞；需说明回调重入与关闭中发送如何避免死锁。

flush fence 应覆盖“调用前已接纳的记录”，而非只发一个未关联前端队列的后端 flush 命令。切换模式的顺序是停止旧模式接纳、排空对应 fence、封口编码状态、切换所有者、恢复接纳，不能与后台编码并发修改同一状态。

提供显式 `shutdown`，可以等待 drain、join 并返回错误。`Drop` 只承担已说明的 best-effort 释放，不能承诺可返回的持久化成功，也不能静默忽略所有退出失败。

## 11. 配置、错误与公共接口演进

### 11.1 配置分层

保留 `XlogConfig` 作为旧兼容 DTO。新增 builder 时分开表达：

- 身份与存储：namespace、根目录、缓存目录、命名、轮转、保留期。
- 文本：现有 `Layout` / `WriteLine`。
- 内容协议：format、压缩及受支持的 encryptor 组合。
- 调度：sync/async、队列上限、背压。
- 可靠性：flush 等级、恢复策略、错误通知。

新类型优先使用 `PathBuf`、`Duration`、经过验证的 `Namespace` / `RelativeFileName`，而不是到处传播 `String`、负数哨兵、无单位整数。FFI 的字符串、`-1` 和状态码只在转换入口保留。

不把这五组内容都做成公开字段结构体并要求调用方填完；常规用户通过少量预设完成配置。首先给出 `MarsCompatible`、`PlainText`、`JsonLines` 等具名预设，再允许局部替换。

所有 format/encryptor/naming 合法性、目录访问和资源预算尽量在创建文件前验证。协议运行中遇到无法提前确定的失败，仍要按真实错误返回，不能通过 `expect` 假设不会发生。

### 11.2 同名 logger 与自定义对象

trait 对象无法自动获得有意义的 `Eq`。不能用对象地址、`TypeId` 或 `Debug` 文本假装配置相等，也不能要求所有插件实现稳定序列化来解决实例复用问题。

建议旧入口保留现有复用语义；新 builder 将“创建”和“取得现有实例”分开。同 namespace 已存在时，创建返回 `AlreadyExists`，调用方用 `get` 显式复用。阻止两个 runtime 写同一文件仍由进程内注册与跨进程锁共同保证。

`FileIdentity`、namespace 和 registry key 的关系需要在 P1 固定，避免仅以名字注册、以另一组目录锁定而产生含混的冲突规则。

### 11.3 失败与持久化等级

新增错误至少区分 `Config`、`Format`、`Encrypt`、`Naming`、`Storage`、`Recovery`、`Queue`、`Worker`，保留路径、操作和原始 source；不要所有 IO 错误都变成 `InitFailed`。

新增 fallible 入口，例如 `try_write_event`、`try_flush`、`shutdown`。异步 `try_write_event` 的成功只代表接纳，不能代表随后编码与 IO 成功；后续错误通过有界错误状态/计数与独立通知 interface 可观察，并由 flush fence 或 shutdown 返回关联失败。

旧 `log()`、宏、tracing callback 不能强制改为返回业务错误；适配到同一个错误观察机制，不再让 `let _ =` 成为唯一策略。错误通知不得在持有内部锁时调用用户代码，不得递归写回同一个 logger。

新接口用具名 enum 区分：

- `Drain`：此前接纳的记录已处理到定义的编码提交点。
- `Flush`：用户态文件缓冲已向操作系统提交。
- `Durable`：在指定平台与故障模型下完成目标同步及必要的元数据提交。

方法返回成功前分别要满足什么，由实现与契约测试定义。原 `flush(bool)` 留在兼容层，不把它的 worker ack 描述为每条记录已 crash-durable。

锁中毒不能统一改成 `into_inner()`，也不能简单套 `catch_unwind` 宣称库不会崩溃。先判断不变量是否仍成立；无法证明时使实例进入 `Failed` 并保留恢复源。平台 FFI unwind/abort 风险单独处理。

## 12. 质量规范与工具门禁

### 12.1 可直接采用的 Rust 规范

- 领域差异用 newtype，封闭状态用 enum；错误使用 `Result` 并保留来源。[R1]
- 外部可实现的 trait 保持精简且有行为契约，不 sealed；只供内部使用的协议细节维持私有，必要时 sealed，而不是全部 `pub`。[R8]
- 优先标准 trait；`From` 表示无损转换，`TryFrom` 表示校验失败，避免在多个入口重复 clamp。[R1]
- 将 `unsafe` 约束在 mmap、平台 TID、console/FFI 等必要位置，审阅 safety invariant；业务 format、naming、crypto 组合本身不需要新增 unsafe。[R1]
- 不为了风格禁止所有 clone、所有 mutex 或所有 `expect`。用户输入、IO、插件返回值和可恢复失败不能靠 panic 处理；测试与可证明不变量分开审阅。

Google guide 中 Chrome/Fuchsia 的线程、runtime、测试框架等要求有产品上下文，不能套用为本项目“禁止 std::thread”或“必须改测试框架”的理由。Impasto 的前端、Tauri 等规则同样只取适用部分。

### 12.2 已有门禁保持不降级

以 `docs/rust_quality_bar.md` 与实际 CI 为基线：

```sh
cargo fmt --all --check
cargo clippy -p mars-xlog-core -p mars-xlog -p mars-xlog-cli --all-targets --all-features --locked -- -D warnings
cargo test -p mars-xlog-core -p mars-xlog -p mars-xlog-cli --all-features --locked
```

保留默认 feature、all-features、rust-backend-only、Linux/macOS/Windows 测试与 release preflight。变更 binding 后补 Android/iOS/Harmony 对应目标的编译检查，不能以桌面测试代替。

新增基线建议：

| 检查 | 引入方式 | 成为阻断门禁的条件 |
| --- | --- | --- |
| MSRV + 当前 stable | 显式测试 Rust 1.85；stable 发现新 lint/依赖变化 | 新设计不隐式升级 MSRV；升级需单独版本决策 |
| Clippy 定向规则 | 先报告生产路径 unwrap、panic、unsafe 文档、过深嵌套、过多参数等 | 基线清理后对新增/修改 module ratchet，不全量开启 pedantic/restriction |
| 覆盖率 | 复用现有 llvm-cov workflow，再生成供质量分析的机器可读报告 | 新增行为有测试；基线固定后再讨论阈值，不虚设总覆盖率数字 |
| CRAP / 重复度 | 参考 Impasto 的 crapper/dryer，但先验证版本、解析能力和报告范围 | 同提交、同 feature、相同 source 集合可重复生成报告后再阻断 |
| 公共兼容性 | cargo-semver-checks + 外部实现 fixture crate | 对发布 crate 检查；工具不能代替下游 trait 实现的编译测试 |
| 依赖与安全 | cargo-deny 等核验许可、重复依赖和公告 | 配置并审阅例外与过期时间，不能把工具通过当密码学审计 |

工具命令、来源及适用限制见 `docs/rust_refactor_reference_notes.md`。本轮不安装工具或修改 CI。

### 12.3 复杂度不能只看一个数字

当前**未采纳任何 CRAP 或相似度数值门槛，也不将外部示例数字设为新增/修改代码的默认审阅目标**。先生成可信的热点报告并人工对照，再单独评审是否采用 ratchet、具体范围与豁免策略。

`CRAP < 10` 与相似度 `>= 0.8` 仅记录为 Impasto `ub-mode` 的历史政策选择，与参考说明第 2 节口径一致，不是 Rust/Google 标准。还必须区分数学边界和工具行为：已核验版本的 `crapper --threshold 10` 阻断的是 `> 10`，允许 `= 10`，并不实现严格 `< 10`；`dryer --threshold 0.8` 只筛选报告候选，发现候选不自动返回失败。若将来采纳阈值，必须同时评审比较符、工具固定版本、报告校验与退出码策略，不能把这两个示例命令当作已经落实的门禁。

不要把 Clippy `cognitive_complexity` 当成可靠的认知复杂度评分；当前上游也明确说明其局限。可结合嵌套、函数长度、分支风险和测试覆盖定位问题，但不能为了过分数把一个不变量拆散到大量浅 module。[R9]

质量报告必须排除或单列 vendored C++、生成 binding、测试夹具和测试重复。覆盖率用 line/function 还是 branch 必须明示；不能把缺失覆盖数据当成 100%，也不能仅靠补几条无意义测试掩盖设计问题。

### 12.4 性能与依赖预算

已有 `scripts/xlog/bench_regression_thresholds.json` 区分 default、baseline、stress、feature 与 Criterion 特例，继续使用，不另拍一个统一“只许慢 5%”门槛。

比较相同硬件、toolchain、feature、buffer、压缩级别和输出量：

- 默认 Mars 路径重构前后吞吐、延迟与文件字节。
- 新模式格式化、分段编码、加密、journal、flush 的分项开销。
- 每条分配次数、峰值内存、队列等待、锁等待和二进制体积。
- metrics 开/关、同步/异步、短记录/超长记录、目录大量历史文件。

抽象本身不保证更快；增加 journal 的可靠性成本也不能伪装成零成本。先保证行为与恢复契约，再评估可接受的性能差异。

## 13. 验证矩阵与验收场景

### 13.1 兼容模式持续通过

保留 `protocol_compat`、`decoder`、`mmap_recovery`、`oneshot_flush`、`async_engine` 等现有测试，特别是：

- magic、头长度、序号、TEA 与 zlib/zstd 的组合。
- sync payload 的历史明文行为，async 分块和尾块回退。
- missing tail、dirty tail、启动恢复、oneshot 连续输出，不把一个 recovered block 拆到多个不连续文件。
- 模式切换不丢 pending，flush 超时与显式 flush 的顺序。
- 同目录、共享缓存目录的跨进程锁拒绝。
- 默认文件名、日期与分片、缓存迁移、保留期与时钟回拨。

旧输出的 golden 测试使用固定时间、runtime IDs、随机密钥或固定样本；有随机会话公钥的输出不能要求每次自然运行整文件相同。官方 decoder 与现有 Python 对照脚本继续作为跨实现证据。

### 13.2 扩展模式必须有真正外部实现

新增独立 fixture/example crate，仅依赖公开入口，实现：

1. 自定义 `WriteLine`，证明不需要引用私有 formatter。
2. 无日期的 `FileNaming` 和非 `.xlog` 后缀，证明重启后仍能列举、滚动、缓存迁移与清理。
3. 自定义完整 segment 格式与自定义 `Encryptor`，证明 format ID、元数据和错误链不依赖 TEA。
4. 对应 reader，证明写入、关机、重启和恢复后可读。

生产加密库之外可在测试中提供确定性的 fake，但不得把 XOR 等测试实现作为可用于生产的密码示例。

### 13.3 故障与输入矩阵

| 类别 | 必须验证 |
| --- | --- |
| IO | 打开失败、短写、磁盘满、flush/sync 失败、rename/复制失败、目标被外部修改 |
| crash | journal 写一半、journal 已同步但目标未写、目标部分写、目标已写但 checkpoint 未提交、删源前后 |
| crypto | 错密钥、错 AAD、错误 tag、不同 nonce/会话、序号溢出、重启、禁用明文回退 |
| 文件名 | 重复名字、短名字、Unicode、Windows 保留名、路径逃逸、无关文件与其他 namespace |
| runtime | 并发写、队列满、关闭中写入、flush fence、线程异常、错误回调重入、模式切换 |
| format | 超长记录、空记录、多行消息、头/尾一次性输出、unsupported 组合、未知版本、拼接不合法 |
| catalog | 暂存文件、悬挂创建意图、清单损坏、目标遗失、时钟回拨、恢复后索引不重复 |

解析器和恢复扫描适合增加 property test/fuzz；私有 IO seam 用于确定性故障注入，子进程 kill 用于验证真实进程退出。不能只用内存 fake 证明文件系统 durability。

## 14. 分阶段实施路线

每阶段是可单独评审、保留默认路径可回退的交付，不要求一次大 PR。阶段复杂度不等价于固定工期，需要按人员与平台验证资源拆任务。

| 阶段 | 主要改动 | 完成标准 | 回退边界 |
| --- | --- | --- | --- |
| P0 行为与质量基线 | 固定兼容矩阵、真实调用图、错误丢弃点与热点报告；独立修复文件名受检切片缺陷 | 短文件名/UTF-8 边界的先红后绿回归；其余基准、公开接口清单可重复运行 | 缺陷修复单独评审和交付，不等待也不混入架构改动 |
| P1 契约与错误 | 领域类型、可靠性等级、实例创建/取得规则、fail-closed 新配置、错误观察方式 | 规范示例、错误链测试、线程和状态转移测试 | 旧 `XlogConfig` 与旧入口保持 |
| P2 收拢默认实现 | 将 Mars 帧/加密/pending 知识集中到兼容 module，整理 facade 与 worker interface | 所有旧测试与跨 decoder 证据通过，无格式变化 | 只做内部替换，保留旧公开路径 |
| P3 文件生命周期 seam | FileStore 接受独立身份与轮转决策；确定 catalog 健康/失败状态、归属和目录隔离规则 | 清单失效禁止破坏性操作；未归属文件保留；禁止后缀 fallback；冲突、短写和无日期命名测试通过 | 旧 scanner 仅留在旧模式；新旧存储根隔离，新路径 feature-gated |
| P4 第二种格式与恢复 | 私有 format/encoder 原型、JSONL + NoEncryption 真实透传、完整 segment、journal/checkpoint | 其他加密/压缩组合初始化拒绝；合法文本转义测试；已知 formatter 错误缺口明确；crash 重放通过 | 不发布未经验证的 trait；新格式单独目录，旧 `.mmap3` 不迁移 |
| P5 自定义加密和自定义协议 | 用真实分段格式验证 Encryptor 的第二种行为及 reader、nonce/密钥生命周期 | 试验 interface 的外部 fixture 可编译；密文路径实际执行、失败不回退、恢复不重新加密 | 未完成验证不进入 P6 公共契约；不动 Mars magic |
| P6 稳定 Rust 接入 | builder/预设、WriteLine adapter、文档、feature/MSRV、semver 检查 | 用户仅实现公开 trait 即可实现四类定制 | 扩展模式仍显式选择，默认不切换 |
| P7 bindings 与收口 | 外语绑定只暴露核准预设；性能、复杂度 ratchet、版本与迁移说明 | 平台构建、兼容与性能门禁通过；安全约束签署 | 回退软件时保留新格式文件，不交给旧 decoder |

依赖顺序：`P0 -> P1 -> P2 -> P3 -> P4 -> P5 -> P6 -> P7`。P0 完成后可先补 formatter 示例与文档，但不要因此绕过文件/恢复契约直接公开任意格式。

首个可验收的端到端切片建议是：

> 新 Rust builder 选择 JSONL、无日期自定义文件名和 `.log` 后缀；并发写入、显式 flush、进程退出重启后仍可正确恢复、列举与保留期清理，默认 Mars 用例不变。

加密切片随后用独立分段二进制格式证明，不把“明文 JSONL 文件 + 任意二进制密文”作为合法 JSONL 输出。

### 14.1 实施时每个 PR 的检查单

1. 说明改变的是 interface、implementation 还是兼容语义，列出对应规范与回归测试。
2. 公开 trait 有实际第二种 adapter，且外部 crate 不依赖 `pub(crate)` 或内部模块路径。
3. 控制新增公开类型、trait 方法和 feature 数；不能为了消除所有 match 扩大公共承诺。
4. 默认兼容路径通过行为与性能门禁；新路径有独立故障模型与支持范围。
5. 涉及格式、key、catalog、journal 的持久化版本都带升级/降级策略。
6. 旧实例、旧 mmap、缓存文件、新格式文件不会在升级或回滚中混写、误删或被错误 decoder 消费。

## 15. 本轮核验结果与边界

初稿只新增方案与参考说明。评审修订额外独立修复 `file_index_from_path` 的输入切片 panic，并增加三个回归测试。随后在 `feature/extensible-log-architecture` 落地了第一个实现切片，没有修改 Mars 协议、默认入口或 CI。初始工作区未跟踪的 `.agents/` 保留不动。

已落地的部分：

- `mars-xlog-core` 的 `extensible` 模块：`FileFormat`、`FormatEncoder`、`Encryptor`、`FileNaming`、具体的 `RotationPolicy`、catalog、journal 和 `ExtensibleStore`。
- 内置 `JsonLinesFormat` 与 `PlainTextFormat`，都只接受 `NoEncryption`，其他 encryptor 在打开文件前被拒绝。
- `IdFileNaming` 与 `TimestampFileNaming`，文件名限制为单个路径分量。
- catalog 校验失败返回 `RecoveryRequired`，不删除、不迁移；维护只删除已登记、已封口、超过保留期的文件，未归属文件保留。
- journal 重放只拷贝已编码字节，不重新执行格式化或加密。
- `mars-xlog` 的 `ExtensibleLogBuilder` 与 `ExtensibleLog`，与现有 `Xlog` registry 分离。

明确没有落地的部分：子进程 kill 级别的崩溃测试、异步队列与背压、`Drain`/`Flush`/`Durable` 三级 flush、第二种真实加密格式，以及对 Mars 兼容模式任何行为的改动。`FileFormat` 等 seam 仍按 P4 的约定视为内部原型，不是稳定的公共契约。

初稿时在 macOS 本机已执行的基线：

- Rust 1.97.0：`cargo fmt --all --check` 通过。
- Rust 1.97.0：release-facing 三个 crate 的 all-targets/all-features Clippy、`-D warnings` 通过。
- Rust 1.97.0：core/xlog/cli 的 all-features 测试通过，121 个普通测试与 1 个文档测试。
- 本机 `+1.85` 工具链（Cargo 1.85.1）：同一组三个 crate 的 all-features 测试通过。CI 当前固定的是 1.85.0，本机结果不冒充对该精确补丁版本的重跑。

评审修订已用 `cargo test -p mars-xlog-core --lib file_index_from_path --locked` 先复现两类 panic；受检切片修复后，`file_naming::tests` 六个测试全部通过。新增测试同时保护无分片、分片、Unicode/下划线前缀以及无关文件的原有返回值。

修订后重新执行：Rust 1.97.0 下 fmt 与 release-facing Clippy `-D warnings` 通过；Rust 1.97.0 和本机 `+1.85` 工具链下，core/xlog/cli 的 all-features 测试均通过 124 个普通测试与 1 个文档测试。

没有实测新的覆盖率、CRAP、重复度或重构后性能；本轮不重跑完整跨平台/移动端构建，也不声称新 trait 草案已经编译验证。现有 Clippy 和测试通过说明基线可维护，并不证明扩展契约、故障模型或密码设计已经完整。

评审建议先确认四个产品决策：

1. 默认仍为 Mars 兼容，新能力显式 opt-in。
2. 首轮任意格式限定为本地追加式、完整可重放 segment，不承诺数据库或随机写入容器。
3. 完全自由命名接受 catalog 成本；若不接受，则降低为受限命名模板而不是省略恢复设计。
4. 新模式默认严格错误与加密失败关闭，旧入口的降级语义单独保留。

这些决策通过后，从 P0/P1 建立可执行任务；不要先把 `Crypt` 改成 `Box<dyn Encrypt>` 然后再补持久化语义。

## 16. 参考资料

外部事实使用一手资料；项目判断以本地源码与契约测试为准。网络资料核验日期为 2026-11-07，不把文档站的最新版本自动当作本项目 MSRV 可用版本。

- [R1] Google Rust Style Guide：`https://google.github.io/styleguide/rustguide.html`，重点是 trait 引入条件、泛型/dyn 取舍、newtype、错误和 unsafe；Chrome/Fuchsia 特有约束不直接迁入。
- [R2] Rust 标准库 `std::io::Write`：`https://doc.rust-lang.org/std/io/trait.Write.html`，确认字节写入、短写和 flush 契约。
- [R3] Rust 标准库 `std::fs::File`：`https://doc.rust-lang.org/std/fs/struct.File.html`，确认 `sync_data` / `sync_all` 不同于用户态 flush。
- [R4] Rust Reference Dyn compatibility：`https://doc.rust-lang.org/reference/items/traits.html#dyn-compatibility`；动态分发取舍参见 `https://doc.rust-lang.org/std/keyword.dyn.html`。
- [R5] Rust 路径组成：`https://doc.rust-lang.org/std/path/enum.Component.html`。语法层校验不代替文件系统路径安全审查。
- [R6] RustCrypto AEAD interface：`https://docs.rs/aead/latest/aead/trait.Aead.html`；流式方案参考 `https://docs.rs/aead-stream/latest/aead_stream/`，不是要求引入该最新版本。
- [R7] RFC 8439：`https://www.rfc-editor.org/rfc/rfc8439`，尤其 nonce 唯一性与认证加密；具体依赖版本、MSRV 和方案仍需单独评审。
- [R8] Rust API Guidelines Future proofing：`https://rust-lang.github.io/api-guidelines/future-proofing.html`，只在不允许下游实现的场景使用 sealed trait。
- [R9] Clippy `cognitive_complexity` 上游定义：`https://github.com/rust-lang/rust-clippy/blob/master/clippy_lints/src/cognitive_complexity.rs`；不要以其输出替代可靠复杂度度量。
- [R10] Cargo MSRV：`https://doc.rust-lang.org/cargo/reference/rust-version.html`；本项目声明为 Rust 1.85、edition 2021。
- [R11] Rust `str` 的 UTF-8 不变量：`https://doc.rust-lang.org/std/primitive.str.html#invariant`；当前 `Event` 使用合法 `&str`，与原始字节输入的校验责任不同。
- [R12] serde_json `to_string` 的失败条件：`https://docs.rs/serde_json/latest/serde_json/fn.to_string.html#errors`；固定字段 JSONL 与任意用户 `Serialize` 的错误面不同。
- 本地 Rust skill：`.agents/skills/rust-best-practice/SKILL.md` 及 `references/traits.md`、`references/strict.md`。
- 用户指定参考：`/Users/zhangfan/develop/github.com/Impasto/.agents/skills/`；其适用性与工具核验见 `docs/rust_refactor_reference_notes.md`。
- 现有约束：`docs/rust_semantic_redlines.md`、`docs/rust_quality_bar.md`、`docs/benchmark_strategy.md` 与已有 refactor 文档。
