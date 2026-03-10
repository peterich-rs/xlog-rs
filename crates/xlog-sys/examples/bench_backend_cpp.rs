use std::env;
use std::ffi::CString;
use std::fs;
use std::path::{Path, PathBuf};
use std::ptr;
use std::sync::{Arc, Barrier, Mutex};
use std::thread;
use std::time::Instant;

use libc::{c_int, c_long, timeval};
use mars_xlog_sys as sys;

const USAGE: &str = "\
Benchmark Mars C++ xlog backend write throughput and latency.

Usage:
  cargo run -p mars-xlog-sys --example bench_backend_cpp -- [options]

Options:
  --out-dir <path>         Output directory for logs (required)
  --prefix <name>          Log file prefix (default: bench)
  --messages <n>           Number of measured log records (default: 20000)
  --warmup <n>             Warmup records before measured phase (default: 500)
  --mode <async|sync>      Appender mode (default: async)
  --compress <zlib|zstd>   Compression mode (default: zlib)
  --compress-level <n>     Compression level (default: 6)
  --msg-size <n>           Payload size per line (default: 96)
  --payload-profile <name> Payload profile: compressible|semi_structured|human_text|high_entropy (default: compressible)
  --payload-seed <n>       Seed for payload generation (default: 20260306)
  --threads <n>            Worker threads (default: 1)
  --flush-every <n>        Call async flush every N writes per thread (default: 0 = disabled)
  --cache-dir <path>       Optional cache directory for mmap/log cache routing
  --cache-days <n>         Cache retention days when --cache-dir is set (default: 0)
  --max-file-size <n>      Max logfile size in bytes (default: 0 = disabled)
  --pub-key <hex>          Optional 128-char public key to enable crypto
  --time-buckets <n>       Number of timeline buckets to emit (default: 0 = disabled)
  --metrics-out <path>     Ignored (Rust metrics only)
  --json-pretty            Pretty-print JSON result
";

#[derive(Debug, Copy, Clone)]
enum AppenderMode {
    Async,
    Sync,
}

#[derive(Debug, Copy, Clone)]
enum CompressMode {
    Zlib,
    Zstd,
}

#[derive(Debug, Copy, Clone)]
enum PayloadProfile {
    Compressible,
    SemiStructured,
    HumanText,
    HighEntropy,
}

impl PayloadProfile {
    fn as_str(self) -> &'static str {
        match self {
            PayloadProfile::Compressible => "compressible",
            PayloadProfile::SemiStructured => "semi_structured",
            PayloadProfile::HumanText => "human_text",
            PayloadProfile::HighEntropy => "high_entropy",
        }
    }
}

#[derive(Debug)]
struct Options {
    out_dir: PathBuf,
    prefix: String,
    messages: usize,
    warmup: usize,
    mode: AppenderMode,
    compress: CompressMode,
    compress_level: i32,
    msg_size: usize,
    payload_profile: PayloadProfile,
    payload_seed: u64,
    threads: usize,
    flush_every: usize,
    cache_dir: Option<PathBuf>,
    cache_days: i32,
    max_file_size: i64,
    pub_key: Option<String>,
    time_buckets: usize,
    metrics_out: Option<PathBuf>,
    json_pretty: bool,
}

#[derive(Debug, Copy, Clone)]
struct LatencySample {
    end_ns: u64,
    lat_ns: u64,
}

struct ThreadResult {
    lat_ns: Vec<u64>,
    samples: Vec<LatencySample>,
}

#[derive(Default)]
struct BucketAccum {
    count: usize,
    lat_sum_ns: u128,
    latencies: Vec<u64>,
}

struct XorShift64 {
    state: u64,
}

impl XorShift64 {
    fn new(seed: u64) -> Self {
        let init = if seed == 0 {
            0x9E37_79B9_7F4A_7C15
        } else {
            seed
        };
        Self { state: init }
    }

    fn next_u64(&mut self) -> u64 {
        let mut x = self.state;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.state = x;
        x
    }
}

struct PayloadPool {
    payloads: Vec<String>,
}

impl PayloadPool {
    fn new(profile: PayloadProfile, msg_size: usize, seed: u64) -> Self {
        let mut rng = XorShift64::new(seed ^ ((msg_size as u64) << 32));
        let mut payloads = Vec::with_capacity(64);
        for idx in 0..64 {
            payloads.push(build_payload(profile, msg_size, idx, &mut rng));
        }
        if payloads.is_empty() {
            payloads.push("X".repeat(msg_size));
        }
        Self { payloads }
    }

