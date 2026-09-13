//! Hardware monitoring backend backed by `sysinfo`.
//!
//! Provides CPU temperature/load/frequency, per-core load & frequency, RAM &
//! swap usage, disk usage and read/write rates, network download/upload rates
//! and component temperatures — plus a `system_info()` inventory.
//!
//! sysinfo does not expose battery / fan / voltage readings, so those sensors
//! are simply absent (rather than shown as permanent "—" clutter).

use std::collections::HashMap;
use std::time::{Duration, Instant};

use sysinfo::{Components, CpuRefreshKind, DiskRefreshKind, Disks, Networks, System};

use crate::format;

const BYTES_PER_MB: f64 = 1024.0 * 1024.0;
/// Per-core frequencies are refreshed at most every two seconds (Windows WMI
/// frequency queries can be slow).
const FREQ_INTERVAL: Duration = Duration::from_secs(2);

/// A single sensor sample. `value` is NaN when unavailable.
#[derive(Debug, Clone)]
pub struct SensorReading {
    pub key: String,
    pub name: String,
    pub unit: String,
    pub value: f64,
}

impl SensorReading {
    pub fn unavailable(key: &str, name: &str, unit: &str) -> Self {
        SensorReading {
            key: key.to_string(),
            name: name.to_string(),
            unit: unit.to_string(),
            value: f64::NAN,
        }
    }

    pub fn available(&self) -> bool {
        !self.value.is_nan() && !self.value.is_infinite()
    }
}

/// One row of the System Info tab.
#[derive(Debug, Clone)]
pub struct InfoEntry {
    pub section: String,
    pub key: String,
    pub label: String,
    pub value: String,
    pub live: bool,
}

/// Min / avg / max accumulator for a single sensor over a session.
#[derive(Debug, Clone, Default)]
pub struct SensorStats {
    min: f64,
    max: f64,
    sum: f64,
    count: u64,
}

impl SensorStats {
    pub fn update(&mut self, value: f64) {
        if value.is_nan() || value.is_infinite() {
            return;
        }
        if self.count == 0 {
            self.min = value;
            self.max = value;
        } else {
            self.min = self.min.min(value);
            self.max = self.max.max(value);
        }
        self.sum += value;
        self.count += 1;
    }

    pub fn available(&self) -> bool {
        self.count > 0
    }

    pub fn min(&self) -> f64 {
        if self.available() { self.min } else { f64::NAN }
    }
    pub fn max(&self) -> f64 {
        if self.available() { self.max } else { f64::NAN }
    }
    pub fn avg(&self) -> f64 {
        if self.available() { self.sum / self.count as f64 } else { f64::NAN }
    }
    pub fn reset(&mut self) {
        *self = SensorStats::default();
    }
}

/// Session-wide statistics for every known sensor key.
#[derive(Debug, Default)]
pub struct LiveStats {
    map: HashMap<String, SensorStats>,
}

impl LiveStats {
    pub fn update(&mut self, key: &str, value: f64) {
        self.map.entry(key.to_string()).or_default().update(value);
    }
    pub fn available(&self, key: &str) -> bool {
        self.map.get(key).map(|s| s.available()).unwrap_or(false)
    }
    pub fn min(&self, key: &str) -> f64 {
        self.map.get(key).map(|s| s.min()).unwrap_or(f64::NAN)
    }
    pub fn avg(&self, key: &str) -> f64 {
        self.map.get(key).map(|s| s.avg()).unwrap_or(f64::NAN)
    }
    pub fn max(&self, key: &str) -> f64 {
        self.map.get(key).map(|s| s.max()).unwrap_or(f64::NAN)
    }
    pub fn reset(&mut self) {
        self.map.clear();
    }
}

/// A display-ready sensor table row.
#[derive(Debug, Clone)]
pub struct SensorRow {
    pub name: String,
    pub unit: String,
    pub current: String,
    pub min: String,
    pub avg: String,
    pub max: String,
    pub available: bool,
}

impl SensorRow {
    pub fn of(reading: &SensorReading, stats: &LiveStats) -> Self {
        if !reading.available() {
            return SensorRow {
                name: reading.name.clone(),
                unit: reading.unit.clone(),
                current: format::DASH.to_string(),
                min: format::DASH.to_string(),
                avg: format::DASH.to_string(),
                max: format::DASH.to_string(),
                available: false,
            };
        }
        let unit = &reading.unit;
        SensorRow {
            name: reading.name.clone(),
            unit: unit.clone(),
            current: format!("{:.1} {}", reading.value, unit),
            min: stat(stats, &reading.key, unit, |s| s.min()),
            avg: stat(stats, &reading.key, unit, |s| s.avg()),
            max: stat(stats, &reading.key, unit, |s| s.max()),
            available: true,
        }
    }
}

