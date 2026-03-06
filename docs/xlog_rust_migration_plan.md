# Xlog Rust 迁移完整技术规划（修订版）

## 0. 文档范围与结论

本文基于当前仓库代码（`crates/*` + `third_party/mars/mars`）重新梳理迁移方案，目标是把 `xlog` 的运行时核心从 C++ 迁移到 Rust，同时保持上层 API 与文件可解码兼容。

本版直接明确了原文中的关键决策项，尤其是：

1. **当前 Mars xlog 加密不是 AES-CTR，而是 `ECDH(secp256k1) + TEA(16 rounds)`**。
2. `formater.cc` 负责的是**日志文本行格式化**，xlog 的“二进制文件协议”实际在 `log_crypt.cc + log_base_buffer.cc + log_zlib/zstd_buffer.cc`。
3. 兼容性验收不能只做“字节完全一致”，应以**官方解码结果一致**为主（压缩流字节可不同但可解码）。

### 0.1 执行状态（截至 2026-03-04，分支 `codex/rust-migration-phase1`）

- Phase 0：已完成（fixture/decoder 基线与 nightly 回归已固化）。
- Phase 1：已完成（commit: `643900d`）。
  - 已落地后端抽象：`crates/xlog/src/backend/{mod.rs,rust.rs}`（`ffi.rs` 为历史阶段文件，Phase 5 起不再参与默认路径）。
  - `xlog` API 已通过 backend trait 间接调用，完成 Rust 运行时接管。
- Phase 2：已完成（commit: `3558c76`，含 2A/2B/2C 全部收口）。
  - 已完成 2A：`xlog-core` 协议/压缩/加密基础模块。
  - 已完成 2B：`xlog` Rust backend 最小写入链路接入（可生成 `.xlog` block）。
  - 已完成 2C-1：fixture 生成与 no-crypt 解码对比脚本。
  - 已完成 2C-2：crypt 用例在 Python2 官方解码环境下的回归固化（`scripts/xlog/setup_py2_decoder_env.sh` + `scripts/xlog/run_phase2c2_official.sh`）。
  - 已完成 2D：`run_phase2c2_official.sh` 已接入 nightly CI，并固化失败日志与产物上传（`.github/workflows/phase2c2_official_nightly.yml`）。
- Phase 3：已完成（commit: `3558c76`）。
- Phase 4：已完成（主功能 + review 阻断项收口完成）。
- Phase 5：已完成。
  - `xlog` 默认后端稳定为 `rust-backend`。
  - JNI/UniFFI/NAPI 绑定覆盖补齐 `mars-xlog` 公开能力面（含 raw metadata 与全局 appender 路径）。
  - `scripts/xlog/run_phase5_regression.sh` + `crates/xlog/examples/bench_backend.rs` 已固定为 Rust 路径回归与性能采样。
- Phase 6：进行中（性能对齐阶段；保留 `mars-xlog-sys` 与 C++ backend 作为长期对照基线）。
- Phase 7：未开始（仅在性能完全对齐后执行 C++ 依赖移除）。

### 0.2 Review 收口清单（截至 2026-03-04）

基于 `docs/rust_migration_review.md`，本轮已收敛的关键项：

