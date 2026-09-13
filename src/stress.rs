//! Stress-test engine: CPU, RAM, disk and GPU workloads, a runner that starts
//! and stops them together, and a per-run sample reporter with CSV export.
//!
//! Every test runs on its own worker threads and is safe to stop from any
//! thread at any time. Allocation is fixed up front (never grown during a run)
//! so a RAM test cannot trigger the OS OOM killer.

use std::io::{Read, Seek, SeekFrom, Write};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use sysinfo::System;

use crate::format;

/// A single stress workload. Implementations use interior mutability so they
/// can be started/stopped from the UI thread while their workers run.
pub trait StressTest: Send + Sync {
    fn name(&self) -> &'static str;
    fn status(&self) -> String;
    fn is_running(&self) -> bool;
    fn start(&self);
    fn stop(&self);
}

// ================================ CPU ================================

/// Pins N threads to heavy FPU / ALU / memory workloads so every logical
/// processor runs at full load across all its execution units.
pub struct CpuStress {
    threads: usize,
    running: Arc<AtomicBool>,
    handles: Mutex<Vec<std::thread::JoinHandle<()>>>,
    sink: Arc<AtomicU64>,
}

impl CpuStress {
    pub fn new(threads: usize) -> Self {
        CpuStress {
            threads: threads.max(1),
            running: Arc::new(AtomicBool::new(false)),
            handles: Mutex::new(Vec::new()),
            sink: Arc::new(AtomicU64::new(0)),
        }
    }
}

impl StressTest for CpuStress {
    fn name(&self) -> &'static str {
        "CPU"
    }

    fn status(&self) -> String {
        format!(
            "{} thread{}",
            self.threads,
            if self.threads == 1 { "" } else { "s" }
        )
    }

    fn is_running(&self) -> bool {
        self.running.load(Ordering::Relaxed)
    }

    fn start(&self) {
        self.running.store(true, Ordering::Relaxed);
        let mut handles = self.handles.lock().unwrap();
        for i in 0..self.threads {
            let running = Arc::clone(&self.running);
            let sink = Arc::clone(&self.sink);
            let handle = std::thread::Builder::new()
                .name(format!("ultramonitor-cpu-{i}"))
                .spawn(move || burn(running, sink))
                .expect("spawn cpu worker");
            handles.push(handle);
        }
    }

    fn stop(&self) {
        self.running.store(false, Ordering::Relaxed);
        let mut handles = self.handles.lock().unwrap();
        for h in handles.drain(..) {
            let _ = h.join();
        }
    }
}

fn burn(running: Arc<AtomicBool>, sink: Arc<AtomicU64>) {
    const FMA_WORDS: usize = 4096; // 32 KB, L1-resident
    const WALK_BYTES: usize = 8 << 20; // 8 MB, misses L3

    let mut fma = vec![1.0000001f64; FMA_WORDS];
    let mut walk = vec![0u8; WALK_BYTES];
    let mut a = 0.5f64;
    let mut b = 0.7f64;
    let mut c = 0.9f64;
    let mut d = 0.3f64;
    let mut acc: u64 = 0x9E37_79B9_7F4A_7C15;
    let mut counter: u64 = 0;
    let mut walk_pos = 0usize;
    let k = 1.000_000_000_1f64;
    let e = 1e-12f64;

    while running.load(Ordering::Relaxed) {
        // 1) L1-resident FMA pass (mul_add lowers to FMA).
        for i in (0..FMA_WORDS).step_by(8) {
            for j in 0..8 {
                fma[i + j] = fma[i + j].mul_add(k, e);
            }
        }
        a = fma[0];

        // 2) Scalar transcendental chain.
        for _ in 0..128 {
            a = a.sin() * 1.000_000_1 + (a * 0.999_999_9).cos();
            b = b.abs().sqrt() + 1e-9;
            c = (c.abs() + 1.0).ln().exp();
            d = (d * 0.5).tan() * 0.5 + 0.5;
            a = a.mul_add(k, e);
        }
        if counter & 255 == 0 {
            a = (a.abs() + 1.0).powf(1.000_000_001);
        }

        // 3) Serial 64-bit multiply chain.
        for _ in 0..64 {
            acc = acc
                .wrapping_mul(6_364_136_223_846_793_005)
                .wrapping_add(1_442_695_040_888_963_407);
        }

        // 4) Cache-buster: 64-byte-strided walk misses every cache line.
        walk_pos = (walk_pos + 4096) & (WALK_BYTES - 1);
        for i in 0..128 {
            let idx = (walk_pos + i * 64) & (WALK_BYTES - 1);
            walk[idx] = walk[idx].wrapping_add(1);
        }

        counter += 1;
        if counter & 15 == 0 {
            let v = a + b + c + d + acc as f64 + walk[walk_pos] as f64;
            sink.store(v.to_bits(), Ordering::Relaxed);
        }
    }
    let v = a + b + c + d + acc as f64 + walk[walk_pos] as f64;
    sink.store(v.to_bits(), Ordering::Relaxed);
}