fn stat<F: Fn(&SensorStats) -> f64>(
    stats: &LiveStats,
    key: &str,
    unit: &str,
    f: F,
) -> String {
    match stats.map.get(key) {
        Some(s) if s.available() => format!("{:.1} {}", f(s), unit),
        _ => format::DASH.to_string(),
    }
}

/// The live hardware monitor.
pub struct Monitor {
    sys: System,
    disks: Disks,
    networks: Networks,
    components: Components,
    counters: HashMap<String, (u64, Instant)>,
    last_freq: Instant,
    freq_cache: Vec<u64>,
    last_net_down: f64,
    last_net_up: f64,
}

impl Monitor {
    pub fn new() -> Self {
        let sys = System::new_all();
        let freq_cache = sys.cpus().iter().map(|c| c.frequency()).collect();
        Monitor {
            sys,
            disks: Disks::new_with_refreshed_list(),
            networks: Networks::new_with_refreshed_list(),
            components: Components::new_with_refreshed_list(),
            counters: HashMap::new(),
            last_freq: Instant::now(),
            freq_cache,
            last_net_down: f64::NAN,
            last_net_up: f64::NAN,
        }
    }

    /// Reads all sensors once.
    pub fn sample(&mut self) -> Vec<SensorReading> {
        let mut out = Vec::new();
        let now = Instant::now();

        // Refresh CPU. Frequency is refreshed on its own 2 s cadence (and also
        // refreshes usage), so we never issue both refreshes in one sample and
        // the usage delta always spans a full sample interval.
        let refresh_freq = now.duration_since(self.last_freq) >= FREQ_INTERVAL;
        if refresh_freq {
            self.sys.refresh_cpu_specifics(CpuRefreshKind::everything());
            self.freq_cache = self.sys.cpus().iter().map(|c| c.frequency()).collect();
            self.last_freq = now;
        } else {
            self.sys.refresh_cpu_usage();
        }
        self.sys.refresh_memory();
        self.disks.refresh_specifics(true, DiskRefreshKind::everything());
        self.networks.refresh(true);
        self.components.refresh(true);

        // CPU temperature.
        let temp = self.cpu_temp();
        out.push(SensorReading {
            key: "cpu.temp".into(),
            name: "CPU Temp".into(),
            unit: "°C".into(),
            value: temp,
        });

        // CPU load (system).
        out.push(SensorReading {
            key: "cpu.load".into(),
            name: "CPU Load".into(),
            unit: "%".into(),
            value: self.sys.global_cpu_usage() as f64,
        });

        // CPU frequency (average, GHz).
        let freq = self.avg_freq();
        out.push(SensorReading {
            key: "cpu.freq".into(),
            name: "CPU Frequency".into(),
            unit: "GHz".into(),
            value: freq,
        });

        // Per-core load & frequency.
        let cpus = self.sys.cpus();
        for (i, cpu) in cpus.iter().enumerate() {
            out.push(SensorReading {
                key: format!("cpu.core.{i}"),
                name: format!("Core {i}"),
                unit: "%".into(),
                value: cpu.cpu_usage() as f64,
            });
            let f = self.freq_cache.get(i).copied().unwrap_or(0);
            out.push(SensorReading {
                key: format!("cpu.core.{i}.freq"),
                name: format!("Core {i} Freq"),
                unit: "GHz".into(),
                value: if f > 0 { f as f64 / 1000.0 } else { f64::NAN },
            });
        }

        // RAM & swap.
        let total_mem = self.sys.total_memory();
        let used_mem = self.sys.used_memory();
        out.push(SensorReading {
            key: "ram.used".into(),
            name: "RAM Used".into(),
            unit: "GB".into(),
            value: used_mem as f64 / (1024.0 * 1024.0 * 1024.0),
        });
        out.push(SensorReading {
            key: "ram.load".into(),
            name: "RAM Load".into(),
            unit: "%".into(),
            value: if total_mem > 0 {
                used_mem as f64 * 100.0 / total_mem as f64
            } else {
                f64::NAN
            },
        });
        let swap_total = self.sys.total_swap();
        out.push(SensorReading {
            key: "swap.used".into(),
            name: "Swap Used".into(),
            unit: "GB".into(),
            value: if swap_total > 0 {
                self.sys.used_swap() as f64 / (1024.0 * 1024.0 * 1024.0)
            } else {
                f64::NAN
            },
        });

        // Disks: usage + read/write rates. Collect the immutable disk data
        // first (releasing the borrow) before computing rates with `&mut self`.
        let disk_data: Vec<(String, f64, u64, u64)> = self
            .disks
            .list()
            .iter()
            .map(|disk| {
                let name = disk_name(disk);
                let total = disk.total_space();
                let available = disk.available_space();
                let used_pct = if total > 0 {
                    (total - available) as f64 * 100.0 / total as f64
                } else {
                    f64::NAN
                };
                let usage = disk.usage();
                (name, used_pct, usage.total_read_bytes, usage.total_written_bytes)
            })
            .collect();
        for (name, used_pct, read_bytes, write_bytes) in disk_data {
            out.push(SensorReading {
                key: format!("disk.used.{name}"),
                name: format!("{name} Used"),
                unit: "%".into(),
                value: used_pct,
            });
            out.push(SensorReading {
                key: format!("disk.read.{name}"),
                name: format!("{name} Read"),
                unit: "MB/s".into(),
                value: self.rate(&format!("disk.{name}.read"), read_bytes),
            });
            out.push(SensorReading {
                key: format!("disk.write.{name}"),
                name: format!("{name} Write"),
                unit: "MB/s".into(),
                value: self.rate(&format!("disk.{name}.write"), write_bytes),
            });
        }

        // Network rates (sum across all interfaces).
        let total_rx: u64 = self.networks.list().values().map(|n| n.total_received()).sum();
        let total_tx: u64 = self.networks.list().values().map(|n| n.total_transmitted()).sum();
        let down = self.rate("net.recv", total_rx);
        let up = self.rate("net.sent", total_tx);
        self.last_net_down = down;
        self.last_net_up = up;
        out.push(SensorReading {
            key: "net.down".into(),
            name: "Network Down".into(),
            unit: "MB/s".into(),
            value: down,
        });
        out.push(SensorReading {
            key: "net.up".into(),
            name: "Network Up".into(),
            unit: "MB/s".into(),
            value: up,
        });

        out
    }

