# xlog-core Rust 重构方案

约束来自项目内的 [rust-best-practice](../.agents/skills/rust-best-practice/SKILL.md)，以及 [语义红线](rust_semantic_redlines.md)。

这次只重排 `mars-xlog-core` 的类型、所有权和模块边界。磁盘格式、magic、序号、ECDH+TEA、mmap 恢复、oneshot、sync 不压缩、单写者锁，行为保持不变。`mars-xlog` 的现有调用方式保持可用；JNI、UniFFI、CLI 只在边界做转换。

不引入 Tokio。现有实现是 `std::thread` 加 `crossbeam-channel`，继续用这套。

## 设计顺序

按 skill 的五步，不从 trait 或泛型开始。

1. 值怎么流。一行日志在 `mars-xlog` 里格式化成字节，交给 core 的帧编码器。帧编码器拥有压缩器和 TEA 尾块，把封口后的块交给唯一的文件运行时。mmap 只保存还没落盘的字节。
2. 哪些状态不能同时存在。压缩算法、是否加密、同步还是异步，组成封闭的帧种类。同步块的 payload 是原文，magic 只记录配置。异步块才压缩；只有异步且配置了密钥才做 TEA。
3. 哪里要稳定、哪里可以替换。帧种类由本 crate 封闭，用枚举。文件和缓冲是具体类型。调用方不能插入第三种压缩器，所以不为压缩和加密做 trait。
4. 并发发生在哪。格式化和压缩留在调用线程，是同步计算。工作线程只负责 flush、滚动和过期清理。锁只包住「当前写到哪个文件、写到哪一字节」。
5. 错误在哪变成给人看的结果。core 用 `thiserror` 保留 `io::Error` 来源。`FileIoAction` 这种整数状态码只出现在 JNI / UniFFI / `mars-xlog` 的兼容层。

## 目标形状

保留现有文件，不新建一套 `wire/` `frame/` `durable/` 目录。只加一个模块。

| 模块 | 拥有什么 | 不拥有什么 |
| --- | --- | --- |
| `protocol` | magic、头、序号 | 压缩器、文件 |
| `compress` | zlib / zstd 的具体编码器 | 对外 trait |
| `crypto` | `Crypt::{Off, Tea}` | `enabled: bool` |
| `frame`（新） | 把一行字节封成一个块 | 线程、mmap |
| `buffer` + `mmap_store` | 持久化字节 | 压缩、路径字符串 |
| `appender_engine` | 工作线程、flush、模式切换 | 第二份 `FileManager`，以及 header 拼装 |
| `file_*` | 目录、锁、滚动、追加 | 协议 |
| `decoder` | 用同一套 magic 解码 | Python 诊断文案的拼装细节散落在解析里 |
| `oneshot` | 恢复并落盘 | 整数返回码 |

`crates/xlog/src/backend/rust.rs` 里的 `AsyncPendingState`、`AsyncCompressor`、`build_sync_block_into` 移入 `frame`。facade 继续做级别过滤、格式化、注册表、控制台和 metrics。

## 类型

封闭状态用枚举。能在边界一次校验完的值用 newtype。缺失用 `Option`。失败用 `Result`。

### 帧种类

```rust
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub enum BlockMagic {
    SyncZlib { encrypted: bool },
    SyncZstd { encrypted: bool },
    AsyncZlib { encrypted: bool },
    AsyncZstd { encrypted: bool },
}

impl From<BlockMagic> for u8 { /* 现有常量，字节不变 */ }

impl TryFrom<u8> for BlockMagic {
    type Error = ProtocolError;
    fn try_from(magic: u8) -> Result<Self, Self::Error> { /* ... */ }
}
```

`LogHeader.magic` 从 `u8` 改成 `BlockMagic`。`select_magic` 变成 `BlockMagic` 的构造函数，再 `u8::from(magic)` 写入磁盘。旧格式 `0x01..=0x05` 单独放 `LegacyMagic`，只给解码器。现代头和旧头不是同一个结构，不要塞进一个带一堆 `Option` 的头里。