1. `appender_engine.rs`：启动即排空 recovered mmap；flush 信号去重；`Async -> Sync` 改为非阻塞触发 flush。
2. `backend/rust.rs`：sync + crypt magic/payload 语义已对齐。
3. `buffer.rs`：torn tail 场景恢复策略放宽，尽量保留可恢复前缀。
4. `file_manager.rs`：append/copy 失败回滚、时钟回拨复用上次文件、1GiB 阈值边界对齐、空 prefix 语义对齐。
5. `platform_console.rs` + `backend/rust.rs`：console 输出补齐 metadata，Android tag 改为逐条 tag，Apple `set_console_fun` 已接入。
6. `oneshot.rs`：改为 exact-size mmap 读取语义（截断返回 `ReadFailed`）。
7. `backend/rust.rs`：非法 pubkey 降级 no-crypt；async seq 改为全局；`dump` 语义改为无默认 appender 返回空串；default appender open 幂等。
8. `backend/rust.rs` + `appender_engine.rs`：async 路径已对齐为“单 pending block + 流式增量压缩/加密 + flush 封尾”。
9. `compress.rs`：zstd async 改为流式并显式 `windowLog=16`。
10. `backend/rust.rs`：4/5 高水位告警已恢复为实际日志注入（同一 pending stream）。
11. `backend/rust.rs` + `appender_engine.rs`：`Async -> Sync` 并发切换下补齐 `InvalidMode` 落盘兜底，避免尾块丢失。
12. `appender_engine.rs` + `buffer.rs`：async pending mmap 改为批量刷盘（保留强制刷盘触发）。
13. `formatter.rs`：日志截断语义改为对齐 C++ 16KB 栈缓冲路径（保留 130 bytes 余量）。
14. `appender_engine.rs`：`flush(sync=true)` 改为 `move_file=false`，对齐 C++ `FlushSync`。
15. `backend/rust.rs`：4/5 高水位改为“替换当前日志为告警行”，不再追加额外告警写入。
16. `file_manager.rs`：`filepaths_from_timespan` 恢复 log_dir -> cache_dir 顺序，不做额外排序。
17. `appender_engine.rs` + `oneshot.rs`：补齐 mmap 恢复 begin/end tip 行（含 mark info）。
18. `platform_console.rs`：Apple console 改为原生 OSLog/NSLog/printf shim 输出。
19. `backend/{mod.rs,rust.rs}` + `xlog/lib.rs`：补齐 `RawLogMeta` 与 `appender_write_with_meta_raw`，对齐 `traceLog` 旁路语义及 `instance_ptr==0` 全局 raw metadata 写入路径。
20. `xlog-uniffi` + `mars-xlog-harmony-napi`：补齐实例控制/全局 appender/路径检索/`oneshot_flush`/`dump` 等接口覆盖，收口 wrapper 能力缺口。

当前 review 阻断项：**0**（无未收口项）。

### 0.3 性能策略（新增，2026-03-05）

在性能达到对齐门槛前，迁移策略调整为：

1. `mars-xlog-sys` crate 不删除，C++ backend 持续保留。
2. Rust/C++ 双后端 benchmark 常态化执行，用于回归门禁与定位。
3. 性能优化仅允许“实现层优化”，不允许修改协议/逻辑语义（见 Phase 6 约束）。
4. `Phase 7（删除 C++ 依赖）` 变更为后置阶段，需满足性能门槛后再执行。

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
- `crates/xlog/src/backend/ffi.rs`（历史阶段迁移原 sys 调用，Phase 5 起移出默认路径）
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
2. 将现有 `sys::*` 调用迁入 `ffi backend`（历史阶段目标）。
3. 保持对外 API 与行为 100% 不变。

涉及文件：

- 修改 `crates/xlog/src/lib.rs`
- 新增 `crates/xlog/src/backend/mod.rs`
- 新增 `crates/xlog/src/backend/ffi.rs`（历史阶段文件）
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
4. **Phase 2D（已完成）**：nightly CI 接入与失败产物保留。

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
- 新增 `.github/workflows/phase2c2_official_nightly.yml`

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
- Phase 2D CI 护栏：
  - `phase2c2_official_nightly.yml` 每日定时执行 `run_phase2c2_official.sh`。
  - 失败日志 (`run_phase2c2_official.log`) 与 fixtures/status 作为 artifact 上传，防止 crypt 回归滞后发现。

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

当前状态：已完成（主功能 + Review 阻断项收口完成）。

任务：

1. 实现 sync/async 写入引擎。
2. 实现 default appender + named instance registry。
3. 实现 `flush/flush_all/set_level/is_enabled` 等控制面。
4. 实现 console 输出与 thread id 填充。
5. 实现 `dump/memory_dump`。
6. 持续维护 `docs/rust_migration_review.md`，确保阻断项保持清零。

涉及文件：

- 新增 `crates/xlog-core/src/appender_engine.rs`
- 新增 `crates/xlog-core/src/registry.rs`
- 新增 `crates/xlog-core/src/platform_console.rs`
- 新增 `crates/xlog-core/src/platform_tid.rs`
- 新增 `crates/xlog-core/src/dump.rs`
- 新增 `crates/xlog-core/tests/async_engine.rs`

实现要点：

