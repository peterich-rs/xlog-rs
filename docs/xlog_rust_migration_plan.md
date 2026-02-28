# Xlog Rust 迁移完整技术规划（修订版）

## 0. 文档范围与结论

本文基于当前仓库代码（`crates/*` + `third_party/mars/mars`）重新梳理迁移方案，目标是把 `xlog` 的运行时核心从 C++ 迁移到 Rust，同时保持上层 API 与文件可解码兼容。

本版直接明确了原文中的关键决策项，尤其是：

1. **当前 Mars xlog 加密不是 AES-CTR，而是 `ECDH(secp256k1) + TEA(16 rounds)`**。
2. `formater.cc` 负责的是**日志文本行格式化**，xlog 的“二进制文件协议”实际在 `log_crypt.cc + log_base_buffer.cc + log_zlib/zstd_buffer.cc`。
3. 兼容性验收不能只做“字节完全一致”，应以**官方解码结果一致**为主（压缩流字节可不同但可解码）。

### 0.1 执行状态（截至 2026-02-28，分支 `codex/rust-migration-phase1`）

- Phase 0：未开始。
- Phase 1：已完成（commit: `643900d`）。
  - 已落地后端抽象：`crates/xlog/src/backend/{mod.rs,ffi.rs,rust.rs}`。
  - `xlog` API 已通过 backend trait 间接调用，实现 FFI/Rust 并行架构。
- Phase 2：已完成（commit: `3558c76`，含 2A/2B/2C 全部收口）。
  - 已完成 2A：`xlog-core` 协议/压缩/加密基础模块。
  - 已完成 2B：`xlog` Rust backend 最小写入链路接入（可生成 `.xlog` block）。
  - 已完成 2C-1：fixture 生成与 no-crypt 解码对比脚本。
  - 已完成 2C-2：crypt 用例在 Python2 官方解码环境下的回归固化（`scripts/xlog/setup_py2_decoder_env.sh` + `scripts/xlog/run_phase2c2_official.sh`）。
- Phase 3：已完成（commit: `3558c76`）。
- Phase 4：已完成（commit: `2586d81`）。
- Phase 5：进行中（当前增量）。
  - `xlog` 默认 feature 已切换到 `rust-backend`，并保留 `ffi-backend` 紧急回退。
  - JNI/UniFFI/NAPI 绑定层均改为可切换 `rust-backend` / `ffi-backend`。
  - 新增 `scripts/xlog/run_phase5_regression.sh` + `crates/xlog/examples/bench_backend.rs` 统一执行回归与性能对比。
  - 已具备性能比值产物与可选门槛校验，门槛达标优化持续中。
- Phase 6：未开始。

---

## 1. 现状基线（代码事实）

### 1.1 当前分层

```
crates/xlog-uniffi | crates/xlog-android-jni | crates/mars-xlog-harmony-napi
                                ↓
                          crates/xlog (safe API)
                                ↓
                        crates/xlog-sys (FFI + build.rs)
                                ↓
                 native/mars_xlog_wrapper.cc (C ABI 薄封装)
                                ↓
                     third_party/mars/mars/xlog + comm (C/C++)
```

### 1.2 当前 C++ 编译入口（必须迁移）

来自 `crates/xlog-sys/build.rs`：

- xlog 核心：
  - `third_party/mars/mars/xlog/src/appender.cc`
  - `third_party/mars/mars/xlog/src/formater.cc`
  - `third_party/mars/mars/xlog/src/log_base_buffer.cc`
  - `third_party/mars/mars/xlog/src/log_zlib_buffer.cc`
  - `third_party/mars/mars/xlog/src/log_zstd_buffer.cc`
  - `third_party/mars/mars/xlog/src/xlogger_interface.cc`
- 加密：
  - `third_party/mars/mars/xlog/crypt/log_crypt.cc`
  - `third_party/mars/mars/xlog/crypt/micro-ecc-master/*.c`
- xlogger：
  - `third_party/mars/mars/comm/xlogger/xlogger.cc`
  - `third_party/mars/mars/comm/xlogger/xlogger_category.cc`
