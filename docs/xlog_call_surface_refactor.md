# xlog Rust 调用面与行格式重构方案

仓库：`xlog-rs`。这一轮只改 `mars-xlog` 的调用面、明文行的所有权，以及行格式在 `open` 时怎么选定。磁盘格式不动。

约束来自 [rust_semantic_redlines.md](rust_semantic_redlines.md) 和上一轮 [xlog_core_rust_refactor.md](xlog_core_rust_refactor.md)。上一轮已经把 magic、压缩、加密、帧收成枚举和具体类型。本轮不重做那一层，也不把它的未收尾项（`FrameWriter` 搬进引擎）塞进来。

设计顺序遵循 [rust-best-practice](../.agents/skills/rust-best-practice/SKILL.md)：先值的流向，再封闭状态，再真正会替换的那一条 seam。trait 只给已经存在的第二种实现。不为 `Xlog` 或 `AppenderEngine` 加类型参数。

## 背景与目标

这一轮改三件事。

1. Rust 调用方拿一个拥有所有权的 `Xlog`，用少量方法写日志。热路径不再把 C 风格的位置参数和 `-1` 哨兵一路传到格式化函数。
2. 行格式是 `init` 时选定的值，不是 `Xlog` 或引擎的类型参数。默认就是现在的 Mars 行，字节不变。内建 Mars 布局在热路径上是一个具体函数。用户自定义布局只有在这次 `init` 真的传入时，才放进 `dyn`。
3. 一次 `log` 里，调用方的字符串只通过 `Event<'_>` 借用。格式化写入现有的可复用 `String`。同步路径把 `&[u8]` 交给 `FrameWriter`，借用在这次调用内结束。异步路径把池里的 `String` move 给 worker，用完还回池。异步命令、worker、`FrameWriter`、`Xlog` 都不带生命周期。

不改的部分：header、magic、序号、ECDH+TEA、mmap 文件名、恢复、oneshot 的连续追加、单写者锁文件、sync/fatal 的耐久语义、引擎那条无界命令通道。`appender_open` / `appender_close` / `instance()` / `RawLogMeta` / `FileIoAction` / 全局写保持为绑定用的兼容层，这一轮不删。`mars-xlog` 现有调用方式继续能编译。JNI、UniFFI、NAPI、Dart 的 Rust 桥在边界上转换，不改它们的源文件。

`dyn XlogBackend` 在发布面上只有一个实现。`Xlog` 改为直接持有具体的 `RustBackend`。C++ backend 不在 `mars-xlog` 的默认发布面上，见下文「现状」。删的是 trait，不是 `crates/xlog-sys`。

## 现状

### 从 `Xlog::log` 到文件

同步和异步在 `crates/xlog/src/backend/rust.rs` 的 `RustBackend::write_with_meta_internal` 分叉。引擎不负责格式化。

调用线程，同步：

1. `crates/xlog/src/lib.rs` `Xlog::log`。级别不够直接返回。`#[track_caller]` 取 `file` 和 `line`，函数名传 `""`。转到 `write_with_meta`。
2. `write_with_meta` 用 `RawLogMeta::default()`（`pid/tid/maintid = -1`，`trace_log = false`）调用 `write_with_meta_raw`。`tag == None` 时换成 `name_prefix`。
3. `Xlog.inner.backend` 的类型是 `Arc<dyn XlogBackend>`。trait 在 `crates/xlog/src/backend/mod.rs`。唯一实现是 `RustBackend::write_with_meta`。
4. `write_with_meta` 以 `MetaResolveMode::Category` 进入 `write_with_meta_internal`。
5. 再次检查级别。`console_open` 为真，或 Android 上 `raw_meta.trace_log` 为真时，调用 `mars_xlog_core::platform_console::write_console_line`。控制台文本和即将落盘的行不是同一份缓冲。
6. `resolve_record_meta` 把 `-1` 变成 `Option<i64>`。规则见「兼容层」。
7. `engine.mode() != Async` 时走同步。`with_hot_path_scratch` 取出线程局部 `HotPathScratch`（`line: String` 初始容量 16KiB，另有 `block` / `compress` / `crypto` 三个 `Vec<u8>`）。
8. `format_record_line_into` 调用 `mars_xlog_core::formatter::format_record_parts_into`。这个函数有 11 个参数，`#[allow(clippy::too_many_arguments)]`。
9. 临时值 `FrameWriter::sync(family, cipher).seal_sync_line(line.as_bytes(), hour, &mut scratch.block)`。`seal_sync_line` 在返回前把行复制进 `scratch.block`（头 + 明文 + `MAGIC_END`）。`FrameWriter` 不保存这段 `&[u8]`。
10. `AppenderEngine::write_sealed(block, level == Fatal)`，`crates/xlog-core/src/appender_engine.rs`。同步臂忽略 `force_flush`，调用 `FileManager::append_log_bytes(block, max_file_size, false, true)`，也就是 `move_file = false`、`keep_open = true`。不是 `append_log_bytes_durable`。短帧进 `ActiveAppendFile.write_buffer`，容量 `ACTIVE_APPEND_BUFFER_CAPACITY`（64KiB，`crates/xlog-core/src/active_append.rs`）。不满这一块时不会写到文件。`AppenderEngine::flush_with_reason` 只在模式已经是同步且参数 `sync == true` 时调用 `flush_active_file_buffer`。`flush(false)` 在同步模式下直接返回，不清这块缓冲。这就是现在的 sync：keep-open 的用户态缓冲，不是每条 fsync。测试要读文件必须 `flush(true)`，不能为了看见字节改掉 `keep_open`。

调用线程，异步，队列接受写时：

1. 同上，直到 `write_async_line`。
2. `SystemTime::now()` 和 `local_hour_from_timestamp` 在调用线程算好。
3. `take_line_buffer` 从当前线程固定的 shard（`current_tid() % 16`）弹出 `String` 并 `clear`；池空则 `String::with_capacity(512)`。池是 `ArrayQueue`，每 shard 最多 256 个，容量超过 8KiB 的不回收。
4. 格式化进这块 `String`。然后 move 进 `AsyncWriteCommand { line, pool_shard, now_hour, force_flush, profile }`。命令里没有 `&str`。
5. `std::sync::mpsc::sync_channel(65536)`。先 `try_send`，满了再 `yield`，zlib 重试 4 次、zstd 重试 1 次，然后阻塞 `send`。失败则把 `String` 还池，再走下面的 inline。
6. 线程 `xlog-rust-async-frontend` 的 `handle_async_frontend_write` 先看 `engine.mode()`。仍是异步时，用 `line.as_bytes()` 调用 `FrameWriter::push_line`，再 `AppenderEngine::write_async_parts`。mmap 追加发生在这根线程持有引擎 `state` 锁的时候，不经过引擎命令通道。`push_line` 返回后 `recycle_line_buffer_to_pool`。
7. 若这条命令还在队列里时 `set_appender_mode` 已经把引擎切到同步，同一函数走另一支：`build_sync_block_from_formatted_line` 对 `cmd.line` 调用 `seal_sync_line`，再 `write_sealed`。不重新格式化。然后照常还池。
8. 水位 `capacity * 4/5` 只在仍然走异步的那一支上发生。worker 不用这条已格式化的行，而写一条固定前缀的告警（`ASYNC_HIGH_WATERMARK_WARNING_PREFIX`，含历史拼写 `BUFFER_BLOCK_LENTH`）。这发生在布局之后，不经过 formatter。

异步 inline（worker 已不接收、入队失败，或测试里 `async_state.pending` 已有 `FrameWriter`）：调用线程自己 `checkout_async_state`，格式化进 `HotPathScratch.line`，`push_line(line.as_bytes())`。不 move `String`。今天 inline 会重新取 `SystemTime::now()`，和排队路径不是同一次取时。