    /// Full system inventory, grouped into sections. Live rows are recomputed
    /// on every call; static rows are cached by sysinfo.
    pub fn system_info(&self) -> Vec<InfoEntry> {
        let mut out = Vec::new();
        let cpus = self.sys.cpus();
        let brand = cpus.first().map(|c| c.brand()).unwrap_or("").trim().to_string();
        let vendor = cpus.first().map(|c| c.vendor_id()).unwrap_or("").trim().to_string();

        let os_name = System::name().unwrap_or_else(|| "?".to_string());
        let os_ver = System::long_os_version().unwrap_or_else(|| "?".to_string());
        let kernel = System::kernel_version().unwrap_or_else(|| "?".to_string());
        let host = System::host_name().unwrap_or_else(|| "?".to_string());
        section(
            &mut out,
            "Operating System",
            &[
                e("os.name", "Name", os_name, false),
                e("os.version", "Version", os_ver, false),
                e("os.kernel", "Kernel", kernel, false),
                e("os.host", "Host", host, false),
                e("os.arch", "Architecture", System::cpu_arch(), false),
                e("os.uptime", "Uptime", format::time(System::uptime() as i64), true),
            ],
        );

        let threads = cpus.len();
        let cores = System::physical_core_count().unwrap_or(threads);
        section(
            &mut out,
            "Processor",
            &[
                e("cpu.name", "Name", if brand.is_empty() { "?".into() } else { brand }, false),
                e("cpu.vendor", "Vendor", if vendor.is_empty() { "?".into() } else { vendor }, false),
                e("cpu.cores", "Physical Cores", cores.to_string(), false),
                e("cpu.threads", "Logical Processors", threads.to_string(), false),
                e("cpu.freq", "Current Frequency", format::ghz(self.avg_freq() * 1e9), true),
                e("cpu.load", "Usage", format::percent(self.sys.global_cpu_usage() as f64), true),
            ],
        );

        let total_mem = self.sys.total_memory();
        let used_mem = self.sys.used_memory();
        let avail_mem = self.sys.available_memory();
        section(
            &mut out,
            "Memory",
            &[
                e("mem.total", "Total", format::bytes(total_mem), false),
                e("mem.used", "Used", format::bytes(used_mem), true),
                e("mem.available", "Available", format::bytes(avail_mem), true),
                e("mem.swap.total", "Swap Total", format::bytes(self.sys.total_swap()), false),
                e("mem.swap.used", "Swap Used", format::bytes(self.sys.used_swap()), true),
            ],
        );

        for (i, disk) in self.disks.list().iter().enumerate() {
            let name = disk_name(disk);
            let mount = disk.mount_point().to_string_lossy().to_string();
            let fs = disk.file_system().to_string_lossy().to_string();
            let total = disk.total_space();
            let avail = disk.available_space();
            let used = total.saturating_sub(avail);
            section(
                &mut out,
                &format!("Disk {}", i + 1),
                &[
                    e("disk.name", "Name", name, false),
                    e("disk.mount", "Mount Point", mount, false),
                    e("disk.fs", "File System", fs, false),
                    e("disk.size", "Size", format::bytes(total), false),
                    e("disk.used", "Used", format::bytes(used), true),
                    e("disk.usedpct", "Used %", format::percent(if total > 0 { used as f64 * 100.0 / total as f64 } else { f64::NAN }), true),
                    e("disk.free", "Free", format::bytes(avail), true),
                ],
            );
        }

        section(
            &mut out,
            "Network",
            &[
                e("net.down", "Download (total)", format::rate(self.last_net_down), true),
                e("net.up", "Upload (total)", format::rate(self.last_net_up), true),
            ],
        );

        for (i, component) in self.components.list().iter().enumerate() {
            let label = component.label();
            let temp = component.temperature();
            section(
                &mut out,
                "Thermal",
                &[e(
                    &format!("thermal.{i}"),
                    if label.is_empty() { format!("Sensor {i}") } else { label.to_string() },
                    match temp {
                        Some(t) => format!("{:.1} °C", t),
                        None => format::DASH.to_string(),
                    },
                    true,
                )],
            );
        }

        section(
            &mut out,
            "Runtime",
            &[e(
                "rt.rust",
                "Rust Version",
                format!("rustc {}", rustc_version()),
                false,
            )],
        );

        out
    }