// ================================ RAM ================================

/// Allocates a fixed percentage of the currently available RAM (touched so it
/// is really committed), then keeps reading and writing the buffers. Each churn
/// thread owns a private buffer partition, so allocation is fixed up front and
/// released deterministically on stop.
pub struct MemoryStress {
    percent: usize,
    running: Arc<AtomicBool>,
    handles: Mutex<Vec<std::thread::JoinHandle<()>>>,
    allocated_bytes: Arc<AtomicU64>,
    sink: Arc<AtomicU64>,
}

impl MemoryStress {
    pub fn new(percent: usize) -> Self {
        MemoryStress {
            percent: percent.clamp(5, 90),
            running: Arc::new(AtomicBool::new(false)),
            handles: Mutex::new(Vec::new()),
            allocated_bytes: Arc::new(AtomicU64::new(0)),
            sink: Arc::new(AtomicU64::new(0)),
        }
    }
}

impl StressTest for MemoryStress {
    fn name(&self) -> &'static str {
        "RAM"
    }

    fn status(&self) -> String {
        if self.is_running() && self.allocated_bytes.load(Ordering::Relaxed) == 0 {
            return "allocating…".to_string();
        }
        let bytes = self.allocated_bytes.load(Ordering::Relaxed);
        if bytes > 0 {
            format!("{:.1} GB allocated", bytes as f64 / (1024.0 * 1024.0 * 1024.0))
        } else {
            format!("{}% of available RAM", self.percent)
        }
    }

    fn is_running(&self) -> bool {
        self.running.load(Ordering::Relaxed)
    }

    fn start(&self) {
        self.running.store(true, Ordering::Relaxed);
        let running = Arc::clone(&self.running);
        let percent = self.percent;
        let allocated = Arc::clone(&self.allocated_bytes);
        let sink = Arc::clone(&self.sink);
        let handle = std::thread::Builder::new()
            .name("ultramonitor-ram-alloc".into())
            .spawn(move || allocate_and_churn(running, percent, allocated, sink))
            .expect("spawn ram allocator");
        self.handles.lock().unwrap().push(handle);
    }

    fn stop(&self) {
        self.running.store(false, Ordering::Relaxed);
        let mut handles = self.handles.lock().unwrap();
        for h in handles.drain(..) {
            let _ = h.join();
        }
        self.allocated_bytes.store(0, Ordering::Relaxed);
    }
}