引擎线程 `xlog-appender-engine`：`AppenderEngine::new` 里 `crossbeam_channel::unbounded()`。它做 flush、滚动、过期清理，不格式化，也不逐行压缩。`flush(true)` 的应答仍是容量 1 的 oneshot。

`Xlog` 本身是 `Arc<Inner>`，`Inner.backend` 又是 `Arc<dyn XlogBackend>`。注册表 `InstanceRegistry<RustBackend>` 存的是 `Weak<RustBackend>`（`crates/xlog-core/src/registry.rs`）。默认实例的槽是强 `Arc`。`Xlog::get` 每次新建一个 `Inner`，再把后端 `Arc` 涂成 trait object。`instance()` 返回的是 `RustBackend.id`（进程内递增的 `usize`），不是 C++ 指针。当前文档注释写错了。

### 浅的部分

位置参数。`write_with_meta`、`write_with_meta_raw`、`XlogBackend::write_with_meta`、`format_record_line_into`、`format_record_parts_into`、`write_async_line`、`write_async_line_inline`、`build_sync_block_into` 都是一长串 `level/tag/file/func/line/msg/pid/tid/maintid`。宏 `xlog!`、`XlogLayer::on_event`、JNI、UniFFI、NAPI、`packages/xlog/rust/src/lib.rs` 的 `mxl_logger_log_with_meta` 都停在 `write_with_meta`。

`-1`。`RawLogMeta` 的默认值是 `-1`。core 的 `LogRecord` 已经是 `Option<i64>`，formatter 用 `unwrap_or(运行时 id)`。哨兵只该留在 facade。今天它留在 `resolve_record_meta`，这一点是对的，但每个写入口都还要传 `RawLogMeta`。

`dyn XlogBackend`。`crates/xlog/src/backend/mod.rs` 在没有 `rust-backend` feature 时 `compile_error!`，文案写明 C++ backend 只留在仓库本地。`crates/xlog/Cargo.toml` 的 `default = ["rust-backend"]`，注释是 “Public release surface is Rust-only”。`mars-xlog` 不依赖 `mars-xlog-sys`。全仓库 `impl XlogBackend` 只有 `RustBackend`。`XlogBackendProvider` 同样只有 `RustBackendProvider`。trait 没有第二种生产实现。

格式化写死。热路径只有 `format_record_parts_into`。没有布局值。`LogRecord` 是拥有字符串的记录，不是热路径类型；热路径不经过它。

控制台是另一套格式，不能复用落盘行：

| 平台 | 函数 | 字节 |
| --- | --- | --- |
| Android | `write_android_line` | `[{file}:{line}, {func}]:{msg}`，空 tag 换成 `mars-xlog`，走 `__android_log_write` |
| Apple `OsLog` | `write_console_line` | 字段分传，不是一行文本 |
| Apple `NsLog`，以及非 Apple 桌面 | `format_basic_console_line` | `[{level}][{tag}][{file}:{line}, {func}][{msg}`，无时间、无 pid/tid，无换行约定 |
| Apple `Printf` | `write_console_line` 内的 `format!` | 带时间，但时间格式是 `%Y-%m-%d %z %H:%M:%S%.3f`，和 Mars 的 `{:+.1}` 小时偏移不同 |

`trace_log` 只在 Android 上能绕过 `console_open`。其它平台在 `write_with_meta_internal` 里被编成 `false`。

### 和已定方向、上一份方案不一致的地方

以代码为准。

- 上一份方案写「命令通道现在无界」。引擎通道 `xlog-appender-engine` 仍是 `unbounded()`。前端通道 `xlog-rust-async-frontend` 已经是 `sync_channel(65536)`，并且会在若干次 `try_send` 之后阻塞。这一轮两条都不改容量，也不改阻塞策略。
- `Xlog::write` 标了 `#[track_caller]`，但实际传入的是 `""`、`""`、`0`。记录下来的位置不是调用点。保持这个字节。
- CLI（`crates/xlog-cli/src/main.rs`）不调用 `appender_*` 或 `write_with_meta`。它只读文件，调用 `mars_xlog_core::decoder::decode_xlog`。这一轮 CLI 零改动。
- 上一份方案里的 `write_line` 收口（删掉 `write_async_parts`，把 `FrameWriter` 搬进引擎）没有做。引擎公开写入口仍是 `write_sealed` 和 `write_async_parts`。这一轮不收口。
- 没有第二个落盘目的地。控制台不是帧的 sink。metrics 是计数。dump 是解码。不引入 `Sink`。

## 设计顺序

按 skill 的五问，只回答这一轮。

1. 值从哪来，到哪去。调用方拥有 tag 和 message 的字符串，`Event<'_>` 只借用它们。`Xlog` 拥有 `Arc<RustBackend>`。`RustBackend` 拥有配置、`Crypt`、`AppenderEngine`、前端 worker、行池，以及一个在 `new` 之后不再变的 `Layout`。格式化在调用线程把借用写进一块已拥有的 `String`：同步用线程局部 `HotPathScratch.line`，异步用池里的 `String`。写完，`Event` 的借用结束。同步的 `&[u8]` 只活过 `seal_sync_line`。异步把 `String` move 进 `AsyncWriteCommand`，worker 用 `as_bytes()` 喂 `push_line`，然后还池。worker 看不到 `Event`。
2. 哪些状态真的存在。布局只有三种，而且一次 `init` 只占一种：`Mars`、一段已组装好的 `Piece` 列表、一个 `WriteLine`。压缩、加密、sync/async 仍是上一轮的枚举，不在这里再拆。`pid/tid/maintid` 在 `Event` 上是 `Option<i64>`：`None` 表示格式化时取运行时 id，`Some(-1)` 是数字 -1，不是哨兵。哨兵只存在于 `RawLogMeta`。没有公开的 Open/Sealed typestate。
3. 哪里要稳定，哪里可以替换。帧、压缩器、`Crypt`、`FileManager`、`PersistentBuffer`、`file_runtime`、`AppenderEngine` 都是具体类型，调用方不能替换。会替换的只有明文行怎么拼：Mars 是具体函数；`Piece` 是封闭枚举；用户函数是这一次 `init` 才出现的 `dyn WriteLine`。`dyn XlogBackend` 没有第二种实现，删掉。注册表继续是 `InstanceRegistry<RustBackend>`。
4. 哪里等待或并发。格式化是调用线程上的同步计算，不进 async。排队发生在已有的 frontend `sync_channel`。mmap 追加在 frontend worker。flush 和滚动在 `xlog-appender-engine`。这一轮不新增线程、运行时或通道。`WriteLine` 只在调用线程执行，必须是 `Sync`，因为 `Xlog` 可以被多线程同时 `log`。
5. 错误在哪变成给人看的结果。这一轮不改错误类型。初始化仍然是 `XlogError`。写路径今天忽略 `seal_sync_line` / `push_line` / `write_sealed` 的 `Err`（返回后当作没写入）。保持这个行为，不把热路径改成 `Result`。`FileIoAction` 仍只出现在 `oneshot_flush` 和绑定。core 不出现 `-1` 或整数状态码。

## 目标接口

### `Xlog`

`RustBackend` 升为 `pub(crate)`。`crates/xlog/src/backend/mod.rs` 再 `pub(crate) use rust::RustBackend`。删掉 `XlogBackend` 和 `XlogBackendProvider`。`provider()` 的返回类型改成 `&'static RustBackendProvider`。