同步「加密」只决定 magic 和 64 字节公钥槽，payload 仍是原文。这个事实写在 `Sync` 变体的 `seal` 里，不写在注释里。

### 压缩与加密

删掉 `StreamCompressor`。它把三种生命周期收成一对方法，`flush` 对 zlib 是结束流，对 zstd chunk 是空操作。上层又用 `AsyncCompressor` 把 trait 擦成 `bool`。

flate2 的 `DeflateEncoder<W>` 和 zstd 的 `Encoder<W>` 已经实现 `std::io::Write`：`write_all` 吃明文，`flush` 吐出已压缩字节。帧编码器直接拿这两个具体类型。

```rust
enum AsyncEncoder {
    Zlib(flate2::write::DeflateEncoder<Vec<u8>>),
    Zstd(zstd::stream::write::Encoder<'static, Vec<u8>>),
}

impl AsyncEncoder {
    fn write_plain(&mut self, bytes: &[u8]) -> Result<(), CompressError> {
        self.as_write().write_all(bytes)?;
        self.as_write().flush()?;
        Ok(())
    }
}
```

`Write::flush` 只表示「把编码器里已经产生的字节交出来」。结束一条流用具体类型的 `try_finish` / `finish`，不伪装成 `Write::flush`。zstd chunk 如果还要保留，就是一个函数 `fn compress_chunk(input: &[u8], level: ZstdLevel) -> Result<Vec<u8>, CompressError>`，不进这个枚举。当前热路径用的是 stream，chunk 留在测试和显式 API 里。

```rust
#[derive(Clone)]
enum Crypt {
    Off,
    Tea { key: [u32; 4], client_pubkey: [u8; 64] },
}
```

没有 `enabled` 字段，没有 `encrypt_sync` 这种永远拷贝的方法。`Debug` 不打印 `key`。空公钥字符串在边界变成 `Crypt::Off`。非法公钥是 `TryFrom` 错误；现在 facade 里「非法则静默禁用」的兼容行为留在 `mars-xlog`，就地写明，不藏进 cipher。

`CompressError` 保住来源，不再 `to_string()`：

```rust
#[derive(Debug, thiserror::Error)]
pub enum CompressError {
    #[error("zlib compress failed")]
    Zlib(#[source] std::io::Error),
    #[error("zstd compress failed")]
    Zstd(#[source] std::io::Error),
    #[error("zlib decompress failed")]
    ZlibDecompress(#[source] std::io::Error),
    #[error("zstd decompress failed")]
    ZstdDecompress(#[source] std::io::Error),
}
```

### 只在边界校验的 newtype

底层表示相同、混用会写错协议的，才包一层。

| 类型 | 转换 | 不变量 |
| --- | --- | --- |
| `ZlibLevel` | `TryFrom<i32>`，超出 `0..=9` 失败 | 不再 `clamp` 掉非法级别 |
| `ZstdLevel` | `TryFrom<i32>` | 用 zstd 接受的范围 |
| `Hour` | `TryFrom<u8>`，`0..=23` | 头里的小时 |
| `ServerPublicKey` | `FromStr`，128 个 hex 字符 | 解码时校验 secp256k1 |
| `PrivateKey` | `FromStr`，64 个 hex 字符 | 32 字节 |
| `CacheDays` | `TryFrom<i32>`，非负 | 不再把负数传进文件层 |

实现 `From`，不实现 `Into`。调用点需要宽松转换时写 `impl Into<ZlibLevel>` 的参数，但只放在真正存储这个值的构造函数上，不往下传。

路径用 `impl AsRef<Path>`，函数内部再 `to_path_buf()`。不返回 `Vec<String>`。`filepaths_from_timespan` 和 `make_logfile_name` 改为 `Vec<PathBuf>`。lossy 字符串转换留在 FFI。

`pid` / `tid` / `maintid` 的 `-1` 只存在于 `mars-xlog` 和 FFI。core 的 `LogRecord` 用 `Option<i64>`：`None` 表示写日志时再取运行时 id。formatter 是唯一把 `None` 渲染成数字的地方。