- 通用：
  - `third_party/mars/mars/comm/autobuffer.cc`
  - `third_party/mars/mars/comm/mmap_util.cc`
  - `third_party/mars/mars/comm/ptrbuffer.cc`
  - `third_party/mars/mars/comm/tickcount.cc`
- Boost 依赖：filesystem/system/iostreams
- 平台文件：`ConsoleLog.cc`、`objc_console.mm`、`xlogger_threadinfo.*`

### 1.3 关键行为基线（迁移后必须保持）

1. **日志文件命名**：`{prefix}_{YYYYMMDD}[_{index}].xlog`（`appender.cc::__MakeLogFileName`）。
2. **mmap 缓冲文件名**：`{cache_dir or log_dir}/{prefix}.mmap3`。
3. **缓冲区大小**：150KB（`kBufferBlockLength = 150 * 1024`）。
4. **异步 flush 触发**：
   - buffer >= 1/3 容量 或 fatal 日志，主动唤醒后台线程。
   - 后台线程最长 15 分钟超时唤醒一次。
5. **默认文件生命周期**：10 天；`set_max_alive_time` 小于 1 天不生效。
6. **缓存迁移策略**：`cache_days > 0` 且 cache 可用空间 > 1GB 时优先写 cache，后续搬迁到 log_dir。
7. **公开 API 兼容**（`crates/xlog/src/lib.rs`）：
   - 实例生命周期：`init/get/release`
   - 控制接口：`set_level/is_enabled/set_appender_mode/flush/flush_all`
   - 路径与检索：`current_log_path/current_log_cache_path/filepaths_from_timespan/make_logfile_name`
   - 一次性恢复：`oneshot_flush`
   - dump：`dump/memory_dump`

---

## 2. 协议与加密决策（已定）

## 2.1 日志内容格式（`formater.cc`）

`formater.cc` 输出的是一行可读文本，核心格式：

- 头部：`[level][timestamp][pid, tid*][tag][file:line, func][`
- body：原始 message（最大截断策略保留）
- 末尾：确保 `\n`

这部分要在 Rust 中 1:1 复刻，包含：

- `ExtractFileName` 行为（仅保留文件名）
- 不同平台时区格式 (`%+.1f` 小时偏移)
- `tid == maintid` 时追加 `*`

## 2.2 二进制块协议（`log_crypt.cc` + `log_magic_num.h`）

当前主路径使用的新协议头：

- Header：`magic(1) + seq(u16) + begin_hour(1) + end_hour(1) + len(u32 LE) + client_pubkey(64)`
- Tailer：`magic_end(1)`，值固定 `0x00`

Magic（当前写入路径使用）：

- zlib sync crypt: `0x06`
- zlib async crypt: `0x07`
- zlib sync no-crypt: `0x08`
- zlib async no-crypt: `0x09`
- zstd sync crypt: `0x0A`
- zstd sync no-crypt: `0x0B`
- zstd async crypt: `0x0C`
- zstd async no-crypt: `0x0D`

seq 规则：

- sync 置 0
- async 递增，跳过 0（`uint16` 回绕时继续跳过 0）

## 2.3 加密算法（关键修正）

迁移保持与当前 C++ 完全一致：

- ECDH：`micro-ecc + secp256k1`（`uECC_secp256k1()`）
- 对称算法：**TEA**（16 rounds，8-byte block）
- key：`ecdh_shared_secret[0..16)` 作为 TEA 128-bit key
- async 路径：只加密 8 字节对齐部分，尾部不足 8 字节明文保留

因此：

- 不采用 `p256`、不采用 `AES-CTR`（与现网协议不兼容）。
- Rust 实现选型：`k256` + 自实现 TEA（几十行，避免引入错误语义）。

---

## 3. 技术选型与原因（ADR）

### ADR-01：迁移采用“双后端并行”，非一次性替换

- 方案：在 `crates/xlog` 引入 backend trait，保留 FFI 后端作为回退，新增 Rust 后端逐步接管。
- 原因：
  - 当前 API 面较大（Android JNI 已覆盖多数接口）。
  - 可逐步切流、逐步对齐行为，降低回归风险。

### ADR-02：新增 `crates/xlog-core` 承载 Rust 引擎