```rust
#[derive(Clone)]
pub struct Xlog {
    inner: Arc<RustBackend>,
}

impl Xlog {
    pub fn init(config: XlogConfig, level: LogLevel) -> Result<Self, XlogError> {
        Self::init_with_layout(config, level, Layout::mars())
    }

    pub fn init_with_layout(
        config: XlogConfig,
        level: LogLevel,
        layout: Layout,
    ) -> Result<Self, XlogError>;

    /// 注册表里那份 `Arc<RustBackend>` 的 clone。没有活实例时是 `None`。
    pub fn get(name_prefix: &str) -> Option<Self>;

    #[track_caller]
    pub fn log(&self, level: LogLevel, tag: Option<&str>, msg: impl AsRef<str>);

    pub fn write_event(&self, event: &Event<'_>);

    // 以下签名保持不变，函数体改成包装。见「兼容层」。
    pub fn write(&self, level: LogLevel, tag: Option<&str>, msg: &str);
    pub fn write_with_meta(&self, level: LogLevel, tag: Option<&str>, file: &str, func: &str, line: u32, msg: &str);
    pub fn write_with_meta_raw(&self, ..., raw_meta: RawLogMeta);
}
```

`Xlog::new` 继续是 `#[doc(hidden)]`，行为等于 `init`（Mars 布局）。

`get` 的返回值是 `Option<Xlog>`，不是 `Option<Arc<dyn XlogBackend>>`。`Xlog` 的 `Clone` 只增加 `RustBackend` 的引用计数。注册表仍然只持有 `Weak`。最后一个 `Xlog` 丢掉后，`get` 得到 `None`，和现在一样。同一 `name_prefix` 上 `instance()` 仍然相等。多出来的那层 `Arc<Inner>` 不再分配。

`name_prefix` 不再复制到 `Inner`。包装层需要 tag 回落时，借用 `RustBackend.config.name_prefix`。`config` 在 `new` 之后不改，读它不加锁。

级别、flush、`set_appender_mode`、控制台开关、文件大小、存活时间这些方法留在 `Xlog` 上，直接调 `RustBackend` 的固有方法。签名不变。

`init_with_layout` 的身份规则：

- `XlogConfig` 的 `PartialEq` 不包含布局。冲突仍然只看现在的配置字段。
- 注册表里还没有这个 `name_prefix` 时，`layout` move 进 `RustBackend::new_with_layout`。之后没有 `set_layout`。热路径读 `&self.layout`，不加锁。
- 已经有活实例且配置相同：沿用第一次的布局，丢掉这次传入的 `layout`，仍然 `set_level`。这和级别不同。级别是运行时旋钮；布局换掉要在热路径上替换 `dyn`，这一轮不做。
- 两个线程同时第一次 `init`：`get_or_try_insert_with` 的现有语义让一个胜出。败者的 `layout` 随未调用的闭包丢掉。
- `appender_open` 只创建 `Layout::mars()`。全局 appender 这一轮不能带自定义布局。要自定义布局就走 `init_with_layout`，用命名实例。

`RustBackend::new(config, level)` 保留两参数，内部调用 `new_with_layout(..., Layout::mars())`。现有测试不用改构造签名。

### `Event`

定义在 `crates/xlog/src/event.rs`，由 `lib.rs` 公开。`level` 用 `mars_xlog::LogLevel`，不把 core 的同名枚举漏进公开字段。两套枚举这一轮不合并；`From` 已经是一对一，留在边界用一次。

```rust
#[derive(Clone, Copy)]
pub struct Event<'a> {
    pub level: LogLevel,
    pub tag: &'a str,
    pub file: &'a str,
    pub func: &'a str,
    pub line: u32,
    pub message: &'a str,
    pub pid: Option<i64>,
    pub tid: Option<i64>,
    pub maintid: Option<i64>,
    pub timestamp: SystemTime,
}

impl<'a> Event<'a> {
    /// `file` / `func` 为空，`line` 为 0，三个 id 为 `None`，`timestamp` 为现在。
    pub fn new(level: LogLevel, tag: &'a str, message: &'a str) -> Self;
}
```

没有 `trace_log` 字段。没有 `Option` tag。`tag == None` 的回落在 `log` / `write` / `write_with_meta*` 里做完，再放进 `Event`。空字符串是空 tag，不会被换成 `name_prefix`。

`log` 的填充：`tag.unwrap_or(name_prefix)`，`file = Location::caller().file()`，`func = ""`，`line = Location::caller().line()`，三个 id 为 `None`，`timestamp = SystemTime::now()`。级别不够时连 `Location` 都不取，和现在一样。

`write_event` 使用调用方放进 `Event` 的时间戳，不再内部改写。`trace_log` 固定当 `false`。

### `Layout`、`Piece`、`WriteLine`

定义在 `crates/xlog/src/layout.rs`。

```rust
pub enum Piece {
    Literal(&'static str),
    Level,
    Time,
    Pid,
    Tid,
    TidSuffix,
    Tag,
    FileName,
    LineNumber,
    Func,
    Message,
}

pub trait WriteLine: Send + Sync {
    fn write_line(&self, out: &mut String, event: &Event<'_>);
}

pub enum Layout {
    Mars,
    Pieces(Box<[Piece]>),
    Custom(Arc<dyn WriteLine>),
}

impl Layout {
    pub fn mars() -> Self;
    pub fn pieces(pieces: impl Into<Box<[Piece]>>) -> Self;
    pub fn custom(writer: Arc<dyn WriteLine>) -> Self;
}
```

`Literal` 只要 `'static`，所以 `Layout` 自己没有生命周期，可以放进 `RustBackend`。需要拥有的动态文本时用 `WriteLine`，不要把 `String` 塞进 `Piece`。

`Layout` 不实现 `Eq`。不要为了放进 `XlogConfig` 去手写比较。

### 热路径函数

core 侧，替换 `format_record_parts_into` 的 11 参数调用。类型放在 `crates/xlog-core/src/formatter.rs`。它不是第二个公开的「事件」，只是 formatter 的入参包。公开调用方不构造它。

```rust
pub struct MarsFields<'a> {
    pub level: LogLevel, // mars_xlog_core::record::LogLevel
    pub tag: &'a str,
    pub file: &'a str,
    pub func_name: &'a str,
    pub line: i32,
    pub timestamp: SystemTime,
    pub pid: Option<i64>,
    pub tid: Option<i64>,
    pub maintid: Option<i64>,
    pub body: &'a str,
}

pub fn write_mars_line(out: &mut String, fields: &MarsFields<'_>);
```

函数体就是今天 `format_record_parts_into` 的函数体。`format_record_parts_into`、`format_record_into`、`format_record` 改成构造 `MarsFields` 再调用它。基准和 `examples/bench_components.rs` 不用改调用形式。

`line` 保持 `i32`，因为 `LogRecord.line` 是 `i32`，今天就会把负数打印出来。facade 的 `Event.line: u32` 在边界做一次 `as i32`，和今天 `format_record_line_into` 里的 `line as i32` 相同。

另外导出两个已经存在、但现在是私有的动作，供 `Piece` 和 Mars 共用，避免两套时间缓存和两套 id 规则：

```rust
pub fn write_time(out: &mut String, timestamp: SystemTime);
pub fn resolve_runtime_ids(pid: Option<i64>, tid: Option<i64>, maintid: Option<i64>) -> (i64, i64, i64);
```

`write_time` 就是现在的 `format_time_into`，含线程局部秒级前缀缓存。`resolve_runtime_ids` 就是现在的 `resolve_id(id, runtime)`：`None` 取 `std::process::id()`、`current_tid()`、`main_tid()`。

facade 热路径，替换 `format_record_line_into`：