    fn pick(&self, thread_idx: usize, local_idx: usize) -> &str {
        let n = self.payloads.len();
        let slot = ((thread_idx.wrapping_mul(131) ^ local_idx.wrapping_mul(911)) + local_idx) % n;
        &self.payloads[slot]
    }
}

struct CppLogger {
    instance: usize,
    prefix: CString,
    tag: CString,
    filename: CString,
    func_name: CString,
}

impl CppLogger {
    fn init(opts: &Options) -> Result<Arc<Self>, String> {
        let logdir = CString::new(opts.out_dir.to_string_lossy().as_bytes())
            .map_err(|_| "logdir contains NUL".to_string())?;
        let prefix =
            CString::new(opts.prefix.clone()).map_err(|_| "prefix contains NUL".to_string())?;
        let pub_key = match &opts.pub_key {
            Some(value) => {
                Some(CString::new(value.as_str()).map_err(|_| "pub_key contains NUL".to_string())?)
            }
            None => None,
        };
        let cache_dir = match &opts.cache_dir {
            Some(path) => Some(
                CString::new(path.to_string_lossy().as_bytes())
                    .map_err(|_| "cache_dir contains NUL".to_string())?,
            ),
            None => None,
        };

        let cfg = sys::MarsXlogConfig {
            mode: to_sys_mode(opts.mode) as c_int,
            logdir: logdir.as_ptr(),
            nameprefix: prefix.as_ptr(),
            pub_key: pub_key.as_ref().map(|v| v.as_ptr()).unwrap_or(ptr::null()),
            compress_mode: to_sys_compress(opts.compress) as c_int,
            compress_level: opts.compress_level,
            cache_dir: cache_dir
                .as_ref()
                .map(|v| v.as_ptr())
                .unwrap_or(ptr::null()),
            cache_days: opts.cache_days,
        };

        unsafe {
            sys::mars_xlog_appender_open(&cfg, sys::TLogLevel::kLevelInfo as c_int);
        }
        let instance =
            unsafe { sys::mars_xlog_new_instance(&cfg, sys::TLogLevel::kLevelInfo as c_int) };
        if instance == 0 {
            return Err("mars_xlog_new_instance failed".to_string());
        }
        unsafe {
            sys::mars_xlog_set_console_log_open(instance, 0);
            sys::mars_xlog_set_appender_mode(instance, to_sys_mode(opts.mode) as c_int);
        }
        if opts.max_file_size > 0 {
            unsafe {
                sys::mars_xlog_set_max_file_size(instance, opts.max_file_size as c_long);
            }
        }

        Ok(Arc::new(Self {
            instance,
            prefix,
            tag: CString::new("bench").expect("static tag"),
            filename: CString::new("bench_backend_cpp.rs").expect("static filename"),
            func_name: CString::new("emit").expect("static func_name"),
        }))
    }

    fn write_message(&self, msg: &str) {
        let c_msg = CString::new(msg).expect("message contains NUL");
        let mut tv = timeval {
            tv_sec: 0,
            tv_usec: 0,
        };
        unsafe {
            libc::gettimeofday(&mut tv, ptr::null_mut());
        }
        let info = sys::XLoggerInfo {
            level: sys::TLogLevel::kLevelInfo,
            tag: self.tag.as_ptr(),
            filename: self.filename.as_ptr(),
            func_name: self.func_name.as_ptr(),
            line: 1,
            timeval: tv,
            pid: -1,
            tid: -1,
            maintid: -1,
            traceLog: 0,
        };
        unsafe {
            sys::mars_xlog_write(self.instance, &info, c_msg.as_ptr());
        }
    }

    fn flush(&self, sync: bool) {
        unsafe {
            sys::mars_xlog_flush(self.instance, if sync { 1 } else { 0 });
        }
    }
}

impl Drop for CppLogger {
    fn drop(&mut self) {
        unsafe {
            sys::mars_xlog_flush(self.instance, 1);
            sys::mars_xlog_release_instance(self.prefix.as_ptr());
            sys::mars_xlog_appender_close();
        }
    }
}

fn main() {
    if let Err(err) = run() {
        eprintln!("{err}");
        std::process::exit(2);
    }
}