`LogLevel`、`AppendMode`、`CompressionKind` 以 core 为准。`mars-xlog` 用 `From` 做一对一映射，不再维持第三套语义相同的枚举。UniFFI / NAPI 可以保留自己的枚举，因为绑定生成器需要，但转换放在绑定 crate。

### 帧编码器

长寿命对象会在「空闲 → 打开一块 → 封口 → 再打开」之间循环。这种状态用内部枚举，不用公开的 typestate 泛型。typestate 会把 `Open` / `Sealed` 泄漏到工作线程和 `mars-xlog`，而调用方其实只需要 `push` 和 `seal`。

```rust
pub struct FrameWriter {
    kind: FrameKind,
}

enum FrameKind {
    Sync {
        family: CompressionFamily,
        crypt: Crypt,
    },
    Async {
        encoder: AsyncEncoder,
        crypt: Crypt,
        open: Option<OpenBlock>,
    },
}

struct OpenBlock {
    header: LogHeader,
    payload_len: usize,
    crypt_tail: Vec<u8>, // 容量 8，复用
}
```

同步 `push` 直接给出完整块：头 + 原文 + `0x00`。异步 `push` 对编码器 `write_all`，取出新字节，按 8 字节 TEA，剩下的尾巴留在 `crypt_tail`。`seal` 调用 `try_finish` / `finish`，补上尾巴和结束符。

缓冲复用：`crypt_tail` 和编码器内部的 `Vec` 跨块 `clear`，不在每条日志上重新分配。这是 `mem-reuse-collections`，不是新的抽象。

`frame` 不碰文件，也不起线程。它的输出是 `&[u8]` 块，或对 `PersistentBuffer` 的几次追加。追加通过 `buffer` 已有的方法完成，不把 mmap 做成 `dyn Write`。mmap 要改 header 长度和 end hour，不是一条纯字节流。

### 引擎只调度

`AppenderEngine` 留下这些事：模式、mmap、flush 阈值、工作线程、过期清理。

模式切换是产品行为，继续用运行时枚举，不拆成 `SyncEngine` / `AsyncEngine` 两个类型。内部可以用 `AtomicU8` 做无锁读，但读写都经过 `EngineMode`，用 `TryFrom<u8>`，禁止裸 `as`。

删掉三套写入口（`write_block`、`begin_async_pending` / `append_async_chunk` / `finalize_async_pending`、`write_async_pending`）。对外变成：

```rust
impl AppenderEngine {
    pub fn write_sealed(&self, block: &[u8]) -> Result<(), AppenderError>;
    pub fn write_async_parts(
        &self,
        header: &LogHeader,
        payload: &[u8],
        sealed: bool,
    ) -> Result<(), AppenderError>;
    pub fn flush(&self, sync: bool) -> Result<(), AppenderError>;
}
```

`write_async_parts` 仍是过渡形态。`FrameWriter` 搬进引擎、并且确认热路径不再从 facade 逐块改 mmap 之后，再把它收成 `write_line`。不要在同一次改动里既搬家又改 flush 语义。

`FileManager` 只保留一份。现在引擎字段里一份、`EngineState` 里 `clone` 一份，靠内部 `Arc<Mutex<RuntimeState>>` 碰巧共享。改成：

- 不可变配置（目录、前缀、`cache_days`）放在引擎上，查询路径不加锁。
- 活动文件、缓冲、滚动状态放进一把 `Mutex`。
- 压缩在锁外完成。锁内只做「选定文件并追加」。这个临界区包含那一次磁盘写，因为不变量就是文件游标；不要为了「锁里不要有 IO」把追加拆到锁外。格式化、压缩、加密不要进这把锁。

工作线程继续用现有 channel。flush 的应答已经是容量为 1 的 oneshot 形状，保留。命令通道现在无界；改成有界会改变背压，触碰语义红线，这次不动。

`set_max_alive_time` 小于一天时现在直接 return。改成 `Result`，让调用方看见拒绝。