    // ---- helpers ----

    /// Average CPU frequency in GHz (from the two-second cache).
    fn avg_freq(&self) -> f64 {
        let non_zero: Vec<u64> = self.freq_cache.iter().copied().filter(|&f| f > 0).collect();
        if non_zero.is_empty() {
            f64::NAN
        } else {
            let sum: u64 = non_zero.iter().sum();
            sum as f64 / non_zero.len() as f64 / 1000.0
        }
    }

    fn cpu_temp(&self) -> f64 {
        let mut best = f64::NAN;
        for c in self.components.list() {
            let label = c.label().to_lowercase();
            if label.contains("cpu") || label.contains("package") || label.contains("core") {
                if let Some(t) = c.temperature() {
                    if t > 0.0 {
                        best = t as f64;
                        break;
                    }
                }
            }
        }
        // Fall back to the first positive temperature if no CPU-labelled one.
        if best.is_nan() {
            for c in self.components.list() {
                if let Some(t) = c.temperature() {
                    if t > 0.0 {
                        best = t as f64;
                        break;
                    }
                }
            }
        }
        best
    }

    /// MB/s between the previous and current cumulative byte counter.
    fn rate(&mut self, bucket: &str, new_bytes: u64) -> f64 {
        let now = Instant::now();
        let prev = self.counters.insert(bucket.to_string(), (new_bytes, now));
        match prev {
            Some((old_bytes, old_time)) => {
                let dt = now.duration_since(old_time).as_secs_f64();
                if dt > 0.0 {
                    ((new_bytes.saturating_sub(old_bytes)) as f64 / BYTES_PER_MB / dt).max(0.0)
                } else {
                    f64::NAN
                }
            }
            None => f64::NAN,
        }
    }
}

fn disk_name(disk: &sysinfo::Disk) -> String {
    let name = disk.name().to_string_lossy().to_string();
    if name.trim().is_empty() {
        disk.mount_point().to_string_lossy().to_string()
    } else {
        name
    }
}

fn section(out: &mut Vec<InfoEntry>, name: &str, entries: &[InfoEntry]) {
    for entry in entries {
        let mut e = entry.clone();
        e.section = name.to_string();
        out.push(e);
    }
}

fn e(key: &str, label: impl Into<String>, value: String, live: bool) -> InfoEntry {
    InfoEntry {
        section: String::new(),
        key: key.to_string(),
        label: label.into(),
        value,
        live,
    }
}

fn rustc_version() -> String {
    let v = option_env!("CARGO_PKG_VERSION").unwrap_or("0.1.0");
    v.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stats_ignore_nan() {
        let mut s = SensorStats::default();
        s.update(f64::NAN);
        assert!(!s.available());
        s.update(10.0);
        s.update(20.0);
        assert_eq!(s.min(), 10.0);
        assert_eq!(s.max(), 20.0);
        assert_eq!(s.avg(), 15.0);
    }

    #[test]
    fn sensor_row_unavailable() {
        let r = SensorReading::unavailable("x", "X", "%");
        let stats = LiveStats::default();
        let row = SensorRow::of(&r, &stats);
        assert!(!row.available);
        assert_eq!(row.current, "—");
    }
}