fn run() -> Result<(), String> {
    let opts = parse_args()?;
    let _ = &opts.metrics_out;
    fs::create_dir_all(&opts.out_dir)
        .map_err(|e| format!("create out dir failed {}: {e}", opts.out_dir.display()))?;
    if let Some(cache_dir) = &opts.cache_dir {
        fs::create_dir_all(cache_dir)
            .map_err(|e| format!("create cache dir failed {}: {e}", cache_dir.display()))?;
    }

    let initial_output_bytes = output_bytes(&opts.out_dir, opts.cache_dir.as_deref());

    let logger = CppLogger::init(&opts)?;

    let payload_pool = Arc::new(PayloadPool::new(
        opts.payload_profile,
        opts.msg_size,
        opts.payload_seed,
    ));

    let measured_work = split_work(opts.messages, opts.threads);
    let warmup_work = split_work(opts.warmup, opts.threads);
    let measured_offsets = split_offsets(&measured_work);

    let warmup_ready = Arc::new(Barrier::new(opts.threads + 1));
    let measured_start = Arc::new(Barrier::new(opts.threads + 1));
    let shared_start = Arc::new(Mutex::new(None::<Instant>));

    let mut handles = Vec::with_capacity(opts.threads);
    for thread_idx in 0..opts.threads {
        let logger = logger.clone();
        let payload_pool = payload_pool.clone();
        let warmup_ready = warmup_ready.clone();
        let measured_start = measured_start.clone();
        let shared_start = shared_start.clone();
        let measured_messages = measured_work[thread_idx];
        let warmup_messages = warmup_work[thread_idx];
        let start_idx = measured_offsets[thread_idx];
        let flush_every = opts.flush_every;
        let capture_timeline = opts.time_buckets > 0;

        handles.push(thread::spawn(move || {
            let mut warmup_written = 0usize;
            for local_idx in 0..warmup_messages {
                let payload = payload_pool.pick(thread_idx, local_idx);
                let msg = format!("BENCH|W|T{thread_idx:02}|{:08}|{}", local_idx, payload);
                logger.write_message(&msg);
                warmup_written += 1;
                if flush_every > 0 && warmup_written % flush_every == 0 {
                    mark_flush_every_hint();
                    logger.flush(false);
                }
            }

            warmup_ready.wait();
            measured_start.wait();
            let start = {
                let guard = shared_start.lock().expect("shared start lock poisoned");
                guard.expect("missing measured start instant")
            };

            let mut lat_ns = Vec::with_capacity(measured_messages);
            let mut samples = if capture_timeline {
                Vec::with_capacity(measured_messages)
            } else {
                Vec::new()
            };

            for local_idx in 0..measured_messages {
                let global_idx = start_idx + local_idx;
                let payload = payload_pool.pick(thread_idx, local_idx + warmup_messages);
                let msg = format!("BENCH|T{thread_idx:02}|{:08}|{}", global_idx, payload);
                let begin = Instant::now();
                logger.write_message(&msg);
                let lat = begin.elapsed().as_nanos() as u64;
                lat_ns.push(lat);
                if capture_timeline {
                    samples.push(LatencySample {
                        end_ns: start.elapsed().as_nanos() as u64,
                        lat_ns: lat,
                    });
                }
                if flush_every > 0 && (local_idx + 1) % flush_every == 0 {
                    mark_flush_every_hint();
                    logger.flush(false);
                }
            }

            ThreadResult { lat_ns, samples }
        }));
    }

    warmup_ready.wait();
    logger.flush(true);
    let output_bytes_after_warmup = output_bytes(&opts.out_dir, opts.cache_dir.as_deref());
    let start = Instant::now();
    {
        let mut guard = shared_start
            .lock()
            .map_err(|_| "shared start lock poisoned".to_string())?;
        *guard = Some(start);
    }
    measured_start.wait();

    let mut lat_ns = Vec::with_capacity(opts.messages);
    let mut samples = if opts.time_buckets > 0 {
        Vec::with_capacity(opts.messages)
    } else {
        Vec::new()
    };
    for handle in handles {
        let mut result = handle
            .join()
            .map_err(|_| "bench worker thread panicked".to_string())?;
        lat_ns.append(&mut result.lat_ns);
        samples.append(&mut result.samples);
    }

    logger.flush(true);
    let total_elapsed = start.elapsed();
    let output_bytes_end = output_bytes(&opts.out_dir, opts.cache_dir.as_deref());

    lat_ns.sort_unstable();
    let min = *lat_ns.first().unwrap_or(&0);
    let max = *lat_ns.last().unwrap_or(&0);
    let p50 = percentile_per_mille(&lat_ns, 500);
    let p95 = percentile_per_mille(&lat_ns, 950);
    let p99 = percentile_per_mille(&lat_ns, 990);
    let p999 = percentile_per_mille(&lat_ns, 999);
    let avg = mean_ns(&lat_ns);
    let stdev = stdev_ns(&lat_ns, avg);
    let throughput = opts.messages as f64 / total_elapsed.as_secs_f64();

    let measured_output_bytes = output_bytes_end.saturating_sub(output_bytes_after_warmup);
    let warmup_output_bytes = output_bytes_after_warmup.saturating_sub(initial_output_bytes);
    let output_bytes_total = output_bytes_end.saturating_sub(initial_output_bytes);
    let bytes_per_msg = if opts.messages == 0 {
        0.0
    } else {
        measured_output_bytes as f64 / opts.messages as f64
    };

    let bucket_lines = if opts.time_buckets > 0 {
        build_bucket_json(&samples, total_elapsed.as_nanos() as u64, opts.time_buckets)
    } else {
        Vec::new()
    };

    let mut json = String::new();
    json.push('{');
    append_json_str(&mut json, "backend", backend_name());
    append_json_num(&mut json, "messages", opts.messages as f64, 0);
    append_json_num(&mut json, "warmup", opts.warmup as f64, 0);
    append_json_str(&mut json, "mode", mode_name(opts.mode));
    append_json_str(&mut json, "compress", compress_name(opts.compress));
    append_json_num(&mut json, "compress_level", opts.compress_level as f64, 0);
    append_json_num(&mut json, "msg_size", opts.msg_size as f64, 0);
    append_json_str(&mut json, "payload_profile", opts.payload_profile.as_str());
    append_json_num(&mut json, "threads", opts.threads as f64, 0);
    append_json_num(&mut json, "flush_every", opts.flush_every as f64, 0);
    append_json_num(&mut json, "time_buckets", opts.time_buckets as f64, 0);
    append_json_num(
        &mut json,
        "elapsed_ms",
        total_elapsed.as_secs_f64() * 1000.0,
        3,
    );
    append_json_num(&mut json, "throughput_mps", throughput, 3);
    append_json_num(&mut json, "lat_min_ns", min as f64, 0);
    append_json_num(&mut json, "lat_avg_ns", avg, 3);
    append_json_num(&mut json, "lat_stdev_ns", stdev, 3);
    append_json_num(&mut json, "lat_p50_ns", p50 as f64, 0);
    append_json_num(&mut json, "lat_p95_ns", p95 as f64, 0);
    append_json_num(&mut json, "lat_p99_ns", p99 as f64, 0);
    append_json_num(&mut json, "lat_p999_ns", p999 as f64, 0);
    append_json_num(&mut json, "lat_max_ns", max as f64, 0);
    append_json_num(&mut json, "output_bytes", measured_output_bytes as f64, 0);
    append_json_num(
        &mut json,
        "warmup_output_bytes",
        warmup_output_bytes as f64,
        0,
    );
    append_json_num(
        &mut json,
        "output_bytes_total",
        output_bytes_total as f64,
        0,
    );
    append_json_num(&mut json, "bytes_per_msg", bytes_per_msg, 3);
    if let Some(pub_key) = &opts.pub_key {
        append_json_str(&mut json, "pub_key", pub_key);
    } else {
        append_json_null(&mut json, "pub_key");
    }
    if !bucket_lines.is_empty() {
        if !json.ends_with('{') {
            json.push(',');
        }
        json.push_str("\"timeline_buckets\":[");
        for (idx, line) in bucket_lines.iter().enumerate() {
            if idx > 0 {
                json.push(',');
            }
            json.push_str(line);
        }
        json.push(']');
    } else {
        append_json_array_empty(&mut json, "timeline_buckets");
    }
    json.push('}');

    if opts.json_pretty {
        println!("{}", pretty_json(&json));
    } else {
        println!("{json}");
    }
    Ok(())
}