- 方案：协议、压缩、加密、mmap、appender 引擎统一落在 `xlog-core`。
- 原因：
  - `xlog` crate 保持“对外 API 层”角色，避免混杂底层细节。
  - 便于后续给 UniFFI/JNI 以外场景复用。

### ADR-03：压缩库

- zlib：`flate2`（raw deflate，支持 `Z_SYNC_FLUSH` 语义等价）
- zstd：`zstd`

原因：

- 都支持流式压缩，能匹配 `log_zlib_buffer.cc` / `log_zstd_buffer.cc` 的“增量写 + flush”。
- zstd 纯 Rust encoder 生态不成熟，优先选择成熟实现保证稳定性。

### ADR-04：mmap

- 方案：`memmap2::MmapMut` + 显式预分配文件（避免 sparse file SIGBUS 类风险）。
- 原因：C++ `mmap_util.cc` 已明确做预写零填充，Rust 必须保留此行为。

### ADR-05：并发模型

- 方案：`crossbeam-channel` + 单后台线程 + `Condvar`（sync flush ack）。
- 原因：
  - 更直接映射 `appender.cc` 行为。
  - 不引入 tokio，减少移动端额外依赖和调度复杂度。

### ADR-06：平台 console + thread id

- console：
  - Android：`android_logger`/`liblog` 直调
  - OHOS：`hilog` FFI
  - Apple：保留 `printf/NSLog/OSLog` 三选一语义
  - Unix：`stdout`
- thread id：`libc` 按平台调用，保持 `pid/tid/maintid` 填充时机。

### ADR-07：兼容性验收标准

- 主标准：官方解码后文本一致、顺序与丢失行为一致。
- 次标准：结构字段（magic/len/seq/hour/end）校验一致。
- 不要求压缩流字节逐字节一致（压缩实现可能不同）。

---

## 4. 目标代码结构（新增/修改文件）

## 4.1 新增 crate

- `crates/xlog-core/Cargo.toml`
- `crates/xlog-core/src/lib.rs`

建议模块：

- `crates/xlog-core/src/config.rs`
- `crates/xlog-core/src/record.rs`
- `crates/xlog-core/src/formatter.rs`
- `crates/xlog-core/src/protocol.rs`
- `crates/xlog-core/src/compress.rs`
- `crates/xlog-core/src/crypto.rs`
- `crates/xlog-core/src/mmap_store.rs`
- `crates/xlog-core/src/buffer.rs`
- `crates/xlog-core/src/file_manager.rs`
- `crates/xlog-core/src/dump.rs`
- `crates/xlog-core/src/appender_engine.rs`
- `crates/xlog-core/src/registry.rs`
- `crates/xlog-core/src/platform_console.rs`
- `crates/xlog-core/src/platform_tid.rs`

## 4.2 `xlog` crate 改造

- `crates/xlog/src/lib.rs`（API 保持，后端切换）
- `crates/xlog/src/backend/mod.rs`（trait）
- `crates/xlog/src/backend/ffi.rs`（迁移原 sys 调用）
- `crates/xlog/src/backend/rust.rs`（调用 `xlog-core`）
- `crates/xlog/Cargo.toml`（新依赖、feature flag）

## 4.3 绑定层影响

接口不改，只验证：

- `crates/xlog-android-jni/src/lib.rs`
- `crates/xlog-uniffi/src/lib.rs`
- `crates/mars-xlog-harmony-napi/src/lib.rs`

---

## 5. 分阶段实施计划（任务 / 文件 / 实现方式）

## Phase 0：基线固化与回归护栏（1 周）

目标：在不改行为前提下，把“当前 C++ 行为”固化为可重复测试。

任务：

1. 补齐 API 行为基线测试。
2. 产出标准 fixture（含 zlib/zstd、sync/async、crypt/no-crypt）。
3. 固化解码对比工具链。

涉及文件：

- 新增 `crates/xlog/tests/api_compat.rs`
- 新增 `crates/xlog/tests/fixtures/*`
- 新增 `scripts/xlog/gen_fixtures.sh`
- 新增 `scripts/xlog/decode_compare.sh`

实现要点：

- 用当前 `mars-xlog-sys` 写出样本。
- 解码脚本路径改为：
  - `third_party/mars/mars/xlog/crypt/decode_mars_nocrypt_log_file.py`
  - `third_party/mars/mars/xlog/crypt/decode_mars_crypt_log_file.py`