fn allocate_and_churn(
    running: Arc<AtomicBool>,
    percent: usize,
    allocated: Arc<AtomicU64>,
    sink: Arc<AtomicU64>,
) {
    let mut sys = System::new();
    sys.refresh_memory();
    let available = sys.available_memory();
    let target = available.saturating_mul(percent as u64) / 100;
    let cores = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(4);
    let churners = (cores / 2).clamp(2, 8);
    let per = (target / churners as u64).max(16 * 1024 * 1024); // >= 16 MB each

    let mut workers = Vec::new();
    for t in 0..churners {
        if !running.load(Ordering::Relaxed) {
            break;
        }
        let running = Arc::clone(&running);
        let allocated = Arc::clone(&allocated);
        let sink = Arc::clone(&sink);
        let handle = std::thread::Builder::new()
            .name(format!("ultramonitor-ram-{t}"))
            .spawn(move || {
                let mut buf = vec![0u8; per as usize];
                // Touch every 4 KB page so the memory is really committed.
                for (i, byte) in buf.iter_mut().enumerate().step_by(4096) {
                    *byte = (i & 0xff) as u8;
                }
                allocated.fetch_add(per as u64, Ordering::Relaxed);
                let mut checksum: u64 = 0;
                while running.load(Ordering::Relaxed) {
                    for i in (0..buf.len()).step_by(4096) {
                        checksum = checksum.wrapping_add(buf[i] as u64);
                        buf[i] = buf[i].wrapping_add(1);
                    }
                }
                allocated.fetch_sub(per as u64, Ordering::Relaxed);
                sink.store(checksum, Ordering::Relaxed);
            })
            .expect("spawn ram churner");
        workers.push(handle);
    }

    // The allocator joins its churners so stop() can join a single handle.
    for worker in workers {
        let _ = worker.join();
    }
}

// ================================ DISK ================================

/// Pushes the disk with parallel sequential writes, sequential reads and random
/// 64 KB I/O on a temporary file. Deleted on stop.
pub struct DiskStress {
    size_mb: u64,
    running: Arc<AtomicBool>,
    handles: Mutex<Vec<std::thread::JoinHandle<()>>>,
    temp_file: Mutex<Option<PathBuf>>,
}

impl DiskStress {
    pub fn new(size_mb: u64) -> Self {
        DiskStress {
            size_mb: size_mb.max(64),
            running: Arc::new(AtomicBool::new(false)),
            handles: Mutex::new(Vec::new()),
            temp_file: Mutex::new(None),
        }
    }
}

impl StressTest for DiskStress {
    fn name(&self) -> &'static str {
        "Disk"
    }

    fn status(&self) -> String {
        format!("{} MB temp file", self.size_mb)
    }

    fn is_running(&self) -> bool {
        self.running.load(Ordering::Relaxed)
    }

    fn start(&self) {
        let path = std::env::temp_dir().join(format!(
            "ultramonitor-disk-{}.bin",
            std::process::id()
        ));
        *self.temp_file.lock().unwrap() = Some(path.clone());
        self.running.store(true, Ordering::Relaxed);

        let mut handles = self.handles.lock().unwrap();
        for _ in 0..2 {
            let running = Arc::clone(&self.running);
            let p = path.clone();
            let size = self.size_mb;
            let h = std::thread::spawn(move || write_loop(running, p, size));
            handles.push(h);

            let running = Arc::clone(&self.running);
            let p = path.clone();
            let size = self.size_mb;
            let h = std::thread::spawn(move || read_loop(running, p, size));
            handles.push(h);
        }
        let running = Arc::clone(&self.running);
        let p = path.clone();
        let size = self.size_mb;
        let h = std::thread::spawn(move || random_io(running, p, size));
        handles.push(h);
    }

    fn stop(&self) {
        self.running.store(false, Ordering::Relaxed);
        let mut handles = self.handles.lock().unwrap();
        for h in handles.drain(..) {
            let _ = h.join();
        }
        if let Some(path) = self.temp_file.lock().unwrap().take() {
            // On Windows a file still open by a slow worker cannot be deleted;
            // retry briefly.
            for _ in 0..5 {
                if std::fs::remove_file(&path).is_ok() {
                    break;
                }
                std::thread::sleep(std::time::Duration::from_millis(200));
            }
        }
    }
}