fn parse_mode(input: &str) -> Result<AppenderMode, String> {
    match input {
        "async" => Ok(AppenderMode::Async),
        "sync" => Ok(AppenderMode::Sync),
        _ => Err(format!("invalid --mode value: {input}")),
    }
}

fn parse_compress(input: &str) -> Result<CompressMode, String> {
    match input {
        "zlib" => Ok(CompressMode::Zlib),
        "zstd" => Ok(CompressMode::Zstd),
        _ => Err(format!("invalid --compress value: {input}")),
    }
}

fn parse_payload_profile(input: &str) -> Result<PayloadProfile, String> {
    match input {
        "compressible" => Ok(PayloadProfile::Compressible),
        "semi_structured" => Ok(PayloadProfile::SemiStructured),
        "human_text" => Ok(PayloadProfile::HumanText),
        "high_entropy" => Ok(PayloadProfile::HighEntropy),
        _ => Err(format!("invalid --payload-profile value: {input}")),
    }
}

fn parse_args() -> Result<Options, String> {
    let mut out_dir: Option<PathBuf> = None;
    let mut prefix = "bench".to_string();
    let mut messages: usize = 20_000;
    let mut warmup: usize = 500;
    let mut mode = AppenderMode::Async;
    let mut compress = CompressMode::Zlib;
    let mut compress_level: i32 = 6;
    let mut msg_size = 96usize;
    let mut payload_profile = PayloadProfile::Compressible;
    let mut payload_seed = 20_260_306u64;
    let mut threads = 1usize;
    let mut flush_every = 0usize;
    let mut cache_dir: Option<PathBuf> = None;
    let mut cache_days = 0i32;
    let mut max_file_size = 0i64;
    let mut pub_key: Option<String> = None;
    let mut time_buckets = 0usize;
    let mut metrics_out: Option<PathBuf> = None;
    let mut json_pretty = false;

    let mut iter = env::args().skip(1);
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "--help" | "-h" => return Err(USAGE.to_string()),
            "--out-dir" => {
                let v = iter
                    .next()
                    .ok_or_else(|| "--out-dir requires a value".to_string())?;
                out_dir = Some(PathBuf::from(v));
            }
            "--prefix" => {
                prefix = iter
                    .next()
                    .ok_or_else(|| "--prefix requires a value".to_string())?;
            }
            "--messages" => {
                let v = iter
                    .next()
                    .ok_or_else(|| "--messages requires a value".to_string())?;
                messages = v
                    .parse::<usize>()
                    .map_err(|e| format!("invalid --messages value {v}: {e}"))?;
            }
            "--warmup" => {
                let v = iter
                    .next()
                    .ok_or_else(|| "--warmup requires a value".to_string())?;
                warmup = v
                    .parse::<usize>()
                    .map_err(|e| format!("invalid --warmup value {v}: {e}"))?;
            }
            "--mode" => {
                let v = iter
                    .next()
                    .ok_or_else(|| "--mode requires a value".to_string())?;
                mode = parse_mode(&v)?;
            }
            "--compress" => {
                let v = iter
                    .next()
                    .ok_or_else(|| "--compress requires a value".to_string())?;
                compress = parse_compress(&v)?;
            }
            "--compress-level" => {
                let v = iter
                    .next()
                    .ok_or_else(|| "--compress-level requires a value".to_string())?;
                compress_level = v
                    .parse::<i32>()
                    .map_err(|e| format!("invalid --compress-level value {v}: {e}"))?;
            }
            "--msg-size" => {
                let v = iter
                    .next()
                    .ok_or_else(|| "--msg-size requires a value".to_string())?;
                msg_size = v
                    .parse::<usize>()
                    .map_err(|e| format!("invalid --msg-size value {v}: {e}"))?;
            }
            "--payload-profile" => {
                let v = iter
                    .next()
                    .ok_or_else(|| "--payload-profile requires a value".to_string())?;
                payload_profile = parse_payload_profile(&v)?;
            }
            "--payload-seed" => {
                let v = iter
                    .next()
                    .ok_or_else(|| "--payload-seed requires a value".to_string())?;
                payload_seed = v
                    .parse::<u64>()
                    .map_err(|e| format!("invalid --payload-seed value {v}: {e}"))?;
            }
            "--threads" => {
                let v = iter
                    .next()
                    .ok_or_else(|| "--threads requires a value".to_string())?;
                threads = v
                    .parse::<usize>()
                    .map_err(|e| format!("invalid --threads value {v}: {e}"))?;
            }
            "--flush-every" => {
                let v = iter
                    .next()
                    .ok_or_else(|| "--flush-every requires a value".to_string())?;
                flush_every = v
                    .parse::<usize>()
                    .map_err(|e| format!("invalid --flush-every value {v}: {e}"))?;
            }
            "--cache-dir" => {
                let v = iter
                    .next()
                    .ok_or_else(|| "--cache-dir requires a value".to_string())?;
                cache_dir = Some(PathBuf::from(v));
            }
            "--cache-days" => {
                let v = iter
                    .next()
                    .ok_or_else(|| "--cache-days requires a value".to_string())?;
                cache_days = v
                    .parse::<i32>()
                    .map_err(|e| format!("invalid --cache-days value {v}: {e}"))?;
            }
            "--max-file-size" => {
                let v = iter
                    .next()
                    .ok_or_else(|| "--max-file-size requires a value".to_string())?;
                max_file_size = v
                    .parse::<i64>()
                    .map_err(|e| format!("invalid --max-file-size value {v}: {e}"))?;
            }
            "--pub-key" => {
                let v = iter
                    .next()
                    .ok_or_else(|| "--pub-key requires a value".to_string())?;
                if v.is_empty() {
                    pub_key = None;
                } else {
                    pub_key = Some(v);
                }
            }
            "--time-buckets" => {
                let v = iter
                    .next()
                    .ok_or_else(|| "--time-buckets requires a value".to_string())?;
                time_buckets = v
                    .parse::<usize>()
                    .map_err(|e| format!("invalid --time-buckets value {v}: {e}"))?;
            }
            "--metrics-out" => {
                let v = iter
                    .next()
                    .ok_or_else(|| "--metrics-out requires a value".to_string())?;
                metrics_out = Some(PathBuf::from(v));
            }
            "--json-pretty" => json_pretty = true,
            unknown => return Err(format!("unknown argument: {unknown}\n\n{USAGE}")),
        }
    }

    let out_dir = out_dir.ok_or_else(|| format!("--out-dir is required\n\n{USAGE}"))?;
    if messages == 0 {
        return Err("--messages must be > 0".to_string());
    }
    if msg_size == 0 {
        return Err("--msg-size must be > 0".to_string());
    }
    if threads == 0 {
        return Err("--threads must be > 0".to_string());
    }
    if cache_dir.is_none() && cache_days != 0 {
        return Err("--cache-days requires --cache-dir".to_string());
    }
    if max_file_size < 0 {
        return Err("--max-file-size must be >= 0".to_string());
    }
    if compress_level < 0 {
        return Err("--compress-level must be >= 0".to_string());
    }

    if metrics_out.is_some() {
        eprintln!("warning: --metrics-out ignored for C++ backend");
    }

    Ok(Options {
        out_dir,
        prefix,
        messages,
        warmup,
        mode,
        compress,
        compress_level,
        msg_size,
        payload_profile,
        payload_seed,
        threads,
        flush_every,
        cache_dir,
        cache_days,
        max_file_size,
        pub_key,
        time_buckets,
        metrics_out,
        json_pretty,
    })
}