- 异步阈值：1/3 唤醒已对齐；4/5 高水位已恢复同 pending stream 告警注入。
- 后台线程周期对齐：15 分钟。
- `flush(sync=true)` 需要阻塞等待后台实际刷盘完成。
- `dump/memory_dump` 输出格式对齐现有实现（包括截断策略）。
- `maintid` 与 sync/crypt magic 语义已按 C++ 当前行为对齐。

DoD：

- `xlog` 公开接口在 Rust 后端全部可用。
- Android JNI 示例应用行为无回归。
- Review 阻断项（P0）为 0，且对应回归测试已落地。

---

## Phase 5：切换默认后端并灰度（1 周）

目标：在仓库内把默认实现切到 Rust，并完成绑定层与回归链路收口。

当前状态：已完成（默认后端、绑定层覆盖、回归脚本均已收口）。

任务：

1. `xlog` 默认 feature 切到 Rust backend。
2. 绑定层回归（JNI/UniFFI/NAPI）并补齐接口覆盖。
3. 增加压测和 soak test。
4. 复跑全链路回归并持续跟踪性能指标。

涉及文件：

- 修改 `crates/xlog/Cargo.toml`
- 修改 `crates/xlog-uniffi/Cargo.toml`
- 修改 `crates/xlog-android-jni/Cargo.toml`
- 修改 `crates/mars-xlog-harmony-napi/Cargo.toml`
- 新增 `crates/xlog/examples/bench_backend.rs`
- 新增 `scripts/xlog/run_phase5_regression.sh`

实现要点：

- 默认 feature 切换：`mars-xlog` 默认启用 `rust-backend`。
- 绑定层统一禁用 `mars-xlog` 的 default-features，并显式启用 `rust-backend`。
- `run_phase5_regression.sh` 一次性执行：
  - `run_phase2c2_official.sh` 官方解码回归（可选跳过）
  - JNI/UniFFI/NAPI Rust backend `cargo check`
  - `bench_backend.rs` 产出 Rust backend 吞吐与延迟 JSON 指标

DoD：

- 回归与性能指标可脚本化复跑并产出 artifacts。
- `run_phase5_regression.sh` 在不跳过回归项时稳定通过（含 `phase2c2_official`）。
- 绑定层覆盖达到 `mars-xlog` 公开能力面（含 raw metadata + global appender 能力）。

---

## Phase 6：性能对齐与双后端保留（持续）

目标：在不改变线上语义的前提下，使 Rust 实现性能完整对齐 C++（吞吐/延迟门槛见 6.4）。

当前状态：进行中（已发现显著性能差距；`mars-xlog-sys` 与 C++ backend 保留中）。

任务：

1. 固化 Rust/C++ 双后端 A/B benchmark，长期追踪并可回放。
2. 深度 review 两侧实现差异，整理“只做实现层优化”的问题清单。
3. 按“不改逻辑协议”原则实施性能优化（生命周期、减少拷贝、减少内存抖动、减少锁竞争）。
4. 每项优化必须附带同参数 Rust/C++ 对比产物与回归结论。

涉及文件：

- `docs/xlog_rust_migration_plan.md`
- `scripts/xlog/run_phase5_regression.sh`（后续补齐 C++ 对照输出）
- `crates/xlog/examples/bench_backend.rs`
- `crates/xlog-sys/*`（保留用于 C++ backend 基线与对照）

基线快照（2026-03-05，`artifacts/bench-compare/20260305-main`，本地 macOS，release，`messages=20000`，`compress=zlib`，`msg-size=96`，3 轮均值）：

| mode | backend | throughput_mps | lat_avg_ns | lat_p99_ns |
| :--- | :--- | ---: | ---: | ---: |
| async | Rust | 95,062.90 | 10,391.62 | 48,375.33 |
| async | C++ | 318,996.36 | 3,021.70 | 6,625.00 |
| sync | Rust | 31,289.61 | 31,765.63 | 51,666.67 |
| sync | C++ | 466,944.82 | 1,951.43 | 10,749.67 |

当前差距（同上口径）：

- async：Rust 吞吐约为 C++ 的 29.8%，平均延迟约 3.44x，p99 约 7.30x。
- sync：Rust 吞吐约为 C++ 的 6.7%，平均延迟约 16.28x，p99 约 4.81x。

P0/P1 执行后快照（2026-03-05，`artifacts/bench-compare/20260305-p0p1`，同口径）：