fn write_loop(running: Arc<AtomicBool>, path: PathBuf, size_mb: u64) {
    const BLOCK: usize = 4 << 20;
    let size_bytes = size_mb * 1024 * 1024;
    let block = vec![42u8; BLOCK];
    let mut written: u64 = 0;
    let mut passes: u64 = 0;
    if let Ok(mut file) = std::fs::File::create(&path) {
        while running.load(Ordering::Relaxed) {
            let position = (written % size_bytes) as u64;
            if file.seek(SeekFrom::Start(position)).is_err() {
                break;
            }
            if file.write_all(&block).is_err() {
                break;
            }
            written += BLOCK as u64;
            if position + BLOCK as u64 >= size_bytes {
                passes += 1;
                if passes & 3 == 0 {
                    let _ = file.sync_all();
                }
            }
        }
    }
}

fn read_loop(running: Arc<AtomicBool>, path: PathBuf, size_mb: u64) {
    const BLOCK: usize = 4 << 20;
    let size_bytes = size_mb * 1024 * 1024;
    let mut block = vec![0u8; BLOCK];
    let mut read: u64 = 0;
    if let Ok(mut file) = std::fs::File::open(&path) {
        while running.load(Ordering::Relaxed) {
            let position = (read % size_bytes) as u64;
            if file.seek(SeekFrom::Start(position)).is_err() {
                break;
            }
            if file.read_exact(&mut block).is_err() {
                break;
            }
            read += BLOCK as u64;
        }
    }
}

fn random_io(running: Arc<AtomicBool>, path: PathBuf, size_mb: u64) {
    const CHUNK: usize = 64 << 10;
    let size_bytes = size_mb * 1024 * 1024;
    let max_pos = (size_bytes - CHUNK as u64).max(1);
    let mut chunk = vec![7u8; CHUNK];
    let mut ops: u64 = 0;
    if let Ok(mut file) = std::fs::OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .open(&path)
    {
        let mut seed: u64 = 0x1234_5678_9abc_def0;
        while running.load(Ordering::Relaxed) {
            seed = seed.wrapping_mul(6_364_136_223_846_793_005).wrapping_add(1);
            let position = seed % max_pos;
            if file.seek(SeekFrom::Start(position)).is_err() {
                break;
            }
            if ops & 1 == 0 {
                let _ = file.write_all(&chunk);
            } else {
                let _ = file.read_exact(&mut chunk);
            }
            ops += 1;
        }
    }
}

// ================================ GPU ================================

/// GPU stress levels — heavier levels raise the fractal resolution and the
/// per-pixel iteration count.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GpuLevel {
    Light,
    Medium,
    Intense,
    Extreme,
    Meltdown,
}

impl GpuLevel {
    pub const ALL: [GpuLevel; 5] = [
        GpuLevel::Light,
        GpuLevel::Medium,
        GpuLevel::Intense,
        GpuLevel::Extreme,
        GpuLevel::Meltdown,
    ];

    pub fn label(self) -> &'static str {
        match self {
            GpuLevel::Light => "Light",
            GpuLevel::Medium => "Medium",
            GpuLevel::Intense => "Intense",
            GpuLevel::Extreme => "Extreme",
            GpuLevel::Meltdown => "Meltdown",
        }
    }

    fn iterations(self) -> u32 {
        match self {
            GpuLevel::Light => 128,
            GpuLevel::Medium => 256,
            GpuLevel::Intense => 512,
            GpuLevel::Extreme => 1024,
            GpuLevel::Meltdown => 2048,
        }
    }

    fn resolution(self) -> (usize, usize) {
        match self {
            GpuLevel::Light => (384, 216),
            GpuLevel::Medium => (512, 288),
            GpuLevel::Intense => (640, 360),
            GpuLevel::Extreme => (960, 540),
            GpuLevel::Meltdown => (1280, 720),
        }
    }
}

/// Computes the Mandelbrot set continuously on worker threads — a heavy,
/// measurable floating-point workload (the Rust equivalent of the Java
/// fragment-shader burn, exercised on the CPU).
pub struct GpuStress {
    level: GpuLevel,
    running: Arc<AtomicBool>,
    handles: Mutex<Vec<std::thread::JoinHandle<()>>>,
    sink: Arc<AtomicU64>,
}