```rust
fn format_line(layout: &Layout, out: &mut String, event: &Event<'_>) {
    out.clear();
    match layout {
        Layout::Mars => {
            let fields = mars_fields(event); // LogLevel::from + line as i32，只重新借用 &str
            write_mars_line(out, &fields);
        }
        Layout::Pieces(pieces) => write_pieces(out, pieces, event),
        Layout::Custom(writer) => writer.write_line(out, event),
    }
}

fn mars_fields<'a>(event: &'a Event<'a>) -> MarsFields<'a>;
```

`Mars` 不进 `Piece` 循环，也不进 `dyn`。`match` 的这一臂是直接调用。`write_mars_line` 自己还会 `out.clear()` 一次，core 的直接调用者行为不变。

`From<LogLevel> for CoreLogLevel` 从 `backend/rust.rs` 挪到 `lib.rs` 里枚举旁边。匹配表不变。`layout.rs` 用这一次转换取 `short()`。

删掉 `RustBackend::format_record_line_into`。

内部写入口收成：

```rust
fn emit(&self, event: &Event<'_>, trace_log: bool);

fn write_sync_line(&self, event: &Event<'_>);
fn write_async_line(&self, event: &Event<'_>);
fn write_async_line_inline(&self, event: &Event<'_>);

fn build_sync_block_into<'a>(
    &self,
    scratch: &'a mut HotPathScratch,
    event: &Event<'_>,
    profile: Option<&mut SyncBuildStage>,
) -> Option<&'a [u8]>;
```

`build_sync_block_into` 上的生命周期只属于返回的 `scratch.block`，和现在一样。它不出现在 `Xlog`、`FrameWriter` 或异步命令上。

`emit` 的顺序：级别检查；按现有条件写控制台；`engine.mode()` 为异步则 `write_async_line`，否则 `write_sync_line`。一次 `emit` 只用 `event.timestamp` 这一个时间，包括入队失败后的 inline 回退。今天回退会再取一次 `now()`。那只影响队列满或 worker 不再接收的路径，正常入队的时间不变。换掉是为了让 `write_event` 带上的时间戳在回退里仍然有效。现有测试不断言这条路径的时间戳。

### 兼容包装

`MetaResolveMode` 和 `resolve_record_meta` 继续是 `RustBackend` 的私有项，不提升到 `pub(crate)`。`Xlog` 上看不到它们。`lib.rs` 里的公开函数只转发参数。`Event` 在 `RustBackend` 里填。

```rust
impl Xlog {
    pub fn write_with_meta_raw(&self, level, tag: Option<&str>, file, func, line, msg, raw_meta: RawLogMeta) {
        self.inner.emit_category(level, tag, file, func, line, msg, raw_meta);
    }

    pub fn appender_write_with_meta_raw(level, tag: Option<&str>, file, func, line, msg, raw_meta: RawLogMeta) {
        // tag == None 时用 ""，不是 name_prefix。然后交给 provider。
        provider().write_global_with_meta(level, tag.unwrap_or(""), file, func, line, msg, raw_meta);
    }
}

impl RustBackend {
    fn emit_category(&self, level, tag: Option<&str>, file, func, line, msg, raw_meta: RawLogMeta) {
        let (pid, tid, maintid) = self.resolve_record_meta(raw_meta, MetaResolveMode::Category);
        let event = Event {
            level,
            tag: tag.unwrap_or(&self.config.name_prefix),
            file,
            func,
            line,
            message: msg,
            pid,
            tid,
            maintid,
            timestamp: SystemTime::now(),
        };
        self.emit(&event, raw_meta.trace_log);
    }

    fn emit_global(&self, level, tag: &str, file, func, line, msg, raw_meta: RawLogMeta) {
        let (pid, tid, maintid) = self.resolve_record_meta(raw_meta, MetaResolveMode::Global);
        let event = Event {
            level,
            tag,
            file,
            func,
            line,
            message: msg,
            pid,
            tid,
            maintid,
            timestamp: SystemTime::now(),
        };
        self.emit(&event, raw_meta.trace_log);
    }
}

impl RustBackendProvider {
    fn write_global_with_meta(&self, level, tag: &str, file, func, line, msg, raw_meta: RawLogMeta) {
        let Some(default) = registry().default_instance() else { return };
        default.emit_global(level, tag, file, func, line, msg, raw_meta);
    }
}
```

`emit_global` 必须把 `raw_meta.trace_log` 传给 `emit`。JNI、UniFFI、NAPI 都会把 `trace_log` 放进 `appender_write_with_meta_raw`。全局写不能写死 `false`。非 Android 上 `emit` 仍把这个标志编成不绕过控制台，和今天 `write_with_meta_internal` 的 `cfg` 一样。

`RustBackend::write_with_meta` 继续只表示 Category。不要让它兼做全局写。全局写留在 `RustBackendProvider::write_global_with_meta`。

## 数据流与所有权

```
调用方字符串 (tag, message, file, func)
        |  只借用
        v
Event<'a>                         哨兵 -1 不进这里
        |
        |  emit：级别、控制台（另一块分配，见下）
        |
        +-- 同步，调用线程
        |     HotPathScratch.line.clear 已由 format_line 做
        |     format_line(&mut line, &event)     <-- Event 借用在这里结束
        |     FrameWriter::sync.seal_sync_line(line.as_bytes())
        |         返回前复制到 scratch.block；FrameWriter 是临时值
        |     AppenderEngine::write_sealed(block)
        |         FileManager::append_log_bytes(..., keep_open = true)
        |
        +-- 异步排队，调用线程
        |     take_line_buffer -> String
        |     format_line(&mut line, &event)     <-- Event 借用在这里结束
        |     move String 进 AsyncWriteCommand   命令无生命周期
        |     sync_channel(65536)
        |           |
        |           v
        |     xlog-rust-async-frontend
        |     若 engine.mode() 仍是 Async：
        |         push_line(line.as_bytes())     &[u8] 在 push_line 返回时结束
        |         write_async_parts -> PersistentBuffer / mmap
        |     若入队之后引擎已被切到 Sync：
        |         build_sync_block_from_formatted_line(cmd.line)
        |         seal_sync_line + write_sealed
        |         不调用 format_line、Layout、Event
        |     然后 recycle String（clear，容量 <= 8KiB 才入池）
        |
        +-- 异步 inline，调用线程
              与同步相同：scratch.line，借用结束于 push_line
              不占用行池

xlog-appender-engine（无界 crossbeam channel）
    只收 flush / stop。不收 Event，不收未格式化的字符串。
```

借用结束的位置就两处：`format_line` 返回，以及同步 `seal_sync_line` / 异步 `push_line` 返回。后者借的是已经拥有的 `String` 的字节，不是调用方的 `&str`。

`set_appender_mode` 从异步切到同步时，队列里已经格式化好的 `AsyncWriteCommand` 仍由 `handle_async_frontend_write` 消化。这一支保持今天的代码：`engine.mode() != EngineMode::Async` 时调用 `build_sync_block_from_formatted_line`，对 `cmd.line.as_bytes()` 做 `seal_sync_line`，再 `write_sealed`。worker 上没有 `Event`，也不能在这里补一次 `format_line` 或 `Layout`。删掉这一支会把已经入队的行丢到异步 mmap 路径上。`WriteLine` 仍然只在调用线程执行。

仍然会发生的复制，以及原因：