- 记录脚本依赖（Python2 + `zstandard` + `pyelliptic`）并提供一键环境脚本：`scripts/xlog/setup_py2_decoder_env.sh`。

DoD：

- 新增测试在当前代码稳定通过。
- 形成可重复的基线产物和对比命令。

---

## Phase 1：`xlog` 后端抽象（1 周）

目标：先把“能力边界”抽出来，为并行后端迁移铺路。

任务：

1. 为 `Xlog` 全部公开方法定义 backend trait。
2. 将现有 `sys::*` 调用迁入 `ffi backend`。
3. 保持对外 API 与行为 100% 不变。

涉及文件：

- 修改 `crates/xlog/src/lib.rs`
- 新增 `crates/xlog/src/backend/mod.rs`
- 新增 `crates/xlog/src/backend/ffi.rs`
- 修改 `crates/xlog/Cargo.toml`

实现要点：

- `Inner` 从裸 `instance: usize` 提升为 `Arc<dyn XlogBackend>`（或等价 enum 后端）。
- Phase 1 落地时默认 feature 仍为 FFI；已在 Phase 5 切换为 Rust 默认。

DoD：

- 绑定层无需改动即可通过。
- 行为对比测试无变化。

---

## Phase 2：协议/压缩/加密核心（2 周）

目标：实现最核心协议，保证写入块可被官方解码器正确解析。

任务：

1. 实现 `LogRecord` 与文本 formatter（复刻 `formater.cc`）。
2. 实现 block header/tailer 编码与修复逻辑。
3. 实现 zlib/zstd 流式压缩。
4. 实现 `secp256k1 + TEA` 加密流程。

当前阶段拆分（落地顺序）：

1. **Phase 2A（已完成）**：核心原语实现。
2. **Phase 2B（已完成）**：`xlog` Rust backend 最小集成。
3. **Phase 2C（已完成）**：官方解码兼容夹具与回归脚本。
   - 2C-1（已完成）：`gen_fixtures.sh + decode_compare.sh` no-crypt 回归。
   - 2C-2（已完成）：Python2 官方 crypt 解码环境固化与回归脚本接入。

涉及文件：

- 新增 `crates/xlog-core/src/record.rs`
- 新增 `crates/xlog-core/src/formatter.rs`
- 新增 `crates/xlog-core/src/protocol.rs`
- 新增 `crates/xlog-core/src/compress.rs`
- 新增 `crates/xlog-core/src/crypto.rs`
- 新增 `crates/xlog-core/tests/compress_roundtrip.rs`
- 新增 `crates/xlog-core/tests/protocol_compat.rs`
- 修改 `crates/xlog/src/backend/rust.rs`
- 修改 `crates/xlog/src/backend/mod.rs`
- 修改 `crates/xlog/Cargo.toml`
- 新增 `crates/xlog/examples/gen_fixture.rs`
- 新增 `scripts/xlog/gen_fixtures.sh`
- 新增 `scripts/xlog/decode_compare.sh`
- 新增 `scripts/xlog/decode_mars_nocrypt_py3.py`
- 新增 `scripts/xlog/setup_py2_decoder_env.sh`
- 新增 `scripts/xlog/run_phase2c2_official.sh`

实现要点：

- `protocol.rs` 提供：
  - magic 选择
  - header 读写（LE）
  - seq 生成器（跳过 0）
- `crypto.rs`：
  - 公钥输入 128 hex -> 64 bytes
  - `k256` 生成临时 keypair，导出 64-byte pubkey（去掉 SEC1 前缀）
  - shared secret 前 16 bytes 作为 TEA key
  - async 仅加密 8-byte 对齐块
- sync 模式保持 seq=0。
- Rust backend 最小接入策略：
  - 复用 `xlog-core` 的 formatter/protocol/compress/crypto 组件拼装完整 block。
  - 先实现直接文件 append 路径，`oneshot_flush` 暂返回 `Unnecessary`。
  - 默认 appender 与 named instance 注册先在 `xlog` 层用 `OnceLock + Mutex<HashMap<...>>` 落地，为 Phase 4 的引擎替换预留接口形态。