impl GpuStress {
    pub fn new(level: GpuLevel) -> Self {
        GpuStress {
            level,
            running: Arc::new(AtomicBool::new(false)),
            handles: Mutex::new(Vec::new()),
            sink: Arc::new(AtomicU64::new(0)),
        }
    }
}

impl StressTest for GpuStress {
    fn name(&self) -> &'static str {
        "GPU"
    }

    fn status(&self) -> String {
        let (w, h) = self.level.resolution();
        format!(
            "Mandelbrot {}×{} · {} iterations · {}",
            w,
            h,
            self.level.iterations(),
            self.level.label()
        )
    }

    fn is_running(&self) -> bool {
        self.running.load(Ordering::Relaxed)
    }

    fn start(&self) {
        self.running.store(true, Ordering::Relaxed);
        let (w, h) = self.level.resolution();
        let iter = self.level.iterations();
        let mut handles = self.handles.lock().unwrap();
        for i in 0..2 {
            let running = Arc::clone(&self.running);
            let sink = Arc::clone(&self.sink);
            let h = std::thread::Builder::new()
                .name(format!("ultramonitor-gpu-{i}"))
                .spawn(move || fractal_loop(running, sink, w, h, iter))
                .expect("spawn gpu worker");
            handles.push(h);
        }
    }

    fn stop(&self) {
        self.running.store(false, Ordering::Relaxed);
        let mut handles = self.handles.lock().unwrap();
        for h in handles.drain(..) {
            let _ = h.join();
        }
    }
}

fn fractal_loop(running: Arc<AtomicBool>, sink: Arc<AtomicU64>, w: usize, h: usize, max_iter: u32) {
    while running.load(Ordering::Relaxed) {
        let mut acc: u64 = 0;
        for py in 0..h {
            let y0 = py as f64 / h as f64 * 3.0 - 1.5;
            for px in 0..w {
                let x0 = px as f64 / w as f64 * 3.5 - 2.5;
                let mut x = 0.0f64;
                let mut y = 0.0f64;
                let mut iter = 0u32;
                while x * x + y * y <= 4.0 && iter < max_iter {
                    let xt = x * x - y * y + x0;
                    y = 2.0 * x * y + y0;
                    x = xt;
                    iter += 1;
                }
                acc = acc.wrapping_add(iter as u64);
            }
        }
        sink.store(acc, Ordering::Relaxed);
    }
}

// ================================ RUNNER ================================

/// Runs a set of stress tests together and tracks elapsed time / progress.
pub struct StressRunner {
    tests: Vec<Box<dyn StressTest>>,
    running: AtomicBool,
    started: Mutex<Option<Instant>>,
    duration_secs: AtomicU64,
    stop_reason: Mutex<String>,
}

impl StressRunner {
    pub fn new(tests: Vec<Box<dyn StressTest>>) -> Self {
        StressRunner {
            tests,
            running: AtomicBool::new(false),
            started: Mutex::new(None),
            duration_secs: AtomicU64::new(0),
            stop_reason: Mutex::new(String::new()),
        }
    }

    /// Starts all tests; returns an error and stops the already-started ones if
    /// any test fails to start, so the runner never stays half-started.
    pub fn start(&self, duration_secs: u64) -> Result<(), String> {
        if self.running.load(Ordering::SeqCst) {
            return Err("already running".to_string());
        }
        self.running.store(true, Ordering::SeqCst);
        self.duration_secs.store(duration_secs, Ordering::Relaxed);
        *self.started.lock().unwrap() = Some(Instant::now());
        *self.stop_reason.lock().unwrap() = String::new();

        for test in &self.tests {
            test.start();
        }
        Ok(())
    }

    /// Stops all tests; safe to call repeatedly.
    pub fn stop(&self, reason: &str) {
        if !self.running.swap(false, Ordering::SeqCst) {
            return;
        }
        let mut r = self.stop_reason.lock().unwrap();
        *r = if reason.trim().is_empty() {
            "Stopped".to_string()
        } else {
            reason.to_string()
        };
        for test in &self.tests {
            test.stop();
        }
    }