- message 写进行缓冲。行是拼接结果，不是调用方字符串的别名。这一次 `push_str` 躲不掉。
- 同步 `seal_sync_line` 把行复制进带协议头的 `scratch.block`。帧不是原文字符串。
- `write_sealed` / `write_async_parts` 把帧字节写入文件或 mmap。这是目的地要的副本。
- 异步排队路径不再克隆那块 `String`。move 到 worker，再 move 回池。
- 入队失败时，已格式化的 `String` 还池，inline 用同一 `Event` 再格式化一次。调用方字符串在 `emit` 返回前一直有效，所以做得到，不必把原始字段也 move 进命令。这是今天的双次格式化，保持。
- 控制台按平台另分配一份文本。它的字节和落盘行不同，不能改成克隆 `scratch.line`。
- 水位告警在 worker 上用 `format!` 分配一条短字符串，替换即将压缩的明文。保持，不做成 `Piece`。
- tag 的 `name_prefix` 回落是借用 `config` 里已有的 `String`，不额外分配。

`CheckedOutAsyncState<'a>` 上的生命周期是 inline 路径把 `FrameWriter` 还回 mutex 用的，现有结构。不要把它扩散到命令或 `Xlog`。不要为了去掉这个内部生命周期去改 checkout。

## 行格式

### Mars 行，字节与今天一致

`write_mars_line` 的段顺序就是今天 `format_record_parts_into` 的顺序。热路径不把这些段做成 `Vec<Piece>` 再解释。