- Phase 2C 脚本化回归：
  - `gen_fixture.rs` 生成带固定消息载荷的 `.xlog` 样本（`FIXTURE|<prefix>|<seq>`）。
  - `gen_fixtures.sh` 批量生成 Rust/FFI 的 zlib/zstd + sync/async 样本与 `manifest.tsv`。
  - `decode_compare.sh` 调用官方 Python2 解码脚本（可用时）或 Python3 no-crypt 兼容解码器进行结果对比，并校验 Rust/FFI 载荷一致性。
  - `setup_py2_decoder_env.sh` 负责 Python2 + `pyelliptic` + `zstandard` 一键安装与兼容补丁。
  - `run_phase2c2_official.sh` 固化“生成 crypt fixtures + official decoder 对比”的收口入口。

DoD：

- `protocol_compat` 覆盖 magic/seq/len/hour。
- `compress_roundtrip` 覆盖 zlib/zstd 压缩回环。
- `xlog` 在 `--features rust-backend` 下可完成写文件的单元测试。
- no-crypt 样本可完成脚本化解码对比。
- crypt 样本可在 Python2 官方解码环境下回归（Phase 2C-2 收口）。

---

## Phase 3：mmap + 文件管理 + oneshot（2 周）

目标：复刻 crash-safe 缓冲与文件生命周期管理。

任务：

1. 实现 150KB mmap 持久缓冲。
2. 实现打开时 buffer 修复（fix）。
3. 实现日志滚动、过期删除、cache 搬迁策略。
4. 实现 `oneshot_flush` 全流程。

涉及文件：

- 新增 `crates/xlog-core/src/mmap_store.rs`
- 新增 `crates/xlog-core/src/buffer.rs`
- 新增 `crates/xlog-core/src/file_manager.rs`
- 新增 `crates/xlog-core/src/oneshot.rs`
- 新增 `crates/xlog-core/tests/mmap_recovery.rs`

实现要点：

- mmap 文件路径：`{cache_dir or log_dir}/{prefix}.mmap3`
- 文件名规则与 index 选择严格对齐 `appender.cc`。
- `set_max_alive_time` 保留最小值 1 天门槛。
- `oneshot_flush` 返回码保持：`Success/Unnecessary/OpenFailed/...`。

DoD：

- 进程异常退出后可通过 `oneshot_flush` 恢复日志。
- 缓存迁移和过期删除行为与基线一致。

---

## Phase 4：Appender 引擎 + 注册表 + 平台适配（2 周）

目标：完成 Rust 运行时替换，覆盖 `xlogger_interface.cc` 能力。

任务：

1. 实现 sync/async 写入引擎。
2. 实现 default appender + named instance registry。
3. 实现 `flush/flush_all/set_level/is_enabled` 等控制面。
4. 实现 console 输出与 thread id 填充。
5. 实现 `dump/memory_dump`。

涉及文件：

- 新增 `crates/xlog-core/src/appender_engine.rs`
- 新增 `crates/xlog-core/src/registry.rs`
- 新增 `crates/xlog-core/src/platform_console.rs`
- 新增 `crates/xlog-core/src/platform_tid.rs`
- 新增 `crates/xlog-core/src/dump.rs`
- 新增 `crates/xlog-core/tests/async_engine.rs`

实现要点：

- 异步阈值对齐：1/3 唤醒，4/5 注入 fatal 提示。
- 后台线程周期对齐：15 分钟。
- `flush(sync=true)` 需要阻塞等待后台实际刷盘完成。
- `dump/memory_dump` 输出格式对齐现有实现（包括截断策略）。

DoD：

- `xlog` 公开接口在 Rust 后端全部可用。
- Android JNI 示例应用行为无回归。

---

## Phase 5：切换默认后端并灰度（1 周）

目标：在仓库内把默认实现切到 Rust，保留 FFI 作为紧急回退。

当前状态：进行中（默认后端切换与回归脚本已落地，性能优化继续推进）。

任务：

1. `xlog` 默认 feature 切到 Rust backend。
2. 绑定层回归（JNI/UniFFI/NAPI）。
3. 增加压测和 soak test。

涉及文件：