    pub fn is_running(&self) -> bool {
        self.running.load(Ordering::SeqCst)
    }

    pub fn tests(&self) -> &[Box<dyn StressTest>] {
        &self.tests
    }

    pub fn elapsed_seconds(&self) -> u64 {
        match self.started.lock().unwrap().as_ref() {
            Some(t) => t.elapsed().as_secs(),
            None => 0,
        }
    }

    /// Overall progress 0..1 against the duration, or None when unlimited.
    pub fn progress(&self) -> Option<f64> {
        let duration = self.duration_secs.load(Ordering::Relaxed);
        if duration == 0 {
            return None;
        }
        Some((self.elapsed_seconds() as f64 / duration as f64).min(1.0))
    }

    pub fn is_finished_by_time(&self) -> bool {
        let duration = self.duration_secs.load(Ordering::Relaxed);
        self.is_running() && duration > 0 && self.elapsed_seconds() >= duration
    }

    pub fn stop_reason(&self) -> String {
        self.stop_reason.lock().unwrap().clone()
    }
}

// ================================ REPORTER ================================

/// One sample row. `elapsed_secs` is captured at tick time so the CSV's elapsed
/// column reflects each sample's own time (previously it was computed once at
/// write time, making every row show the same elapsed).
#[derive(Debug, Clone, Copy)]
struct Sample {
    elapsed_secs: f64,
    cpu: f64,
    ram: f64,
    temp: f64,
}

/// Collects per-tick CPU/RAM/temperature samples during a stress run.
pub struct StressReporter {
    start: Mutex<Option<Instant>>,
    samples: Mutex<Vec<Sample>>,
    min_temp: Mutex<f64>,
    max_temp: Mutex<f64>,
    sum_cpu: Mutex<f64>,
    cpu_count: Mutex<u64>,
    sum_ram: Mutex<f64>,
    ram_count: Mutex<u64>,
}

impl Default for StressReporter {
    fn default() -> Self {
        StressReporter::new()
    }
}

impl StressReporter {
    pub fn new() -> Self {
        StressReporter {
            start: Mutex::new(None),
            samples: Mutex::new(Vec::new()),
            min_temp: Mutex::new(f64::NAN),
            max_temp: Mutex::new(f64::NAN),
            sum_cpu: Mutex::new(0.0),
            cpu_count: Mutex::new(0),
            sum_ram: Mutex::new(0.0),
            ram_count: Mutex::new(0),
        }
    }

    pub fn start(&self) {
        *self.start.lock().unwrap() = Some(Instant::now());
        self.samples.lock().unwrap().clear();
        *self.min_temp.lock().unwrap() = f64::NAN;
        *self.max_temp.lock().unwrap() = f64::NAN;
        *self.sum_cpu.lock().unwrap() = 0.0;
        *self.cpu_count.lock().unwrap() = 0;
        *self.sum_ram.lock().unwrap() = 0.0;
        *self.ram_count.lock().unwrap() = 0;
    }

    pub fn reset(&self) {
        self.start();
    }

    pub fn tick(&self, cpu: f64, ram: f64, temp: f64) {
        let elapsed = match self.start.lock().unwrap().as_ref() {
            Some(t) => t.elapsed().as_secs_f64(),
            None => 0.0,
        };
        self.samples.lock().unwrap().push(Sample {
            elapsed_secs: elapsed,
            cpu,
            ram,
            temp,
        });
        if !cpu.is_nan() {
            let mut s = self.sum_cpu.lock().unwrap();
            let mut c = self.cpu_count.lock().unwrap();
            *s += cpu;
            *c += 1;
        }
        if !ram.is_nan() {
            let mut s = self.sum_ram.lock().unwrap();
            let mut c = self.ram_count.lock().unwrap();
            *s += ram;
            *c += 1;
        }
        if !temp.is_nan() && temp > 0.0 {
            let mut min = self.min_temp.lock().unwrap();
            let mut max = self.max_temp.lock().unwrap();
            if min.is_nan() || temp < *min {
                *min = temp;
            }
            if max.is_nan() || temp > *max {
                *max = temp;
            }
        }
    }