### 解码和 oneshot

解码器的输入保持 `&[u8]`。它要按 magic 扫描、回退、切片，这是随机访问，不是 `Read` 的顺序流。CLI 已经把文件读进内存，不必把解析器改成 `impl Read`。

解码输出改成 `impl Write`：

```rust
pub fn decode_xlog(
    input: &[u8],
    output: &mut impl Write,
    opts: &DecodeOptions,
) -> Result<DecodeSummary, DecodeError>
```

诊断行用 `write!`，集中在一个函数里。解析函数只返回块和缺口，不自己 `format!` 进业务缓冲。文案仍是现在的 `[F]decode_log_file.py ...`，避免破坏比对脚本。

`oneshot_flush` 返回 `Result<OneshotOutcome, OneshotError>`，`OneshotError` 带 `#[source] io::Error`。`FileIoAction` 的 `From<OneshotError>` 放在 `mars-xlog` 和绑定 crate。core 不再出现 `None = 0` 这种 C 返回码。

### 标准库 trait 怎么用

| 需求 | 用什么 | 不用什么 |
| --- | --- | --- |
| 往编码器或文件写字节 | `Write::write_all`、`Write::write_vectored` | 自建 `StreamCompressor`；把「结束压缩流」叫做 `Write::flush` |
| 解码写出明文 | `&mut impl Write` | 写死 `&mut Vec<u8>`，再让调用方拷走 |
| 读入参与解析的缓冲 | `&[u8]` | 把扫描器改成 `impl Read` |
| 无损转换 | 实现 `From`，从而得到 `Into` | 手写 `impl Into` |
| 会失败的转换 | `TryFrom`，得到 `TryInto` | `as` 截断；`clamp` 吞掉非法级别 |
| 解析 hex / 公钥 | `FromStr`，调用点用 `parse()` | `new(hex: &str) -> Result` 散落多套 |
| 路径参数 | `AsRef<Path>` | `&String`、返回 `Vec<String>` |
| 只读文本 | `&str` | `&String` |
| 错误展示 | `thiserror` 的 `Display` + `Error::source` | `Box<dyn Error>`，或提前 `to_string()` |
| 诊断 | `Debug`；密钥字段红acted | 为了日志实现 `Display` |
| 空加密 | `Crypt::Off`，需要时再 `Default` | `enabled: bool` 加全零密钥 |
| 相等和拷贝 | magic、level、hour 是 `Copy` | 给 `FileManager`、编码器派生 `Copy` |

`ActiveAppendFile` 继续用 `File` 的 `Write` 和 `write_vectored`。它还有回滚长度和 `sync_data`，`BufWriter` 包不住这两件事，不换。

不为了测试做 `Clock` trait 或 `dyn Write` 落盘口。现有测试写临时目录，足够覆盖文件层。

## 明确不做

- 不做 `Appender<C: Compressor, K: Crypt, M: Mode>`。算法在 `init` 时选定，放进枚举；调度不和算法相乘。
- 不做 `Box<dyn StreamCompressor>`，也不做 sealed trait。压缩器没有外部实现者。
- 不把 `FileManager`、`PersistentBuffer`、`AppenderEngine` 抽成 trait。各只有一个实现。
- 不对引擎做公开 typestate。非法的 begin/append/finalize 会随这三套 API 一起消失。
- 不把 core 改成 async fn，不引入 Tokio。
- 不把命令通道改成有界，除非单独接受背压语义的变化。
- 不改 magic、头长、TEA、sync 明文、mmap 文件名和恢复策略。
- 不为每个整数做 newtype。偏移、容量、序号保持 `usize` / `u16` / `u32`。
- 这次不打开 workspace 级 `unwrap_used` / `unsafe_code` deny。那是独立的一次清理；mmap 和 `libc::gettid` 的 `unsafe` 仍留在原模块，并补上 SAFETY 注释。

## 分步

每一步都要让现有 `protocol_compat`、`decoder`、`compress_roundtrip`、`mmap_recovery`、`oneshot_flush`、`async_engine` 通过。磁盘字节不变。一步只做一类改动。