1. `[`
2. `level.short()`，一个字母：`V D I W E F N`
3. `][`
4. 时间：`YYYY-MM-DD {偏移} HH:MM:SS.mmm`。偏移是 `(local_minus_utc as f64) / 3600.0`，格式 `{:+.1}`。同一秒的前缀走现有线程局部缓存。`UNIX_EPOCH` 之前走现在的 fallback 分支。
5. `][`
6. 解析后的 pid，十进制
7. `, `
8. 解析后的 tid，十进制
9. tid 与 maintid 相等时是 `*`，否则空
10. `][`
11. tag，原样
12. `][`
13. `extract_file_name(file)`，只保留最后一个 `/` 或 `\` 之后的部分
14. `:`
15. `line`，按 `i32` 打印
16. `, `
17. `func_name`。空字符串就空着，逗号和空格仍在。结果像 `[c.rs:42, ]`
18. `][`
19. body
20. 换行，规则见下

截断和换行只写在 `write_mars_line` 里，不做成通用 `Piece`，也不放到 `format_line`。

- body 上限：`min(0xFFFF, saturating(16 * 1024 - out.len() - 130))`。按 UTF-8 边界截，不拆码点。常量保持 `MAX_LOG_BODY_BYTES`、`LEGACY_STACK_BUFFER_BYTES`、`LEGACY_BODY_RESERVED_BYTES`。
- 若结果不以 `\n` 结尾且长度 `< 16KiB`，补一个 `\n`。
- 若长度仍 `> 16KiB`，把整行截到 16KiB，同样不拆 UTF-8。

`Layout::pieces` 和 `WriteLine` 不套用这三条。`MAX_LOG_BODY_BYTES` 和 16KiB 整行上限只存在于 `write_mars_line`。这一轮不加新的自定义行上限。兼容测试只锁 `Layout::mars()`。

两条路径对超长明文的拒绝不是同一个错误，也不要把 `FrameError::LengthOverflow` 当成共用上限：

- 同步：`FrameWriter::seal_sync_line` 里 `u32::try_from(line.len())` 失败才返回 `FrameError::LengthOverflow`。短于 4GiB 的明文会封进块。`push_line` 不返回这个错误。
- 异步：`push_line` 不检查明文长度。压缩后的 pending 块仍走 `PersistentBuffer::append_to_pending_with_flush`。放不进 mmap 容量是 `BufferError::BlockTooLarge`（默认 `DEFAULT_BUFFER_BLOCK_LEN` 为 150KiB）。头里的长度字段写不进 `u32` 才是 `BufferError::BlockLenOverflow`。都不是 `FrameError::LengthOverflow`。

长于 16KiB、又能放进 mmap 的自定义行，今天两条路径都接受。测试不要期待 `LengthOverflow`。模式切换后那一支因为走的是 `seal_sync_line`，才可能对已经格式化好的字节返回 `LengthOverflow`；那仍然不是 Mars 的 16KiB 上限。

### `Piece` 怎么拼

`write_pieces` 是 `layout.rs` 的私有函数。它只追加，清空由 `format_line` 做。每个变体：

| `Piece` | 写入 |
| --- | --- |
| `Literal` | `push_str` |
| `Level` | core `LogLevel::short()` |
| `Time` | `write_time` |
| `Pid` / `Tid` | `resolve_runtime_ids` 之后的十进制 |
| `TidSuffix` | 与 Mars 相同的 `*` 或空 |
| `Tag` | `event.tag` |
| `FileName` | `extract_file_name(event.file)` |
| `LineNumber` | `event.line` 的十进制（`u32`，不经 `as i32`） |
| `Func` | `event.func` |
| `Message` | `event.message`，不截断 |

括号、逗号、空格由 `Literal` 提供。用 `Piece` 拼一份看起来像 Mars 的行，不会自动得到 Mars 的截断和换行。要字节兼容就用 `Layout::mars()`，不要用 `Piece` 去仿真。

这一轮没有格式串。不提供 `Layout::parse`。调用方在代码里组装 `Layout::pieces([...])`。格式串如果以后要做，也是在 `init` 时解析一次成 `Box<[Piece]>`，不进热路径。文法现在不定。

### `WriteLine` 存在哪

只有 `Layout::Custom` 持有 `Arc<dyn WriteLine>`。`format_line` 在调用前已经 `out.clear()`。实现可以再 `clear` 一次。不要假定缓冲里有上一条日志。

`write_line` 在调用线程执行。不要在里面等待 worker，也不要保存 `Event` 里的引用。`Send + Sync` 是因为 `Xlog` 跨线程共享，不是因为 worker 会回调它。

自定义行只替换 payload 里的明文。`FrameWriter`、magic、序号、加密、mmap 都不看布局。

### 控制台

不改 `platform_console.rs` 的输出字节。`emit` 在 `format_line` 之前，用 `Event` 的原始字段调用现有的 `write_console_line`（file 传未裁剪的路径，函数内部自己取 basename）。

- Android：仍然是 logcat 那条短格式。`trace_log` 仍只在 Android 上、且只从 `RawLogMeta` 进来时，才能在 `console_open == false` 时打控制台。`write_event` / `log` 不带这个标志。
- Apple `OsLog`：仍然分字段。
- Apple `NsLog` 和桌面 stderr：仍然是 `format_basic_console_line`。
- Apple `Printf`：仍然是它自己的时间格式。

自定义布局不改变控制台行。控制台看的是 `Event`，不是 payload。

水位告警替换的是即将压缩的明文，Mars 和自定义布局都会被替换。这是现有异步行为，不是新的布局规则。前缀字符串一个字都不改。

## 兼容层

`-1` 只在 `resolve_record_meta` 解释。

| 模式 | 谁走进来 | 规则 |
| --- | --- | --- |
| `Category` | `write_with_meta_raw`、因此也包括 `log` / `write` / `write_with_meta` / 宏 / tracing / JNI 与 NAPI 的实例写 | 三个字段都是 `-1` 时得到 `(None, None, None)`，格式化时再取运行时 id。否则三个都保留为 `Some`，哪怕其中有 `-1`。`Some(-1)` 会印成 `-1` |
| `Global` | `appender_write_with_meta_raw` | 每个字段单独看。`-1` 变成 `None`，其它值变成 `Some` |

`RawLogMeta::default()` 是三个 `-1`。所以现有的 `log` / `write_with_meta` 在 Category 下仍然是「运行时填 id」。这个默认不要改成单独填某个字段。

| 旧入口 | 这一轮 | 转到 |
| --- | --- | --- |
| `Xlog::init` | 签名不变 | `init_with_layout(..., Layout::mars())` |
| `Xlog::new` | 保持 `doc(hidden)` | 同 `init` |
| `Xlog::get` | 仍是 `Option<Xlog>` | `registry().get` 的 `Arc<RustBackend>` |
| `Xlog::log` | 签名不变 | `Event` + `emit(..., trace_log: false)` |
| `Xlog::write` | 签名不变 | `file = ""`，`func = ""`，`line = 0`。不使用 `Location` |
| `Xlog::write_with_meta` | 签名不变，变成包装 | `RawLogMeta::default()`，Category |
| `Xlog::write_with_meta_raw` | 签名不变，变成包装 | Category，`trace_log` 传入 `emit` |
| `Xlog::appender_write_with_meta_raw` | 保持 `doc(hidden)` | `tag == None` 时用 `""`。`RustBackendProvider::write_global_with_meta` 调 `emit_global`：`MetaResolveMode::Global`，并且 `emit(&event, raw_meta.trace_log)` |
| `Xlog::appender_open` / `appender_close` | 保持 `doc(hidden)` | `Layout::mars()`。`close` 仍只清默认槽 |
| `Xlog::flush_all`、路径 / oneshot / dump | 签名不变 | 仍是 `RustBackendProvider` 的固有方法，不挪到 `RustBackend` |
| `Xlog::set_console_fun` | 保持 `doc(hidden)`，仍只在 Apple 目标编译 | 仍调 `RustBackendProvider::set_console_fun`，再转到 `set_apple_console_fun`。不要掉这一钩子 |
| `Xlog::instance` | 加上 `doc(hidden)`，仍返回 `usize` | `RustBackend.id`。注释改成进程内 id，不要再写 C++ 句柄 |
| `RawLogMeta` / `FileIoAction` | 不删，不改字段 | 哨兵和整数码留在这一层 |
| `xlog!` 及四个级别宏 | 宏语法不变 | 仍调用 `write_with_meta` |
| `XlogLayer` | 不改 | 仍调用 `write_with_meta` |
| `crates/xlog-android-jni` | 不改这个 crate | `write_with_meta` / `write_with_meta_raw` / `appender_*` |
| `crates/xlog-uniffi` | 不改 | 同上 |
| `crates/mars-xlog-harmony-napi` | 不改 | 同上 |
| `packages/xlog/rust` | 不改 | `mxl_logger_log` 走 `write`，`mxl_logger_log_with_meta` 走 `write_with_meta` |
| `crates/xlog-cli` | 不改 | 只 `decode_xlog` |
| `Xlog::init_with_layout` / `Xlog::write_event` | 新增 | 见上 |

`rust-backend` feature 保留。绑定 crate 的 `features = ["rust-backend"]` 不动。没有这个 feature 时的 `compile_error!` 保留。不要在这一轮把 `mars-xlog-core` 改成无条件依赖。

## 明确不做

- 不改 header、magic、序号、ECDH+TEA、sync 明文、mmap 文件名、恢复策略、oneshot 连续追加。这些是协议和恢复红线。
- 不改单写者锁文件。`FileManager::new` 的 `<name_prefix>.lock` 保持。
- 不把 sync 改成每条 fsync，也不把 fatal 改成「每条都 durable」。同步臂继续 `append_log_bytes(..., keep_open = true)`，64KiB 用户态缓冲保持。测试读不到文件时调用 `flush(true)` 或 `flush_all(true)`，不要把 `keep_open` 改成 `false`。fatal 继续只作为现有的 `force_flush` 传入 `write_sealed` / `push_line`。
- 不把 `xlog-appender-engine` 的 `unbounded()` 改成有界。那会改变背压。
- 不改 frontend `sync_channel(65536)` 的容量、重试次数和阻塞 `send`。它已经有界；红线指的是引擎那条，不是把两条通道拧成同一种。
- 不引入 Tokio，不在 core 或 facade 热路径加 `async fn`。现有模型是 `std::thread` 加通道。
- 不把 `Compressor`、`FileManager`、`PersistentBuffer`、`AppenderEngine`、`file_runtime` 做成 trait。各只有一个实现。
- 不做 `Appender<C, K, M>`，不做 `Chain<A, B>`，不给 `Xlog` 加布局类型参数。算法和布局都在 `init` 时定成值。
- 不做公开 typestate。`FrameWriter` 的 open/seal 继续是内部枚举。
- 不把生命周期放上 `AsyncWriteCommand`、frontend worker、`FrameWriter` 或 `Xlog`。调用方字符串活不过 `format_line`。
- 不引入 `Sink`。没有第二个要写帧的目的地。控制台不是 sink。
- 不把格式化挪到 worker。那样要么给命令加生命周期，要么在入队前把每段字符串都克隆一份。
- 不在这一轮做格式串文法。`Piece` 由代码组装。
- 不删 `appender_open` / `appender_close` / `instance` / `RawLogMeta` / `FileIoAction` / 全局写，也不改它们的签名。绑定还在用。
- 不删 `mars-xlog-sys`，也不把它接回 `mars-xlog`。它不在发布面上。
- 不改控制台字节去复用 Mars 行。四套控制台格式都和落盘行不同。
- 不合并 `mars_xlog::LogLevel` 和 `mars_xlog_core::record::LogLevel`。绑定和公开 API 用的是前者。合并是另一次全仓库改名。
- 不把 `FrameWriter` 搬进 `AppenderEngine`，也不删 `write_async_parts`。那是上一轮留下的引擎内部收口，不是这一轮的调用面。
- 不改水位告警的前缀文本。外部有人按 `BUFFER_BLOCK_LENTH` 这条历史拼写检索。
- 不把 `Xlog::write` 改成记录真实调用点。今天的空 file、空 func、`line = 0` 是实际字节。
- 不给热路径新的 `Result`。写失败仍然吞掉，和现在一样。
- 不宣称更快。没有作为本轮门槛的基准数字。

## 测试与语义红线

下面这些必须保持通过，且不改断言的协议含义：

- `crates/xlog-core/tests/` 下的 `protocol_compat`、`decoder`、`compress_roundtrip`、`mmap_recovery`、`oneshot_flush`、`async_engine`
- `formatter.rs` 里的 `format_includes_expected_fields`、`format_truncates_oversized_body_on_utf8_boundary`、`format_time_matches_reference_for_cached_and_fallback_paths`
- `platform_console.rs` 里 `format_basic_console_line_matches_stderr_layout` 和空 func 那条
- `backend/rust.rs` 里 Category / Global 的 `-1` 测试、主线程 `*`、sync 加密块的 magic、async zlib/zstd 流、水位、行池 shard
- `lib.rs` 里 `init` 复用、配置冲突、`appender_open` 冲突
- `tracing_layer.rs` 里不改 logger 级别的那两条

`format_record_parts_into` 变成包装之后，原有 formatter 测试仍然打在同一份实现上。

新增：

- `formatter.rs`：`mars_line_matches_legacy_segments`。固定时间戳、pid 12、tid 34、maintid 34、tag `core`、文件 `c.rs`、func `module::f`、body `msg`。用 `write_time` 生成时间子串，断言整行等于 `[E][{time}][12, 34*][core][c.rs:42, module::f][msg\n`。body 后面没有 `]`。`format_record_parts_into` 在 `][` 之后直接写 body，再至多补一个 `\n`。不要为了让测试通过去改 formatter。超长 body 另断言 `<= 16KiB` 且是合法 UTF-8，对应函数里先补换行、再在 `len > 16KiB` 时截断。这锁的是段顺序和换行，不是时区里的绝对字符串。
- `mars-xlog` 的布局测试都显式 `.mode(AppenderMode::Sync)`，不设公钥。`XlogConfig::new` 的默认是异步、zlib、无公钥，magic 会是 `MAGIC_ASYNC_NO_CRYPT_ZLIB_START`（0x09），payload 是压缩流，不能直接当明文看。无公钥的同步 zlib 是 `MAGIC_SYNC_NO_CRYPT_ZLIB_START`（0x08）。`MAGIC_SYNC_ZLIB_START`（0x06）只在同步且加密时出现。读文件之前对这个 `Xlog` 调用 `flush(true)`。`flush(false)` 不会把 64KiB keep-open 缓冲写进文件，一条 `hello\n` 或一行 Mars 因此不在磁盘上。比较的是 flush 之后 header 与 `MAGIC_END` 之间的明文，不比较整个文件。不要把 `keep_open` 改成 `false` 来让字节出现。
- 默认 `Xlog::init`（不传 layout）在上述同步配置下，`flush(true)` 之后明文以 `[I][` 开头。这锁「默认布局就是 Mars」，并且头没有被布局碰到。
- `Layout::pieces([Message, Literal("\n")])`：同一同步配置，`flush(true)` 之后明文等于 `hello\n`，magic 为 `MAGIC_SYNC_NO_CRYPT_ZLIB_START`。
- `Layout::custom` 只写 message：`flush(true)` 之后明文等于写入的字节，magic 同上。控制台形状仍测 `format_basic_console_line` 这个纯函数，不测 stderr。
- `mars-xlog`：`write_event_drops_caller_string_before_flush`。在块里用局部 `String` 调用 `write_event`，离开块后 `flush(true)`。同步用例同样依赖这次 `flush(true)` 才把 keep-open 缓冲落盘，然后看 header 与 `MAGIC_END` 之间的明文；异步用例先按 magic 解压再找文本。两路都要含这段文本。这锁「借用在 `write_event` 返回前结束」。`flush(false)` 不能代替。
- 已有的 Category / Global 单测不改期望，也不要并成一次输入。`category_mode_only_fills_when_all_meta_are_minus_one` 对 `RawLogMeta::new(123, -1, -1)` 期望 `(Some(123), Some(-1), Some(-1))`。`global_mode_fills_each_missing_meta_field_independently` 对 `RawLogMeta::new(321, -1, -1)` 期望 `(Some(321), None, None)`。它们直接调 `resolve_record_meta`，覆盖不到包装是否走了 Global。
- 新增 `appender_write_uses_global_meta_rule`，放在 `crates/xlog/src/lib.rs` 的测试模块里，紧挨 `appender_open_rejects_conflicting_config_when_default_exists`。不要放进 `backend/rust.rs`：`APPENDER_TEST_LOCK` 是 `lib.rs` 测试里的私有 `OnceLock<Mutex<()>>`，后端测试看不见。`appender_open` 只 `registry().set_default`，不把实例插进命名表，所以 `Xlog::get(name_prefix)` 是 `None`，不能靠它 `flush`。测试拿 `appender_test_lock()`，用现有的 `AppenderCloseGuard` 在 drop 时 `appender_close`，进入时再主动 `appender_close` 一次，避免和并行的全局 appender 测试抢默认槽。配置是同步、无公钥。`appender_write_with_meta_raw` 传入 `RawLogMeta::new(321, -1, -1)`。读目录之前调用 `Xlog::flush_all(true)`，不是 `flush(false)`。同步明文里要有 `321`，tid 位置不是字面量 `-1`（Global 把这个 `-1` 填成运行时 tid）。若包装误用 Category，tid 会印成 `-1`。`trace_log` 不在这条测试里另造 Android 断言；函数必须仍把标志传进 `emit`，由现有的 `cfg(target_os = "android")` 决定是否绕过控制台。

基准不是合并条件。要看编译是否还在，跑已有入口：`cargo bench -p mars-xlog --bench criterion_write_path` 和 `cargo bench -p mars-xlog-core --bench criterion_components`。它们应仍调用 `write_with_meta` 和 `format_record_parts_into`。这一轮不记录、不承诺数字。

语义红线的回归点不变：锁文件、oneshot/recovery 的连续追加、sync 不是每条 durable。这一轮的 PR 不改这些文件的行为。若某个 PR 的 diff 碰到 `protocol.rs`、`crypto.rs`、`mmap_store.rs`、`recovery.rs`、`oneshot.rs`、`file_manager.rs` 的锁或追加逻辑，这个 PR 就越界了。

## 分步

每步都让上一节列出的现有测试通过。一步只动一种东西。

### 1. core 收拢 Mars 行

在 `formatter.rs` 增加 `MarsFields`、`write_mars_line`、`write_time`、`resolve_runtime_ids`。`format_record_parts_into` 的函数体挪进 `write_mars_line`，原函数变成包装。加上段顺序测试。

`mars-xlog` 还继续调用 `format_record_parts_into`。对外字节不变。

### 2. facade 的格式化改走 `write_mars_line`

只改 `format_record_line_into` 的函数体：组 `MarsFields`，调用 `write_mars_line`。11 个参数先留着。`From<LogLevel>` 可以在这一步挪到 `lib.rs`，也可以留到第 5 步；不要两处各写一份匹配。

### 3. 去掉 `dyn XlogBackend`

删两个 trait。`Xlog` 改为 `Arc<RustBackend>`。`provider()` 返回具体的 `RustBackendProvider`。`XlogBackend` 的方法变成 `RustBackend` 的固有方法。`XlogBackendProvider` 的方法变成 `RustBackendProvider` 的固有方法，包括 `appender_open`、`appender_close`、`flush_all`、`write_global_with_meta`、`oneshot_flush`、路径查询，以及 Apple 上的 `set_console_fun`。`write_global_with_meta` 留在 provider 上，继续传 `MetaResolveMode::Global`，不要改道到 `RustBackend::write_with_meta`（那条是 Category）。`get_instance` 不再 `as Arc<dyn XlogBackend>`。`instance()` 加 `doc(hidden)` 并改正注释。

公开写签名不变，内部仍是位置参数。这一步不引入 `Event`。

### 4. 写路径改为 `Event`

增加 `event.rs` 和 `Xlog::write_event`。`log` / `write` / `write_with_meta*` / 全局写变成包装。`emit`、`write_sync_line`、`write_async_line`、`write_async_line_inline`、`build_sync_block_into` 改为接 `&Event`。删 `format_record_line_into`。

`AsyncWriteCommand` 的字段保持 `line: String`、`pool_shard`、`now_hour`、`force_flush`、`profile`。

`handle_async_frontend_write` 里 `engine.mode() != Async` 的那一支留下：用 `build_sync_block_from_formatted_line` 封 `cmd.line.as_bytes()`，再 `write_sealed`。不调用 `format_line`、`Layout` 或 `Event`。

布局仍只有 Mars，直接调用 `write_mars_line`。借用测试在这一步就能加（同步看明文，异步先解压）。

同模块里调用 `build_sync_block_into` 的测试改成传 `Event`。`resolve_record_meta` 的两条测试按各自的输入保持原期望。`appender_write_uses_global_meta_rule` 加在 `lib.rs` 的测试模块，持有 `APPENDER_TEST_LOCK`，drop 时 `appender_close`，读文件前 `flush_all(true)`。确认全局包装走的是 Global 而不是 Category，并且 `trace_log` 传入 `emit`。不改 `keep_open`。

### 5. `init` 时选定布局

增加 `layout.rs`、`init_with_layout`、`RustBackend::new_with_layout`、`format_line`。`new` 固定 Mars。默认 `init` 的行为与第 4 步相同。

加上 pieces 和 custom 的测试。配置必须是同步且无公钥。读文件前对返回的 `Xlog` 调用 `flush(true)`。magic 断言 `MAGIC_SYNC_NO_CRYPT_ZLIB_START`，比较的是 header 与 `MAGIC_END` 之间的明文。`flush(false)` 不够，因为短帧还在 64KiB keep-open 缓冲里。

绑定 crate、CLI、Dart 桥、宏、tracing 不在任何一步修改。

## Key Decisions

1. `Xlog` 持有 `Arc<RustBackend>`，删除 `XlogBackend` 和 `XlogBackendProvider`。发布面只有这一个实现，C++ backend 不在 `mars-xlog` 的依赖里。再留 trait 只为了涂类型。`mars-xlog-sys` 不动。

2. `Event` 放在 `mars-xlog`，`level` 用 facade 的 `LogLevel`。core 用 `MarsFields` 接住 `write_mars_line`。两套 `LogLevel` 不在这轮合并，避免绑定和公开 API 一起改名。转换只有 `From` 和一次 `as i32`。

3. Mars 是 `write_mars_line` 这个具体函数，不是 `Piece` 列表，也不是 `dyn`。截断和换行是 Mars 的历史栈缓冲行为，放进通用片段会让自定义行被迫兼容 C++ 的 16KiB。默认热路径因此不经过间接调用。

4. 自定义布局用 `Arc<dyn WriteLine>`，并且只有 `Layout::Custom` 才分配它。这是这一轮唯一的新 trait：调用方确实要换实现，而且实现类型在 `init` 时才知道。`Piece` 覆盖封闭变体，不再为每种片段做泛型链。

5. 布局不进 `XlogConfig` 的相等性。第一次 `init` 赢，之后不能换。级别仍然每次 `init` 都更新。布局换 `dyn` 要么加锁，要么在热路径上做 RCU，这一轮都不做。全局 `appender_open` 固定 Mars，因为那条 API 是兼容入口，不增加参数。

6. 格式化留在调用线程，异步命令只拥有 `String`。这是现有的池和 worker 形状。把 `Event` 的借用放进命令会把生命周期传染到 worker。队列里的行在引擎被切到同步之后，仍然用已经格式化的 `cmd.line` 走 `seal_sync_line`，worker 不回头调用 `Layout`。

7. 一次 `emit` 只有一个 `timestamp`，入队失败的 inline 回退不再第二次 `now()`。这是对罕见失败路径的行为收敛，换来 `write_event` 的时间戳可信。协议字节无关。正常入队路径的取时点仍在调用线程格式化之前。

8. `-1` 留在 `resolve_record_meta`，不进 `Event`。Category 的「三个都是 -1 才填充」和 Global 的「按字段填充」是 C++ `XloggerWrite` 的差异，测试已经锁住。新 API 用 `Option`，`Some(-1)` 就是数字。

9. 控制台继续用自己的格式函数，不读 payload。Android logcat、Apple os_log、桌面 stderr 的字节都和 Mars 行不同。共用缓冲会改这些字节。

10. 引擎的 `write_sealed` / `write_async_parts` 和两条通道保持原样。这一轮的 seam 是明文行，不是帧和文件。

## Open Questions

没有。格式串文法、`Sink`、有界的引擎通道、删除兼容 API、把 fatal 做成每条 fsync，都已经在上面否掉，不作为待定产品选择。

## PR Plan

五个 PR，按顺序。后一个依赖前一个。每个只改一类事情，现有协议测试保持通过。没有一个 PR 改协议字节。

### PR 1 — core：`write_mars_line` 收拢 Mars 行

- 文件：`crates/xlog-core/src/formatter.rs`
- 依赖：无
- 增加 `MarsFields`、`write_mars_line`、`write_time`、`resolve_runtime_ids`。`format_record_parts_into` / `format_record_into` / `format_record` 变成包装。新增 `mars_line_matches_legacy_segments`，期望整行以 `msg\n` 结束，body 后没有 `]`。不改 `mars-xlog` 的调用。不要改 formatter 去迎合多出来的括号。

### PR 2 — facade 格式化改调用 `write_mars_line`

- 文件：`crates/xlog/src/backend/rust.rs`，以及 `From<LogLevel>` 若在这一步搬动则含 `crates/xlog/src/lib.rs`
- 依赖：PR 1
- `format_record_line_into` 的函数体改为构造 `MarsFields` 并调用 `write_mars_line`。公开签名和 11 参数的内部函数先保留。字节与 PR 1 之前相同。

### PR 3 — 删除 `dyn XlogBackend`

- 文件：`crates/xlog/src/lib.rs`，`crates/xlog/src/backend/mod.rs`，`crates/xlog/src/backend/rust.rs`
- 依赖：PR 2
- 删除 `XlogBackend`、`XlogBackendProvider`。`Xlog` 持有 `Arc<RustBackend>`。`provider()` 返回 `&'static RustBackendProvider`。`XlogBackend` 的方法变成 `RustBackend` 的固有方法。`XlogBackendProvider` 的方法，包括 `set_console_fun` 和 `write_global_with_meta`，变成 `RustBackendProvider` 的固有方法。`write_global_with_meta` 继续传 `MetaResolveMode::Global`。`instance()` 改为 `doc(hidden)` 并改正注释。不引入 `Event` 或 `Layout`。`get` 仍返回 `Option<Xlog>`。

### PR 4 — 写路径改为 `Event` 借用

- 文件：`crates/xlog/src/event.rs`（新），`crates/xlog/src/lib.rs`，`crates/xlog/src/backend/rust.rs`
- 依赖：PR 3
- 增加 `Event`、`write_event`、`emit`、`emit_category`、`emit_global`。位置参数的公开方法只转发；`Event` 和 `-1` 解释在 `RustBackend` 内完成。`write_global_with_meta` 调 `emit_global`，传入 `raw_meta.trace_log`。内部 `write_async_line` / `write_async_line_inline` / `build_sync_block_into` 改为接 `&Event`。删除 `format_record_line_into`。`AsyncWriteCommand` 仍拥有 `String`。`handle_async_frontend_write` 在 `engine.mode() != Async` 时继续用 `build_sync_block_from_formatted_line` 封已有的 `cmd.line`，不调用 `format_line`。布局调用固定为 `write_mars_line`。更新同模块里 `build_sync_block_into` 的测试。已有的两条 meta 测试保持各自输入。`appender_write_uses_global_meta_rule` 写在 `crates/xlog/src/lib.rs` 的测试里，持有 `APPENDER_TEST_LOCK`，用 `AppenderCloseGuard`，读日志前 `Xlog::flush_all(true)`。借用测试在 `write_event` 返回后调用 `flush(true)`。不改宏、tracing、绑定，也不改 `keep_open`。

### PR 5 — `init` 时选定 `Layout`

- 文件：`crates/xlog/src/layout.rs`（新），`crates/xlog/src/lib.rs`，`crates/xlog/src/backend/rust.rs`
- 依赖：PR 4
- 增加 `Piece`、`WriteLine`、`Layout`、`init_with_layout`、`RustBackend::new_with_layout`、`format_line`、`write_pieces`。`Xlog::init` 和 `RustBackend::new` 固定 `Layout::mars()`。第一次 init 的布局胜出。pieces 与 custom 的测试使用 `AppenderMode::Sync`、无公钥，读文件前 `logger.flush(true)`，magic 为 `MAGIC_SYNC_NO_CRYPT_ZLIB_START`，只比较 header 与 `MAGIC_END` 之间的明文。不把 `FrameError::LengthOverflow` 当作这些行的上限。不解析格式串。不改控制台实现。不改 `keep_open`。