fn build_payload(
    profile: PayloadProfile,
    msg_size: usize,
    idx: usize,
    rng: &mut XorShift64,
) -> String {
    match profile {
        PayloadProfile::Compressible => {
            let token = format!(
                "LV=I TAG=core EVT=steady RETRY=0 IDX={idx:04} PATH=/xlog/bench COMPONENT=writer;"
            );
            fit_to_size(token, msg_size, "AAAAAAAAAAAAAAAA")
        }
        PayloadProfile::SemiStructured => {
            let uid = (rng.next_u64() % 100_000) as usize;
            let seq = (rng.next_u64() % 10_000_000) as usize;
            let service = ["gateway", "storage", "auth", "router"][idx % 4];
            let level = ["INFO", "WARN", "DEBUG"][idx % 3];
            let seed = rng.next_u64();
            let body = format!(
                "{{\"service\":\"{service}\",\"level\":\"{level}\",\"uid\":{uid},\"seq\":{seq},\"ok\":true,\"trace\":\"{seed:016x}\"}}"
            );
            fit_to_size(body, msg_size, "X")
        }
        PayloadProfile::HumanText => {
            let fragments = [
                "user request completed after retry with stable latency; ",
                "disk pressure stayed low while queue depth remained predictable; ",
                "network jitter was visible but did not trigger timeout fallback; ",
                "flush cadence aligned with workload and avoided burst amplification; ",
            ];
            let mut out = String::new();
            while out.len() < msg_size {
                let pos = (rng.next_u64() as usize + idx) % fragments.len();
                out.push_str(fragments[pos]);
            }
            fit_to_size(out, msg_size, " ")
        }
        PayloadProfile::HighEntropy => {
            const ALPHABET: &[u8] =
                b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
            let mut out = String::with_capacity(msg_size);
            for _ in 0..msg_size {
                let i = (rng.next_u64() as usize) % ALPHABET.len();
                out.push(ALPHABET[i] as char);
            }
            out
        }
    }
}