| mode | backend | throughput_mps | lat_avg_ns | lat_p99_ns |
| :--- | :--- | ---: | ---: | ---: |
| async | Rust | 138,055.98 | 7,164.61 | 35,750.00 |
| async | C++ | 333,156.55 | 2,845.75 | 6,625.00 |
| sync | Rust | 50,334.56 | 19,676.99 | 37,000.00 |
| sync | C++ | 487,713.61 | 1,793.75 | 13,819.33 |

P0/P1 执行后差距（同上口径）：

- async：Rust 吞吐约为 C++ 的 41.4%，平均延迟约 2.52x，p99 约 5.40x。
- sync：Rust 吞吐约为 C++ 的 10.3%，平均延迟约 10.97x，p99 约 2.68x。
- 相比基线：Rust async 吞吐约提升 45.2%，Rust sync 吞吐约提升 60.9%。

P2/P3 执行后快照（2026-03-06，`artifacts/bench-compare/20260306-perf5`，本地 macOS，release，`messages=20000`，`compress=zlib`，`msg-size=96`，3 轮均值）：

说明：当前 `crates/xlog/src/backend/mod.rs` 仅接入 `rust-backend`，本轮未直接重跑 C++ backend benchmark；下表中的 C++ 数据继续沿用 `20260305-p0p1` 保留基线，用于衡量 Rust 对齐进度。

| mode | backend | throughput_mps | lat_avg_ns | lat_p99_ns |
| :--- | :--- | ---: | ---: | ---: |
| async | Rust | 143,674.91 | 6,813.59 | 46,444.67 |
| async | C++ | 333,156.55 | 2,845.75 | 6,625.00 |
| sync | Rust | 170,180.00 | 5,728.23 | 23,861.33 |
| sync | C++ | 487,713.61 | 1,793.75 | 13,819.33 |

当前分支新增进展（2026-03-06）：

| snapshot | mode | throughput_mps | lat_avg_ns | lat_p99_ns | 说明 |
| :--- | :--- | ---: | ---: | ---: | :--- |
| `20260306-perf6` | async Rust | 139,272.36 | 7,045.37 | 47,736.00 | `async flush copy/clear` 收敛 + `append target cache` 落地后的全量重跑 |
| `20260306-perf6` | sync Rust | 192,058.91 | 4,968.84 | 18,347.00 | 当前可复现的 sync 参考结果 |
| `20260306-perf7` | async Rust | 213,855.31 | 4,537.35 | 39,194.67 | `async mmap persist cadence` 调优后的 async 专项结果 |
| `20260306-perf8` | async Rust | 256,989.09 | 3,772.17 | 36,416.67 | `async_state` 短锁 checkout + 锁外压缩/加密/engine 提交后的 async 专项结果 |
| `20260306-perf9` | async Rust | 234,499.66 | 4,055.71 | 35,583.67 | `finalize/recover/oneshot` 边界复制与尾块处理收敛后的专项结果；仅记录，不替换主基线 |
| `20260306-p0p1wave` | async Rust（4 threads smoke） | 184,021.92 | 19,261.10 | 63,958.00 | 新 threaded harness + 同轮 Rust/C++ smoke，结果见 `artifacts/bench-compare/20260306-p0p1wave/results_smoke.jsonl` |
| `20260306-p0p1wave` | sync Rust（4 threads smoke） | 117,198.59 | 29,534.00 | 484,792.00 | 同上；仅用于验证 harness 与当前实现路径，不替换阶段基线 |
| `20260306-p0p1wave` | async C++（4 threads smoke） | 157,537.94 | 24,974.10 | 246,916.00 | 同上；首次恢复同轮同参数 smoke 对照 |
| `20260306-p0p1wave` | sync C++（4 threads smoke） | 256,480.81 | 14,460.13 | 160,000.00 | 同上；仅 smoke，不作为阶段结论 |
| `20260306-harness-matrix` | sync Rust（plain 1T / 4T） | 161,859.15 / 133,643.58 | 5,882.66 / 27,437.78 | 44,610.67 / 189,277.33 | 新 harness 多轮矩阵；用于诊断 sync 主差距位置 |
| `20260306-syncsteady-perf2` | sync Rust（plain 1T / 4T） | 220,175.70 / 167,391.97 | 4,233.15 / 20,882.39 | 24,180.33 / 194,805.67 | sync steady-state 锁作用域/热路径 syscall 收敛后的重跑 |
| `20260306-syncbuffer-perf` | sync Rust / C++（plain 1T / 4T） | Rust `646,165.73 / 327,021.12`；C++ `577,243.26 / 375,983.16` | Rust `1,398.50 / 11,893.72`；C++ `1,598.28 / 10,359.51` | Rust `5,111.00 / 83,319.67`；C++ `5,861.33 / 52,555.67` | sync keep-open 活跃文件改为 stdio 风格缓冲写；重新定位剩余差距为多线程竞争而非固定成本 |