- 修改 `crates/xlog/Cargo.toml`
- 修改 `crates/xlog-uniffi/Cargo.toml`
- 修改 `crates/xlog-android-jni/Cargo.toml`
- 修改 `crates/mars-xlog-harmony-napi/Cargo.toml`
- 新增 `crates/xlog/examples/bench_backend.rs`
- 新增 `scripts/xlog/run_phase5_regression.sh`

实现要点：

- 默认 feature 切换：`mars-xlog` 默认启用 `rust-backend`。
- 绑定层默认禁用 `mars-xlog` 的 default-features，并以各自 feature 显式转发 `rust-backend` / `ffi-backend`。
- `run_phase5_regression.sh` 一次性执行：
  - `run_phase2c2_official.sh` 官方解码回归（可选跳过）
  - JNI/UniFFI/NAPI 在 Rust/FFI 双后端下的 `cargo check`
  - `bench_backend.rs` 产出 Rust/FFI 吞吐与延迟 JSON 指标；支持可选门槛判定（90% 吞吐/110% p99）

DoD：

- 回归与性能指标可脚本化复跑并产出 artifacts。
- 保留 `ffi-backend` feature 可一键回退。

---

## Phase 6：移除 C++ 依赖（1 周）

目标：完成收尾，默认构建不再编译 Mars C++ xlog。

任务：

1. 移除 `crates/xlog-sys` 在默认路径中的强依赖。
2. 清理 `build.rs` 的 C++/Boost 编译链路。
3. 调整 workspace 成员与 README。

涉及文件：

- 修改根 `Cargo.toml`
- 修改 `crates/xlog/Cargo.toml`
- 修改/冻结 `crates/xlog-sys/*`（可保留为 legacy feature）
- 更新 `README.md`

DoD：

- 默认 `cargo build` 不依赖 C++14/Boost。
- `third_party/mars` 仅用于参考与兼容测试，不参与主构建。

---

## 6. 兼容性测试矩阵（必须落地）

### 6.1 协议兼容

- magic + header/tailer 字段校验
- seq 断档检测行为一致
- zlib/zstd 解压成功率
- crypt/no-crypt 解码结果一致

### 6.2 API 行为兼容

- `Xlog::init/get/drop`
- `set_level/is_enabled`
- `set_appender_mode/flush/flush_all`
- `set_max_file_size/set_max_alive_time`
- `current_log_path/current_log_cache_path`
- `filepaths_from_timespan/make_logfile_name`
- `oneshot_flush`
- `dump/memory_dump`

### 6.3 平台兼容

- Android（JNI 示例）
- macOS/iOS（UniFFI）
- HarmonyOS（NAPI）
- Linux（纯 Rust）

### 6.4 性能门槛（建议）

- 吞吐不低于现网 C++ 的 90%
- p99 写入延迟不高于 C++ 的 110%
- crash 恢复成功率 100%

---

## 7. 风险与缓解

| 风险 | 触发点 | 缓解 |
| :--- | :--- | :--- |
| 加密实现偏差 | `k256` 公钥格式、TEA block 边界处理错误 | 增加对照用例：与 C++ 同输入比对输出块 |
| 压缩流语义偏差 | `Z_SYNC_FLUSH` / zstd flush 行为不同 | 只以“可解码 + 文本一致”验收，增加长时间流式写入测试 |
| mmap 恢复失败 | 文件预分配或 fix 逻辑差异 | 严格复刻 `Fix + GetLogLen + tail` 校验，增加断电模拟 |
| 文件管理回归 | cache/log_dir 迁移时机细节复杂 | 回放真实目录样本，逐条断言生成文件名与删除条件 |
| 并发死锁/丢日志 | flush ack 与后台线程状态竞争 | loom/miri + 压测 + 关机/崩溃场景测试 |

---

## 8. 执行顺序（建议）

1. 先完成 Phase 0 与 Phase 1，建立“可回退、可对比”的安全网。
2. 再做 Phase 2~4，把 Rust 引擎跑通并与现有基线对齐。
3. 最后做 Phase 5~6 切换默认与清理。

这个顺序可以保证：每个阶段都可验证、可回滚，不会出现“到最后才发现协议不兼容”的高风险收敛问题。