fn fit_to_size(mut text: String, msg_size: usize, filler: &str) -> String {
    if text.len() > msg_size {
        text.truncate(msg_size);
        return text;
    }
    while text.len() < msg_size {
        let remain = msg_size - text.len();
        if remain >= filler.len() {
            text.push_str(filler);
        } else {
            text.push_str(&filler[..remain]);
        }
    }
    text
}

fn split_work(total: usize, threads: usize) -> Vec<usize> {
    let base = total / threads;
    let extra = total % threads;
    (0..threads)
        .map(|idx| base + usize::from(idx < extra))
        .collect()
}

fn split_offsets(chunks: &[usize]) -> Vec<usize> {
    let mut out = Vec::with_capacity(chunks.len());
    let mut acc = 0usize;
    for &chunk in chunks {
        out.push(acc);
        acc = acc.saturating_add(chunk);
    }
    out
}

fn mean_ns(values: &[u64]) -> f64 {
    if values.is_empty() {
        return 0.0;
    }
    values.iter().map(|&v| v as f64).sum::<f64>() / values.len() as f64
}

fn stdev_ns(values: &[u64], mean: f64) -> f64 {
    if values.is_empty() {
        return 0.0;
    }
    let variance = values
        .iter()
        .map(|&v| {
            let d = v as f64 - mean;
            d * d
        })
        .sum::<f64>()
        / values.len() as f64;
    variance.max(0.0).sqrt()
}