当前有效对齐结论（Rust 对保留 C++ 基线）：

- async：以 `20260306-perf8` 为当前参考，Rust 吞吐约为 C++ 的 77.1%，平均延迟约 1.33x，p99 约 5.50x。
- sync：以 `20260306-syncbuffer-perf` 为当前 plain steady-state 参考，Rust `1T` 已达 C++ 的 `111.9%`，`4T` 已达 `87.0%`；平均延迟分别约为 C++ 的 `0.88x / 1.15x`，p99 约为 `0.87x / 1.59x`。
- 说明：`20260306-perf8` 仍作为 async 主参考；`20260306-perf9` 主要验证边界路径复制收敛，p99 略降但均值受环境波动影响，不提升为阶段基线。sync plain steady-state 当前采用 `20260306-syncbuffer-perf` 作为主参考。
- `20260306-p0p1wave` 已恢复同轮 Rust/C++ threaded smoke，对照不再完全依赖历史产物；但由于仅单次 smoke、线程数和消息规模与阶段主基线不同，不替换上述阶段参考。
- `20260306-harness-matrix`、`20260306-syncsteady-perf2` 与 `20260306-syncbuffer-perf` 共同表明：sync 的主差距不在轮转边界，而在 plain steady-state 热路径。加入 keep-open 活跃文件缓冲后，固定成本已基本清掉，剩余差距集中在 `4T` 多线程竞争。

Phase 6 已完成项（仅保留仍有信息价值的条目）：

1. sync 热路径已移除每次写入 housekeeping，并去掉每次 `append_bytes` 后的 `file.flush()`。
2. async pending block 已从“整段重建”改为 mmap 增量维护，flush 成功后只清理已用区间。
3. sync 文件写入已具备活跃句柄复用，以及按目录/按天的 append target cache，steady-state 不再每次重新 `read_dir/stat/path-select`。
4. flush 与 housekeeping 已解耦，目录迁移/过期扫描由后台周期任务处理。
5. formatter/compress/encrypt 热路径已复用 scratch buffer，async TEA 改为原位加密。
6. async mmap 持久化已从固定小步长触发改为“更新次数 + 增量字节数 + 时间窗 + force flush”的组合策略。
7. `RustBackend::async_state` 已改为短锁 checkout + 锁外压缩/加密/engine 提交，保留单 backend 顺序语义的同时收窄串行区。
8. `PersistentBuffer` 启动恢复与 `oneshot_flush` 已改为 scan + slice 路径，避免整段 `recover_blocks()`/`read_exact()` 复制；async finalize 空尾块不再执行无意义 append。
9. `AppenderEngine` 后台 async flush 在 state 忙时改为 `try_lock + requeue`，避免 flush worker 在热写入期间长时间阻塞主串行区。
10. benchmark harness 已恢复 compile-time Rust/C++ backend 选择，并补齐 `--threads`、`--flush-every` 等 threaded smoke 能力。
11. sync steady-state 写入已改为先 snapshot `AppenderEngine` 配置，再锁外执行文件 append，避免将 engine 锁持有到文件 I/O 完成。
12. `FileManager` plain sync 热路径已收敛到单次 runtime 锁；steady-state 不再重复做 `active_append_path + append_slices_with_runtime` 双锁往返，目录创建也下沉到 `open(NotFound)` 兜底重试。
13. sync keep-open 活跃文件已改为 stdio 风格用户态缓冲写，缓冲容量与 `BUFSIZ` 对齐；关闭、换文件与维护路径会显式冲刷缓冲，plain sync `1T` 已反超 C++，`4T` 明显缩小差距。