### 1. magic 收成枚举

改 `protocol.rs` 和 `decoder.rs` 的匹配。`LogHeader` 存 `BlockMagic`。编码仍写出原来的字节。解码用 `BlockMagic::try_from`。旧 magic 进入 `LegacyMagic`。

完成标准：fixture 往返字节相同；`select_magic` 的调用点改为枚举构造。

### 2. 错误保留来源

`CompressError`、`CryptoError`、`OneshotError` 使用 `#[source]`。删掉压缩路径上的 `to_string()`。`oneshot_flush` 改为 `Result`。`mars-xlog`、JNI、UniFFI、NAPI 用 `From` 转回 `FileIoAction`，对外整数码不变。

完成标准：调用方还能匹配「打开失败 / 没有可恢复数据」；`Error::source()` 能看到 `io::Error`。

### 3. 密钥和级别在边界解析

加入 `ZlibLevel`、`ZstdLevel`、`ServerPublicKey`、`PrivateKey`、`Hour`、`CacheDays` 的 `TryFrom` / `FromStr`。`Crypt` 替换 `EcdhTeaCipher` 的旗标。非法公钥的兼容回退只留在 `mars-xlog`。

`LogRecord` 的 id 改为 `Option<i64>`。`-1` 的解析放在 facade，一处完成。

完成标准：core 里不再出现「空字符串表示关闭加密」和「-1 表示稍后填充」。

### 4. 帧编码器从 facade 移入 core

新增 `frame.rs`，吃掉 `AsyncPendingState` 和同步组块。`AsyncEncoder` 通过 `Write::write_all` 驱动 flate2 / zstd。`mars-xlog` 调用 `FrameWriter`，引擎暂时仍接收它产出的字节。

完成标准：`rust.rs` 不再 match 压缩器，也不再手写 header；async/sync 集成测试和现有 benchmark 入口仍能编译。

### 5. 引擎只有一个文件所有者

拆开不可变路径配置和运行时 `Mutex`。删除引擎上的第二份 `FileManager`。查询 API 返回 `PathBuf`。`set_max_alive_time` 对过小的值返回错误。

完成标准：同步写和异步 flush 使用同一运行时状态；不改变单写者锁文件的行为。

### 6. 写入口收拢，解码写出 `Write`

在第 4 步稳定后，删除 `begin_async_pending`、`append_async_chunk`、`finalize_async_pending`、`write_async_pending`。解码输出改为 `&mut impl Write`，诊断文案收进一个函数。

完成标准：core 的公开写路径只剩封口块和 flush；CLI 解码结果与现有比对脚本一致。

### 7. 公开面整理

`lib.rs` 只重导出调用方还需要的类型：`FrameWriter`、`AppenderEngine`、`BlockMagic`、`decode_xlog`、文件与恢复。压缩器具体类型改为 `pub(crate)`，除非基准测试还要直接压一块缓冲。基准若需要，留一个小的公开函数，不留 trait。

`InstanceRegistry::get_or_insert_with` 改为锁内只查表，构造放到锁外，再复查。现在的闭包会在锁里做 mmap 和起线程。

完成标准：`mars-xlog` 之外没有代码再依赖压缩器 trait；注册表单测仍覆盖「失败不缓存」。

## 风险

- 第 4 步是行为风险最高的一步。移动时保持 TEA 尾块、序号跳过 0、sync payload 不压缩，并用现有 roundtrip fixture 对比字节。
- `TryFrom` 拒绝非法压缩级别之后，过去被 `clamp` 的配置会变成初始化错误。这是边界行为变化，要写进 `mars-xlog` 的错误类型，不能在 core 里悄悄夹紧。
- 路径从 `String` 改为 `PathBuf` 会动到 FFI。转换留在绑定层，不让 core 再 `to_string_lossy`。
- 不要把第 5 步的锁拆开到压缩之前以外的地方。文件游标的临界区仍然包含那一次追加。