fn percentile_per_mille(sorted: &[u64], per_mille: usize) -> u64 {
    if sorted.is_empty() {
        return 0;
    }
    let clamped = per_mille.min(1000);
    let idx = ((sorted.len() - 1) * clamped) / 1000;
    sorted[idx]
}

fn build_bucket_json(samples: &[LatencySample], total_ns: u64, buckets: usize) -> Vec<String> {
    if buckets == 0 || samples.is_empty() {
        return Vec::new();
    }
    let bucket_count = buckets.max(1);
    let total_ns = total_ns.max(1);
    let bucket_width = (total_ns / bucket_count as u64).max(1);
    let mut acc = Vec::with_capacity(bucket_count);
    for _ in 0..bucket_count {
        acc.push(BucketAccum::default());
    }

    for sample in samples {
        let idx = if sample.end_ns == 0 {
            0
        } else {
            ((sample.end_ns - 1) / bucket_width).min((bucket_count - 1) as u64) as usize
        };
        let bucket = &mut acc[idx];
        bucket.count += 1;
        bucket.lat_sum_ns = bucket.lat_sum_ns.saturating_add(sample.lat_ns as u128);
        bucket.latencies.push(sample.lat_ns);
    }

    let mut out = Vec::with_capacity(bucket_count);
    for (idx, bucket) in acc.iter_mut().enumerate() {
        bucket.latencies.sort_unstable();
        let start_ns = idx as u64 * bucket_width;
        let end_ns = if idx + 1 == bucket_count {
            total_ns
        } else {
            ((idx + 1) as u64 * bucket_width).min(total_ns)
        };
        let duration_ns = end_ns.saturating_sub(start_ns).max(1);
        let bucket_mps = (bucket.count as f64) * 1_000_000_000.0 / duration_ns as f64;
        let lat_avg_ns = if bucket.count == 0 {
            0.0
        } else {
            bucket.lat_sum_ns as f64 / bucket.count as f64
        };
        let lat_p99_ns = percentile_per_mille(&bucket.latencies, 990);
        let lat_p999_ns = percentile_per_mille(&bucket.latencies, 999);
        out.push(format!(
            "{{\"bucket\":{},\"start_ms\":{:.3},\"end_ms\":{:.3},\"messages\":{},\"bucket_mps\":{:.3},\"bucket_lat_avg_ns\":{:.3},\"bucket_lat_p99_ns\":{},\"bucket_lat_p999_ns\":{}}}",
            idx,
            start_ns as f64 / 1_000_000.0,
            end_ns as f64 / 1_000_000.0,
            bucket.count,
            bucket_mps,
            lat_avg_ns,
            lat_p99_ns,
            lat_p999_ns
        ));
    }

    out
}