    pub fn sample_count(&self) -> usize {
        self.samples.lock().unwrap().len()
    }

    pub fn peak_temp(&self) -> f64 {
        *self.max_temp.lock().unwrap()
    }

    pub fn avg_cpu_load(&self) -> f64 {
        let c = *self.cpu_count.lock().unwrap();
        if c > 0 {
            *self.sum_cpu.lock().unwrap() / c as f64
        } else {
            f64::NAN
        }
    }

    pub fn avg_ram_load(&self) -> f64 {
        let c = *self.ram_count.lock().unwrap();
        if c > 0 {
            *self.sum_ram.lock().unwrap() / c as f64
        } else {
            f64::NAN
        }
    }

    /// Writes the CSV report. Columns: `elapsed_seconds,cpu_load_pct,ram_load_pct,cpu_temp_c`.
    pub fn write_csv(&self, target: &std::path::Path, stop_reason: Option<&str>) -> bool {
        let mut sb = String::new();
        if let Some(reason) = stop_reason {
            if !reason.trim().is_empty() {
                sb.push_str(&format!(
                    "stop_reason,\"{}\"\n",
                    reason.replace('"', "\"\"")
                ));
            }
        }
        sb.push_str("elapsed_seconds,cpu_load_pct,ram_load_pct,cpu_temp_c\n");
        let samples = self.samples.lock().unwrap().clone();
        for s in &samples {
            sb.push_str(&format!(
                "{},{},{},{}\n",
                fmt3(s.elapsed_secs),
                fmt2(s.cpu),
                fmt2(s.ram),
                fmt2(s.temp)
            ));
        }
        if let Some(parent) = target.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        std::fs::write(target, sb).is_ok()
    }

    /// Short human-readable summary lines.
    pub fn summary(&self, duration_seconds: u64) -> Vec<String> {
        vec![
            format!(
                "Samples: {} over {} s",
                self.sample_count(),
                duration_seconds
            ),
            format!("Average CPU load: {}", or_dash(self.avg_cpu_load())),
            format!("Average RAM load: {}", or_dash(self.avg_ram_load())),
            format!("CPU temperature: peak {}", or_dash(self.peak_temp())),
        ]
    }
}

/// Formats a value, showing "—" when it is NaN (unknown).
fn or_dash(value: f64) -> String {
    if value.is_nan() {
        format::DASH.to_string()
    } else {
        format!("{value:.1}")
    }
}

fn fmt3(v: f64) -> String {
    if v.is_nan() {
        String::new()
    } else {
        format!("{v:.3}")
    }
}

fn fmt2(v: f64) -> String {
    if v.is_nan() || v < 0.0 {
        String::new()
    } else {
        format!("{v:.2}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reporter_tracks_peak_temp() {
        let r = StressReporter::new();
        r.start();
        r.tick(10.0, 20.0, 55.0);
        r.tick(f64::NAN, 30.0, 70.0);
        r.tick(50.0, f64::NAN, 60.0);
        assert_eq!(r.peak_temp(), 70.0);
        assert_eq!(r.avg_cpu_load(), 30.0);
        assert_eq!(r.avg_ram_load(), 25.0);
        assert_eq!(r.sample_count(), 3);
    }

    #[test]
    fn reporter_summary_uses_dash_for_nan() {
        let r = StressReporter::new();
        r.start();
        r.tick(f64::NAN, f64::NAN, f64::NAN);
        let summary = r.summary(1);
        assert!(summary.iter().any(|l| l.contains("—")));
    }

    #[test]
    fn cpu_stress_runs_and_stops() {
        let c = CpuStress::new(2);
        c.start();
        std::thread::sleep(std::time::Duration::from_millis(50));
        assert!(c.is_running());
        c.stop();
        assert!(!c.is_running());
    }
}