当前剩余差距（只保留主计划内仍值得做的项）：

1. sync：plain steady-state 的固定成本已基本收敛，当前剩余主差距集中在 `4T` 场景下 `FileManager::runtime` 与活跃文件写入的串行区竞争。
2. benchmark：已具备多轮诊断矩阵，但仍缺少整理后的长期基线与自动回归门槛。
3. async：`AppenderEngine::state` 仍保留必要串行区，flush overlap 与单线程 p99 仍需要后续 profiling 决定是否继续拆分。

性能优化约束（必须遵守）：

1. 不修改协议格式：magic/header/tailer/seq/crypt 语义保持一致。
2. 不修改功能逻辑：flush/move_file、cache 迁移、文件命名、过期策略保持一致。
3. 不修改对外 API 行为：JNI/UniFFI/NAPI 与 `mars-xlog` 公开语义保持一致。
4. 优化只能发生在实现细节：生命周期管理、内存布局、拷贝路径、锁粒度、缓冲复用。

任务看板（仅保留当前仍需要跟踪的条目）：

| 状态 | 任务项 | 代码位置 | 验收与产物 |
| :--- | :--- | :--- | :--- |
| 已完成 | async mmap 持久化增量化 + flush 成功后只清理已用区间 | `crates/xlog-core/src/buffer.rs`、`crates/xlog-core/src/appender_engine.rs` | `cargo test -p mars-xlog-core buffer:: -- --nocapture` 通过；见 `20260306-perf6` |
| 已完成 | sync 活跃文件句柄复用 + append target cache | `crates/xlog-core/src/file_manager.rs`、`crates/xlog-core/src/oneshot.rs` | `cargo test -p mars-xlog-core file_manager:: -- --nocapture` 通过；见 `20260306-perf6` |
| 已完成 | async mmap persist cadence 调优 | `crates/xlog-core/src/appender_engine.rs` | `cargo test -p mars-xlog --lib -- --nocapture` 通过；见 `20260306-perf7` |
| 已完成 | 收窄 `RustBackend::async_state` 串行区，改为短锁 checkout + 锁外压缩/加密/engine 提交 | `crates/xlog/src/backend/rust.rs` | `cargo test -p mars-xlog --lib -- --nocapture` 通过；见 `20260306-perf8` |
| 已完成 | 收敛 finalize / recover / oneshot 边界复制与尾块处理 | `crates/xlog/src/backend/rust.rs`、`crates/xlog-core/src/buffer.rs`、`crates/xlog-core/src/oneshot.rs` | `cargo test -p mars-xlog-core --test mmap_recovery --test oneshot_flush -- --nocapture` 通过；见 `20260306-perf9` |
| 已完成 | `AppenderEngine` async flush worker 改为 busy 时 `try_lock + requeue` | `crates/xlog-core/src/appender_engine.rs` | `cargo test -p mars-xlog-core --test async_engine -- --nocapture` 通过 |
| 已完成 | 恢复独立 C++ backend benchmark harness，并补齐 threaded benchmark 入口 | `crates/xlog/Cargo.toml`、`crates/xlog/src/backend/mod.rs`、`crates/xlog/examples/bench_backend.rs`、`scripts/xlog/run_phase5_regression.sh` | `cargo check -p mars-xlog --example bench_backend --no-default-features --features rust-backend/cpp-backend` 通过；见 `20260306-p0p1wave/results_smoke.jsonl` |
| 已完成 | sync steady-state：`AppenderEngine` 锁外执行文件写入 + `FileManager` plain 热路径收敛到单次 runtime 锁 | `crates/xlog-core/src/appender_engine.rs`、`crates/xlog-core/src/file_manager.rs` | `cargo check -p mars-xlog-core -p mars-xlog`、`cargo test -p mars-xlog-core file_manager:: -- --nocapture` 通过；见 `20260306-syncsteady-perf2/results_raw.jsonl` |
| 已完成 | sync keep-open 活跃文件对齐 C++ `FILE*` 生命周期，改为用户态缓冲写 | `crates/xlog-core/src/file_manager.rs` | `cargo test -p mars-xlog-core file_manager:: -- --nocapture`、`cargo test -p mars-xlog --lib -- --nocapture` 通过；见 `20260306-syncbuffer-perf/summary.md` |
| 已完成 | 同轮 Rust/C++ 多轮诊断矩阵（1T/4T/flush overlap/rotate/cache） | `crates/xlog/examples/bench_backend.rs`、`artifacts/bench-compare/20260306-harness-matrix/*` | 产物：`results_raw.jsonl`、`summary.md` |
| 进行中 | 继续缩小 sync `4T` 与 C++ 的差距 | `crates/xlog-core/src/file_manager.rs`、`crates/xlog-core/src/appender_engine.rs` | 目标：进一步压缩多线程下活跃文件写入串行区 |
| 待执行 | 基于诊断矩阵继续定位 async flush overlap 与单线程 p99 | `crates/xlog-core/src/appender_engine.rs`、`crates/xlog/src/backend/rust.rs` | 目标：确定是否继续拆分 `EngineState` |