fn mode_name(mode: AppenderMode) -> &'static str {
    match mode {
        AppenderMode::Async => "async",
        AppenderMode::Sync => "sync",
    }
}

fn compress_name(mode: CompressMode) -> &'static str {
    match mode {
        CompressMode::Zlib => "zlib",
        CompressMode::Zstd => "zstd",
    }
}

fn to_sys_mode(mode: AppenderMode) -> sys::TAppenderMode {
    match mode {
        AppenderMode::Async => sys::TAppenderMode::kAppenderAsync,
        AppenderMode::Sync => sys::TAppenderMode::kAppenderSync,
    }
}

fn to_sys_compress(mode: CompressMode) -> sys::TCompressMode {
    match mode {
        CompressMode::Zlib => sys::TCompressMode::kZlib,
        CompressMode::Zstd => sys::TCompressMode::kZstd,
    }
}

fn json_escape(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    for ch in input.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if c.is_control() => out.push(' '),
            c => out.push(c),
        }
    }
    out
}

fn append_json_str(json: &mut String, key: &str, value: &str) {
    if !json.ends_with('{') {
        json.push(',');
    }
    json.push('"');
    json.push_str(key);
    json.push_str("\":\"");
    json.push_str(&json_escape(value));
    json.push('"');
}

fn append_json_num(json: &mut String, key: &str, value: f64, precision: usize) {
    if !json.ends_with('{') {
        json.push(',');
    }
    json.push('"');
    json.push_str(key);
    json.push_str("\":");
    if precision == 0 {
        json.push_str(&format!("{value:.0}"));
    } else {
        json.push_str(&format!("{value:.precision$}"));
    }
}

fn append_json_null(json: &mut String, key: &str) {
    if !json.ends_with('{') {
        json.push(',');
    }
    json.push('"');
    json.push_str(key);
    json.push_str("\":null");
}

fn append_json_array_empty(json: &mut String, key: &str) {
    if !json.ends_with('{') {
        json.push(',');
    }
    json.push('"');
    json.push_str(key);
    json.push_str("\":[]");
}

fn mark_flush_every_hint() {}

fn pretty_json(input: &str) -> String {
    let mut out = String::with_capacity(input.len() + input.len() / 2);
    let mut indent = 0usize;
    let mut in_string = false;
    let mut escaped = false;

    for ch in input.chars() {
        if in_string {
            out.push(ch);
            if escaped {
                escaped = false;
            } else if ch == '\\' {
                escaped = true;
            } else if ch == '"' {
                in_string = false;
            }
            continue;
        }

        match ch {
            '"' => {
                in_string = true;
                out.push(ch);
            }
            '{' | '[' => {
                out.push(ch);
                out.push('\n');
                indent += 1;
                out.push_str(&"  ".repeat(indent));
            }
            '}' | ']' => {
                out.push('\n');
                indent = indent.saturating_sub(1);
                out.push_str(&"  ".repeat(indent));
                out.push(ch);
            }
            ',' => {
                out.push(',');
                out.push('\n');
                out.push_str(&"  ".repeat(indent));
            }
            ':' => {
                out.push(':');
                out.push(' ');
            }
            _ => out.push(ch),
        }
    }

    out
}

fn output_bytes(out_dir: &Path, cache_dir: Option<&Path>) -> u64 {
    let mut total = dir_size(out_dir);
    if let Some(cache_dir) = cache_dir {
        total = total.saturating_add(dir_size(cache_dir));
    }
    total
}

fn dir_size(path: &Path) -> u64 {
    let meta = match fs::symlink_metadata(path) {
        Ok(meta) => meta,
        Err(_) => return 0,
    };
    if meta.is_file() {
        return meta.len();
    }
    if !meta.is_dir() {
        return 0;
    }

    let mut total = 0u64;
    let entries = match fs::read_dir(path) {
        Ok(entries) => entries,
        Err(_) => return 0,
    };
    for entry in entries.flatten() {
        total = total.saturating_add(dir_size(&entry.path()));
    }
    total
}

fn backend_name() -> &'static str {
    "cpp"
}