已从主计划移除（不再进入当前实现排期）：

1. `madvise/msync(MS_ASYNC)`、`sendfile/fcopyfile` 等 OS 指令级调优。
2. 通用 `Lock-Free/Atomic` 重构与 SIMD/TEA 指令级优化。
3. 泛化的 `Borrowed Bytes` / string interning 类重写。

Rust 实现层优化原则（当前仍有效）：

1. 优先优化生命周期、缓冲复用、拷贝路径和锁作用域，而不是引入新的协议或行为语义。
2. 所有性能优化都必须保持 crash-recovery、文件协议、cache/move/expire 规则不变。
3. 只有在 profiling 明确指向后，才考虑更激进的并发或 OS 层优化。

DoD：

- Rust 在目标平台达到并稳定满足 6.4 性能门槛。
- benchmark 产物同时包含 Rust/C++，可直接审计差异来源。
- 保持协议兼容、回归兼容、绑定层兼容全部通过。

---

## Phase 7：移除 C++ 依赖（1 周，后置）

目标：在性能对齐完成后，收尾默认构建链路，默认构建不再编译 Mars C++ xlog。

前置条件（必须同时满足）：

1. Phase 6 性能门槛达标并稳定通过。
2. Rust/C++ 双后端 A/B 回归连续通过（建议至少 7 天 nightly）。
3. 线上灰度窗口内无新增协议/行为回归。

任务：

1. 移除 `crates/xlog-sys` 在默认路径中的强依赖。
2. 清理 `build.rs` 的 C++/Boost 编译链路。
3. 调整 workspace 成员与 README。

涉及文件：

- 修改根 `Cargo.toml`
- 修改 `crates/xlog/Cargo.toml`
- 修改/冻结 `crates/xlog-sys/*`（legacy crate，转为可选/归档路径）
- 更新 `README.md`

DoD：

- 默认 `cargo build` 不依赖 C++14/Boost。
- `third_party/mars` 仅用于参考与兼容测试，不参与主构建。
- `mars-xlog-core` / `mars-xlog` / bindings 可按顺序执行 `cargo publish --dry-run`。

当前发布状态（2026-03-04）：

- `mars-xlog-core`：`cargo publish --dry-run` 通过。
- `mars-xlog`：需在 `mars-xlog-core` 真正发布后再 dry-run/publish（当前因 crates.io 尚无 `mars-xlog-core` 而失败）。
- `mars-xlog-uniffi` / `oh-xlog`：需在 `mars-xlog` 发布后再 dry-run/publish。
- `mars-xlog-sys`：当前 dry-run 验证失败（打包后找不到 `third_party/mars` 源码路径），应单独作为 legacy 发布问题处理，不阻塞 Rust 主链路发布。

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

### 6.4 性能门槛（强制）

- 吞吐不低于现网 C++ 的 90%
- p99 写入延迟不高于 C++ 的 110%
- crash 恢复成功率 100%

当前状态（2026-03-05）：

- 未达标（见 Phase 6 基线快照）。
- 基线产物：`artifacts/bench-compare/20260305-main/*.jsonl`。

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
3. Phase 5 切默认后，优先执行 Phase 6 性能对齐与双后端验证。
4. 仅在 Phase 6 达标后执行 Phase 7（移除 C++ 依赖）。

这个顺序可以保证：每个阶段都可验证、可回滚，不会出现“到最后才发现协议不兼容”的高风险收敛问题。
